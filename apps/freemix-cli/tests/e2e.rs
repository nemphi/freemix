use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fm_model::{InputKind, SimulatedAudio, SimulatedVideo};
use fm_persistence::ProjectStore;
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, ClientType, CommandMessage, CommandPayload,
    CommandResult, EngineIdentity, EventCursor, EventMessage, EventPayload, HandshakeOutcome,
    HandshakeResponse, MAX_LINE_BYTES, OverlayStatus, ProtocolVersion, Role, RuntimeDomainBoundary,
    RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity, SnapshotMessage, SnapshotReason,
    WireInputId, WireMessage, decode_line, encode_line,
};
use fm_types::InputId;

static TEST_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const TEST_PEER_TIMEOUT: Duration = Duration::from_secs(4);
const TEST_CLI_TIMEOUT: Duration = Duration::from_secs(4);

struct FakeRemoteServer {
    address: SocketAddr,
    worker: JoinHandle<()>,
}

impl FakeRemoteServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_remote_sessions(&listener));
        Self { address, worker }
    }

    fn start_peer_event_interleave() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_peer_event_interleave(&listener));
        Self { address, worker }
    }

    fn start_silent_response() -> Self {
        Self::start_test_peer(|stream| {
            stream.set_read_timeout(Some(TEST_PEER_TIMEOUT)).unwrap();
            let mut reader = BufReader::new(stream);
            assert_handshake_request_viewer(read_message(&mut reader));
            let mut byte = [0];
            assert_eq!(reader.get_mut().read(&mut byte).unwrap(), 0);
        })
    }

    fn start_diagnostics(response_log_id: &'static str) -> Self {
        Self::start_test_peer(move |stream| serve_diagnostics_peer(stream, response_log_id))
    }

    fn start_unterminated_response() -> Self {
        Self::start_test_peer(|stream| {
            stream.set_read_timeout(Some(TEST_PEER_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TEST_PEER_TIMEOUT)).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            assert_handshake_request_viewer(read_message(&mut reader));
            if let Err(error) = writer.write_all(&vec![b'x'; MAX_LINE_BYTES + 1]) {
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                ));
            }
            let mut byte = [0];
            let _ = reader.get_mut().read(&mut byte);
        })
    }

    fn start_test_peer(handler: impl FnOnce(TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || handler(accept_test_peer(&listener)));
        Self { address, worker }
    }

    fn start_alpha_fade() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_alpha_fade(&listener));
        Self { address, worker }
    }

    fn start_slide() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_slide(&listener));
        Self { address, worker }
    }

    fn start_zoom() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_zoom(&listener));
        Self { address, worker }
    }

    fn start_audio_strip() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_audio_strip(&listener));
        Self { address, worker }
    }

    fn start_stinger() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_stinger(&listener));
        Self { address, worker }
    }

    fn start_stinger_configuration() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_stinger_configuration(&listener));
        Self { address, worker }
    }

    fn start_fade_to_black() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_fade_to_black(&listener));
        Self { address, worker }
    }

    fn start_manual_position() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_manual_position(&listener));
        Self { address, worker }
    }

    fn start_manual_alpha_fade() -> Self {
        Self::start_manual(
            fm_protocol::ManualTransitionKind::AlphaFade,
            "remote-manual-alpha",
        )
    }

    fn start_manual_slide() -> Self {
        Self::start_manual(
            fm_protocol::ManualTransitionKind::Slide,
            "remote-manual-slide",
        )
    }

    fn start_manual(kind: fm_protocol::ManualTransitionKind, key: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_manual_start(&listener, kind, key));
        Self { address, worker }
    }

    fn address(&self) -> String {
        self.address.to_string()
    }

    fn finish(self) {
        self.worker.join().unwrap();
    }
}

fn accept_test_peer(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + TEST_PEER_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::yield_now();
            }
            Err(error) => panic!("test peer did not connect: {error}"),
        }
    }
}

fn serve_remote_sessions(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let mut revision = 0;

    for session in 0..5 {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let role = if matches!(session, 0 | 4) {
            Role::Viewer
        } else {
            Role::Operator
        };
        assert_handshake_request_role(read_message(&mut reader), role);
        write_handshake_role(&mut writer, &engine, revision, role);

        if matches!(session, 0 | 4) {
            continue;
        }

        let WireMessage::Command(command) = read_message(&mut reader) else {
            panic!("expected remote command");
        };
        match session {
            1 => assert_command(
                &command,
                CommandPayload::SelectPreview { input: input(1) },
                "remote-preview",
                0,
            ),
            2 => assert_command(&command, CommandPayload::Cut, "remote-cut", 1),
            3 => assert_command(
                &command,
                CommandPayload::Fade { duration_frames: 4 },
                "remote-fade",
                2,
            ),
            _ => unreachable!(),
        }

        revision += 1;
        let result = CommandResult::Accepted {
            id: command.id.clone(),
            revision,
            scheduled_frame: Some(revision),
        };
        write_message(&mut writer, &WireMessage::CommandResult(result));
        write_command_events(
            &mut writer,
            &engine,
            revision,
            command.payload,
            input(1),
            input(1),
        );
    }
}

fn serve_diagnostics_peer(stream: TcpStream, response_log_id: &str) {
    let engine = EngineIdentity {
        engine_id: "diag-engine".into(),
        state_epoch: 7,
        log_id: "secret-log".into(),
    };
    stream.set_read_timeout(Some(TEST_PEER_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(TEST_PEER_TIMEOUT)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let hello = read_message(&mut reader);
    assert_handshake_request_viewer(hello);
    write_handshake_role(&mut writer, &engine, 0, Role::Viewer);
    let WireMessage::DiagnosticsRequest(request) = read_message(&mut reader) else {
        panic!("expected diagnostics request");
    };
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 0,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
            },
        }),
    );
    let mut response_engine = engine.clone();
    response_engine.log_id = response_log_id.into();
    write_message(
        &mut writer,
        &WireMessage::DiagnosticsResponse(fm_protocol::DiagnosticsResponse {
            protocol: CURRENT_PROTOCOL_VERSION,
            request_id: request.request_id,
            engine: response_engine,
            current_revision: 12,
            oldest_retained_revision: Some(3),
            newest_retained_revision: Some(12),
            subscriber_count: 2,
            retained_events_limit: 64,
            subscriber_limit: 8,
            subscriber_queue_limit: 16,
        }),
    );
}

fn assert_handshake_request_viewer(message: WireMessage) {
    assert_handshake_request_role(message, Role::Viewer);
}

fn assert_handshake_request_role(message: WireMessage, expected_role: Role) {
    let WireMessage::HandshakeRequest(hello) = message else {
        panic!("expected handshake request");
    };
    assert_eq!(hello.desired_role, expected_role);
    assert_eq!(hello.client_type, ClientType::Cli);
    assert_eq!(hello.protocol, CURRENT_PROTOCOL_VERSION);
    assert_eq!(hello.resume_cursor, None);
}

fn serve_peer_event_interleave(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_handshake_request(read_message(&mut reader));
    write_handshake(&mut writer, &engine, 0);
    let WireMessage::Command(command) = read_message(&mut reader) else {
        panic!("expected remote command");
    };
    assert_eq!(
        command.payload,
        CommandPayload::SelectPreview { input: input(2) }
    );
    assert_eq!(command.idempotency_key, "peer-event-interleave");
    assert_eq!(command.expected_revision, None);
    write_command_events(
        &mut writer,
        &engine,
        1,
        CommandPayload::Cut,
        input(2),
        input(1),
    );
    write_message(
        &mut writer,
        &WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 2,
            scheduled_frame: Some(2),
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 2,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(2),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }),
    );
    for (sequence, domain) in [(1, "audio"), (2, "switcher")] {
        write_message(
            &mut writer,
            &WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: server_identity(&engine),
                revision: 2,
                generation: 2,
                sequence,
                event: RuntimeLifecycleEvent::Realized {
                    domain: domain.into(),
                    manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                    fade_to_black: live_fade_to_black(),
                },
            }),
        );
    }
}

