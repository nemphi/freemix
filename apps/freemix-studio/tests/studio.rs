use std::{
    collections::VecDeque,
    fs,
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use fm_client::{ClientError, CommandStatus, Intake, SessionEvent, SyncMode, TcpSessionError};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, CommandPayload, CommandResult,
    DiagnosticsResponse, EngineIdentity, EventCursor, EventMessage, EventPayload,
    FadeToBlackPosition, FadeToBlackState, HandshakeOutcome, HandshakeResponse,
    HeartbeatAcknowledgementMessage, HeartbeatMessage, LineDecoder, ManualTransitionStatus,
    OverlayStatus, ProtocolVersion, Role, RuntimeEventMessage, RuntimeLifecycleEvent,
    ServerIdentity, SnapshotMessage, SnapshotReason, WireInputId, WireMessage, encode_line,
};
use fm_types::{InputId, ProjectId};
use freemix_studio::{
    Command, ConnectionConfig, ControlTransport, DaemonSupervisor, ExistingConfig, LifecycleState,
    ReadinessRecord, RestartPolicy, StudioConfig, StudioError, StudioRuntime, SupervisedConfig,
    SupervisorError, SupervisorState, parse_args,
};
use tungstenite::{
    Message, accept_hdr,
    handshake::server::{Request, Response},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DIAGNOSE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const PROJECT_VALUE: u128 = 18_446_744_073_709_551_657;
static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const WEB_OPEN_CHILD_MARKER: &str = "FREEMIX_STUDIO_WEB_OPEN_CHILD";

fn project_id() -> ProjectId {
    ProjectId::new(NonZeroU128::new(PROJECT_VALUE).unwrap())
}

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
}

fn model_input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
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

fn engine() -> EngineIdentity {
    EngineIdentity {
        engine_id: "engine-studio".to_owned(),
        state_epoch: 3,
        log_id: "log-studio".to_owned(),
    }
}

fn server(project: ProjectId) -> ServerIdentity {
    ServerIdentity {
        engine_id: "engine-studio".to_owned(),
        project_id: project.to_string(),
        state_epoch: 3,
        log_id: "log-studio".to_owned(),
    }
}

fn handshake(project: ProjectId, revision: u64, outcome: HandshakeOutcome) -> HandshakeResponse {
    handshake_version(CURRENT_PROTOCOL_VERSION, project, revision, outcome)
}

fn handshake_version(
    protocol: ProtocolVersion,
    project: ProjectId,
    revision: u64,
    outcome: HandshakeOutcome,
) -> HandshakeResponse {
    HandshakeResponse {
        protocol,
        granted_role: Role::Operator,
        permissions: vec!["switcher.take".to_owned()],
        capabilities: CapabilityReportSummary {
            digest: "sha256:studio-test".to_owned(),
            total: 1,
            available: 1,
            degraded: 0,
            unavailable: 0,
        },
        server: server(project),
        current_revision: revision,
        outcome,
    }
}

