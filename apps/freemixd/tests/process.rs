use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use fm_client::{
    Client as ProtocolClient, ClientConfig, ClientError, ConnectionState, Intake, Outbound,
};
use fm_model::{
    AudioBus, Input, InputKind, Layer, LayerGeometry, MainMix, Output, Project, ProjectSettings,
    RectMask, RestartPolicy, Rgba8, Rotation, Scene, SimulatedAudio, SimulatedInput,
    SimulatedVideo, SolidColor, SourceRef, StartupPolicy,
};
use fm_persistence::{
    ManualTransitionKind as PersistedManualTransitionKind, ProjectPosition, ProjectStore,
    RuntimeRouting, StoredProject,
};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientType, CommandMessage, CommandPayload, CommandResult,
    EngineIdentity, EventCursor, HandshakeOutcome, HandshakeRequest, ManualTransitionKind,
    ManualTransitionPosition, ManualTransitionStatus, ProtocolVersion, ResumeCursor, Role,
    RuntimeLifecycleEvent, ServerIdentity, SnapshotReason, StingerAudioPolicy,
    StingerMissingMediaFallback, WireInputId, WireMessage, WireStingerSlotId, decode_line,
    encode_line,
};
use fm_types::{
    AudioFormat, BusId, ChannelLayout, ColorMetadata, FrameRate, InputId, OutputId, PixelFormat,
    ProjectId, SampleFormat, SampleRate, ScanMode, SceneId, VideoDimensions, VideoFormat,
};
use freemixd::ReadinessRecord;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const U64_MAX_ID: u128 = 18_446_744_073_709_551_615;
const PROJECT_ID: u128 = U64_MAX_ID + 42;
const INPUT_ID_BASE: u128 = U64_MAX_ID + 100;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("freemixd-{}-{sequence}-{name}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn project_path(&self) -> PathBuf {
        self.0.join("show.freemix")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Daemon {
    child: Option<Child>,
    address: SocketAddr,
    project_id: ProjectId,
}

impl Daemon {
    fn start(project: &Path) -> Self {
        let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
            .arg("serve")
            .arg(project)
            .arg("--once")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut output = BufReader::new(stdout);
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let readiness = line.parse::<ReadinessRecord>().unwrap_or_else(|error| {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("daemon did not become ready: {error}; stdout={line:?}, stderr={stderr:?}");
        });
        Self {
            child: Some(child),
            address: readiness.address,
            project_id: readiness.project_id,
        }
    }

    fn connect(&self) -> Client {
        Client::connect(self.address)
    }

    fn wait_success(mut self) {
        let mut child = self.child.take().unwrap();
        let status = child.wait().unwrap();
        if !status.success() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("daemon exited with {status}: {stderr}");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

struct TestHandshake {
    protocol: ProtocolVersion,
    engine: EngineIdentity,
    current_revision: u64,
    resume: bool,
}

impl Client {
    fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream.set_nodelay(true).unwrap();
        Self {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }

    fn send(&mut self, message: &WireMessage) {
        let line = encode_line(message).unwrap();
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn receive(&mut self) -> WireMessage {
        let mut line = String::new();
        assert_ne!(
            self.reader.read_line(&mut line).unwrap(),
            0,
            "daemon closed TCP stream"
        );
        decode_line(&line).unwrap()
    }

    fn handshake(&mut self, cursor: Option<EventCursor>) -> TestHandshake {
        self.handshake_version(CURRENT_PROTOCOL_VERSION, cursor)
    }

    fn handshake_version(
        &mut self,
        version: ProtocolVersion,
        cursor: Option<EventCursor>,
    ) -> TestHandshake {
        let resume_cursor = cursor.map(|cursor| ResumeCursor {
            server: ServerIdentity {
                engine_id: cursor.engine.engine_id,
                project_id: PROJECT_ID.to_string(),
                state_epoch: cursor.engine.state_epoch,
                log_id: cursor.engine.log_id,
            },
            revision: cursor.revision,
        });
        self.send(&WireMessage::HandshakeRequest(HandshakeRequest {
            protocol: version,
            build: "process-test".into(),
            client_type: ClientType::Integration,
            desired_role: Role::Operator,
            resume_cursor,
        }));
        let WireMessage::HandshakeResponse(response) = self.receive() else {
            panic!("expected handshake response");
        };
        let resume = matches!(response.outcome, HandshakeOutcome::Resume { .. });
        TestHandshake {
            protocol: response.protocol,
            engine: EngineIdentity {
                engine_id: response.server.engine_id,
                state_epoch: response.server.state_epoch,
                log_id: response.server.log_id,
            },
            current_revision: response.current_revision,
            resume,
        }
    }

    fn next_result(&mut self) -> CommandResult {
        loop {
            if let WireMessage::CommandResult(result) = self.receive() {
                return result;
            }
        }
    }
}

fn assert_fade_runtime_round_trip(protocol_client: &mut ProtocolClient, transport: &mut Client) {
    let command = protocol_client
        .queue_command(
            CommandPayload::Fade { duration_frames: 4 },
            "current-fade-key",
            Some(0),
            None,
        )
        .unwrap();
    assert_eq!(
        protocol_client.pop_outbound(),
        Some(Outbound::Command(command.clone()))
    );
    transport.send(&WireMessage::Command(command));

    let result = transport.receive();
    assert!(matches!(
        result,
        WireMessage::CommandResult(CommandResult::Accepted { revision: 1, .. })
    ));
    assert_eq!(
        protocol_client.intake(result).unwrap(),
        Intake::ResultReconciled
    );
    let event = transport.receive();
    let WireMessage::Event(durable_event) = &event else {
        panic!("expected durable event after command result");
    };
    let durable_revision = durable_event.cursor.revision;
    assert_eq!(durable_revision, 1);
    assert_eq!(protocol_client.intake(event).unwrap(), Intake::EventApplied);
    let durable_cursor = protocol_client.last_applied_cursor().unwrap();

    let runtime_event = transport.receive();
    let WireMessage::RuntimeEvent(runtime) = &runtime_event else {
        panic!("expected runtime event after durable event");
    };
    assert_eq!(runtime.revision, durable_revision);
    assert_eq!(runtime.generation, 1);
    assert_eq!(runtime.sequence, 1);
    assert!(matches!(
        &runtime.event,
        RuntimeLifecycleEvent::Realized { domain, .. } if domain == "switcher"
    ));
    assert_eq!(
        protocol_client.intake(runtime_event).unwrap(),
        Intake::RuntimeEventObserved
    );
    assert_eq!(protocol_client.last_applied_cursor(), Some(durable_cursor));
    let switcher = protocol_client.model().state().unwrap().switcher();
    assert_eq!(switcher.realized.program, input(2).to_domain());
    assert_eq!(switcher.realized.preview, input(1).to_domain());
    assert_eq!(switcher.runtime_generation, Some(1));
}

#[test]
fn current_client_handshake_heartbeat_and_resume_use_ordered_wire_records() {
    let directory = TestDirectory::new("current-client");
    let project_path = directory.project_path();
    create_project(&project_path);

    let mut protocol_client = ProtocolClient::new(ClientConfig::new(
        "process-test",
        ClientType::Integration,
        Role::Operator,
        "process-client",
        project_id(),
    ))
    .unwrap();
    protocol_client.start_connect().unwrap();

    let daemon = Daemon::start(&project_path);
    assert_eq!(daemon.project_id, project_id());
    let mut transport = daemon.connect();
    let request = protocol_client.transport_connected().unwrap();
    assert_eq!(request.resume_cursor, None);
    transport.send(&WireMessage::HandshakeRequest(request));

    let WireMessage::HandshakeResponse(response) = transport.receive() else {
        panic!("expected current handshake response");
    };
    assert_eq!(
        response.outcome,
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor
        }
    );
    assert_eq!(response.server.project_id, PROJECT_ID.to_string());
    assert_eq!(
        protocol_client
            .intake(WireMessage::HandshakeResponse(response))
            .unwrap(),
        Intake::Handshake
    );
    let snapshot = transport.receive();
    assert!(matches!(snapshot, WireMessage::Snapshot(_)));
    assert_eq!(
        protocol_client.intake(snapshot).unwrap(),
        Intake::SnapshotApplied
    );
    assert_eq!(protocol_client.state(), &ConnectionState::Ready);

    let heartbeat = protocol_client.queue_heartbeat(1_234).unwrap();
    assert!(heartbeat.last_applied.is_some());
    assert_eq!(
        protocol_client.pop_outbound(),
        Some(Outbound::Heartbeat(heartbeat.clone()))
    );
    transport.send(&WireMessage::Heartbeat(heartbeat));
    assert_fade_runtime_round_trip(&mut protocol_client, &mut transport);

    drop(transport);
    daemon.wait_success();
    let _ = protocol_client.transport_disconnected();
    protocol_client.start_connect().unwrap();

    let daemon = Daemon::start(&project_path);
    let mut transport = daemon.connect();
    let request = protocol_client.transport_connected().unwrap();
    let resume_cursor = request.resume_cursor.clone().unwrap();
    assert_eq!(resume_cursor.revision, 1);
    transport.send(&WireMessage::HandshakeRequest(request));
    let WireMessage::HandshakeResponse(response) = transport.receive() else {
        panic!("expected current resume response");
    };
    assert_eq!(
        response.outcome,
        HandshakeOutcome::Resume {
            cursor: resume_cursor
        }
    );
    assert_eq!(response.server.project_id, PROJECT_ID.to_string());
    protocol_client
        .intake(WireMessage::HandshakeResponse(response))
        .unwrap();
    assert_eq!(protocol_client.state(), &ConnectionState::Ready);

    drop(transport);
    daemon.wait_success();
}

#[test]
fn current_client_receives_structured_handshake_rejection() {
    let directory = TestDirectory::new("current-rejection");
    let project_path = directory.project_path();
    create_project(&project_path);
    let mut protocol_client = ProtocolClient::new(ClientConfig::new(
        "process-test",
        ClientType::Integration,
        Role::Replay,
        "rejected-client",
        project_id(),
    ))
    .unwrap();
    protocol_client.start_connect().unwrap();

    let daemon = Daemon::start(&project_path);
    let mut transport = daemon.connect();
    transport.send(&WireMessage::HandshakeRequest(
        protocol_client.transport_connected().unwrap(),
    ));
    let response = transport.receive();
    assert!(matches!(
        &response,
        WireMessage::HandshakeResponse(response)
            if matches!(
                &response.outcome,
                HandshakeOutcome::Rejected { error }
                    if error.code == "permission_denied"
            )
    ));
    assert!(matches!(
        protocol_client.intake(response),
        Err(ClientError::HandshakeRejected(error)) if error.code == "permission_denied"
    ));

    drop(transport);
    daemon.wait_success();
}

#[test]
fn commands_survive_restart_resume_and_duplicate_replay() {
    let directory = TestDirectory::new("restart");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let initial = client.handshake(None);
    assert!(!initial.resume);
    assert_eq!(initial.current_revision, 0);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));

    client.send(&command(
        "preview-command",
        "preview-key",
        CommandPayload::SelectPreview { input: input(3) },
    ));
    let first_after_command = client.receive();
    assert!(matches!(
        first_after_command,
        WireMessage::CommandResult(CommandResult::Accepted { revision: 1, .. })
    ));

    client.send(&command("cut-command", "cut-key", CommandPayload::Cut));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 2, .. }
    ));

    client.send(&command(
        "fade-command",
        "fade-key",
        CommandPayload::Fade { duration_frames: 4 },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 3, .. }
    ));
    drop(client);
    daemon.wait_success();

    let store = ProjectStore::new(&project_path).unwrap();
    let persisted = store.load().unwrap();
    assert_eq!(persisted.position().revision, 3);
    assert_eq!(persisted.position().event_sequence, 3);
    assert_eq!(persisted.position().runtime_generation, 3);
    assert_eq!(persisted.position().frames_rendered, 6);
    assert_eq!(persisted.position().clock_time_nanos, 200_000_000);
    assert_eq!(
        persisted.runtime_routing().desired_program_id,
        Some(domain_input(1))
    );
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(domain_input(1))
    );
    assert_eq!(
        persisted.runtime_routing().desired_preview_id,
        Some(domain_input(3))
    );
    assert_eq!(
        persisted.runtime_routing().realized_preview_id,
        Some(domain_input(3))
    );
    assert_eq!(persisted.idempotency_receipts().len(), 3);
    assert_eq!(
        persisted.project().scenes()[0].layers[0].mask,
        Some(RectMask::new(100, 50, 1_000, 600).inverted(true))
    );
    let mut expected_project = canonical_project();
    expected_project.set_main_mix(MainMix::new(domain_input(1), domain_input(3)));
    assert_eq!(persisted.project(), &expected_project);

    let cursor = EventCursor {
        engine: initial.engine,
        revision: 3,
    };
    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let resumed = client.handshake(Some(cursor));
    assert!(resumed.resume);
    assert_eq!(resumed.current_revision, 3);

    client.send(&command(
        "duplicate-command",
        "fade-key",
        CommandPayload::Cut,
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted {
            id,
            revision: 3,
            ..
        } if id == "fade-command"
    ));
    drop(client);
    daemon.wait_success();

    assert_eq!(store.load().unwrap(), persisted);
}

