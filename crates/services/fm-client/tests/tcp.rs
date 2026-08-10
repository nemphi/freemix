#![cfg(feature = "std-tcp")]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroU128;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fm_client::{
    Client, ClientConfig, ClientError, CommandStatus, ConnectionState, DisconnectCause, Intake,
    SessionEvent, SyncMode, TcpSession, TcpSessionError,
};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, ClientType, CodecError, CommandPayload,
    CommandResult, EngineIdentity, EventCursor, EventMessage, EventPayload, FadeToBlackPosition,
    FadeToBlackState, HandshakeOutcome, HandshakeRequest, HandshakeResponse, LineDecoder,
    MAX_LINE_BYTES, ManualTransitionStatus, OverlayStatus, ProtocolVersion, ResumeCursor, Role,
    RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity, SnapshotMessage, SnapshotReason,
    WireInputId, WireMessage, encode_line,
};
use fm_types::ProjectId;
use fm_ui_model::ModelError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

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

fn project_id() -> ProjectId {
    ProjectId::new(NonZeroU128::new(7).unwrap())
}

fn engine() -> EngineIdentity {
    EngineIdentity {
        engine_id: "engine-a".to_owned(),
        state_epoch: 3,
        log_id: "log-a".to_owned(),
    }
}

fn server() -> ServerIdentity {
    ServerIdentity {
        engine_id: "engine-a".to_owned(),
        project_id: project_id().to_string(),
        state_epoch: 3,
        log_id: "log-a".to_owned(),
    }
}

fn client(capacity: usize) -> Client {
    client_with_completed_history(capacity, fm_client::DEFAULT_COMPLETED_COMMAND_CAPACITY)
}

fn client_with_completed_history(capacity: usize, completed_command_capacity: usize) -> Client {
    let mut config = ClientConfig::new(
        "tcp-test",
        ClientType::Integration,
        Role::Operator,
        "tcp-client",
        project_id(),
    );
    config.outbound_capacity = capacity;
    config.completed_command_capacity = completed_command_capacity;
    config.initial_backoff_ms = 10;
    config.max_backoff_ms = 40;
    Client::new(config).unwrap()
}

fn handshake(revision: u64, outcome: HandshakeOutcome) -> HandshakeResponse {
    handshake_version(CURRENT_PROTOCOL_VERSION, revision, outcome)
}

fn handshake_version(
    protocol: ProtocolVersion,
    revision: u64,
    outcome: HandshakeOutcome,
) -> HandshakeResponse {
    HandshakeResponse {
        protocol,
        granted_role: Role::Operator,
        permissions: vec!["switcher.take".to_owned()],
        capabilities: CapabilityReportSummary {
            digest: "sha256:test".to_owned(),
            total: 1,
            available: 1,
            degraded: 0,
            unavailable: 0,
        },
        server: server(),
        current_revision: revision,
        outcome,
    }
}

fn snapshot(revision: u64) -> SnapshotMessage {
    SnapshotMessage {
        engine: engine(),
        revision,
        show_name: "Show".to_owned(),
        inputs: input_statuses(),
        input_audio_strips: input_audio_strips(),
        desired_program: input(1),
        desired_preview: input(2),
        realized_program: input(1),
        realized_preview: input(2),
        desired_manual_transition: ManualTransitionStatus::Inactive,
        realized_manual_transition: ManualTransitionStatus::Inactive,
        desired_fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        realized_fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        stingers: Vec::new(),
        desired_overlays: OverlayStatus::empty_channels(),
        realized_overlays: OverlayStatus::empty_channels(),
    }
}

fn event(revision: u64) -> EventMessage {
    EventMessage {
        cursor: EventCursor {
            engine: engine(),
            revision,
        },
        payload: EventPayload::DesiredSwitcher {
            program: input(2),
            preview: input(1),
            manual_transition: ManualTransitionStatus::Inactive,
            fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            overlays: OverlayStatus::empty_channels(),
            input_audio_strips: input_audio_strips(),
        },
    }
}