fn serve_fade_to_black(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_handshake_request(read_message(&mut reader));
    write_handshake_version_with_fade_to_black(
        &mut writer,
        &engine,
        0,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        live_fade_to_black(),
    );

    let WireMessage::Command(command) = read_message(&mut reader) else {
        panic!("expected remote FTB command");
    };
    assert_eq!(command.protocol, fm_protocol::CURRENT_PROTOCOL_VERSION);
    assert_eq!(
        command.payload,
        CommandPayload::FadeToBlack {
            active: true,
            duration_frames: 2,
        }
    );
    assert_eq!(command.idempotency_key, "remote-blackout");
    assert_eq!(command.expected_revision, Some(0));
    write_message(
        &mut writer,
        &WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 1,
            scheduled_frame: Some(1),
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 1,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: fm_protocol::FadeToBlackState {
                    target_active: true,
                    position: fm_protocol::FadeToBlackPosition::LIVE,
                },
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 1,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: fm_protocol::FadeToBlackState {
                    target_active: true,
                    position: fm_protocol::FadeToBlackPosition::BLACK,
                },
            },
        }),
    );
}

fn serve_alpha_fade(listener: &TcpListener) {
    serve_automatic_transition(
        listener,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        CommandPayload::AlphaFade { duration_frames: 3 },
        "remote-alpha-fade",
    );
}

fn serve_slide(listener: &TcpListener) {
    serve_automatic_transition(
        listener,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        CommandPayload::Slide { duration_frames: 3 },
        "remote-slide",
    );
}

fn serve_zoom(listener: &TcpListener) {
    serve_automatic_transition(
        listener,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        CommandPayload::Zoom { duration_frames: 3 },
        "remote-zoom",
    );
}

fn serve_audio_strip(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_handshake_request_role(read_message(&mut reader), Role::Audio);
    write_handshake_role(&mut writer, &engine, 0, Role::Audio);

    let WireMessage::Command(command) = read_message(&mut reader) else {
        panic!("expected remote input-audio-strip command");
    };
    assert_command(
        &command,
        CommandPayload::SetInputAudioStrip {
            input: input(2),
            gain_millidb: -6_000,
            balance_basis_points: 2_500,
            muted: true,
            soloed: true,
            follow_video: false,
            delay_samples: 2_400,
        },
        "remote-audio-strip",
        0,
    );
    write_message(
        &mut writer,
        &WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 1,
            scheduled_frame: Some(1),
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 1,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: vec![
                    fm_protocol::InputAudioStripStatus {
                        input: input(1),
                        gain_millidb: 0,
                        balance_basis_points: 0,
                        muted: false,
                        soloed: false,
                        follow_video: true,
                        delay_samples: 0,
                    },
                    fm_protocol::InputAudioStripStatus {
                        input: input(2),
                        gain_millidb: -6_000,
                        balance_basis_points: 2_500,
                        muted: true,
                        soloed: true,
                        follow_video: false,
                        delay_samples: 2_400,
                    },
                ],
            },
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 1,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "audio".into(),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
            },
        }),
    );
}

fn serve_stinger(listener: &TcpListener) {
    serve_automatic_transition(
        listener,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        CommandPayload::Stinger {
            slot: fm_protocol::WireStingerSlotId::new(8).unwrap(),
            duration_frames: 3,
        },
        "remote-stinger",
    );
}

fn serve_stinger_configuration(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let expected = [
        CommandPayload::ConfigureStinger {
            slot: fm_protocol::WireStingerSlotId::new(3).unwrap(),
            media_input: input(2),
            preload: true,
            cut_point_frames: 45,
            audio_policy: fm_protocol::StingerAudioPolicy::MixWithProgram,
            missing_media_fallback: fm_protocol::StingerMissingMediaFallback::Fade,
        },
        CommandPayload::RemoveStinger {
            slot: fm_protocol::WireStingerSlotId::new(3).unwrap(),
        },
    ];
    for (index, expected_payload) in expected.into_iter().enumerate() {
        let revision = index as u64;
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        assert_handshake_request(read_message(&mut reader));
        write_handshake(&mut writer, &engine, revision);
        let WireMessage::Command(command) = read_message(&mut reader) else {
            panic!("expected stinger command");
        };
        assert_command(
            &command,
            expected_payload,
            if index == 0 {
                "configure-stinger"
            } else {
                "remove-stinger"
            },
            revision,
        );
        let new_revision = revision + 1;
        write_message(
            &mut writer,
            &WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision: new_revision,
                scheduled_frame: None,
            }),
        );
        let cursor = EventCursor {
            engine: engine.clone(),
            revision: new_revision,
        };
        write_message(
            &mut writer,
            &WireMessage::Event(EventMessage {
                cursor,
                payload: EventPayload::StingerSlotsChanged {
                    program: input(1),
                    preview: input(2),
                    manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                    fade_to_black: live_fade_to_black(),
                    stingers: Vec::new(),
                    overlays: OverlayStatus::empty_channels(),
                    input_audio_strips: input_audio_strips(),
                },
            }),
        );
        write_message(
            &mut writer,
            &WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: server_identity(&engine),
                revision: new_revision,
                generation: new_revision,
                sequence: 1,
                event: RuntimeLifecycleEvent::Realized {
                    domain: "switcher".into(),
                    manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                    fade_to_black: live_fade_to_black(),
                },
            }),
        );
    }
}

fn serve_automatic_transition(
    listener: &TcpListener,
    protocol: ProtocolVersion,
    expected_payload: CommandPayload,
    expected_key: &str,
) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_handshake_request(read_message(&mut reader));
    write_handshake_version_with_fade_to_black(
        &mut writer,
        &engine,
        0,
        protocol,
        live_fade_to_black(),
    );

    let WireMessage::Command(command) = read_message(&mut reader) else {
        panic!("expected remote automatic transition command");
    };
    assert_eq!(command.protocol, protocol);
    assert_eq!(command.payload, expected_payload);
    assert_eq!(command.idempotency_key, expected_key);
    assert_eq!(command.expected_revision, Some(0));
    write_message(
        &mut writer,
        &WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 1,
            scheduled_frame: Some(1),
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 1,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 1,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Scheduled {
                domains: vec![RuntimeDomainBoundary {
                    domain: "switcher".into(),
                    boundary: 1,
                }],
            },
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 1,
            generation: 1,
            sequence: 2,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
            },
        }),
    );
}

fn serve_manual_start(
    listener: &TcpListener,
    kind: fm_protocol::ManualTransitionKind,
    expected_key: &str,
) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_handshake_request(read_message(&mut reader));
    write_handshake_version_with_fade_to_black(
        &mut writer,
        &engine,
        0,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        live_fade_to_black(),
    );

    let WireMessage::Command(command) = read_message(&mut reader) else {
        panic!("expected remote manual transition command");
    };
    assert_eq!(command.protocol, fm_protocol::CURRENT_PROTOCOL_VERSION);
    assert_eq!(
        command.payload,
        CommandPayload::StartManualTransition { kind }
    );
    assert_eq!(command.idempotency_key, expected_key);
    assert_eq!(command.expected_revision, Some(0));
    write_message(
        &mut writer,
        &WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 1,
            scheduled_frame: Some(1),
        }),
    );
    let manual_transition =
        fm_protocol::ManualTransitionStatus::Active(fm_protocol::ManualTransitionState {
            kind,
            from: input(1),
            to: input(2),
            interval_start: fm_protocol::ManualTransitionPosition::START,
            position: fm_protocol::ManualTransitionPosition::START,
        });
    write_message(
        &mut writer,
        &WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 1,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition,
                fade_to_black: live_fade_to_black(),
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }),
    );
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 1,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition,
                fade_to_black: live_fade_to_black(),
            },
        }),
    );
}

fn serve_manual_position(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_handshake_request(read_message(&mut reader));
    let initial_manual =
        fm_protocol::ManualTransitionStatus::Active(fm_protocol::ManualTransitionState {
            kind: fm_protocol::ManualTransitionKind::Fade,
            from: input(1),
            to: input(2),
            interval_start: fm_protocol::ManualTransitionPosition::START,
            position: fm_protocol::ManualTransitionPosition::START,
        });
    write_handshake_version_with_manual(
        &mut writer,
        &engine,
        0,
        fm_protocol::CURRENT_PROTOCOL_VERSION,
        initial_manual,
        Role::Operator,
    );

    let WireMessage::Command(command) = read_message(&mut reader) else {
        panic!("expected remote manual-position command");
    };
    assert_eq!(command.protocol, fm_protocol::CURRENT_PROTOCOL_VERSION);
    assert_eq!(
        command.payload,
        CommandPayload::SetManualTransitionPosition {
            position: fm_protocol::ManualTransitionPosition::END,
        }
    );
    assert_eq!(command.idempotency_key, "manual-endpoint");
    assert_eq!(command.expected_revision, Some(0));
    write_message(
        &mut writer,
        &WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 1,
            scheduled_frame: Some(1),
        }),
    );
    let desired_state = fm_protocol::ManualTransitionState {
        kind: fm_protocol::ManualTransitionKind::Fade,
        from: input(1),
        to: input(2),
        interval_start: fm_protocol::ManualTransitionPosition::START,
        position: fm_protocol::ManualTransitionPosition::END,
    };
    write_message(
        &mut writer,
        &WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 1,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: fm_protocol::ManualTransitionStatus::Active(desired_state),
                fade_to_black: live_fade_to_black(),
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }),
    );
    let realized =
        fm_protocol::ManualTransitionStatus::Active(fm_protocol::ManualTransitionState {
            interval_start: fm_protocol::ManualTransitionPosition::END,
            ..desired_state
        });
    write_message(
        &mut writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 1,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: realized,
                fade_to_black: live_fade_to_black(),
            },
        }),
    );
}