fn snapshot(revision: u64) -> SnapshotMessage {
    SnapshotMessage {
        engine: engine(),
        revision,
        show_name: "Studio test".to_owned(),
        inputs: input_statuses(),
        outputs: Vec::new(),
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

fn runtime_event(revision: u64) -> RuntimeEventMessage {
    RuntimeEventMessage {
        server: server(project_id()),
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

fn existing_config(address: SocketAddr, expected_project_id: ProjectId) -> StudioConfig {
    StudioConfig {
        connection: ConnectionConfig::Existing(ExistingConfig {
            address,
            expected_project_id,
        }),
        client_id: "studio-test".to_owned(),
        desired_role: Role::Operator,
        restart_policy: RestartPolicy::default(),
        osc_listen: None,
        transport: ControlTransport::Tcp,
    }
}

fn spawn_server(run: impl FnOnce(TcpListener) + Send + 'static) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (address, thread::spawn(move || run(listener)))
}

fn remaining(deadline: Instant) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(!remaining.is_zero(), "scenario deadline elapsed");
    remaining
}

fn assert_web_child_success(mut child: Child, token: &str, deadline: Instant) {
    let failure = loop {
        match child.try_wait() {
            Ok(Some(_)) => break None,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => break Some("WebSocket test child did not exit before deadline".to_owned()),
            Err(error) => break Some(format!("cannot wait for WebSocket test child: {error}")),
        }
    };
    if let Some(failure) = failure {
        if !terminate_child(&mut child) {
            panic!("{failure}; cleanup_stopped=false");
        }
        let output = child.wait_with_output().unwrap_or_else(|error| {
            panic!("{failure}; cleanup_stopped=true; cannot collect WebSocket test output: {error}")
        });
        assert!(!String::from_utf8_lossy(&output.stdout).contains(token));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(token));
        panic!(
            "{failure}; cleanup_stopped=true; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = child.wait_with_output().unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains(token));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(token));
    assert!(
        output.status.success(),
        "WebSocket test child failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn accept_stream_until(listener: &TcpListener, deadline: Instant) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "timed out waiting for Studio");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot accept Studio: {error}"),
        }
    }
}

struct Peer {
    stream: TcpStream,
    decoder: LineDecoder,
    pending: VecDeque<WireMessage>,
}

impl Peer {
    fn from_stream(stream: TcpStream) -> Self {
        Self {
            stream,
            decoder: LineDecoder::new(),
            pending: VecDeque::new(),
        }
    }

    fn accept(listener: &TcpListener) -> Self {
        let (stream, _) = listener.accept().unwrap();
        Self::from_stream(stream)
    }

    fn accept_until(listener: &TcpListener, deadline: Instant) -> Self {
        Self::from_stream(accept_stream_until(listener, deadline))
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

    fn receive_until(&mut self, deadline: Instant) -> WireMessage {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return message;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out reading Studio message");
            self.stream.set_read_timeout(Some(remaining)).unwrap();
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

    fn send_until(&mut self, message: &WireMessage, deadline: Instant) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out writing Studio message");
        self.stream.set_write_timeout(Some(remaining)).unwrap();
        self.send(message);
    }
}

fn serve_snapshot_then_resume(listener: &TcpListener) {
    let mut first = Peer::accept(listener);
    let WireMessage::HandshakeRequest(request) = first.receive() else {
        panic!("expected modern handshake request");
    };
    assert_eq!(request.protocol, CURRENT_PROTOCOL_VERSION);
    assert_eq!(request.resume_cursor, None);
    first.send(&WireMessage::HandshakeResponse(handshake(
        project_id(),
        4,
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor,
        },
    )));
    first.send(&WireMessage::Snapshot(snapshot(4)));

    let WireMessage::Heartbeat(heartbeat) = first.receive() else {
        panic!("expected heartbeat");
    };
    assert_eq!(heartbeat.sent_at_ms, 1234);
    assert_eq!(heartbeat.last_applied.as_ref().unwrap().revision, 4);
    first.send(&WireMessage::HeartbeatAcknowledgement(
        HeartbeatAcknowledgementMessage {
            server: heartbeat.server,
            heartbeat_sequence: heartbeat.sequence,
            received_at_ms: 1_235,
        },
    ));
    let WireMessage::Command(command) = first.receive() else {
        panic!("expected command");
    };
    first.send(&WireMessage::CommandResult(CommandResult::Accepted {
        id: command.id,
        revision: 5,
        scheduled_frame: Some(9),
    }));
    first.send(&WireMessage::Event(event(5)));
    first.send(&WireMessage::RuntimeEvent(runtime_event(5)));
    drop(first);

    let mut second = Peer::accept(listener);
    let WireMessage::HandshakeRequest(request) = second.receive() else {
        panic!("expected reconnect handshake request");
    };
    let cursor = request.resume_cursor.expect("resume cursor");
    assert_eq!(cursor.revision, 5);
    second.send(&WireMessage::HandshakeResponse(handshake(
        project_id(),
        6,
        HandshakeOutcome::Resume {
            cursor: cursor.clone(),
        },
    )));
    second.send(&WireMessage::Event(event(6)));
}

#[test]
fn existing_runtime_handles_status_runtime_heartbeat_eof_and_resume() {
    let (address, server_thread) = spawn_server(|listener| serve_snapshot_then_resume(&listener));

    let mut runtime = StudioRuntime::new(existing_config(address, project_id())).unwrap();
    assert_eq!(runtime.lifecycle().unwrap(), LifecycleState::Disconnected);
    assert_eq!(
        runtime.connect(CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Snapshot
        }
    );
    assert_eq!(runtime.lifecycle().unwrap(), LifecycleState::Ready);
    assert_eq!(
        runtime.session().client().session().unwrap().protocol,
        CURRENT_PROTOCOL_VERSION
    );

    runtime.send_heartbeat(1234).unwrap();
    assert!(matches!(
        runtime.receive().unwrap(),
        SessionEvent::HeartbeatAcknowledged { .. }
    ));
    let command = runtime
        .queue_command(CommandPayload::Cut, "cut-once", Some(4), None)
        .unwrap();
    assert_eq!(runtime.flush().unwrap(), 1);
    assert!(matches!(
        runtime.receive().unwrap(),
        SessionEvent::CommandResult {
            intake: Intake::ResultReconciled,
            ..
        }
    ));
    assert!(matches!(
        runtime
            .session()
            .client()
            .command(&command.id)
            .unwrap()
            .status,
        CommandStatus::Completed(_)
    ));
    assert!(matches!(
        runtime.receive().unwrap(),
        SessionEvent::Event { .. }
    ));
    assert!(matches!(
        runtime.receive().unwrap(),
        SessionEvent::RuntimeEvent { .. }
    ));
    assert!(matches!(
        runtime.receive().unwrap(),
        SessionEvent::Disconnected { .. }
    ));
    assert!(matches!(
        runtime.lifecycle().unwrap(),
        LifecycleState::Backoff(_)
    ));

    assert!(matches!(
        runtime.reconnect(Duration::from_millis(249), CONNECT_TIMEOUT),
        Err(StudioError::BackoffNotElapsed { .. })
    ));
    assert_eq!(
        runtime
            .reconnect(Duration::from_millis(250), CONNECT_TIMEOUT)
            .unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Resume
        }
    );
    assert_eq!(
        runtime
            .session()
            .client()
            .last_applied_cursor()
            .unwrap()
            .revision,
        6
    );
    server_thread.join().unwrap();
}

#[test]
fn existing_runtime_rejects_wrong_project_handshake() {
    let (address, server_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
        let wrong = ProjectId::new(NonZeroU128::new(999).unwrap());
        peer.send(&WireMessage::HandshakeResponse(handshake(
            wrong,
            0,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        )));
    });
    let mut runtime = StudioRuntime::new(existing_config(address, project_id())).unwrap();
    assert!(matches!(
        runtime.connect(CONNECT_TIMEOUT),
        Err(StudioError::Session(TcpSessionError::Client(
            ClientError::InvalidHandshake("server selected a different project")
        )))
    ));
    server_thread.join().unwrap();
}