fn runtime_event(revision: u64) -> RuntimeEventMessage {
    RuntimeEventMessage {
        server: server(),
        revision,
        generation: 1,
        sequence: 1,
        event: RuntimeLifecycleEvent::Realized {
            domain: "switcher".to_owned(),
            manual_transition: ManualTransitionStatus::Inactive,
            fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
        },
    }
}

fn accept_snapshot(peer: &mut Peer, revision: u64) -> HandshakeRequest {
    accept_snapshot_version(peer, CURRENT_PROTOCOL_VERSION, revision)
}

fn accept_snapshot_version(
    peer: &mut Peer,
    protocol: ProtocolVersion,
    revision: u64,
) -> HandshakeRequest {
    let WireMessage::HandshakeRequest(request) = peer.receive() else {
        panic!("adapter emitted a non-current or invalid handshake")
    };
    peer.send(&WireMessage::HandshakeResponse(handshake_version(
        protocol,
        revision,
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor,
        },
    )));
    peer.send(&WireMessage::Snapshot(snapshot(revision)));
    request
}

fn accept_resume(peer: &mut Peer, revision: u64) -> ResumeCursor {
    accept_resume_version(peer, CURRENT_PROTOCOL_VERSION, revision)
}

fn accept_resume_version(
    peer: &mut Peer,
    protocol: ProtocolVersion,
    revision: u64,
) -> ResumeCursor {
    let WireMessage::HandshakeRequest(request) = peer.receive() else {
        panic!("adapter emitted a non-current or invalid handshake")
    };
    let cursor = request.resume_cursor.expect("resume cursor");
    peer.send(&WireMessage::HandshakeResponse(handshake_version(
        protocol,
        revision,
        HandshakeOutcome::Resume {
            cursor: cursor.clone(),
        },
    )));
    cursor
}

fn spawn_server(run: impl FnOnce(TcpListener) + Send + 'static) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || run(listener));
    (address, handle)
}

struct Peer {
    stream: TcpStream,
    decoder: LineDecoder,
    pending: VecDeque<WireMessage>,
}

impl Peer {
    fn accept(listener: &TcpListener) -> Self {
        let (stream, _) = listener.accept().unwrap();
        Self {
            stream,
            decoder: LineDecoder::new(),
            pending: VecDeque::new(),
        }
    }

    fn receive(&mut self) -> WireMessage {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return message;
            }
            let mut buffer = [0_u8; 4096];
            let read = self.stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "unexpected client EOF");
            self.pending
                .extend(self.decoder.push(&buffer[..read]).unwrap());
        }
    }

    fn send(&mut self, message: &WireMessage) {
        self.stream
            .write_all(encode_line(message).unwrap().as_bytes())
            .unwrap();
        self.stream.flush().unwrap();
    }
}

#[test]
fn snapshot_heartbeat_command_result_and_events_preserve_wire_order() {
    let (address, server_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        let request = accept_snapshot(&mut peer, 4);
        assert_eq!(request.resume_cursor, None);

        let WireMessage::Heartbeat(heartbeat) = peer.receive() else {
            panic!("expected heartbeat before command")
        };
        assert_eq!(heartbeat.last_applied.unwrap().revision, 4);
        let WireMessage::Command(command) = peer.receive() else {
            panic!("expected command")
        };
        peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 5,
            scheduled_frame: Some(9),
        }));
        peer.send(&WireMessage::Event(event(5)));
        peer.send(&WireMessage::RuntimeEvent(runtime_event(5)));
    });

    let mut session = TcpSession::new(client(4));
    assert_eq!(
        session.connect(address, CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Snapshot
        }
    );
    session.queue_heartbeat(1_234).unwrap();
    let command = session
        .queue_command(CommandPayload::Cut, "cut-once", Some(4), None)
        .unwrap();
    assert_eq!(session.flush().unwrap(), 2);

    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::CommandResult {
            result: CommandResult::Accepted { ref id, .. },
            intake: Intake::ResultReconciled,
        } if id == &command.id
    ));
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::Event {
            intake: Intake::EventApplied,
            ..
        }
    ));
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::RuntimeEvent {
            intake: Intake::RuntimeEventObserved,
            ..
        }
    ));
    assert_eq!(session.client().last_applied_cursor().unwrap().revision, 5);
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::Disconnected {
            cause: DisconnectCause::Eof,
            backoff
        } if backoff.attempt == 1 && backoff.delay_ms == 10
    ));
    assert!(matches!(
        session.receive(),
        Err(TcpSessionError::NotConnected)
    ));
    server_thread.join().unwrap();
}