#[test]
fn live_stinger_slot_mutations_fire_immediately_and_survive_restart() {
    let directory = TestDirectory::new("live-stinger-configuration");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    assert_eq!(
        client
            .handshake_version(CURRENT_PROTOCOL_VERSION, None)
            .protocol,
        CURRENT_PROTOCOL_VERSION
    );
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));

    configure_and_fire_stinger(&mut client);
    replace_live_stinger(&mut client);
    drop(client);
    daemon.wait_success();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 3);
    assert_eq!(persisted.idempotency_receipts().len(), 3);
    assert_eq!(
        (
            persisted.runtime_routing().desired_program_id,
            persisted.runtime_routing().realized_program_id,
            persisted.runtime_routing().desired_preview_id,
            persisted.runtime_routing().realized_preview_id,
        ),
        (
            Some(domain_input(2)),
            Some(domain_input(2)),
            Some(domain_input(1)),
            Some(domain_input(1)),
        )
    );
    let replacement = persisted.project().stingers()[0];
    assert_eq!(replacement.slot.number(), 8);
    assert_eq!(replacement.media_input, domain_input(2));
    assert!(!replacement.preload);
    assert_eq!(replacement.cut_point_frames, 7);
    assert_eq!(
        replacement.audio_policy,
        fm_model::StingerAudioPolicy::Muted
    );
    assert_eq!(
        replacement.missing_media_fallback,
        fm_model::StingerMissingMediaFallback::KeepProgram
    );

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    assert_eq!(
        client
            .handshake_version(CURRENT_PROTOCOL_VERSION, None)
            .current_revision,
        3
    );
    let WireMessage::Snapshot(snapshot) = client.receive() else {
        panic!("expected restarted snapshot");
    };
    let restarted = &snapshot.stingers[0];
    assert_eq!(restarted.slot.number(), 8);
    assert_eq!(restarted.media_input, input(2));
    assert!(!restarted.preload);
    assert_eq!(restarted.cut_point_frames, 7);
    assert_eq!(restarted.audio_policy, StingerAudioPolicy::Muted);
    assert_eq!(
        restarted.missing_media_fallback,
        StingerMissingMediaFallback::KeepProgram
    );
    assert_eq!(
        restarted.readiness,
        fm_protocol::StingerReadiness::NotRequested
    );

    remove_live_stinger(&mut client);
    drop(client);
    daemon.wait_success();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 4);
    assert_eq!(persisted.idempotency_receipts().len(), 4);
    assert!(persisted.project().stingers().is_empty());

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    assert_eq!(
        client
            .handshake_version(CURRENT_PROTOCOL_VERSION, None)
            .current_revision,
        4
    );
    let WireMessage::Snapshot(snapshot) = client.receive() else {
        panic!("expected second restarted snapshot");
    };
    assert!(snapshot.stingers.is_empty());
    drop(client);
    daemon.wait_success();
}