#[test]
fn existing_runtime_accepts_the_current_contract() {
    let (address, server_thread) = spawn_server(|listener| {
        let mut peer = Peer::accept(&listener);
        let WireMessage::HandshakeRequest(request) = peer.receive() else {
            panic!("expected modern handshake request");
        };
        assert_eq!(request.protocol, CURRENT_PROTOCOL_VERSION);
        peer.send(&WireMessage::HandshakeResponse(handshake_version(
            CURRENT_PROTOCOL_VERSION,
            project_id(),
            4,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        )));
        let mut initial = snapshot(4);
        initial.desired_manual_transition = ManualTransitionStatus::Inactive;
        initial.realized_manual_transition = ManualTransitionStatus::Inactive;
        initial.desired_fade_to_black = FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        };
        initial.realized_fade_to_black = initial.desired_fade_to_black;
        peer.send(&WireMessage::Snapshot(initial));
    });

    let mut runtime = StudioRuntime::new(existing_config(address, project_id())).unwrap();
    assert_eq!(
        runtime.connect(CONNECT_TIMEOUT).unwrap(),
        SessionEvent::Connected {
            mode: SyncMode::Snapshot
        }
    );
    assert_eq!(
        runtime.session().client().session().unwrap().protocol,
        CURRENT_PROTOCOL_VERSION
    );
    server_thread.join().unwrap();
}

fn terminate_child(child: &mut Child) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + DIAGNOSE_CLEANUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => return false,
        }
    }
}

fn diagnose_cleanup(mut child: Child, failure: impl std::fmt::Display) -> String {
    if !terminate_child(&mut child) {
        return format!("{failure}; cleanup_stopped=false");
    }
    match child.wait_with_output() {
        Ok(output) => format!(
            "{failure}; cleanup_stopped=true; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => format!(
            "{failure}; cleanup_stopped=true; cannot collect Studio diagnose output: {error}"
        ),
    }
}

fn run_diagnose(address: SocketAddr, web_token: Option<&str>) -> Result<Output, String> {
    let (connect_option, address_option) = if web_token.is_some() {
        ("--web-connect", address.to_string())
    } else {
        ("--connect", address.to_string())
    };
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_freemix-studio"))
        .args([
            "--diagnose",
            connect_option,
            &address_option,
            "--project-id",
            &PROJECT_VALUE.to_string(),
        ])
        .envs(
            web_token
                .into_iter()
                .map(|token| ("FREEMIXD_WEB_TOKEN", token)),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start Studio diagnose: {error}"))?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("cannot collect Studio diagnose output: {error}"));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                return Err(diagnose_cleanup(
                    child,
                    format!("Studio diagnose did not exit before {CONNECT_TIMEOUT:?}"),
                ));
            }
            Err(error) => {
                return Err(diagnose_cleanup(
                    child,
                    format!("cannot wait for Studio diagnose: {error}"),
                ));
            }
        }
    }
}

fn websocket_message_until(
    peer: &mut tungstenite::WebSocket<TcpStream>,
    deadline: Instant,
) -> Message {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out reading WebSocket message");
        peer.get_ref().set_read_timeout(Some(remaining)).unwrap();
        match peer.read() {
            Ok(Message::Ping(payload)) => {
                peer.get_ref().set_write_timeout(Some(remaining)).unwrap();
                peer.send(Message::Pong(payload)).unwrap();
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Text(line)) => {
                assert!(line.ends_with('\n'), "WebSocket record missing newline");
                return Message::Text(line);
            }
            Ok(message) => return message,
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
            {
                panic!("timed out reading WebSocket message")
            }
            Err(error) => panic!("WebSocket read failed: {error:?}"),
        }
    }
}

fn websocket_send_line(
    peer: &mut tungstenite::WebSocket<TcpStream>,
    message: &WireMessage,
    deadline: Instant,
) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(!remaining.is_zero(), "timed out writing WebSocket message");
    peer.get_ref().set_write_timeout(Some(remaining)).unwrap();
    peer.send(Message::Text(encode_line(message).unwrap().into()))
        .unwrap();
}

fn receive_diagnose_heartbeat(peer: &mut Peer, deadline: Instant) -> HeartbeatMessage {
    let WireMessage::HandshakeRequest(request) = peer.receive_until(deadline) else {
        panic!("expected modern handshake request");
    };
    assert_eq!(request.protocol, CURRENT_PROTOCOL_VERSION);
    assert_eq!(request.desired_role, Role::Viewer);
    let mut response = handshake(
        project_id(),
        4,
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor,
        },
    );
    response.granted_role = Role::Viewer;
    response.permissions = vec!["view_status".to_owned()];
    peer.send_until(&WireMessage::HandshakeResponse(response), deadline);
    peer.send_until(&WireMessage::Snapshot(snapshot(4)), deadline);

    let WireMessage::Heartbeat(heartbeat) = peer.receive_until(deadline) else {
        panic!("expected heartbeat after snapshot");
    };
    assert_eq!(heartbeat.last_applied.as_ref().unwrap().revision, 4);
    heartbeat
}