#[test]
fn reconnect_consumes_resume_events_before_ready() {
    let (release_tx, release_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut first = Peer::accept(&listener);
        accept_snapshot(&mut first, 4);
        release_rx.recv().unwrap();
        drop(first);

        let mut second = Peer::accept(&listener);
        let cursor = accept_resume(&mut second, 5);
        assert_eq!(cursor.revision, 4);
        second.send(&WireMessage::Event(event(5)));
    });

    let mut session = TcpSession::new(client(4));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    release_tx.send(()).unwrap();
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::Disconnected { .. }
    ));
    assert_eq!(
        session.connect(address, CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Resume
        }
    );
    assert_eq!(session.client().state(), &ConnectionState::Ready);
    assert_eq!(session.client().last_applied_cursor().unwrap().revision, 5);
    server_thread.join().unwrap();
}

#[test]
fn unresolved_command_retries_original_envelope_but_completed_result_does_not() {
    let (first_command_tx, first_command_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut first = Peer::accept(&listener);
        accept_snapshot(&mut first, 4);
        let WireMessage::Command(command) = first.receive() else {
            panic!("expected first command")
        };
        first_command_tx.send(command.clone()).unwrap();
        drop(first);

        let mut second = Peer::accept(&listener);
        accept_resume(&mut second, 4);
        let WireMessage::Command(retried) = second.receive() else {
            panic!("expected unresolved retry")
        };
        assert_eq!(retried, command);
        second.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: retried.id,
            revision: 4,
            scheduled_frame: None,
        }));
        assert_eq!(second.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut third = Peer::accept(&listener);
        accept_resume(&mut third, 4);
        assert!(matches!(third.receive(), WireMessage::Heartbeat(_)));
    });

    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    let command = session
        .queue_command(CommandPayload::Cut, "durable-key", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    assert_eq!(first_command_rx.recv().unwrap(), command);
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::Disconnected { .. }
    ));

    session.connect(address, CONNECT_TIMEOUT).unwrap();
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::CommandResult {
            intake: Intake::ResultReconciled,
            ..
        }
    ));
    assert!(matches!(
        session.client().command(&command.id).unwrap().status,
        CommandStatus::Completed(_)
    ));
    session.disconnect().unwrap();

    session.connect(address, CONNECT_TIMEOUT).unwrap();
    session.queue_heartbeat(9_999).unwrap();
    assert_eq!(session.flush().unwrap(), 1);
    server_thread.join().unwrap();
}