fn assert_handshake_request(message: WireMessage) {
    assert_handshake_request_role(message, Role::Operator);
}

fn write_handshake(writer: &mut TcpStream, engine: &EngineIdentity, revision: u64) {
    write_handshake_role(writer, engine, revision, Role::Operator);
}

fn write_handshake_role(
    writer: &mut TcpStream,
    engine: &EngineIdentity,
    revision: u64,
    role: Role,
) {
    write_handshake_version_with_manual(
        writer,
        engine,
        revision,
        CURRENT_PROTOCOL_VERSION,
        fm_protocol::ManualTransitionStatus::Inactive,
        role,
    );
}

fn write_handshake_version_with_manual(
    writer: &mut TcpStream,
    engine: &EngineIdentity,
    revision: u64,
    protocol: ProtocolVersion,
    manual_transition: fm_protocol::ManualTransitionStatus,
    role: Role,
) {
    let permissions = handshake_permissions(role);
    write_message(
        writer,
        &WireMessage::HandshakeResponse(HandshakeResponse {
            protocol,
            granted_role: role,
            permissions,
            capabilities: CapabilityReportSummary {
                digest: "fake-capabilities".into(),
                total: 0,
                available: 0,
                degraded: 0,
                unavailable: 0,
            },
            server: server_identity(engine),
            current_revision: revision,
            outcome: HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        }),
    );
    let preview = if revision == 0 { input(2) } else { input(1) };
    write_message(
        writer,
        &WireMessage::Snapshot(SnapshotMessage {
            engine: engine.clone(),
            revision,
            show_name: "Remote Contract".into(),
            inputs: input_statuses(),
            outputs: Vec::new(),
            input_audio_strips: input_audio_strips(),
            desired_program: input(1),
            desired_preview: preview,
            realized_program: input(1),
            realized_preview: preview,
            desired_manual_transition: manual_transition,
            realized_manual_transition: manual_transition,
            desired_fade_to_black: live_fade_to_black(),
            realized_fade_to_black: live_fade_to_black(),
            stingers: Vec::new(),
            desired_overlays: OverlayStatus::empty_channels(),
            realized_overlays: OverlayStatus::empty_channels(),
        }),
    );
}

fn handshake_permissions(role: Role) -> Vec<String> {
    match role {
        Role::Viewer => vec!["view_status".into()],
        Role::Audio => ["view_status", "control_audio"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        Role::Operator => [
            "view_status",
            "select_preview",
            "transition",
            "control_audio",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        _ => unreachable!("test handshake role must be Viewer, Audio, or Operator"),
    }
}

fn write_handshake_version_with_fade_to_black(
    writer: &mut TcpStream,
    engine: &EngineIdentity,
    revision: u64,
    protocol: ProtocolVersion,
    fade_to_black: fm_protocol::FadeToBlackState,
) {
    write_message(
        writer,
        &WireMessage::HandshakeResponse(HandshakeResponse {
            protocol,
            granted_role: Role::Operator,
            permissions: handshake_permissions(Role::Operator),
            capabilities: CapabilityReportSummary {
                digest: "fake-capabilities".into(),
                total: 0,
                available: 0,
                degraded: 0,
                unavailable: 0,
            },
            server: server_identity(engine),
            current_revision: revision,
            outcome: HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        }),
    );
    write_message(
        writer,
        &WireMessage::Snapshot(SnapshotMessage {
            engine: engine.clone(),
            revision,
            show_name: "Remote Contract".into(),
            inputs: input_statuses(),
            outputs: Vec::new(),
            input_audio_strips: input_audio_strips(),
            desired_program: input(1),
            desired_preview: input(2),
            realized_program: input(1),
            realized_preview: input(2),
            desired_manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
            realized_manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
            desired_fade_to_black: fade_to_black,
            realized_fade_to_black: fade_to_black,
            desired_overlays: OverlayStatus::empty_channels(),
            realized_overlays: OverlayStatus::empty_channels(),
            stingers: Vec::new(),
        }),
    );
}

fn live_fade_to_black() -> fm_protocol::FadeToBlackState {
    fm_protocol::FadeToBlackState {
        target_active: false,
        position: fm_protocol::FadeToBlackPosition::LIVE,
    }
}

fn assert_command(
    command: &CommandMessage,
    payload: CommandPayload,
    key: &str,
    expected_revision: u64,
) {
    assert_eq!(command.protocol, CURRENT_PROTOCOL_VERSION);
    assert_eq!(command.idempotency_key, key);
    assert_eq!(command.expected_revision, Some(expected_revision));
    assert_eq!(command.deadline_ms, None);
    assert_eq!(command.payload, payload);
}

fn write_command_events(
    writer: &mut TcpStream,
    engine: &EngineIdentity,
    revision: u64,
    payload: CommandPayload,
    program: WireInputId,
    preview: WireInputId,
) {
    let cursor = EventCursor {
        engine: engine.clone(),
        revision,
    };
    write_message(
        writer,
        &WireMessage::Event(EventMessage {
            cursor,
            payload: EventPayload::DesiredSwitcher {
                program,
                preview,
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }),
    );
    let generation = revision;
    let mut sequence = 1;
    if matches!(
        payload,
        CommandPayload::Fade { duration_frames } if duration_frames > 1
    ) {
        write_message(
            writer,
            &WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: server_identity(engine),
                revision,
                generation,
                sequence,
                event: RuntimeLifecycleEvent::Scheduled {
                    domains: vec![RuntimeDomainBoundary {
                        domain: "switcher".into(),
                        boundary: revision,
                    }],
                },
            }),
        );
        sequence += 1;
    }
    write_message(
        writer,
        &WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(engine),
            revision,
            generation,
            sequence,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: fm_protocol::ManualTransitionStatus::Inactive,
                fade_to_black: live_fade_to_black(),
            },
        }),
    );
}

fn server_identity(engine: &EngineIdentity) -> ServerIdentity {
    ServerIdentity {
        engine_id: engine.engine_id.clone(),
        project_id: "42".into(),
        state_epoch: engine.state_epoch,
        log_id: engine.log_id.clone(),
    }
}

fn read_message(reader: &mut impl BufRead) -> WireMessage {
    let mut line = String::new();
    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
    decode_line(&line).unwrap()
}

fn write_message(writer: &mut TcpStream, message: &WireMessage) {
    writer
        .write_all(encode_line(message).unwrap().as_bytes())
        .unwrap();
    writer.flush().unwrap();
}

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
}

fn input_statuses() -> Vec<fm_protocol::InputStatus> {
    [(1, "Camera"), (2, "Slides")]
        .into_iter()
        .map(|(value, name)| fm_protocol::InputStatus {
            input: input(value),
            name: name.into(),
        })
        .collect()
}

fn input_audio_strips() -> Vec<fm_protocol::InputAudioStripStatus> {
    [input(1), input(2)]
        .into_iter()
        .map(|input| fm_protocol::InputAudioStripStatus {
            input,
            gain_millidb: 0,
            balance_basis_points: 0,
            muted: false,
            soloed: false,
            follow_video: true,
            delay_samples: 0,
        })
        .collect()
}

#[test]
fn remote_commands_use_protocol_server() {
    let server = FakeRemoteServer::start();
    let address = server.address();
    let initial = invoke(&["remote-status", &address]);
    assert_success(&initial);
    assert_remote_status(&stdout(&initial), 0, 1, 1, 2, 2);

    let preview = invoke(&[
        "remote-preview",
        &address,
        "1",
        "--key",
        "remote-preview",
        "--expect",
        "0",
    ]);
    assert_success(&preview);
    assert_remote_status(&stdout(&preview), 1, 1, 1, 1, 1);

    let cut = invoke(&[
        "remote-cut",
        &address,
        "--key",
        "remote-cut",
        "--expect",
        "1",
    ]);
    assert_success(&cut);
    assert_remote_status(&stdout(&cut), 2, 1, 1, 1, 1);

    let fade = invoke(&[
        "remote-fade",
        &address,
        "4",
        "--key",
        "remote-fade",
        "--expect",
        "2",
    ]);
    assert_success(&fade);
    let final_status = stdout(&fade);
    assert_remote_status(&final_status, 3, 1, 1, 1, 1);

    let current = invoke(&["remote-status", &address]);
    assert_success(&current);
    assert_eq!(stdout(&current), final_status);

    server.finish();
}