fn assert_diagnose_success(output: Result<Output, String>) {
    let output = output.unwrap_or_else(|error| panic!("{error}"));
    assert!(
        output.status.success(),
        "Studio diagnose failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "liveness=ok sequence=1 received_at_ms=1234\ndiagnostics=v1 engine_id=engine-studio state_epoch=3 revision=5 retained_oldest=4 retained_newest=5 subscribers=1/8 retained_limit=64 subscriber_queue=16\n"
    );
}

#[test]
fn websocket_diagnose_uses_current_session_contract() {
    let token = "web-diagnostic-token-abcdefghijklmnopqrstuvwxyz-0123456789";
    let (address, server_thread) = spawn_server(move |listener| {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let stream = accept_stream_until(&listener, deadline);
        let remaining = deadline.saturating_duration_since(Instant::now());
        stream.set_read_timeout(Some(remaining)).unwrap();
        stream.set_write_timeout(Some(remaining)).unwrap();
        let callback = |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/v1/control");
            let authorization = request
                .headers()
                .get("authorization")
                .expect("missing authorization")
                .to_str()
                .expect("invalid authorization");
            assert!(
                authorization == format!("Bearer {token}"),
                "authorization mismatch"
            );
            Ok(response)
        };
        let mut peer = accept_hdr(stream, callback).expect("WebSocket handshake failed");
        let remaining = deadline.saturating_duration_since(Instant::now());
        peer.get_ref().set_write_timeout(Some(remaining)).unwrap();
        peer.send(Message::Ping(Vec::new().into())).unwrap();
        let line = match websocket_message_until(&mut peer, deadline) {
            Message::Text(line) => line,
            message => panic!("expected text record, got {message:?}"),
        };
        let WireMessage::HandshakeRequest(request) = fm_protocol::decode_line(&line).unwrap()
        else {
            panic!("expected handshake request")
        };
        assert_eq!(request.protocol, CURRENT_PROTOCOL_VERSION);
        assert_eq!(request.desired_role, Role::Viewer);
        let mut response = handshake(
            project_id(),
            4,
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        );
        response.granted_role = Role::Viewer;
        response.permissions = vec!["view_status".to_owned()];
        websocket_send_line(
            &mut peer,
            &WireMessage::HandshakeResponse(response),
            deadline,
        );
        websocket_send_line(&mut peer, &WireMessage::Snapshot(snapshot(4)), deadline);
        let line = match websocket_message_until(&mut peer, deadline) {
            Message::Text(line) => line,
            message => panic!("expected heartbeat text record, got {message:?}"),
        };
        let WireMessage::Heartbeat(heartbeat) = fm_protocol::decode_line(&line).unwrap() else {
            panic!("expected heartbeat")
        };
        websocket_send_line(&mut peer, &WireMessage::Event(event(5)), deadline);
        websocket_send_line(
            &mut peer,
            &WireMessage::HeartbeatAcknowledgement(HeartbeatAcknowledgementMessage {
                server: heartbeat.server,
                heartbeat_sequence: heartbeat.sequence,
                received_at_ms: 1_234,
            }),
            deadline,
        );
        let line = match websocket_message_until(&mut peer, deadline) {
            Message::Text(line) => line,
            message => panic!("expected diagnostics text record, got {message:?}"),
        };
        let WireMessage::DiagnosticsRequest(request) = fm_protocol::decode_line(&line).unwrap()
        else {
            panic!("expected diagnostics request")
        };
        websocket_send_line(
            &mut peer,
            &WireMessage::DiagnosticsResponse(DiagnosticsResponse {
                protocol: CURRENT_PROTOCOL_VERSION,
                request_id: request.request_id,
                engine: engine(),
                current_revision: 5,
                oldest_retained_revision: Some(4),
                newest_retained_revision: Some(5),
                subscriber_count: 1,
                retained_events_limit: 64,
                subscriber_limit: 8,
                subscriber_queue_limit: 16,
            }),
            deadline,
        );
    });

    let output = run_diagnose(address, Some(token));
    let server = server_thread.join();
    assert!(server.is_ok(), "WebSocket diagnostic server failed");
    let output = output.unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains(token));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(token));
    assert_diagnose_success(Ok(output));
}