#[test]
fn replayed_evicted_receipt_forces_snapshot_without_retransmitting_collision() {
    let (address, server_thread) = spawn_server(move |listener| {
        let mut first = Peer::accept(&listener);
        accept_snapshot(&mut first, 4);

        let WireMessage::Command(old) = first.receive() else {
            panic!("expected original command")
        };
        first.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: old.id.clone(),
            revision: 4,
            scheduled_frame: None,
        }));
        let WireMessage::Command(evictor) = first.receive() else {
            panic!("expected history evictor")
        };
        first.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: evictor.id,
            revision: 4,
            scheduled_frame: None,
        }));
        let WireMessage::Command(collision) = first.receive() else {
            panic!("expected key-collision command")
        };
        assert_eq!(collision.idempotency_key, old.idempotency_key);
        first.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: old.id,
            revision: 4,
            scheduled_frame: None,
        }));
        assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut second = Peer::accept(&listener);
        let request = accept_snapshot(&mut second, 4);
        assert_eq!(request.resume_cursor, None);
        assert!(matches!(second.receive(), WireMessage::Heartbeat(_)));
    });

    let mut session = TcpSession::new(client_with_completed_history(2, 1));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    let old = session
        .queue_command(CommandPayload::Cut, "retained-server-key", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    session.receive().unwrap();
    session
        .queue_command(CommandPayload::Cut, "evictor", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    session.receive().unwrap();
    assert!(session.client().command(&old.id).is_none());

    let collision = session
        .queue_command(
            CommandPayload::SelectPreview { input: input(1) },
            "retained-server-key",
            Some(4),
            None,
        )
        .unwrap();
    session.flush().unwrap();
    let error = session.receive().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("authoritative snapshot is required")
    );
    assert!(matches!(
        error,
        TcpSessionError::ResyncRequired(inner)
            if matches!(inner.as_ref(), ClientError::IdempotencyReplayCollision {
                received_command_id,
                affected_command_ids,
            } if received_command_id == &old.id
                && affected_command_ids == &vec![collision.id.clone()])
    ));
    assert!(matches!(
        session.client().command(&collision.id).unwrap().status,
        CommandStatus::TerminalUncertain(_)
    ));
    assert!(session.client().model().pending_commands().is_empty());
    assert_eq!(session.in_flight_len(), 1);

    assert!(matches!(
        session.connect(address, CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Snapshot
        }
    ));
    assert_eq!(session.in_flight_len(), 0);
    session.queue_heartbeat(9_999).unwrap();
    assert_eq!(session.flush().unwrap(), 1);
    server_thread.join().unwrap();
}

#[test]
fn in_flight_and_outbound_queues_remain_bounded() {
    let (command_tx, command_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut peer = Peer::accept(&listener);
        accept_snapshot(&mut peer, 4);
        let command = peer.receive();
        command_tx.send(command).unwrap();
        release_rx.recv().unwrap();
    });

    let mut session = TcpSession::new(client(1));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    session
        .queue_command(CommandPayload::Cut, "first", Some(4), None)
        .unwrap();
    assert_eq!(session.flush().unwrap(), 1);
    assert!(matches!(
        command_rx.recv().unwrap(),
        WireMessage::Command(_)
    ));
    session
        .queue_command(CommandPayload::Cut, "second", Some(4), None)
        .unwrap();
    assert_eq!(session.flush().unwrap(), 0);
    assert_eq!(session.in_flight_len(), 1);
    assert_eq!(session.client().outbound_len(), 1);
    assert!(matches!(
        session.queue_heartbeat(0),
        Err(TcpSessionError::Client(ClientError::QueueFull {
            capacity: 1
        }))
    ));
    release_tx.send(()).unwrap();
    server_thread.join().unwrap();
}

#[test]
fn oversized_line_disconnects_once_and_enters_backoff() {
    let (address, server_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
        peer.stream
            .write_all(&vec![b'x'; MAX_LINE_BYTES + 1])
            .unwrap();
        peer.stream.flush().unwrap();
    });

    let mut session = TcpSession::new(client(2));
    assert!(matches!(
        session.connect(address, CONNECT_TIMEOUT),
        Err(TcpSessionError::Codec(CodecError::LineTooLong))
    ));
    assert_eq!(session.reconnect_backoff().unwrap().attempt, 1);
    assert!(matches!(
        session.receive(),
        Err(TcpSessionError::NotConnected)
    ));
    assert_eq!(session.reconnect_backoff().unwrap().attempt, 1);
    server_thread.join().unwrap();
}