fn configure_and_fire_stinger(client: &mut Client) {
    client.send(&command_version(
        CURRENT_PROTOCOL_VERSION,
        "configure-stinger",
        "configure-stinger-key",
        CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            media_input: input(3),
            preload: true,
            cut_point_frames: 1,
            audio_policy: StingerAudioPolicy::MixWithProgram,
            missing_media_fallback: StingerMissingMediaFallback::Cut,
        },
    ));
    assert!(matches!(
        client.receive(),
        WireMessage::CommandResult(CommandResult::Accepted { revision: 1, .. })
    ));
    assert!(matches!(
        client.receive(),
        WireMessage::Event(fm_protocol::EventMessage {
            payload: fm_protocol::EventPayload::StingerSlotsChanged { .. },
            ..
        })
    ));
    assert!(matches!(
        client.receive(),
        WireMessage::RuntimeEvent(fm_protocol::RuntimeEventMessage { revision: 1, .. })
    ));
    client.send(&command_version(
        CURRENT_PROTOCOL_VERSION,
        "fire-stinger",
        "fire-stinger-key",
        CommandPayload::Stinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            duration_frames: 2,
        },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 2, .. }
    ));
}

fn replace_live_stinger(client: &mut Client) {
    client.send(&command_version(
        CURRENT_PROTOCOL_VERSION,
        "replace-stinger",
        "replace-stinger-key",
        CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            media_input: input(2),
            preload: false,
            cut_point_frames: 7,
            audio_policy: StingerAudioPolicy::Muted,
            missing_media_fallback: StingerMissingMediaFallback::KeepProgram,
        },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 3, .. }
    ));
}