#[test]
fn web_open_runtime_uses_websocket_transport() {
    let token = "web-open-token-abcdefghijklmnopqrstuvwxyz-0123456789";
    if std::env::var_os(WEB_OPEN_CHILD_MARKER).is_none() {
        let child = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "web_open_runtime_uses_websocket_transport",
                "--exact",
                "--nocapture",
            ])
            .env(WEB_OPEN_CHILD_MARKER, "1")
            .env("FREEMIXD_WEB_TOKEN", token)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_web_child_success(child, token, Instant::now() + CONNECT_TIMEOUT);
        return;
    }
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let (address, server_thread) = spawn_server(move |listener| {
        let stream = accept_stream_until(&listener, deadline);
        stream.set_read_timeout(Some(remaining(deadline))).unwrap();
        stream.set_write_timeout(Some(remaining(deadline))).unwrap();
        let callback = |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/v1/control");
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            let expected_authorization = format!("Bearer {token}");
            if authorization != Some(expected_authorization.as_str()) {
                panic!("authorization mismatch");
            }
            Ok(response)
        };
        let mut peer = accept_hdr(stream, callback).expect("WebSocket handshake failed");
        let Message::Text(line) = websocket_message_until(&mut peer, deadline) else {
            panic!("expected handshake text")
        };
        let WireMessage::HandshakeRequest(request) = fm_protocol::decode_line(&line).unwrap()
        else {
            panic!("expected handshake request")
        };
        assert_eq!(request.protocol, CURRENT_PROTOCOL_VERSION);
        websocket_send_line(
            &mut peer,
            &WireMessage::HandshakeResponse(handshake(
                project_id(),
                4,
                HandshakeOutcome::Snapshot {
                    reason: SnapshotReason::NoCursor,
                },
            )),
            deadline,
        );
        websocket_send_line(&mut peer, &WireMessage::Snapshot(snapshot(4)), deadline);
        let Message::Text(line) = websocket_message_until(&mut peer, deadline) else {
            panic!("expected command text")
        };
        let WireMessage::Command(command) = fm_protocol::decode_line(&line).unwrap() else {
            panic!("expected command")
        };
        assert_eq!(command.payload, CommandPayload::Cut);
        websocket_send_line(
            &mut peer,
            &WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision: 5,
                scheduled_frame: Some(9),
            }),
            deadline,
        );
        websocket_send_line(&mut peer, &WireMessage::Event(event(5)), deadline);
        websocket_send_line(
            &mut peer,
            &WireMessage::RuntimeEvent(runtime_event(5)),
            deadline,
        );
    });

    let client = (|| -> Result<(), String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Command::Open(config) = parse_args(
                [
                    "--web-connect",
                    &address.to_string(),
                    "--project-id",
                    &PROJECT_VALUE.to_string(),
                ]
                .map(str::to_owned),
            )
            .unwrap() else {
                panic!("expected normal Open command")
            };
            assert_eq!(config.transport, ControlTransport::WebSocket);
            let mut runtime = StudioRuntime::new(config).unwrap();
            assert_eq!(
                runtime.connect(remaining(deadline)).unwrap(),
                SessionEvent::Connected {
                    mode: SyncMode::Snapshot,
                }
            );
            let command = runtime
                .queue_command(CommandPayload::Cut, "web-cut", Some(4), None)
                .unwrap();
            assert_eq!(runtime.flush().unwrap(), 1);
            assert!(matches!(
                runtime
                    .receive_timeout(remaining(deadline))
                    .unwrap()
                    .unwrap_or_else(|| panic!("timed out waiting for command result")),
                SessionEvent::CommandResult {
                    intake: Intake::ResultReconciled,
                    ..
                }
            ));
            assert!(matches!(
                runtime
                    .session()
                    .client()
                    .command(&command.id)
                    .unwrap()
                    .status,
                CommandStatus::Completed(CommandResult::Accepted { revision: 5, .. })
            ));
            assert!(matches!(
                runtime
                    .receive_timeout(remaining(deadline))
                    .unwrap()
                    .unwrap_or_else(|| panic!("timed out waiting for durable event")),
                SessionEvent::Event {
                    intake: Intake::EventApplied,
                    ..
                }
            ));
            assert_eq!(
                runtime
                    .session()
                    .client()
                    .last_applied_cursor()
                    .unwrap()
                    .revision,
                5
            );
            assert_eq!(
                runtime
                    .session()
                    .client()
                    .model()
                    .state()
                    .unwrap()
                    .switcher()
                    .desired
                    .program,
                model_input(2)
            );
            assert!(matches!(
                runtime
                    .receive_timeout(remaining(deadline))
                    .unwrap()
                    .unwrap_or_else(|| panic!("timed out waiting for runtime event")),
                SessionEvent::RuntimeEvent {
                    intake: Intake::RuntimeEventObserved,
                    ..
                }
            ));
            assert!(runtime.session().is_connected());
            assert!(
                Instant::now() <= deadline,
                "WebSocket scenario exceeded deadline"
            );
        }))
        .map_err(|_| "WebSocket client failed".to_owned())
    })();
    let server = server_thread
        .join()
        .map_err(|_| "WebSocket server failed".to_owned());
    assert!(
        client.is_ok() && server.is_ok(),
        "WebSocket scenario failed: client={client:?}; server={server:?}"
    );
}