#[test]
fn wrong_first_record_and_wrong_project_reject_current_handshake() {
    let (address, wrong_record_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
        peer.send(&WireMessage::Snapshot(snapshot(0)));
    });
    let mut wrong_record = TcpSession::new(client(2));
    assert!(matches!(
        wrong_record.connect(address, CONNECT_TIMEOUT),
        Err(TcpSessionError::ExpectedHandshake)
    ));
    assert_eq!(wrong_record.reconnect_backoff().unwrap().attempt, 1);
    wrong_record_thread.join().unwrap();

    let (address, wrong_project_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
        let mut response = handshake(
            0,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        );
        response.server.project_id = "999".to_owned();
        peer.send(&WireMessage::HandshakeResponse(response));
    });
    let mut wrong_project = TcpSession::new(client(2));
    assert!(matches!(
        wrong_project.connect(address, CONNECT_TIMEOUT),
        Err(TcpSessionError::Client(ClientError::InvalidHandshake(
            "server selected a different project"
        )))
    ));
    assert_eq!(wrong_project.reconnect_backoff().unwrap().attempt, 1);
    wrong_project_thread.join().unwrap();
}

#[test]
fn protocol_mismatch_remains_terminal_after_socket_close() {
    let (address, server_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
        let mut response = handshake(
            0,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        );
        response.protocol = ProtocolVersion::new(99, 0);
        peer.send(&WireMessage::HandshakeResponse(response));
        assert_eq!(peer.stream.read(&mut [0_u8; 1]).unwrap(), 0);
    });
    let mut session = TcpSession::new(client(2));

    assert!(matches!(
        session.connect(address, CONNECT_TIMEOUT),
        Err(TcpSessionError::Client(ClientError::ProtocolMismatch(
            ProtocolVersion {
                major: 99,
                minor: 0
            }
        )))
    ));
    assert_eq!(
        session.client().state(),
        &ConnectionState::ProtocolMismatch {
            protocol: ProtocolVersion::new(99, 0)
        }
    );
    assert_eq!(session.reconnect_backoff(), None);
    assert!(session.connection().is_none());
    server_thread.join().unwrap();
}

#[test]
fn encoding_failure_disconnects_and_preserves_unresolved_envelope() {
    let (address, server_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        accept_snapshot(&mut peer, 4);
        assert_eq!(peer.stream.read(&mut [0_u8; 1]).unwrap(), 0);
    });
    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    let command = session
        .queue_command(
            CommandPayload::Cut,
            "x".repeat(MAX_LINE_BYTES),
            Some(4),
            None,
        )
        .unwrap();

    assert!(matches!(
        session.flush(),
        Err(TcpSessionError::Codec(
            CodecError::LineTooLong | CodecError::FieldValueTooLong
        ))
    ));
    assert_eq!(session.in_flight_len(), 1);
    assert_eq!(session.reconnect_backoff().unwrap().attempt, 1);
    assert!(session.connection().is_none());
    let record = session.client().command(&command.id).unwrap();
    assert_eq!(record.command, command);
    assert_eq!(record.status, CommandStatus::Sent);
    server_thread.join().unwrap();
}