fn remove_live_stinger(client: &mut Client) {
    client.send(&command_version(
        CURRENT_PROTOCOL_VERSION,
        "remove-stinger",
        "remove-stinger-key",
        CommandPayload::RemoveStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
        },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 4, .. }
    ));
}

#[test]
fn slide_command_settles_and_survives_daemon_restart() {
    let directory = TestDirectory::new("slide-restart");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let initial = client.handshake_version(CURRENT_PROTOCOL_VERSION, None);
    assert_eq!(initial.protocol, CURRENT_PROTOCOL_VERSION);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));

    client.send(&command_version(
        CURRENT_PROTOCOL_VERSION,
        "slide-command",
        "slide-key",
        CommandPayload::Slide { duration_frames: 3 },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 1, .. }
    ));
    drop(client);
    daemon.wait_success();

    let store = ProjectStore::new(&project_path).unwrap();
    let persisted = store.load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(persisted.position().event_sequence, 1);
    assert_eq!(persisted.position().runtime_generation, 1);
    assert_eq!(persisted.position().frames_rendered, 3);
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(domain_input(2))
    );
    assert_eq!(
        persisted.runtime_routing().realized_preview_id,
        Some(domain_input(1))
    );

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let resumed = client.handshake_version(
        CURRENT_PROTOCOL_VERSION,
        Some(EventCursor {
            engine: initial.engine,
            revision: 1,
        }),
    );
    assert!(resumed.resume);
    assert_eq!(resumed.current_revision, 1);
    drop(client);
    daemon.wait_success();
    assert_eq!(store.load().unwrap(), persisted);
}