#[test]
fn remote_commands_reject_non_loopback_addresses_before_connecting() {
    let output = invoke(&["remote-status", "192.0.2.1:9123"]);
    assert_failure_contains(&output, "requires a loopback address");
}

#[test]
fn remote_status_times_out_when_peer_sends_no_response() {
    let server = FakeRemoteServer::start_silent_response();
    let output = invoke_bounded(&["remote-status", &server.address()]);
    assert_failure_contains(&output, "daemon response timed out");
    server.finish();
}

#[test]
fn remote_diagnostics_reports_exact_identity_and_rejects_mismatched_log_id() {
    let server = FakeRemoteServer::start_diagnostics("secret-log");
    let address = server.address();
    let output = invoke_bounded(&["remote-diagnostics", &address]);
    server.finish();
    assert_success(&output);
    let rendered = stdout(&output);
    assert_eq!(
        rendered,
        "diagnostics=v1 engine_id=diag-engine state_epoch=7 revision=12 retained_oldest=3 retained_newest=12 subscribers=2/8 retained_limit=64 subscriber_queue=16"
    );
    assert!(!rendered.contains("log_id"));

    let server = FakeRemoteServer::start_diagnostics("mismatched-log");
    let output = invoke_bounded(&["remote-diagnostics", &server.address()]);
    server.finish();
    assert_failure_contains(&output, "diagnostics response does not match the request");
}

#[test]
fn remote_status_rejects_an_unterminated_oversized_record() {
    let server = FakeRemoteServer::start_unterminated_response();
    let output = invoke_bounded(&["remote-status", &server.address()]);
    assert_failure_contains(&output, "protocol line exceeds maximum length");
    server.finish();
}