#[test]
fn silent_handshake_can_be_cancelled_without_waiting_for_peer_eof() {
    let (request_tx, request_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut peer = Peer::accept(&listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
        request_tx.send(()).unwrap();
        assert_eq!(peer.stream.read(&mut [0_u8; 1]).unwrap(), 0);
    });

    let mut session = TcpSession::new(client(2));
    let mut polls = 0;
    let started = Instant::now();
    let result =
        session.connect_cancellable(address, CONNECT_TIMEOUT, Duration::from_millis(10), || {
            polls += 1;
            polls >= 7
        });
    request_rx.recv().unwrap();
    assert!(matches!(
        result,
        Err(TcpSessionError::Cancelled { backoff })
            if backoff.attempt == 1 && backoff.delay_ms == 10
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    server_thread.join().unwrap();
}

#[test]
fn silent_command_response_can_be_cancelled_and_retried() {
    let (command_tx, command_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut peer = Peer::accept(&listener);
        accept_snapshot(&mut peer, 4);
        assert!(matches!(peer.receive(), WireMessage::Command(_)));
        command_tx.send(()).unwrap();
        assert_eq!(peer.stream.read(&mut [0_u8; 1]).unwrap(), 0);
    });

    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    session
        .queue_command(CommandPayload::Cut, "silent-command", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    command_rx.recv().unwrap();

    let mut polls = 0;
    assert!(matches!(
        session.receive_cancellable(Duration::from_millis(10), || {
            polls += 1;
            polls >= 3
        }),
        Err(TcpSessionError::Cancelled { backoff })
            if backoff.attempt == 1 && backoff.delay_ms == 10
    ));
    assert_eq!(session.in_flight_len(), 1);
    assert_eq!(session.reconnect_backoff().unwrap().attempt, 1);
    server_thread.join().unwrap();
}

#[test]
fn cancellable_receive_preserves_a_partial_framed_record() {
    let (address, server_thread) = spawn_server(move |listener| {
        let mut peer = Peer::accept(&listener);
        accept_snapshot(&mut peer, 4);
        let WireMessage::Command(command) = peer.receive() else {
            panic!("expected command")
        };
        let encoded = encode_line(&WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 4,
            scheduled_frame: None,
        }))
        .unwrap();
        let split = encoded.len() / 2;
        peer.stream.write_all(&encoded.as_bytes()[..split]).unwrap();
        peer.stream.flush().unwrap();
        thread::sleep(Duration::from_millis(40));
        peer.stream.write_all(&encoded.as_bytes()[split..]).unwrap();
        peer.stream.flush().unwrap();
    });

    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    let command = session
        .queue_command(CommandPayload::Cut, "partial-result", Some(4), None)
        .unwrap();
    session.flush().unwrap();

    let mut polls = 0;
    assert!(matches!(
        session
            .receive_cancellable(Duration::from_millis(5), || {
                polls += 1;
                false
            })
            .unwrap(),
        SessionEvent::CommandResult {
            result: CommandResult::Accepted { ref id, .. },
            ..
        } if id == &command.id
    ));
    assert!(polls > 1, "partial record did not span polling cycles");
    server_thread.join().unwrap();
}

#[test]
fn tcp_establishment_checks_cancellation_before_blocking() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut session = TcpSession::new(client(2));
    let started = Instant::now();

    assert!(matches!(
        session.connect_cancellable(
            address,
            CONNECT_TIMEOUT,
            Duration::from_millis(10),
            || true,
        ),
        Err(TcpSessionError::Cancelled { backoff }) if backoff.attempt == 1
    ));
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(listener.set_nonblocking(true).is_ok());
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn event_gap_forces_snapshot_and_preserves_unresolved_command() {
    let (original_tx, original_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut first = Peer::accept(&listener);
        accept_snapshot(&mut first, 4);
        let WireMessage::Command(original) = first.receive() else {
            panic!("expected original command")
        };
        original_tx.send(original.clone()).unwrap();
        first.send(&WireMessage::Event(event(6)));
        assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut second = Peer::accept(&listener);
        let WireMessage::HandshakeRequest(request) = second.receive() else {
            panic!("expected snapshot reconnect")
        };
        assert_eq!(request.resume_cursor, None);
        second.send(&WireMessage::HandshakeResponse(handshake(
            6,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::HistoryUnavailable,
            },
        )));
        second.send(&WireMessage::Snapshot(snapshot(6)));
        let WireMessage::Command(retried) = second.receive() else {
            panic!("expected unresolved command retry")
        };
        assert_eq!(retried, original);
    });

    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    let command = session
        .queue_command(CommandPayload::Cut, "event-gap", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    assert_eq!(original_rx.recv().unwrap(), command);
    assert!(
        matches!(session.receive(), Err(TcpSessionError::ResyncRequired(error))
        if matches!(error.as_ref(), ClientError::ResyncRequired {
                expected_revision: 5,
                received_revision: 6,
            }))
    );
    assert_eq!(session.in_flight_len(), 1);
    assert_eq!(session.reconnect_backoff().unwrap().attempt, 1);
    assert!(matches!(
        session.connect(address, CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Snapshot
        }
    ));
    server_thread.join().unwrap();
}