#[test]
fn diagnose_reports_validated_heartbeat_and_control_diagnostics() {
    let (address, server_thread) = spawn_server(|listener| {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut peer = Peer::accept_until(&listener, deadline);
        let heartbeat = receive_diagnose_heartbeat(&mut peer, deadline);
        peer.send_until(&WireMessage::Event(event(5)), deadline);
        peer.send_until(
            &WireMessage::HeartbeatAcknowledgement(HeartbeatAcknowledgementMessage {
                server: heartbeat.server,
                heartbeat_sequence: heartbeat.sequence,
                received_at_ms: 1_234,
            }),
            deadline,
        );
        let WireMessage::DiagnosticsRequest(request) = peer.receive_until(deadline) else {
            panic!("expected diagnostics request");
        };
        assert!(!request.request_id.is_empty());
        assert!(request.request_id.len() <= 128);
        assert!(
            request
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
        );
        peer.send_until(
            &WireMessage::DiagnosticsResponse(DiagnosticsResponse {
                protocol: CURRENT_PROTOCOL_VERSION,
                request_id: request.request_id,
                engine: engine(),
                current_revision: 5,
                oldest_retained_revision: Some(4),
                newest_retained_revision: Some(5),
                subscriber_count: 1,
                retained_events_limit: 64,
                subscriber_limit: 8,
                subscriber_queue_limit: 16,
            }),
            deadline,
        );
    });

    let output = run_diagnose(address, None);
    let server = server_thread.join();
    assert!(server.is_ok(), "Studio diagnostic server failed");
    assert_diagnose_success(output);
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "freemix-studio-{}-{sequence}-{name}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn helper(directory: &TestDirectory, behavior: &str) -> PathBuf {
    let path = directory.path("freemixd-test-helper");
    let body = match behavior {
        "stable" => format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$2.args\"\nprintf '%s\\n' \"$$\" > \"$2.pid\"\nprintf 'FREEMIXD_READY\\tv=1\\taddress=127.0.0.1:32123\\tproject_id={PROJECT_VALUE}\\n'\nIFS= read -r hold\n"
        ),
        "crash" => format!(
            "#!/bin/sh\nprintf 'FREEMIXD_READY\\tv=1\\taddress=127.0.0.1:32123\\tproject_id={PROJECT_VALUE}\\n'\nsleep 0.1\nexit 7\n"
        ),
        "identity-change" => format!(
            "#!/bin/sh\nif test -e \"$2.count\"; then id=42; else id={PROJECT_VALUE}; : > \"$2.count\"; fi\nprintf 'FREEMIXD_READY\\tv=1\\taddress=127.0.0.1:32123\\tproject_id=%s\\n' \"$id\"\nIFS= read -r hold\n"
        ),
        "exit-with-descendant" => {
            "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > \"$2.descendant.pid\"\nexit 7\n"
                .to_owned()
        }
        "ready-exit-with-descendant" => format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$2.leader.pid\"\nsleep 30 </dev/null >/dev/null 2>&1 &\nprintf '%s\\n' \"$!\" > \"$2.descendant.pid\"\nprintf 'FREEMIXD_READY\\tv=1\\taddress=127.0.0.1:32123\\tproject_id={PROJECT_VALUE}\\n'\nsleep 0.1\nexit 7\n"
        ),
        "oversized-readiness" => {
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$2.pid\"\nsleep 30 &\nprintf '%s\\n' \"$!\" > \"$2.descendant.pid\"\nhead -c 4097 /dev/zero | tr '\\000' x\nIFS= read -r hold\n".to_owned()
        }
        _ => unreachable!(),
    };
    fs::write(&path, body).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

#[cfg(unix)]
fn supervised(directory: &TestDirectory, executable: &Path) -> SupervisedConfig {
    SupervisedConfig {
        project_bundle: directory.path("show.freemix"),
        daemon_executable: executable.to_owned(),
        listen: "127.0.0.1:0".parse().unwrap(),
    }
}

#[cfg(unix)]
fn descendant_pid(directory: &TestDirectory) -> String {
    fs::read_to_string(
        directory
            .path("show.freemix")
            .with_extension("freemix.descendant.pid"),
    )
    .unwrap()
}