#[test]
fn remote_manual_alpha_fade_preserves_kind_protocol_and_replicated_state() {
    let server = FakeRemoteServer::start_manual_alpha_fade();
    let output = invoke(&[
        "remote-tbar-start",
        &server.address(),
        "alpha-fade",
        "--key",
        "remote-manual-alpha",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    assert!(
        stdout(&output).contains("TBar(desired=alpha_fade:1->2@0, realized=alpha_fade:1->2@0)")
    );
    server.finish();
}

#[test]
fn remote_manual_slide_preserves_kind_protocol_and_replicated_state() {
    let server = FakeRemoteServer::start_manual_slide();
    let output = invoke(&[
        "remote-tbar-start",
        &server.address(),
        "slide",
        "--key",
        "remote-manual-slide",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    assert!(stdout(&output).contains("TBar(desired=slide:1->2@0, realized=slide:1->2@0)"));
    server.finish();
}

#[test]
fn remote_alpha_fade_preserves_duration_and_protocol() {
    let server = FakeRemoteServer::start_alpha_fade();
    let output = invoke(&[
        "remote-alpha-fade",
        &server.address(),
        "3",
        "--key",
        "remote-alpha-fade",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    server.finish();
}

#[test]
fn remote_slide_preserves_duration_and_protocol() {
    let server = FakeRemoteServer::start_slide();
    let output = invoke(&[
        "remote-slide",
        &server.address(),
        "3",
        "--key",
        "remote-slide",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    server.finish();
}

#[test]
fn remote_zoom_preserves_duration_and_protocol() {
    let server = FakeRemoteServer::start_zoom();
    let output = invoke(&[
        "remote-zoom",
        &server.address(),
        "3",
        "--key",
        "remote-zoom",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    server.finish();
}

#[test]
fn remote_audio_strip_preserves_controls_and_replicated_state() {
    let server = FakeRemoteServer::start_audio_strip();
    let output = invoke(&[
        "remote-audio-strip",
        &server.address(),
        "2",
        "-6000",
        "2500",
        "on",
        "on",
        "off",
        "2400",
        "--key",
        "remote-audio-strip",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    assert!(stdout(&output).contains(
        "AudioStrips=[1:\"Camera\":0:0:false:false:true:0,2:\"Slides\":-6000:2500:true:true:false:2400]"
    ));
    server.finish();
}

#[test]
fn remote_stinger_preserves_slot_duration_and_protocol() {
    let server = FakeRemoteServer::start_stinger();
    let output = invoke(&[
        "remote-stinger",
        &server.address(),
        "8",
        "3",
        "--key",
        "remote-stinger",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    server.finish();
}

#[test]
fn remote_stinger_configure_and_remove_preserve_current_protocol() {
    let server = FakeRemoteServer::start_stinger_configuration();
    let configured = invoke(&[
        "remote-stinger-configure",
        &server.address(),
        "3",
        "2",
        "true",
        "45",
        "mix-with-program",
        "fade",
        "--key",
        "configure-stinger",
        "--expect",
        "0",
    ]);
    assert_success(&configured);
    let removed = invoke(&[
        "remote-stinger-remove",
        &server.address(),
        "3",
        "--key",
        "remove-stinger",
        "--expect",
        "1",
    ]);
    assert_success(&removed);
    server.finish();
}

#[test]
fn remote_fade_to_black_preserves_target_duration_and_replicated_state() {
    let server = FakeRemoteServer::start_fade_to_black();
    let output = invoke(&[
        "remote-ftb",
        &server.address(),
        "black",
        "2",
        "--key",
        "remote-blackout",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    assert!(stdout(&output).contains("FTB(desired=black@0/65535, realized=black@65535/65535)"));
    server.finish();
}

#[test]
fn remote_t_bar_position_preserves_the_exact_endpoint_and_replicated_status() {
    let server = FakeRemoteServer::start_manual_position();
    let output = invoke(&[
        "remote-tbar-position",
        &server.address(),
        "10000",
        "--key",
        "manual-endpoint",
        "--expect",
        "0",
    ]);
    assert_success(&output);
    assert!(stdout(&output).contains("TBar(desired=fade:1->2@10000, realized=fade:1->2@10000)"));
    server.finish();
}

#[test]
fn remote_commands_accept_peer_events_while_waiting() {
    let server = FakeRemoteServer::start_peer_event_interleave();
    let output = invoke_bounded(&[
        "remote-preview",
        &server.address(),
        "2",
        "--key",
        "peer-event-interleave",
    ]);
    assert_success(&output);
    assert_remote_status(&stdout(&output), 2, 2, 2, 2, 2);
    server.finish();
}

#[test]
fn ppm_publication_complete_deterministic_mvp_contract() {
    let context = ContractContext::new();
    let (project_id, created_status) = assert_contract_creation(&context);
    assert_initial_contract_state(&context, project_id, &created_status);
    assert_contract_render(&context, &context.first_image, 3, 2, [73, 151, 199]);

    select_contract_preview(&context);
    assert_contract_rejections(&context);
    let (after_cut, cut_manifest) = cut_contract(&context);
    assert_duplicate_cut_replay(&context, &after_cut, &cut_manifest);
    assert_contract_render(&context, &context.second_image, 2, 2, [146, 46, 142]);
    fade_and_assert_final_contract(&context, project_id);

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_wipe_preserves_duration_and_idempotency_contract() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let wipe = invoke(&[
        "wipe",
        context.project_path(),
        "3",
        "--key",
        "wipe-three",
        "--expect",
        "0",
    ]);
    assert_success(&wipe);
    let wipe_status = stdout(&wipe);
    assert_status(&wipe_status, 1, 3, 2, 2, 1, 1);
    let wipe_manifest = manifest(&context.project);

    let duplicate = invoke(&[
        "wipe",
        context.project_path(),
        "99",
        "--key",
        "wipe-three",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), wipe_status);
    assert_eq!(manifest(&context.project), wipe_manifest);

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_alpha_fade_settles_and_preserves_idempotency_contract() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let alpha_fade = invoke(&[
        "alpha-fade",
        context.project_path(),
        "3",
        "--key",
        "alpha-fade-three",
        "--expect",
        "0",
    ]);
    assert_success(&alpha_fade);
    let alpha_fade_status = stdout(&alpha_fade);
    assert_status(&alpha_fade_status, 1, 3, 2, 2, 1, 1);
    let alpha_fade_manifest = manifest(&context.project);

    let duplicate = invoke(&[
        "alpha-fade",
        context.project_path(),
        "99",
        "--key",
        "alpha-fade-three",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), alpha_fade_status);
    assert_eq!(manifest(&context.project), alpha_fade_manifest);

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_slide_settles_and_preserves_idempotency_contract() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let slide = invoke(&[
        "slide",
        context.project_path(),
        "3",
        "--key",
        "slide-three",
        "--expect",
        "0",
    ]);
    assert_success(&slide);
    let slide_status = stdout(&slide);
    assert_status(&slide_status, 1, 3, 2, 2, 1, 1);
    let slide_manifest = manifest(&context.project);

    let duplicate = invoke(&[
        "slide",
        context.project_path(),
        "99",
        "--key",
        "slide-three",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), slide_status);
    assert_eq!(manifest(&context.project), slide_manifest);

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_zoom_settles_and_preserves_idempotency_contract() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let zoom = invoke(&[
        "zoom",
        context.project_path(),
        "3",
        "--key",
        "zoom-three",
        "--expect",
        "0",
    ]);
    assert_success(&zoom);
    let zoom_status = stdout(&zoom);
    assert_status(&zoom_status, 1, 3, 2, 2, 1, 1);
    let zoom_manifest = manifest(&context.project);

    let duplicate = invoke(&[
        "zoom",
        context.project_path(),
        "99",
        "--key",
        "zoom-three",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), zoom_status);
    assert_eq!(manifest(&context.project), zoom_manifest);

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_configured_stinger_settles_and_preserves_idempotency_contract() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));
    let configured = invoke(&[
        "stinger-configure",
        context.project_path(),
        "8",
        "2",
        "true",
        "1",
        "muted",
        "cut",
    ]);
    assert_success(&configured);
    assert!(stdout(&configured).contains("Stingers=[8:2:preload:1:muted:cut]"));

    let stinger = invoke(&[
        "stinger",
        context.project_path(),
        "8",
        "3",
        "--key",
        "stinger-eight",
        "--expect",
        "0",
    ]);
    assert_success(&stinger);
    let stinger_status = stdout(&stinger);
    assert_status(&stinger_status, 1, 3, 2, 2, 1, 1);
    let stinger_manifest = manifest(&context.project);
    assert!(stinger_manifest.contains(r#""slot": 8"#));
    assert!(stinger_manifest.contains(r#""cut_point_frames": 1"#));

    let duplicate = invoke(&[
        "stinger",
        context.project_path(),
        "8",
        "99",
        "--key",
        "stinger-eight",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), stinger_status);
    assert_eq!(manifest(&context.project), stinger_manifest);

    let reconfigured = invoke(&[
        "stinger-configure",
        context.project_path(),
        "8",
        "1",
        "false",
        "9",
        "mix-with-program",
        "keep-program",
    ]);
    assert_success(&reconfigured);
    assert_status(&stdout(&reconfigured), 1, 3, 2, 2, 1, 1);
    assert!(
        stdout(&reconfigured).contains("Stingers=[8:1:deferred:9:mix-with-program:keep-program]")
    );
    let reconfigured_manifest = manifest(&context.project);
    assert_eq!(reconfigured_manifest.matches(r#""slot": 8"#).count(), 1);
    assert!(reconfigured_manifest.contains(r#""preload": false"#));
    assert!(reconfigured_manifest.contains(r#""cut_point_frames": 9"#));

    let removed = invoke(&["stinger-remove", context.project_path(), "8"]);
    assert_success(&removed);
    assert_status(&stdout(&removed), 1, 3, 2, 2, 1, 1);
    assert!(stdout(&removed).contains("Stingers=[]"));
    assert!(manifest(&context.project).contains(r#""stingers": []"#));

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_fade_to_black_settles_persists_and_reverses() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let black = invoke(&[
        "ftb",
        context.project_path(),
        "black",
        "3",
        "--key",
        "local-blackout",
        "--expect",
        "0",
    ]);
    assert_success(&black);
    assert_status(&stdout(&black), 1, 3, 1, 1, 2, 2);
    assert!(stdout(&black).contains("FTB(desired=black@65535/65535, realized=black@65535/65535)"));
    assert_eq!(status(&context.project), stdout(&black));
    let stored = manifest(&context.project);
    assert!(stored.contains(r#""desired": {"target_active": true, "position_numerator": 65535}"#));
    assert!(stored.contains(r#""realized": {"target_active": true, "position_numerator": 65535}"#));

    let repeated = invoke(&[
        "ftb",
        context.project_path(),
        "black",
        "99",
        "--key",
        "local-blackout-repeat",
        "--expect",
        "1",
    ]);
    assert_success(&repeated);
    assert_status(&stdout(&repeated), 2, 4, 1, 1, 2, 2);
    assert!(
        stdout(&repeated).contains("FTB(desired=black@65535/65535, realized=black@65535/65535)")
    );

    let live = invoke(&[
        "ftb",
        context.project_path(),
        "live",
        "2",
        "--key",
        "local-live",
        "--expect",
        "2",
    ]);
    assert_success(&live);
    assert_status(&stdout(&live), 3, 6, 1, 1, 2, 2);
    assert!(stdout(&live).contains("FTB(desired=live@0/65535, realized=live@0/65535)"));
    assert_eq!(status(&context.project), stdout(&live));

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_input_add_persists_default_simulated_strip() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));
    let name = "Exact  input name  ";
    let added = invoke(&["input-add", context.project_path(), "3", name]);
    assert_success(&added);

    let stored = ProjectStore::new(&context.project).unwrap().load().unwrap();
    let input = stored
        .project()
        .inputs()
        .iter()
        .find(|input| input.id == InputId::new(NonZeroU128::new(3).unwrap()))
        .unwrap();
    assert_eq!(input.name, name);
    assert!(matches!(
        &input.kind,
        InputKind::Simulated(simulated)
            if simulated.video == SimulatedVideo::Bars
                && simulated.audio == SimulatedAudio::Silence
    ));
    assert_eq!(
        stored.project().input_audio_strip(input.id).unwrap(),
        Default::default()
    );

    let before_duplicate = manifest(&context.project);
    let duplicate = invoke(&["input-add", context.project_path(), "3", "Other"]);
    assert_failure_contains(&duplicate, "domain project failed validation");
    assert_eq!(manifest(&context.project), before_duplicate);
    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_input_duplicate_copies_source_and_strip() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));
    assert_success(&invoke(&[
        "audio-strip",
        context.project_path(),
        "2",
        "-1200",
        "2500",
        "on",
        "off",
        "off",
        "480",
    ]));

    let duplicate = invoke(&[
        "input-duplicate",
        context.project_path(),
        "2",
        "3",
        "Duplicated source",
    ]);
    assert_success(&duplicate);

    let stored = ProjectStore::new(&context.project).unwrap().load().unwrap();
    let inputs = stored.project().inputs();
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.id.get().get())
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let source = &inputs[1];
    let copy = &inputs[2];
    assert_eq!(copy.name, "Duplicated source");
    assert_eq!(copy.kind, source.kind);
    assert_eq!(copy.required_capabilities, source.required_capabilities);
    assert_eq!(
        stored.project().input_audio_strip(copy.id),
        stored.project().input_audio_strip(source.id)
    );
    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_manual_t_bar_holds_reverses_cancels_commits_and_survives_each_restart() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let start_wipe = invoke(&[
        "tbar-start",
        context.project_path(),
        "wipe",
        "--key",
        "manual-wipe-start",
        "--expect",
        "0",
    ]);
    assert_success(&start_wipe);
    assert!(stdout(&start_wipe).contains("TBar(desired=wipe:1->2@0, realized=wipe:1->2@0)"));
    assert_eq!(status(&context.project), stdout(&start_wipe));

    let endpoint = invoke(&[
        "tbar-position",
        context.project_path(),
        "10000",
        "--key",
        "manual-end",
        "--expect",
        "1",
    ]);
    assert_success(&endpoint);
    assert!(stdout(&endpoint).contains("TBar(desired=wipe:1->2@10000, realized=wipe:1->2@10000)"));
    assert_eq!(status(&context.project), stdout(&endpoint));
    let endpoint_manifest = manifest(&context.project);
    assert!(endpoint_manifest.contains(
        r#""desired": {"kind": "wipe", "from_id": 1, "to_id": 2, "interval_start_basis_points": 0, "position_basis_points": 10000}"#
    ));
    assert!(endpoint_manifest.contains(
        r#""realized": {"kind": "wipe", "from_id": 1, "to_id": 2, "interval_start_basis_points": 10000, "position_basis_points": 10000}"#
    ));

    let reversed = invoke(&[
        "tbar-position",
        context.project_path(),
        "2500",
        "--key",
        "manual-reverse",
        "--expect",
        "2",
    ]);
    assert_success(&reversed);
    assert!(stdout(&reversed).contains("TBar(desired=wipe:1->2@2500, realized=wipe:1->2@2500)"));
    assert_eq!(status(&context.project), stdout(&reversed));

    let cancelled = invoke(&[
        "tbar-cancel",
        context.project_path(),
        "--key",
        "manual-cancel",
        "--expect",
        "3",
    ]);
    assert_success(&cancelled);
    assert_status(&stdout(&cancelled), 4, 4, 1, 1, 2, 2);
    assert!(stdout(&cancelled).contains("TBar(desired=inactive, realized=inactive)"));

    for (command, revision, key) in [
        ("tbar-start", "4", "manual-fade-start"),
        ("tbar-position", "5", "manual-fade-end"),
    ] {
        let value = if command == "tbar-start" {
            "fade"
        } else {
            "10000"
        };
        let output = invoke(&[
            command,
            context.project_path(),
            value,
            "--key",
            key,
            "--expect",
            revision,
        ]);
        assert_success(&output);
    }
    let committed = invoke(&[
        "tbar-commit",
        context.project_path(),
        "--key",
        "manual-commit",
        "--expect",
        "6",
    ]);
    assert_success(&committed);
    assert_status(&stdout(&committed), 7, 7, 2, 2, 1, 1);
    assert!(stdout(&committed).contains("TBar(desired=inactive, realized=inactive)"));
    assert_eq!(status(&context.project), stdout(&committed));

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_manual_alpha_fade_survives_restart_and_replays_idempotently() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let started = invoke(&[
        "tbar-start",
        context.project_path(),
        "alpha-fade",
        "--key",
        "manual-alpha-start",
        "--expect",
        "0",
    ]);
    assert_success(&started);
    assert!(
        stdout(&started).contains("TBar(desired=alpha_fade:1->2@0, realized=alpha_fade:1->2@0)")
    );
    assert_eq!(status(&context.project), stdout(&started));

    let positioned = invoke(&[
        "tbar-position",
        context.project_path(),
        "6250",
        "--key",
        "manual-alpha-position",
        "--expect",
        "1",
    ]);
    assert_success(&positioned);
    let positioned_status = stdout(&positioned);
    assert!(
        positioned_status
            .contains("TBar(desired=alpha_fade:1->2@6250, realized=alpha_fade:1->2@6250)")
    );
    assert_eq!(status(&context.project), positioned_status);
    let positioned_manifest = manifest(&context.project);
    assert!(positioned_manifest.contains(
        r#""desired": {"kind": "alpha_fade", "from_id": 1, "to_id": 2, "interval_start_basis_points": 0, "position_basis_points": 6250}"#
    ));
    assert!(positioned_manifest.contains(
        r#""realized": {"kind": "alpha_fade", "from_id": 1, "to_id": 2, "interval_start_basis_points": 6250, "position_basis_points": 6250}"#
    ));

    let repeated = invoke(&[
        "tbar-position",
        context.project_path(),
        "9000",
        "--key",
        "manual-alpha-position",
        "--expect",
        "0",
    ]);
    assert_success(&repeated);
    assert_eq!(stdout(&repeated), positioned_status);
    assert_eq!(manifest(&context.project), positioned_manifest);

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn local_manual_slide_survives_restart_with_exact_desired_and_realized_state() {
    let context = ContractContext::new();
    assert_success(&invoke(&["new", context.project_path()]));

    let started = invoke(&[
        "tbar-start",
        context.project_path(),
        "slide",
        "--key",
        "manual-slide-start",
        "--expect",
        "0",
    ]);
    assert_success(&started);
    assert!(stdout(&started).contains("TBar(desired=slide:1->2@0, realized=slide:1->2@0)"));

    let positioned = invoke(&[
        "tbar-position",
        context.project_path(),
        "6250",
        "--key",
        "manual-slide-position",
        "--expect",
        "1",
    ]);
    assert_success(&positioned);
    assert!(
        stdout(&positioned).contains("TBar(desired=slide:1->2@6250, realized=slide:1->2@6250)")
    );
    assert_eq!(status(&context.project), stdout(&positioned));
    let stored = manifest(&context.project);
    assert!(stored.contains(
        r#""desired": {"kind": "slide", "from_id": 1, "to_id": 2, "interval_start_basis_points": 0, "position_basis_points": 6250}"#
    ));
    assert!(stored.contains(
        r#""realized": {"kind": "slide", "from_id": 1, "to_id": 2, "interval_start_basis_points": 6250, "position_basis_points": 6250}"#
    ));

    fs::remove_dir_all(context.root).unwrap();
}

#[test]
fn cli_rejects_fractional_and_out_of_range_t_bar_positions_before_project_io() {
    let fractional = invoke(&["tbar-position", "missing.freemix", "62.5"]);
    assert_failure_contains(&fractional, "invalid basis points value `62.5`");
    let out_of_range = invoke(&["tbar-position", "missing.freemix", "10001"]);
    assert_failure_contains(
        &out_of_range,
        "basis points must be in 0..=10000, got 10001",
    );
}

struct ContractContext {
    root: PathBuf,
    project: PathBuf,
    first_image: PathBuf,
    second_image: PathBuf,
    final_image: PathBuf,
}

impl ContractContext {
    fn new() -> Self {
        let root = unique_test_root();
        fs::create_dir_all(&root).unwrap();
        Self {
            project: root.join("contract.freemix"),
            first_image: root.join("first.ppm"),
            second_image: root.join("second.ppm"),
            final_image: root.join("final.ppm"),
            root,
        }
    }

    fn project_path(&self) -> &str {
        self.project.to_str().unwrap()
    }
}

fn assert_contract_creation(context: &ContractContext) -> (u128, String) {
    let created = invoke(&["new", context.project_path(), "--name", "Contract Show"]);
    assert_success(&created);
    let created_status = stdout(&created);
    let project_id = status_u128(&created_status, "project_id");
    assert!(project_id > u128::from(u64::MAX));
    assert_status(&created_status, 0, 0, 1, 1, 2, 2);
    (project_id, created_status)
}

fn assert_initial_contract_state(
    context: &ContractContext,
    project_id: u128,
    created_status: &str,
) {
    let initial_status = status(&context.project);
    assert_eq!(initial_status, created_status);
    let initial_manifest = manifest(&context.project);
    assert_eq!(json_u128(&initial_manifest, "id"), project_id);
    assert_eq!(json_number(&initial_manifest, "frames_rendered"), 0);
    assert_eq!(json_number(&initial_manifest, "runtime_generation"), 0);
    assert_eq!(json_number(&initial_manifest, "clock_time_nanos"), 0);
}

fn assert_contract_render(
    context: &ContractContext,
    image: &Path,
    width: u32,
    height: u32,
    rgb: [u8; 3],
) {
    fs::write(image, b"previous complete PPM").unwrap();
    assert_success(&invoke(&[
        "render",
        context.project_path(),
        image.to_str().unwrap(),
        "--width",
        &width.to_string(),
        "--height",
        &height.to_string(),
    ]));
    assert_solid_ppm(image, width, height, rgb);
    assert_eq!(
        fs::read_dir(image.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count(),
        0
    );
}

fn select_contract_preview(context: &ContractContext) {
    let preview = invoke(&[
        "preview",
        context.project_path(),
        "2",
        "--key",
        "preview-two",
        "--expect",
        "0",
    ]);
    assert_success(&preview);
    let after_preview = stdout(&preview);
    assert_status(&after_preview, 1, 1, 1, 1, 2, 2);
}

fn assert_contract_rejections(context: &ContractContext) {
    let before_invalid = status(&context.project);
    let invalid = invoke(&[
        "preview",
        context.project_path(),
        "99",
        "--key",
        "invalid-input",
        "--expect",
        "1",
    ]);
    assert_failure_contains(&invalid, "not_found");
    assert_eq!(status(&context.project), before_invalid);
    let invalid_manifest = manifest(&context.project);
    assert!(invalid_manifest.contains("\"key\": \"invalid-input\""));
    assert!(invalid_manifest.contains("\"code\": \"not_found\""));

    let invalid_replay = invoke(&[
        "preview",
        context.project_path(),
        "1",
        "--key",
        "invalid-input",
        "--expect",
        "1",
    ]);
    assert_failure_contains(&invalid_replay, "not_found");
    assert_eq!(manifest(&context.project), invalid_manifest);

    let stale = invoke(&[
        "cut",
        context.project_path(),
        "--key",
        "stale-cut",
        "--expect",
        "0",
    ]);
    assert_failure_contains(&stale, "revision_conflict");
    assert_eq!(status(&context.project), before_invalid);
    let stale_manifest = manifest(&context.project);
    assert!(stale_manifest.contains("\"key\": \"stale-cut\""));
    assert!(stale_manifest.contains("\"code\": \"revision_conflict\""));
    assert_eq!(json_number(&stale_manifest, "revision"), 1);
    assert_eq!(json_number(&stale_manifest, "frames_rendered"), 1);

    let stale_replay = invoke(&[
        "cut",
        context.project_path(),
        "--key",
        "stale-cut",
        "--expect",
        "1",
    ]);
    assert_failure_contains(&stale_replay, "revision_conflict");
    assert_eq!(manifest(&context.project), stale_manifest);
}

fn cut_contract(context: &ContractContext) -> (String, String) {
    let cut = invoke(&[
        "cut",
        context.project_path(),
        "--key",
        "cut-one",
        "--expect",
        "1",
    ]);
    assert_success(&cut);
    let after_cut = stdout(&cut);
    assert_status(&after_cut, 2, 2, 2, 2, 1, 1);
    let cut_manifest = manifest(&context.project);
    assert_eq!(json_number(&cut_manifest, "runtime_generation"), 2);
    assert!(json_number(&cut_manifest, "clock_time_nanos") > 0);
    (after_cut, cut_manifest)
}

fn assert_duplicate_cut_replay(
    context: &ContractContext,
    expected_status: &str,
    cut_manifest: &str,
) {
    let duplicate = invoke(&[
        "fade",
        context.project_path(),
        "99",
        "--key",
        "cut-one",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), expected_status);
    assert_eq!(manifest(&context.project), cut_manifest);
}

fn fade_and_assert_final_contract(context: &ContractContext, project_id: u128) {
    let fade = invoke(&[
        "fade",
        context.project_path(),
        "4",
        "--key",
        "fade-four",
        "--expect",
        "2",
    ]);
    assert_success(&fade);
    let after_fade = stdout(&fade);
    assert_status(&after_fade, 3, 6, 1, 1, 2, 2);
    assert_eq!(status(&context.project), after_fade);

    let final_manifest = manifest(&context.project);
    assert_eq!(json_u128(&final_manifest, "id"), project_id);
    assert_eq!(json_number(&final_manifest, "revision"), 3);
    assert_eq!(json_number(&final_manifest, "event_sequence"), 3);
    assert_eq!(json_number(&final_manifest, "frames_rendered"), 6);
    assert_eq!(json_number(&final_manifest, "runtime_generation"), 3);
    assert!(json_number(&final_manifest, "clock_time_nanos") > 0);
    assert!(final_manifest.contains("\"target_frame\": 0"));
    assert!(final_manifest.contains("\"target_frame\": 1"));
    assert!(final_manifest.contains("\"target_frame\": 2"));

    let entries: Vec<_> = fs::read_dir(&context.project)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["project.json"]);

    assert_contract_render(context, &context.final_image, 2, 1, [73, 151, 199]);
}

#[test]
fn demo_persists_one_nonzero_identity_through_reload() {
    let root = unique_test_root();
    let project = root.join("demo.freemix");
    fs::create_dir_all(&root).unwrap();

    let demo = invoke(&["demo", project.to_str().unwrap()]);
    assert_success(&demo);
    let output = stdout(&demo);
    let statuses: Vec<_> = output
        .lines()
        .filter(|line| line.starts_with("project_id="))
        .collect();
    assert_eq!(statuses.len(), 4);
    let project_id = status_u128(statuses[0], "project_id");
    assert_ne!(project_id, 0);
    assert!(
        statuses
            .iter()
            .all(|line| { status_u128(line, "project_id") == project_id })
    );
    assert_eq!(statuses[2], statuses[3]);
    assert_status(statuses[3], 2, 5, 1, 1, 2, 2);
    assert_eq!(status(&project), statuses[3]);
    assert_eq!(json_u128(&manifest(&project), "id"), project_id);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn new_refuses_to_replace_an_existing_bundle() {
    let root = unique_test_root();
    let project = root.join("existing.freemix");
    fs::create_dir_all(&root).unwrap();
    let project_path = project.to_str().unwrap();

    let created = invoke(&["new", project_path, "--name", "Original Show"]);
    assert_success(&created);
    let original_status = stdout(&created);
    let original_manifest = manifest(&project);
    let original_id = status_u128(&original_status, "project_id");

    let replacement = invoke(&["new", project_path, "--name", "Replacement Show"]);
    assert_failure_contains(&replacement, "already exists");
    assert_eq!(status_u128(&status(&project), "project_id"), original_id);
    assert_eq!(status(&project), original_status);
    assert_eq!(manifest(&project), original_manifest);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn default_keys_do_not_replay_a_rejection_for_a_corrected_command() {
    let root = unique_test_root();
    let project = root.join("corrected.freemix");
    fs::create_dir_all(&root).unwrap();
    let project_path = project.to_str().unwrap();
    assert_success(&invoke(&["new", project_path]));

    let invalid = invoke(&["preview", project_path, "99"]);
    assert_failure_contains(&invalid, "not_found");
    assert_eq!(json_number(&manifest(&project), "revision"), 0);

    let corrected = invoke(&["preview", project_path, "2"]);
    assert_success(&corrected);
    assert_status(&stdout(&corrected), 1, 1, 1, 1, 2, 2);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn blank_explicit_key_fails_without_mutating_the_project() {
    let root = unique_test_root();
    let project = root.join("blank-key.freemix");
    fs::create_dir_all(&root).unwrap();
    let project_path = project.to_str().unwrap();
    assert_success(&invoke(&["new", project_path]));
    let before = manifest(&project);

    let blank = invoke(&["cut", project_path, "--key", "  \t"]);
    assert_failure_contains(&blank, "idempotency key must not be blank");
    assert_eq!(manifest(&project), before);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_escapes_show_names_onto_one_line() {
    let root = unique_test_root();
    let project = root.join("escaped.freemix");
    fs::create_dir_all(&root).unwrap();

    let created = invoke(&[
        "new",
        project.to_str().unwrap(),
        "--name",
        "First line\n\"Second line\"",
    ]);
    assert_success(&created);
    let output = stdout(&created);
    assert_eq!(output.lines().count(), 1);
    assert!(output.contains(r#"show="First line\n\"Second line\"""#));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn edited_current_configuration_survives_commands_restart_and_render() {
    let root = unique_test_root();
    let project = root.join("edited.freemix");
    let solid_image = root.join("solid.ppm");
    let bars_image = root.join("bars.ppm");
    fs::create_dir_all(&root).unwrap();
    let project_path = project.to_str().unwrap();
    assert_success(&invoke(&["new", project_path, "--name", "Edited V3"]));

    edit_current_configuration(&project);

    let before = status(&project);
    assert_status(&before, 0, 0, 1, 1, 2, 2);
    assert_success(&invoke(&[
        "render",
        project_path,
        solid_image.to_str().unwrap(),
        "--width",
        "3",
        "--height",
        "2",
    ]));
    assert_solid_ppm(&solid_image, 3, 2, [12, 34, 56]);

    let cut = invoke(&["cut", project_path, "--key", "edited-cut", "--expect", "0"]);
    assert_success(&cut);
    let after_cut = stdout(&cut);
    assert_eq!(status(&project), after_cut);
    assert_status(&after_cut, 1, 1, 2, 2, 1, 1);
    assert_edited_configuration(&manifest(&project));

    assert_success(&invoke(&[
        "render",
        project_path,
        bars_image.to_str().unwrap(),
        "--width",
        "7",
        "--height",
        "2",
    ]));
    assert_ppm_top_row(
        &bars_image,
        7,
        2,
        &[
            [191, 191, 191],
            [191, 191, 0],
            [0, 191, 191],
            [0, 191, 0],
            [191, 0, 191],
            [191, 0, 0],
            [0, 0, 191],
        ],
    );

    fs::remove_dir_all(root).unwrap();
}

fn edit_current_configuration(project: &Path) {
    let mut source = manifest(project);
    let default_rate = r#"{"numerator": 60000, "denominator": 1001}"#;
    assert_eq!(source.matches(default_rate).count(), 2);
    source = source.replace(default_rate, r#"{"numerator": 24000, "denominator": 1001}"#);
    replace_once(&mut source, r#""width": 1920"#, r#""width": 1280"#);
    replace_once(&mut source, r#""height": 1080"#, r#""height": 720"#);
    replace_once(
        &mut source,
        r#""pixel_format": "nv12""#,
        r#""pixel_format": "rgba8""#,
    );
    replace_once(
        &mut source,
        r#""sample_rate_hz": 48000"#,
        r#""sample_rate_hz": 96000"#,
    );
    replace_once(
        &mut source,
        r#""sample_format": "f32""#,
        r#""sample_format": "i24""#,
    );
    replace_once(
        &mut source,
        r#""video": {"type": "solid", "red": 73, "green": 151, "blue": 199, "alpha": 255}, "audio": {"type": "silence"}"#,
        r#""video": {"type": "solid", "red": 12, "green": 34, "blue": 56, "alpha": 128}, "audio": {"type": "sine", "frequency_hz": 777}"#,
    );
    replace_once(
        &mut source,
        r#""video": {"type": "solid", "red": 146, "green": 46, "blue": 142, "alpha": 255}, "audio": {"type": "sine", "frequency_hz": 1000}"#,
        r#""video": {"type": "bars"}, "audio": {"type": "silence"}"#,
    );
    replace_once(
        &mut source,
        r#""required_capabilities": []"#,
        r#""required_capabilities": ["sim.input.edited"]"#,
    );
    replace_once(
        &mut source,
        r#""scenes": []"#,
        r#""scenes": [{"id": 11, "name": "Edited scene", "background": {"red": 0, "green": 0, "blue": 0, "alpha": 255}, "layers": [{"name": "Source", "source": {"type": "input", "id": 1}, "enabled": true, "geometry": {"translation_x": 0, "translation_y": 0, "width": 1280, "height": 720, "rotation": "deg0"}, "crop": null, "mask": null, "opacity": 255, "z_order": 0}]}]"#,
    );
    replace_once(
        &mut source,
        r#""audio_buses": []"#,
        r#""audio_buses": [{"id": 21, "name": "Edited bus", "sends": []}]"#,
    );
    replace_once(
        &mut source,
        r#""outputs": []"#,
        r#""outputs": [{"id": 31, "name": "Edited output", "video_source": 11, "audio_source": 21, "startup": "reconcile_desired_state", "required_capabilities": ["output.edited"]}]"#,
    );
    replace_once(
        &mut source,
        r#""restart_policy": {"type": "never"}"#,
        r#""restart_policy": {"type": "on_failure", "max_attempts": 7}"#,
    );
    fs::write(project.join("project.json"), source).unwrap();
}

fn assert_edited_configuration(saved: &str) {
    for expected in [
        r#"{"numerator": 24000, "denominator": 1001}"#,
        r#""width": 1280"#,
        r#""height": 720"#,
        r#""pixel_format": "rgba8""#,
        r#""sample_rate_hz": 96000"#,
        r#""sample_format": "i24""#,
        r#""red": 12, "green": 34, "blue": 56, "alpha": 128"#,
        r#""frequency_hz": 777"#,
        r#""video": {"type": "bars"}, "audio": {"type": "silence"}"#,
        r#""sim.input.edited""#,
        r#""Edited scene""#,
        r#""Edited bus""#,
        r#""Edited output""#,
        r#""output.edited""#,
        r#""type": "on_failure", "max_attempts": 7"#,
    ] {
        assert!(saved.contains(expected), "missing persisted `{expected}`");
    }
    assert_eq!(
        saved
            .matches(r#"{"numerator": 24000, "denominator": 1001}"#)
            .count(),
        2
    );
}

#[test]
fn render_rejects_non_simulated_inputs_clearly() {
    let root = unique_test_root();
    let project = root.join("unsupported.freemix");
    let image = root.join("unsupported.ppm");
    fs::create_dir_all(&root).unwrap();
    assert_success(&invoke(&["new", project.to_str().unwrap()]));

    let mut source = manifest(&project);
    replace_once(
        &mut source,
        r#"{"type": "simulated", "video": {"type": "solid", "red": 73, "green": 151, "blue": 199, "alpha": 255}, "audio": {"type": "silence"}}"#,
        r#"{"type": "color"}"#,
    );
    fs::write(project.join("project.json"), source).unwrap();

    let rendered = invoke(&["render", project.to_str().unwrap(), image.to_str().unwrap()]);
    assert_failure_contains(
        &rendered,
        "input 1 (\"Input 1\") is not simulated; render supports only simulated inputs",
    );
    assert!(!image.exists());

    fs::remove_dir_all(root).unwrap();
}

fn invoke(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_freemix-cli"))
        .args(arguments)
        .output()
        .unwrap()
}

fn invoke_bounded(arguments: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_freemix-cli"))
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + TEST_CLI_TIMEOUT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "CLI process did not exit before {TEST_CLI_TIMEOUT:?}: status={} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn replace_once(source: &mut String, from: &str, to: &str) {
    assert!(source.contains(from), "manifest did not contain `{from}`");
    *source = source.replacen(from, to, 1);
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "process failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure_contains(output: &Output, expected: &str) {
    assert!(!output.status.success(), "process unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "stderr did not contain `{expected}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .trim()
        .to_owned()
}

fn status(project: &Path) -> String {
    let output = invoke(&["status", project.to_str().unwrap()]);
    assert_success(&output);
    stdout(&output)
}

fn assert_status(
    status: &str,
    revision: u64,
    frame: u64,
    desired_program: u64,
    realized_program: u64,
    desired_preview: u64,
    realized_preview: u64,
) {
    assert_eq!(status_number(status, "revision"), revision);
    assert_eq!(status_number(status, "frame"), frame);
    assert!(status.contains(&format!(
        "Program(desired={desired_program}, realized={realized_program})"
    )));
    assert!(status.contains(&format!(
        "Preview(desired={desired_preview}, realized={realized_preview})"
    )));
}

fn assert_remote_status(
    status: &str,
    revision: u64,
    desired_program: u64,
    realized_program: u64,
    desired_preview: u64,
    realized_preview: u64,
) {
    assert_eq!(status_number(status, "revision"), revision);
    assert!(status.contains("frame=unavailable"));
    assert!(status.contains(&format!(
        "Program(desired={desired_program}, realized={realized_program})"
    )));
    assert!(status.contains(&format!(
        "Preview(desired={desired_preview}, realized={realized_preview})"
    )));
}

fn status_number(status: &str, field: &str) -> u64 {
    number_after(status, &format!("{field}="))
}

fn status_u128(status: &str, field: &str) -> u128 {
    u128_after(status, &format!("{field}="))
}

fn json_number(source: &str, field: &str) -> u64 {
    number_after(source, &format!("\"{field}\": "))
}

fn json_u128(source: &str, field: &str) -> u128 {
    u128_after(source, &format!("\"{field}\": "))
}

fn number_after(source: &str, prefix: &str) -> u64 {
    u128_after(source, prefix).try_into().unwrap()
}

fn u128_after(source: &str, prefix: &str) -> u128 {
    let start = source.find(prefix).unwrap() + prefix.len();
    let digits: String = source[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
}

fn manifest(project: &Path) -> String {
    fs::read_to_string(project.join("project.json")).unwrap()
}

fn assert_solid_ppm(path: &Path, width: u32, height: u32, rgb: [u8; 3]) {
    let bytes = fs::read(path).unwrap();
    let header = format!("P6\n{width} {height}\n255\n");
    assert!(bytes.starts_with(header.as_bytes()));
    let pixels = &bytes[header.len()..];
    assert_eq!(pixels.len(), (width * height * 3) as usize);
    assert!(pixels.chunks_exact(3).all(|pixel| pixel == rgb));
}

fn assert_ppm_top_row(path: &Path, width: u32, height: u32, expected: &[[u8; 3]]) {
    let bytes = fs::read(path).unwrap();
    let header = format!("P6\n{width} {height}\n255\n");
    assert!(bytes.starts_with(header.as_bytes()));
    let top_row = &bytes[header.len()..header.len() + width as usize * 3];
    assert_eq!(
        top_row.chunks_exact(3).collect::<Vec<_>>(),
        expected.iter().map(<[u8; 3]>::as_slice).collect::<Vec<_>>()
    );
}

fn unique_test_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = TEST_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "freemix-cli-{}-{nonce}-{sequence}",
        std::process::id()
    ))
}