#[test]
fn zoom_command_settles_and_survives_daemon_restart() {
    let directory = TestDirectory::new("zoom-restart");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let initial = client.handshake_version(CURRENT_PROTOCOL_VERSION, None);
    assert_eq!(initial.protocol, CURRENT_PROTOCOL_VERSION);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));

    client.send(&command_version(
        CURRENT_PROTOCOL_VERSION,
        "zoom-command",
        "zoom-key",
        CommandPayload::Zoom { duration_frames: 3 },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 1, .. }
    ));
    drop(client);
    daemon.wait_success();

    let store = ProjectStore::new(&project_path).unwrap();
    let persisted = store.load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(persisted.position().event_sequence, 1);
    assert_eq!(persisted.position().runtime_generation, 1);
    assert_eq!(persisted.position().frames_rendered, 3);
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(domain_input(2))
    );
    assert_eq!(
        persisted.runtime_routing().realized_preview_id,
        Some(domain_input(1))
    );

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let resumed = client.handshake_version(
        CURRENT_PROTOCOL_VERSION,
        Some(EventCursor {
            engine: initial.engine,
            revision: 1,
        }),
    );
    assert!(resumed.resume);
    assert_eq!(resumed.current_revision, 1);
    drop(client);
    daemon.wait_success();
    assert_eq!(store.load().unwrap(), persisted);
}