#[cfg(unix)]
fn process_exists(pid: &str) -> bool {
    ProcessCommand::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
fn assert_descendant_stopped(pid: &str) {
    assert!(
        !process_exists(pid),
        "supervised descendant survived cleanup"
    );
}

#[cfg(unix)]
fn wait_for_process_exit(pid: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let output = ProcessCommand::new("/bin/ps")
            .args(["-o", "state=", "-p", pid.trim()])
            .output()
            .unwrap();
        let state = String::from_utf8(output.stdout).unwrap();
        if !output.status.success() || state.trim_start().starts_with('Z') {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "supervised leader did not exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn supervisor_launches_exact_arguments_restarts_bounded_and_cleans_up_child() {
    let directory = TestDirectory::new("supervisor");
    let executable = helper(&directory, "stable");
    let config = supervised(&directory, &executable);
    let project = config.project_bundle.clone();
    let mut supervisor = DaemonSupervisor::launch(
        config,
        RestartPolicy {
            maximum_restarts: 1,
        },
    )
    .unwrap();
    assert_eq!(
        supervisor.readiness(),
        Some(ReadinessRecord {
            version: 1,
            address: "127.0.0.1:32123".parse().unwrap(),
            project_id: project_id(),
        })
    );
    assert_eq!(
        fs::read_to_string(project.with_extension("freemix.args")).unwrap(),
        format!("serve\n{}\n--listen\n127.0.0.1:0\n", project.display())
    );
    supervisor.restart().unwrap();
    assert_eq!(supervisor.restart_count(), 1);
    assert!(matches!(
        supervisor.restart(),
        Err(SupervisorError::RestartLimitReached {
            maximum_restarts: 1
        })
    ));
    assert_eq!(supervisor.state(), SupervisorState::RestartLimitReached);

    let cleanup = TestDirectory::new("cleanup");
    let executable = helper(&cleanup, "stable");
    let project = cleanup.path("show.freemix");
    let supervisor =
        DaemonSupervisor::launch(supervised(&cleanup, &executable), RestartPolicy::default())
            .unwrap();
    let pid = fs::read_to_string(project.with_extension("freemix.pid")).unwrap();
    drop(supervisor);
    assert!(
        !ProcessCommand::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
fn supervisor_observes_crash_and_rejects_identity_change_on_restart() {
    let crash = TestDirectory::new("crash");
    let executable = helper(&crash, "crash");
    let mut supervisor = DaemonSupervisor::launch(
        supervised(&crash, &executable),
        RestartPolicy {
            maximum_restarts: 1,
        },
    )
    .unwrap();
    assert_eq!(
        supervisor.wait_for_exit().unwrap(),
        SupervisorState::Exited { code: Some(7) }
    );
    supervisor.restart().unwrap();
    assert_eq!(
        supervisor.wait_for_exit().unwrap(),
        SupervisorState::Exited { code: Some(7) }
    );

    let changed = TestDirectory::new("changed");
    let executable = helper(&changed, "identity-change");
    let mut supervisor = DaemonSupervisor::launch(
        supervised(&changed, &executable),
        RestartPolicy {
            maximum_restarts: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        supervisor.restart(),
        Err(SupervisorError::ProjectIdentityChanged { expected, received })
            if expected == project_id() && received.to_string() == "42"
    ));
}

#[cfg(unix)]
#[test]
fn supervisor_cleans_descendants_before_joining_readiness_after_direct_exit() {
    let directory = TestDirectory::new("exit-descendant");
    let executable = helper(&directory, "exit-with-descendant");
    let project = directory.path("show.freemix");
    let descendant_pid_path = PathBuf::from(format!("{}.descendant.pid", project.display()));
    let started = std::time::Instant::now();

    assert!(matches!(
        DaemonSupervisor::launch(
            supervised(&directory, &executable),
            RestartPolicy::default()
        ),
        Err(SupervisorError::ExitedBeforeReady { status }) if status.code() == Some(7)
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
    let descendant_pid = fs::read_to_string(descendant_pid_path).unwrap();
    assert!(
        !ProcessCommand::new("/bin/kill")
            .args(["-0", descendant_pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "direct-exit descendant survived readiness cleanup"
    );
}

#[cfg(unix)]
#[test]
fn supervisor_rejects_oversized_readiness_and_reaps_its_group() {
    let directory = TestDirectory::new("oversized-readiness");
    let executable = helper(&directory, "oversized-readiness");
    let project = directory.path("show.freemix");
    let started = std::time::Instant::now();

    let error = DaemonSupervisor::launch(
        supervised(&directory, &executable),
        RestartPolicy::default(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to read freemixd readiness: freemixd readiness record exceeds 4096 bytes"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let helper_pid = fs::read_to_string(project.with_extension("freemix.pid")).unwrap();
    assert!(!process_exists(&helper_pid));
    assert_descendant_stopped(&descendant_pid(&directory));
}

#[cfg(unix)]
#[test]
fn supervisor_poll_cleans_descendants_after_ready_leader_exit() {
    let directory = TestDirectory::new("ready-exit-descendant-poll");
    let executable = helper(&directory, "ready-exit-with-descendant");
    let mut supervisor = DaemonSupervisor::launch(
        supervised(&directory, &executable),
        RestartPolicy::default(),
    )
    .unwrap();
    let pid = descendant_pid(&directory);
    let started = std::time::Instant::now();

    loop {
        match supervisor.poll().unwrap() {
            SupervisorState::Ready(_) => thread::sleep(Duration::from_millis(10)),
            SupervisorState::Exited { code: Some(7) } => break,
            state => panic!("unexpected supervisor state: {state:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(3));
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_descendant_stopped(&pid);
}

#[cfg(unix)]
#[test]
fn supervisor_wait_cleans_descendants_after_ready_leader_exit() {
    let directory = TestDirectory::new("ready-exit-descendant-wait");
    let executable = helper(&directory, "ready-exit-with-descendant");
    let mut supervisor = DaemonSupervisor::launch(
        supervised(&directory, &executable),
        RestartPolicy::default(),
    )
    .unwrap();
    let pid = descendant_pid(&directory);
    let started = std::time::Instant::now();

    assert_eq!(
        supervisor.wait_for_exit().unwrap(),
        SupervisorState::Exited { code: Some(7) }
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_descendant_stopped(&pid);
}

#[cfg(unix)]
#[test]
fn supervisor_drop_cleans_descendants_after_ready_leader_exit() {
    let directory = TestDirectory::new("ready-exit-descendant-drop");
    let executable = helper(&directory, "ready-exit-with-descendant");
    let supervisor = DaemonSupervisor::launch(
        supervised(&directory, &executable),
        RestartPolicy::default(),
    )
    .unwrap();
    let pid = descendant_pid(&directory);
    let leader_pid = fs::read_to_string(
        directory
            .path("show.freemix")
            .with_extension("freemix.leader.pid"),
    )
    .unwrap();
    wait_for_process_exit(&leader_pid);
    let started = std::time::Instant::now();

    drop(supervisor);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_descendant_stopped(&pid);
}

#[test]
fn arguments_preserve_supervised_and_existing_configuration_distinction() {
    let Command::Open(supervised) = parse_args(
        [
            "--project",
            "show.freemix",
            "--daemon",
            "/opt/freemixd",
            "--listen",
            "127.0.0.1:0",
        ]
        .map(str::to_owned),
    )
    .unwrap() else {
        panic!("expected open command");
    };
    assert!(matches!(
        supervised.connection,
        ConnectionConfig::Supervised(SupervisedConfig { project_bundle, daemon_executable, listen })
            if project_bundle == Path::new("show.freemix")
                && daemon_executable == Path::new("/opt/freemixd")
                && listen == "127.0.0.1:0".parse().unwrap()
    ));

    let Command::Open(existing) = parse_args(
        [
            "--connect",
            "127.0.0.1:9000",
            "--project-id",
            &PROJECT_VALUE.to_string(),
        ]
        .map(str::to_owned),
    )
    .unwrap() else {
        panic!("expected open command");
    };
    assert!(matches!(
        existing.connection,
        ConnectionConfig::Existing(ExistingConfig { address, expected_project_id })
            if address == "127.0.0.1:9000".parse().unwrap()
                && expected_project_id == project_id()
    ));
}

#[test]
fn diagnose_flag_selects_one_shot_mode_and_rejects_duplicates() {
    let arguments = [
        "--connect",
        "127.0.0.1:9000",
        "--project-id",
        &PROJECT_VALUE.to_string(),
        "--diagnose",
    ];
    assert!(matches!(
        parse_args(arguments.map(str::to_owned)).unwrap(),
        Command::Diagnose(_)
    ));
    let duplicate = ["--project", "show.freemix", "--diagnose", "--diagnose"];
    assert!(matches!(
        parse_args(duplicate.map(str::to_owned)),
        Err(freemix_studio::ArgsError::DuplicateOption("--diagnose"))
    ));

    let web = [
        "--web-connect",
        "127.0.0.1:9001",
        "--project-id",
        &PROJECT_VALUE.to_string(),
        "--diagnose",
    ];
    assert!(matches!(
        parse_args(web.map(str::to_owned)),
        Ok(Command::Diagnose(StudioConfig {
            connection: ConnectionConfig::Existing(ExistingConfig { address, expected_project_id }),
            ..
        })) if address == "127.0.0.1:9001".parse().unwrap() && expected_project_id == project_id()
    ));
    assert!(matches!(
        parse_args(
            [
                "--web-connect",
                "127.0.0.1:9001",
                "--project-id",
                &PROJECT_VALUE.to_string()
            ]
            .map(str::to_owned),
        ),
        Ok(Command::Open(StudioConfig {
            transport: ControlTransport::WebSocket,
            ..
        }))
    ));
    assert!(matches!(
        parse_args(
            [
                "--web-connect",
                "127.0.0.1:9001",
                "--connect",
                "127.0.0.1:9000",
                "--project-id",
                &PROJECT_VALUE.to_string(),
                "--diagnose"
            ]
            .map(str::to_owned)
        ),
        Err(freemix_studio::ArgsError::WebConnectConflicting)
    ));
    for address in ["0.0.0.0:9001", "127.0.0.1:0"] {
        assert!(matches!(
            parse_args(
                [
                    "--web-connect",
                    address,
                    "--project-id",
                    &PROJECT_VALUE.to_string(),
                    "--diagnose"
                ]
                .map(str::to_owned)
            ),
            Err(freemix_studio::ArgsError::InvalidWebConnect(_))
        ));
    }
}

#[test]
fn dependency_tree_contains_only_client_side_freemix_crates() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = ProcessCommand::new(cargo)
        .args([
            "tree",
            "--package",
            "freemix-studio",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree should execute");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let allowed = [
        "fm-client",
        "fm-command",
        "fm-protocol",
        "fm-types",
        "fm-ui-egui",
        "fm-ui-model",
        "freemix-studio",
    ];
    let tree = String::from_utf8(output.stdout).expect("cargo tree output should be UTF-8");
    let packages = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<Vec<_>>();
    for package in packages
        .iter()
        .copied()
        .filter(|package| package.starts_with("fm-") || package.starts_with("freemix-"))
    {
        assert!(
            allowed.contains(&package),
            "non-client dependency linked: {package}"
        );
    }
    let forbidden = [
        "fm-engine",
        "fm-control",
        "fm-persistence",
        "fm-gpu",
        "fm-color",
        "fm-compositor",
        "fm-scopes",
        "fm-clock",
        "fm-frame",
        "fm-graph",
        "fm-video",
        "fm-audio",
        "fm-playback",
        "fm-record",
        "fm-replay",
        "fm-scheduler",
        "fm-sim",
    ];
    assert!(
        forbidden
            .iter()
            .all(|forbidden| !packages.contains(forbidden)),
        "Studio linked an engine, control, persistence, GPU, or media crate"
    );
}