#[test]
fn runtime_sequence_gap_enters_backoff_and_retries_unresolved_command() {
    let (original_tx, original_rx) = mpsc::channel();
    let (address, server_thread) = spawn_server(move |listener| {
        let mut first = Peer::accept(&listener);
        first
            .stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .unwrap();
        accept_snapshot(&mut first, 4);
        let WireMessage::Command(original) = first.receive() else {
            panic!("expected original command")
        };
        original_tx.send(original.clone()).unwrap();
        first.send(&WireMessage::RuntimeEvent(runtime_event(4)));
        let mut gap = runtime_event(4);
        gap.sequence = 3;
        first.send(&WireMessage::RuntimeEvent(gap));
        assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut second = Peer::accept(&listener);
        second
            .stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .unwrap();
        assert_eq!(accept_resume(&mut second, 4).revision, 4);
        let WireMessage::Command(retried) = second.receive() else {
            panic!("expected unresolved command retry")
        };
        assert_eq!(retried, original);
    });

    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    let command = session
        .queue_command(CommandPayload::Cut, "runtime-gap", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    assert_eq!(original_rx.recv().unwrap(), command);
    assert!(matches!(
        session.receive().unwrap(),
        SessionEvent::RuntimeEvent {
            intake: Intake::RuntimeEventObserved,
            ..
        }
    ));
    assert!(matches!(
        session.receive(),
        Err(TcpSessionError::ResyncRequired(error))
            if matches!(error.as_ref(), ClientError::RuntimeSequenceGap {
                generation: 1,
                expected_sequence: 2,
                received_sequence: 3,
            })
    ));
    assert_eq!(session.in_flight_len(), 1);
    assert_eq!(session.reconnect_backoff().unwrap().attempt, 1);
    assert!(matches!(
        session.connect(address, CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Resume
        }
    ));
    server_thread.join().unwrap();
}

#[test]
fn model_error_forces_snapshot_and_preserves_unresolved_command() {
    let (address, server_thread) = spawn_server(move |listener| {
        let mut first = Peer::accept(&listener);
        accept_snapshot(&mut first, 4);
        let WireMessage::Command(original) = first.receive() else {
            panic!("expected original command")
        };
        first.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(99),
                manual_transition: ManualTransitionStatus::Inactive,
                fade_to_black: FadeToBlackState {
                    target_active: false,
                    position: FadeToBlackPosition::LIVE,
                },
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(),
            },
        }));
        assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut second = Peer::accept(&listener);
        let WireMessage::HandshakeRequest(request) = second.receive() else {
            panic!("expected snapshot reconnect")
        };
        assert_eq!(request.resume_cursor, None);
        second.send(&WireMessage::HandshakeResponse(handshake(
            5,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::HistoryUnavailable,
            },
        )));
        second.send(&WireMessage::Snapshot(snapshot(5)));
        let WireMessage::Command(retried) = second.receive() else {
            panic!("expected unresolved command retry")
        };
        assert_eq!(retried, original);
    });

    let mut session = TcpSession::new(client(2));
    session.connect(address, CONNECT_TIMEOUT).unwrap();
    session
        .queue_command(CommandPayload::Cut, "model-error", Some(4), None)
        .unwrap();
    session.flush().unwrap();
    assert!(
        matches!(session.receive(), Err(TcpSessionError::ResyncRequired(error))
        if matches!(error.as_ref(), ClientError::Model(ModelError::UnknownInput(value))
            if *value == input(99).to_domain()))
    );
    assert_eq!(session.in_flight_len(), 1);
    assert!(matches!(
        session.connect(address, CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Snapshot
        }
    ));
    server_thread.join().unwrap();
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