#[test]
#[allow(clippy::too_many_lines)]
fn manual_alpha_fade_state_and_receipts_survive_restart_through_commit_and_cancel() {
    for (name, terminal, swaps_routes) in [
        (
            "manual-commit-restart",
            CommandPayload::CommitManualTransition,
            true,
        ),
        (
            "manual-cancel-restart",
            CommandPayload::CancelManualTransition,
            false,
        ),
    ] {
        let directory = TestDirectory::new(name);
        let project_path = directory.project_path();
        create_project(&project_path);

        let daemon = Daemon::start(&project_path);
        let mut client = daemon.connect();
        let hello = client.handshake_version(CURRENT_PROTOCOL_VERSION, None);
        assert_eq!(hello.protocol, CURRENT_PROTOCOL_VERSION);
        assert!(matches!(client.receive(), WireMessage::Snapshot(_)));

        client.send(&command_version(
            CURRENT_PROTOCOL_VERSION,
            "manual-start",
            "manual-start-key",
            CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::AlphaFade,
            },
        ));
        assert!(matches!(
            client.next_result(),
            CommandResult::Accepted { revision: 1, .. }
        ));
        client.send(&command_version(
            CURRENT_PROTOCOL_VERSION,
            "manual-set",
            "manual-set-key",
            CommandPayload::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(6_250).unwrap(),
            },
        ));
        assert!(matches!(
            client.next_result(),
            CommandResult::Accepted { revision: 2, .. }
        ));
        drop(client);
        daemon.wait_success();

        let store = ProjectStore::new(&project_path).unwrap();
        let checkpoint = store.load().unwrap();
        let manual = checkpoint.runtime_manual_transitions();
        let desired = manual
            .desired
            .expect("desired manual state must be durable");
        let realized = manual
            .realized
            .expect("realized manual state must be durable");
        assert_eq!(desired.kind, PersistedManualTransitionKind::AlphaFade);
        assert_eq!(desired.interval_start_basis_points, 0);
        assert_eq!(desired.position_basis_points, 6_250);
        assert_eq!(realized.interval_start_basis_points, 6_250);
        assert_eq!(realized.position_basis_points, 6_250);
        assert_eq!(checkpoint.idempotency_receipts().len(), 2);

        let daemon = Daemon::start(&project_path);
        let mut client = daemon.connect();
        let hello = client.handshake_version(CURRENT_PROTOCOL_VERSION, None);
        assert_eq!(hello.current_revision, 2);
        let WireMessage::Snapshot(snapshot) = client.receive() else {
            panic!("restart must return a snapshot");
        };
        assert!(matches!(
            snapshot.desired_manual_transition,
            ManualTransitionStatus::Active(state)
                if state.kind == ManualTransitionKind::AlphaFade
                    && state.interval_start == ManualTransitionPosition::START
                    && state.position.basis_points() == 6_250
        ));
        assert!(matches!(
            snapshot.realized_manual_transition,
            ManualTransitionStatus::Active(state)
                if state.interval_start.basis_points() == 6_250
                    && state.position.basis_points() == 6_250
        ));

        client.send(&command_version(
            CURRENT_PROTOCOL_VERSION,
            "manual-replay-different-command",
            "manual-set-key",
            CommandPayload::CancelManualTransition,
        ));
        assert!(matches!(
            client.next_result(),
            CommandResult::Accepted {
                id,
                revision: 2,
                ..
            } if id == "manual-set"
        ));
        client.send(&command_version(
            CURRENT_PROTOCOL_VERSION,
            "manual-terminal",
            "manual-terminal-key",
            terminal,
        ));
        assert!(matches!(
            client.next_result(),
            CommandResult::Accepted { revision: 3, .. }
        ));
        drop(client);
        daemon.wait_success();

        let final_state = store.load().unwrap();
        assert_eq!(final_state.runtime_manual_transitions().desired, None);
        assert_eq!(final_state.runtime_manual_transitions().realized, None);
        assert_eq!(final_state.idempotency_receipts().len(), 3);
        let routing = final_state.runtime_routing();
        let expected_program = if swaps_routes {
            domain_input(2)
        } else {
            domain_input(1)
        };
        let expected_preview = if swaps_routes {
            domain_input(1)
        } else {
            domain_input(2)
        };
        assert_eq!(routing.desired_program_id, Some(expected_program));
        assert_eq!(routing.realized_program_id, Some(expected_program));
        assert_eq!(routing.desired_preview_id, Some(expected_preview));
        assert_eq!(routing.realized_preview_id, Some(expected_preview));
    }
}

#[test]
fn incompatible_handshake_returns_protocol_error() {
    let directory = TestDirectory::new("incompatible");
    let project_path = directory.project_path();
    create_project(&project_path);
    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    client.send(&WireMessage::HandshakeRequest(HandshakeRequest {
        protocol: ProtocolVersion::new(99, 0),
        build: "future".into(),
        client_type: ClientType::Integration,
        desired_role: Role::Operator,
        resume_cursor: None,
    }));
    assert!(matches!(
        client.receive(),
        WireMessage::HandshakeResponse(response)
            if matches!(&response.outcome, HandshakeOutcome::Rejected { error } if error.code == "incompatible_version")
    ));
    drop(client);
    daemon.wait_success();
}

#[test]
fn non_loopback_bind_is_rejected_before_listening() {
    let directory = TestDirectory::new("exposed-bind");
    let project_path = directory.project_path();
    create_project(&project_path);
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
        .arg("serve")
        .arg(project_path)
        .arg("--listen")
        .arg("0.0.0.0:0")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("requires a loopback")
    );
}

#[test]
fn help_documents_program_output_and_fullscreen_selection() {
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
        .arg("help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--fullscreen-program"));
    assert!(stdout.contains("--fullscreen-display 0"));
    assert!(stdout.contains("--record-program output.mp4"));
    assert!(stdout.contains("--camera-helper PATH"));
    assert!(stdout.contains("never requests permission"));
    assert!(stdout.contains("--record-program=<path>"));
    assert!(stdout.contains("Existing files are never overwritten"));
    assert!(stdout.contains("configured startup support"));
    assert!(stdout.contains("FREEMIXD_RECORDER reports runtime health"));
    assert!(stdout.contains("zero-based index"));
}

#[test]
fn recording_without_native_media_fails_before_readiness() {
    let directory = TestDirectory::new("recording-requires-native");
    let project_path = directory.project_path();
    create_project(&project_path);
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
        .arg("serve")
        .arg(project_path)
        .args(["--record-program", "capture.mp4"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("FREEMIXD_READY")
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--record-program requires --native-media")
    );
}

fn create_project(path: &Path) {
    let project = StoredProject::from_project(
        canonical_project(),
        RuntimeRouting {
            desired_program_id: Some(domain_input(1)),
            realized_program_id: Some(domain_input(1)),
            desired_preview_id: Some(domain_input(2)),
            realized_preview_id: Some(domain_input(2)),
        },
        ProjectPosition {
            revision: 0,
            state_epoch: 1,
            event_sequence: 0,
            frames_rendered: 0,
            runtime_generation: 0,
            clock_time_nanos: 0,
        },
        Vec::new(),
    )
    .unwrap();
    ProjectStore::new(path).unwrap().save(&project).unwrap();
}

fn canonical_project() -> Project {
    let frame_rate = FrameRate::new(25, 1).unwrap();
    let mut project = Project::new(
        project_id(),
        "Process Test",
        ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(1_280, 720).unwrap(),
                frame_rate,
                pixel_format: PixelFormat::Rgba8,
                scan: ScanMode::Progressive,
                color: ColorMetadata::default(),
            },
            audio: AudioFormat {
                sample_rate: SampleRate::new(44_100).unwrap(),
                sample_format: SampleFormat::I24,
                channels: ChannelLayout::stereo(),
            },
        },
    )
    .with_restart_policy(RestartPolicy::Always);
    for (index, input) in [
        SimulatedInput::new(
            SimulatedVideo::Solid(SolidColor::new(12, 34, 56, 255)),
            SimulatedAudio::Sine { frequency_hz: 997 },
        ),
        SimulatedInput::new(SimulatedVideo::Bars, SimulatedAudio::Silence),
        SimulatedInput::new(
            SimulatedVideo::Solid(SolidColor::new(210, 90, 30, 255)),
            SimulatedAudio::Sine { frequency_hz: 440 },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let number = u128::try_from(index + 1).unwrap();
        project.add_input(Input {
            id: domain_input(number),
            name: format!("Simulated {number}"),
            kind: InputKind::Simulated(input),
            required_capabilities: vec![format!("simulation.source.{number}")],
        });
    }
    project.set_main_mix(MainMix::new(domain_input(1), domain_input(2)));
    project.add_scene(Scene {
        id: scene_id(1),
        name: "Program scene".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![Layer {
            name: "Source".into(),
            source: SourceRef::Input(domain_input(1)),
            enabled: true,
            geometry: LayerGeometry::new(0, 0, 1_280, 720, Rotation::Deg0),
            crop: None,
            mask: Some(RectMask::new(100, 50, 1_000, 600).inverted(true)),
            opacity: u8::MAX,
            z_order: 0,
        }],
    });
    project.add_audio_bus(AudioBus {
        id: bus_id(1),
        name: "Program bus".into(),
        sends: Vec::new(),
    });
    project.add_output(Output {
        id: output_id(1),
        name: "Program output".into(),
        video_source: scene_id(1),
        audio_source: bus_id(1),
        startup: StartupPolicy::ReconcileDesiredState,
        required_capabilities: vec!["simulation.output".into()],
    });
    project
}

fn command(id: &str, key: &str, payload: CommandPayload) -> WireMessage {
    command_version(CURRENT_PROTOCOL_VERSION, id, key, payload)
}

fn command_version(
    protocol: ProtocolVersion,
    id: &str,
    key: &str,
    payload: CommandPayload,
) -> WireMessage {
    WireMessage::Command(CommandMessage {
        protocol,
        id: id.into(),
        idempotency_key: key.into(),
        expected_revision: None,
        deadline_ms: None,
        payload,
    })
}

fn project_id() -> ProjectId {
    ProjectId::new(NonZeroU128::new(PROJECT_ID).unwrap())
}

fn input(value: u128) -> WireInputId {
    WireInputId::from_domain(domain_input(value))
}

fn domain_input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(INPUT_ID_BASE + value).unwrap())
}

fn scene_id(value: u128) -> SceneId {
    SceneId::new(NonZeroU128::new(INPUT_ID_BASE + 100 + value).unwrap())
}

fn bus_id(value: u128) -> BusId {
    BusId::new(NonZeroU128::new(INPUT_ID_BASE + 200 + value).unwrap())
}

fn output_id(value: u128) -> OutputId {
    OutputId::new(NonZeroU128::new(INPUT_ID_BASE + 300 + value).unwrap())
}
