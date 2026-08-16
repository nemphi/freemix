use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use fm_client::{
    Client as ProtocolClient, ClientConfig, ClientError, ConnectionState, DisconnectCause, Intake,
    Outbound, SessionEvent, TcpSession,
};
use fm_model::{
    AudioBus, Input, InputAudioStripState, InputGainMilliDb, InputKind, Layer, LayerGeometry,
    MainMix, Output, Project, ProjectSettings, RectMask, RestartPolicy, Rgba8, Rotation, Scene,
    SimulatedAudio, SimulatedInput, SimulatedVideo, SolidColor, SourceRef, StartupPolicy,
};
use fm_persistence::{
    ManualTransitionKind as PersistedManualTransitionKind, ProjectPosition, ProjectStore,
    RuntimeRouting, StoredProject,
};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientType, CommandMessage, CommandPayload, CommandResult,
    DiagnosticsRequest, DiagnosticsResponse, EngineIdentity, EventCursor, EventPayload,
    HandshakeOutcome, HandshakeRequest, HeartbeatMessage, ManualTransitionKind,
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
        Self::start_with_once(project, true)
    }

    fn start_without_once(project: &Path) -> Self {
        Self::start_with_once(project, false)
    }

    fn start_web(project: &Path, token: &str) -> (Self, SocketAddr) {
        let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
            .arg("serve")
            .arg(project)
            .args(["--listen", "127.0.0.1:0", "--web-listen", "127.0.0.1:0"])
            .env("FREEMIXD_WEB_TOKEN", token)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = read_startup_lines(&mut child, 2);
        let line = lines.remove(0);
        let web_line = lines.remove(0);
        let readiness = line.parse::<ReadinessRecord>().unwrap_or_else(|error| {
            startup_failure(
                &mut child,
                vec![line.clone(), web_line.clone()],
                error.to_string(),
                None,
            )
        });
        let web_address = web_line
            .strip_prefix("FREEMIXD_WEB_READY\tv=1\taddress=")
            .and_then(|line| line.strip_suffix('\n'))
            .and_then(|address| address.parse().ok())
            .unwrap_or_else(|| {
                startup_failure(
                    &mut child,
                    vec![line, web_line],
                    "invalid web readiness".into(),
                    None,
                )
            });
        (
            Self {
                child: Some(child),
                address: readiness.address,
                project_id: readiness.project_id,
            },
            web_address,
        )
    }

    fn start_with_once(project: &Path, once: bool) -> Self {
        let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"));
        command.arg("serve").arg(project);
        if once {
            command.arg("--once");
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = read_startup_lines(&mut child, 1);
        let readiness = match lines[0].parse::<ReadinessRecord>() {
            Ok(readiness) => readiness,
            Err(error) => startup_failure(&mut child, lines, error.to_string(), None),
        };
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
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => terminate_after_wait_failure(
                    &mut child,
                    "daemon did not exit within two seconds",
                ),
                Err(error) => {
                    terminate_after_wait_failure(
                        &mut child,
                        &format!("could not inspect daemon status: {error}"),
                    );
                }
            }
        };
        if !status.success() {
            let stderr = child_stderr(&mut child);
            panic!("daemon exited with {status}: {stderr}");
        }
    }

    fn stop(mut self) {
        let stopped = terminate_child(self.child.as_mut().unwrap());
        assert!(stopped, "daemon did not stop within one second");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = terminate_child(child);
        }
    }
}

fn terminate_child(child: &mut Child) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

fn read_startup_lines(child: &mut Child, count: usize) -> Vec<String> {
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::sync_channel::<std::io::Result<Option<String>>>(count);
    let reader = std::thread::spawn(move || {
        let mut output = BufReader::new(stdout);
        for _ in 0..count {
            let mut line = String::new();
            let result = output
                .read_line(&mut line)
                .map(|read| (read != 0).then_some(line));
            if sender.send(result).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut lines = Vec::with_capacity(count);
    while lines.len() < count {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = match receiver.recv_timeout(remaining) {
            Ok(result) => result,
            Err(error) => startup_failure(child, lines, error.to_string(), Some(reader)),
        };
        match result {
            Ok(Some(line)) => lines.push(line),
            Ok(None) => {
                startup_failure(
                    child,
                    lines,
                    "daemon stdout closed before readiness".into(),
                    Some(reader),
                );
            }
            Err(error) => {
                startup_failure(child, lines, error.to_string(), Some(reader));
            }
        }
    }
    if reader.join().is_err() {
        startup_failure(child, lines, "startup stdout reader panicked".into(), None);
    }
    lines
}

fn startup_failure(
    child: &mut Child,
    stdout: Vec<String>,
    failure: String,
    reader: Option<std::thread::JoinHandle<()>>,
) -> ! {
    let stopped = terminate_child(child);
    if stopped && let Some(reader) = reader {
        let _ = reader.join();
    }
    let stderr = stopped.then(|| child_stderr(child));
    panic!(
        "daemon startup failed: {failure}; stdout={stdout:?}; cleanup_stopped={stopped}; stderr={stderr:?}"
    );
}

fn terminate_after_wait_failure(child: &mut Child, failure: &str) -> ! {
    let stopped = terminate_child(child);
    let stderr = stopped.then(|| child_stderr(child));
    panic!("{failure}; cleanup_stopped={stopped}; stderr={stderr:?}");
}

fn child_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    stderr
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
        self.handshake_as(Role::Operator, cursor)
    }

    fn handshake_as(&mut self, role: Role, cursor: Option<EventCursor>) -> TestHandshake {
        self.handshake_version_as(CURRENT_PROTOCOL_VERSION, role, cursor)
    }

    fn handshake_version(
        &mut self,
        version: ProtocolVersion,
        cursor: Option<EventCursor>,
    ) -> TestHandshake {
        self.handshake_version_as(version, Role::Operator, cursor)
    }

    fn handshake_version_as(
        &mut self,
        version: ProtocolVersion,
        role: Role,
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
            desired_role: role,
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

fn connect_current_client(daemon: &Daemon, client: &mut ProtocolClient) -> Client {
    let mut transport = daemon.connect();
    transport
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    transport.send(&WireMessage::HandshakeRequest(
        client.transport_connected().unwrap(),
    ));
    assert_eq!(
        client.intake(transport.receive()).unwrap(),
        Intake::Handshake
    );
    assert_eq!(
        client.intake(transport.receive()).unwrap(),
        Intake::SnapshotApplied
    );
    assert_eq!(client.state(), &ConnectionState::Ready);
    transport
}

fn assert_heartbeat(client: &mut ProtocolClient, transport: &mut Client, sent_at_ms: u64) {
    let cursor = client.last_applied_cursor();
    let heartbeat = client.queue_heartbeat(sent_at_ms).unwrap();
    assert_eq!(heartbeat.last_applied, cursor);
    assert_eq!(
        client.pop_outbound(),
        Some(Outbound::Heartbeat(heartbeat.clone()))
    );
    transport.send(&WireMessage::Heartbeat(heartbeat.clone()));
    let WireMessage::HeartbeatAcknowledgement(acknowledgement) = transport.receive() else {
        panic!("expected heartbeat acknowledgement");
    };
    assert_eq!(acknowledgement.server, heartbeat.server);
    assert_eq!(acknowledgement.heartbeat_sequence, heartbeat.sequence);
    assert_eq!(
        client
            .intake(WireMessage::HeartbeatAcknowledgement(acknowledgement))
            .unwrap(),
        Intake::HeartbeatAcknowledged
    );
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
fn daemon_acknowledges_only_valid_heartbeats() {
    let directory = TestDirectory::new("heartbeat-acknowledgement");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    let handshake = client.handshake(None);
    assert_eq!(handshake.protocol, CURRENT_PROTOCOL_VERSION);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
    let server = ServerIdentity {
        engine_id: handshake.engine.engine_id,
        project_id: PROJECT_ID.to_string(),
        state_epoch: handshake.engine.state_epoch,
        log_id: handshake.engine.log_id,
    };
    let heartbeat = |sequence| HeartbeatMessage {
        server: server.clone(),
        sequence,
        sent_at_ms: 1_234,
        last_applied: Some(ResumeCursor {
            server: server.clone(),
            revision: handshake.current_revision,
        }),
    };

    client.send(&WireMessage::Heartbeat(heartbeat(1)));
    let WireMessage::HeartbeatAcknowledgement(acknowledgement) = client.receive() else {
        panic!("expected heartbeat acknowledgement");
    };
    assert_eq!(acknowledgement.server, server);
    assert_eq!(acknowledgement.heartbeat_sequence, 1);
    assert!(acknowledgement.received_at_ms > 0);

    let mut invalid = heartbeat(2);
    invalid.server.engine_id = "wrong-engine".to_owned();
    invalid.last_applied.as_mut().unwrap().server.engine_id = "wrong-engine".to_owned();
    client.send(&WireMessage::Heartbeat(invalid));
    let WireMessage::Error(error) = client.receive() else {
        panic!("expected invalid heartbeat error");
    };
    assert_eq!(error.error.code, "invalid_heartbeat");

    client.send(&WireMessage::Heartbeat(heartbeat(3)));
    let WireMessage::HeartbeatAcknowledgement(acknowledgement) = client.receive() else {
        panic!("invalid heartbeat produced an acknowledgement");
    };
    assert_eq!(acknowledgement.heartbeat_sequence, 3);

    drop(client);
    daemon.wait_success();
}

#[test]
fn once_daemon_replaces_disconnected_peer_but_admits_one_live_peer() {
    let directory = TestDirectory::new("once-single-live-peer");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start(&project_path);
    drop(TcpStream::connect(daemon.address).unwrap());

    let mut first = daemon.connect();
    first
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut second = daemon.connect();
    second
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    second.send(&WireMessage::HandshakeRequest(HandshakeRequest {
        protocol: CURRENT_PROTOCOL_VERSION,
        build: "process-test".into(),
        client_type: ClientType::Integration,
        desired_role: Role::Operator,
        resume_cursor: None,
    }));

    first.handshake(None);
    assert!(matches!(first.receive(), WireMessage::Snapshot(_)));
    drop(first);

    daemon.wait_success();

    let mut line = String::new();
    match second.reader.read_line(&mut line) {
        Ok(0) => {}
        Ok(_) => panic!("second live peer received a daemon response: {line}"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ) => {}
        Err(error) => panic!("could not inspect second live peer: {error}"),
    }
}

#[test]
fn malformed_post_handshake_record_does_not_stop_daemon() {
    let directory = TestDirectory::new("malformed-post-handshake-record");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start_without_once(&project_path);
    let mut malformed_client = daemon.connect();
    malformed_client
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    malformed_client.handshake(None);
    assert!(matches!(
        malformed_client.receive(),
        WireMessage::Snapshot(_)
    ));
    malformed_client
        .writer
        .write_all(b"malformed-record\n")
        .unwrap();
    malformed_client.writer.flush().unwrap();

    let mut closed = String::new();
    assert_eq!(malformed_client.reader.read_line(&mut closed).unwrap(), 0);

    let mut next_client = daemon.connect();
    next_client
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    next_client.handshake(None);
    assert!(matches!(next_client.receive(), WireMessage::Snapshot(_)));

    drop(next_client);
    drop(malformed_client);
    daemon.stop();
}

#[test]
fn idle_viewer_receives_operator_events_without_blocking() {
    let directory = TestDirectory::new("idle-viewer-live-events");
    let project_path = directory.project_path();
    create_project(&project_path);

    let viewer_id = "idle-viewer";
    let operator_id = "show-operator";
    assert_ne!(viewer_id, operator_id);
    let mut viewer_client = ProtocolClient::new(ClientConfig::new(
        "process-test",
        ClientType::Integration,
        Role::Viewer,
        viewer_id,
        project_id(),
    ))
    .unwrap();
    let mut operator_client = ProtocolClient::new(ClientConfig::new(
        "process-test",
        ClientType::Integration,
        Role::Operator,
        operator_id,
        project_id(),
    ))
    .unwrap();
    viewer_client.start_connect().unwrap();

    let daemon = Daemon::start_without_once(&project_path);
    let mut viewer = connect_current_client(&daemon, &mut viewer_client);
    assert_eq!(viewer_client.session().unwrap().granted_role, Role::Viewer);

    operator_client.start_connect().unwrap();
    let mut operator = connect_current_client(&daemon, &mut operator_client);
    assert_eq!(
        operator_client.session().unwrap().granted_role,
        Role::Operator
    );

    let command = operator_client
        .queue_command(CommandPayload::Cut, "two-peer-cut", Some(0), None)
        .unwrap();
    assert_eq!(command.id, "show-operator:1");
    assert_eq!(
        operator_client.pop_outbound(),
        Some(Outbound::Command(command.clone()))
    );
    operator.send(&WireMessage::Command(command.clone()));

    let result = operator.receive();
    let WireMessage::CommandResult(CommandResult::Accepted { id, revision, .. }) = &result else {
        panic!("expected accepted command result");
    };
    assert_eq!(id, &command.id);
    assert_eq!(*revision, 1);
    assert_eq!(
        operator_client.intake(result).unwrap(),
        Intake::ResultReconciled
    );

    let operator_event = operator.receive();
    let WireMessage::Event(event) = &operator_event else {
        panic!("expected durable event after command result");
    };
    assert_eq!(event.cursor.revision, 1);
    assert!(matches!(
        &event.payload,
        fm_protocol::EventPayload::DesiredSwitcher {
            program,
            preview,
            ..
        } if *program == input(2) && *preview == input(1)
    ));
    assert_eq!(
        operator_client.intake(operator_event.clone()).unwrap(),
        Intake::EventApplied
    );
    let operator_runtime = operator.receive();
    let WireMessage::RuntimeEvent(runtime) = &operator_runtime else {
        panic!("expected runtime event after durable event");
    };
    assert_eq!(runtime.revision, 1);
    assert!(matches!(
        &runtime.event,
        RuntimeLifecycleEvent::Realized { domain, .. } if domain == "switcher"
    ));
    assert_eq!(
        operator_client.intake(operator_runtime.clone()).unwrap(),
        Intake::RuntimeEventObserved
    );

    let viewer_event = viewer.receive();
    assert_eq!(viewer_event, operator_event);
    assert_eq!(
        viewer_client.intake(viewer_event).unwrap(),
        Intake::EventApplied
    );
    let viewer_runtime = viewer.receive();
    assert_eq!(viewer_runtime, operator_runtime);
    assert_eq!(
        viewer_client.intake(viewer_runtime).unwrap(),
        Intake::RuntimeEventObserved
    );

    assert_heartbeat(&mut viewer_client, &mut viewer, 1_234);
    assert_heartbeat(&mut operator_client, &mut operator, 1_235);

    drop(operator);
    drop(viewer);
    daemon.stop();
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
    let acknowledgement = transport.receive();
    assert!(matches!(
        acknowledgement,
        WireMessage::HeartbeatAcknowledgement(_)
    ));
    assert_eq!(
        protocol_client.intake(acknowledgement).unwrap(),
        Intake::HeartbeatAcknowledged
    );
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
fn remote_input_rename_is_authorized_replicated_replay_safe_and_survives_restart() {
    let directory = TestDirectory::new("remote-input-rename");
    let project_path = directory.project_path();
    create_rename_project(&project_path);

    let daemon = Daemon::start_without_once(&project_path);
    let mut unauthorized = daemon.connect();
    unauthorized.handshake_as(Role::Operator, None);
    assert!(matches!(unauthorized.receive(), WireMessage::Snapshot(_)));
    unauthorized.send(&command(
        "reorder-denied",
        "reorder-denied-key",
        CommandPayload::ReorderInputs {
            inputs: vec![input(2), input(1)],
        },
    ));
    assert!(matches!(
        unauthorized.next_result(),
        CommandResult::Rejected {
            code,
            current_revision: 0,
            ..
        } if code == "permission_denied"
    ));
    drop(unauthorized);

    let mut graphics = daemon.connect();
    graphics.handshake_as(Role::Graphics, None);
    assert!(matches!(graphics.receive(), WireMessage::Snapshot(_)));
    graphics.send(&command(
        "rename-accepted",
        "rename-accepted-key",
        CommandPayload::RenameInput {
            input: input(1),
            name: "Camera Left".into(),
        },
    ));
    let accepted = graphics.receive();
    let WireMessage::CommandResult(CommandResult::Accepted {
        id,
        revision,
        scheduled_frame,
    }) = &accepted
    else {
        panic!("unexpected rename result: {accepted:?}");
    };
    assert_eq!(
        (id.as_str(), *revision, *scheduled_frame),
        ("rename-accepted", 1, Some(0))
    );
    assert!(matches!(
        graphics.receive(),
        WireMessage::Event(event)
            if event.cursor.revision == 1
                && matches!(
                    event.payload,
                    fm_protocol::EventPayload::InputRenamed { input: renamed, ref name }
                        if renamed == input(1) && name == "Camera Left"
                )
    ));

    assert!(matches!(
        graphics.receive(),
        WireMessage::RuntimeEvent(runtime)
            if runtime.revision == 1
                && runtime.generation == 1
                && matches!(
                    runtime.event,
                    RuntimeLifecycleEvent::Realized { ref domain, .. }
                        if domain == "project"
                )
    ));

    graphics.send(&command(
        "reorder-accepted",
        "reorder-accepted-key",
        CommandPayload::ReorderInputs {
            inputs: vec![input(2), input(1)],
        },
    ));
    assert!(matches!(
        graphics.next_result(),
        CommandResult::Accepted {
            id,
            revision: 2,
            scheduled_frame: Some(1),
        } if id == "reorder-accepted"
    ));
    assert!(matches!(
        graphics.receive(),
        WireMessage::Event(event)
            if event.cursor.revision == 2
                && matches!(
                    event.payload,
                    fm_protocol::EventPayload::InputOrderChanged { ref inputs }
                        if inputs == &vec![input(2), input(1)]
                )
    ));
    graphics.send(&command(
        "rename-replay",
        "rename-accepted-key",
        CommandPayload::RenameInput {
            input: input(1),
            name: "Replay must not apply".into(),
        },
    ));
    assert!(matches!(
        graphics.next_result(),
        CommandResult::Accepted {
            id,
            revision: 1,
            ..
        } if id == "rename-accepted"
    ));
    graphics.send(&command(
        "reorder-replay",
        "reorder-accepted-key",
        CommandPayload::ReorderInputs {
            inputs: vec![input(1), input(2)],
        },
    ));
    assert!(matches!(
        graphics.next_result(),
        CommandResult::Accepted {
            id,
            revision: 2,
            ..
        } if id == "reorder-accepted"
    ));
    drop(graphics);

    let mut snapshot_client = daemon.connect();
    let handshake = snapshot_client.handshake_as(Role::Graphics, None);
    assert_eq!(handshake.current_revision, 2);
    let WireMessage::Snapshot(snapshot) = snapshot_client.receive() else {
        panic!("expected snapshot after input rename");
    };
    assert_eq!(
        snapshot
            .inputs
            .iter()
            .map(|status| (status.input, status.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(input(2), "Input 2"), (input(1), "Camera Left")]
    );
    drop(snapshot_client);
    daemon.stop();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 2);
    assert_eq!(persisted.position().frames_rendered, 2);
    assert_eq!(persisted.idempotency_receipts().len(), 2);
    assert_eq!(
        persisted
            .project()
            .inputs()
            .iter()
            .map(|input| (input.id, input.name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (domain_input(2), "Input 2"),
            (domain_input(1), "Camera Left")
        ]
    );
    assert_eq!(
        persisted
            .project()
            .input_audio_strips()
            .iter()
            .find(|strip| strip.input == domain_input(1))
            .unwrap()
            .state
            .gain
            .get(),
        -1_000
    );
    assert_eq!(
        persisted
            .project()
            .input_audio_strips()
            .iter()
            .find(|strip| strip.input == domain_input(2))
            .unwrap()
            .state
            .gain
            .get(),
        -2_000
    );

    let daemon = Daemon::start(&project_path);
    let mut restarted = daemon.connect();
    let handshake = restarted.handshake_as(Role::Graphics, None);
    assert_eq!(handshake.current_revision, 2);
    let WireMessage::Snapshot(snapshot) = restarted.receive() else {
        panic!("expected snapshot after restart");
    };
    assert_eq!(
        snapshot
            .inputs
            .iter()
            .map(|status| (status.input, status.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(input(2), "Input 2"), (input(1), "Camera Left")]
    );
    drop(restarted);
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
fn manual_slide_state_survives_restart_through_commit_and_cancel() {
    for (name, terminal, swaps_routes) in [
        (
            "manual-slide-commit-restart",
            CommandPayload::CommitManualTransition,
            true,
        ),
        (
            "manual-slide-cancel-restart",
            CommandPayload::CancelManualTransition,
            false,
        ),
    ] {
        let directory = TestDirectory::new(name);
        let project_path = directory.project_path();
        create_project(&project_path);

        let daemon = Daemon::start(&project_path);
        let mut client = daemon.connect();
        client.handshake_version(CURRENT_PROTOCOL_VERSION, None);
        assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
        for (id, key, payload, revision) in [
            (
                "manual-slide-start",
                "manual-slide-start-key",
                CommandPayload::StartManualTransition {
                    kind: ManualTransitionKind::Slide,
                },
                1,
            ),
            (
                "manual-slide-position",
                "manual-slide-position-key",
                CommandPayload::SetManualTransitionPosition {
                    position: ManualTransitionPosition::new(6_250).unwrap(),
                },
                2,
            ),
        ] {
            client.send(&command_version(CURRENT_PROTOCOL_VERSION, id, key, payload));
            assert!(matches!(
                client.next_result(),
                CommandResult::Accepted {
                    revision: accepted,
                    ..
                } if accepted == revision
            ));
        }
        drop(client);
        daemon.wait_success();

        let store = ProjectStore::new(&project_path).unwrap();
        let checkpoint = store.load().unwrap();
        let manual = checkpoint.runtime_manual_transitions();
        assert!(matches!(
            (manual.desired, manual.realized),
            (Some(desired), Some(realized))
                if desired.kind == PersistedManualTransitionKind::Slide
                    && desired.interval_start_basis_points == 0
                    && desired.position_basis_points == 6_250
                    && realized.kind == PersistedManualTransitionKind::Slide
                    && realized.interval_start_basis_points == 6_250
                    && realized.position_basis_points == 6_250
        ));

        let daemon = Daemon::start(&project_path);
        let mut client = daemon.connect();
        let hello = client.handshake_version(CURRENT_PROTOCOL_VERSION, None);
        assert_eq!(hello.current_revision, 2);
        let WireMessage::Snapshot(snapshot) = client.receive() else {
            panic!("restart must return a snapshot");
        };
        assert!(matches!(
            (
                snapshot.desired_manual_transition,
                snapshot.realized_manual_transition,
            ),
            (
                ManualTransitionStatus::Active(desired),
                ManualTransitionStatus::Active(realized),
            ) if desired.kind == ManualTransitionKind::Slide
                && desired.interval_start == ManualTransitionPosition::START
                && desired.position.basis_points() == 6_250
                && realized.kind == ManualTransitionKind::Slide
                && realized.interval_start.basis_points() == 6_250
                && realized.position.basis_points() == 6_250
        ));
        client.send(&command_version(
            CURRENT_PROTOCOL_VERSION,
            "manual-slide-terminal",
            "manual-slide-terminal-key",
            terminal,
        ));
        assert!(matches!(
            client.next_result(),
            CommandResult::Accepted { revision: 3, .. }
        ));
        drop(client);
        daemon.wait_success();

        let final_state = store.load().unwrap();
        assert_eq!(
            final_state.runtime_manual_transitions(),
            fm_persistence::RuntimeManualTransitions::default()
        );
        let routing = final_state.runtime_routing();
        let expected = if swaps_routes {
            (domain_input(2), domain_input(1))
        } else {
            (domain_input(1), domain_input(2))
        };
        assert_eq!(routing.desired_program_id, Some(expected.0));
        assert_eq!(routing.realized_program_id, Some(expected.0));
        assert_eq!(routing.desired_preview_id, Some(expected.1));
        assert_eq!(routing.realized_preview_id, Some(expected.1));
    }
}

#[test]
fn non_current_handshake_returns_protocol_mismatch() {
    let directory = TestDirectory::new("protocol-mismatch");
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
            if matches!(&response.outcome, HandshakeOutcome::Rejected { error } if error.code == "protocol_mismatch")
    ));
    drop(client);
    daemon.wait_success();
}

#[test]
fn viewer_diagnostics_query_is_read_only_and_correlated() {
    let directory = TestDirectory::new("viewer-diagnostics");
    let project_path = directory.project_path();
    create_project(&project_path);
    let daemon = Daemon::start_without_once(&project_path);
    let mut client = daemon.connect();
    let handshake = client.handshake_as(Role::Viewer, None);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
    client.send(&WireMessage::DiagnosticsRequest(DiagnosticsRequest {
        protocol: CURRENT_PROTOCOL_VERSION,
        request_id: "diagnostics-1".into(),
    }));
    let response = client.receive();
    let WireMessage::DiagnosticsResponse(DiagnosticsResponse {
        protocol,
        request_id,
        engine,
        current_revision,
        oldest_retained_revision,
        newest_retained_revision,
        subscriber_count,
        retained_events_limit,
        subscriber_limit,
        subscriber_queue_limit,
    }) = response
    else {
        panic!("expected diagnostics response");
    };
    assert_eq!(protocol, CURRENT_PROTOCOL_VERSION);
    assert_eq!(request_id, "diagnostics-1");
    assert_eq!(engine, handshake.engine);
    assert_eq!(current_revision, handshake.current_revision);
    assert_eq!(oldest_retained_revision, None);
    assert_eq!(newest_retained_revision, None);
    assert_eq!(subscriber_count, 1);
    assert!(retained_events_limit > 0);
    assert!(subscriber_limit > 0);
    assert!(subscriber_queue_limit > 0);
    drop(client);
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, handshake.current_revision);
    assert!(persisted.idempotency_receipts().is_empty());
    daemon.stop();
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

#[cfg(unix)]
#[test]
fn sigterm_notifies_established_client_then_exits_cleanly() {
    let directory = TestDirectory::new("sigterm-notifies-client");
    let project_path = directory.project_path();
    create_project(&project_path);

    let daemon = Daemon::start_without_once(&project_path);
    let mut session = TcpSession::new(
        ProtocolClient::new(ClientConfig::new(
            "process-test",
            ClientType::Integration,
            Role::Operator,
            "process-client",
            project_id(),
        ))
        .unwrap(),
    );
    assert!(matches!(
        session
            .connect(daemon.address, Duration::from_secs(1))
            .unwrap(),
        SessionEvent::Connected { .. }
    ));
    let status = ProcessCommand::new("/bin/kill")
        .args(["-TERM", &daemon.child.as_ref().unwrap().id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "SIGTERM command failed: {status}");
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(matches!(
        session
            .receive_cancellable(Duration::from_millis(25), || Instant::now() >= deadline)
            .unwrap(),
        SessionEvent::Disconnected {
            cause: DisconnectCause::ServerShutdown,
            backoff,
        } if backoff.attempt == 1
    ));
    daemon.wait_success();
}

#[cfg(unix)]
#[test]
fn websocket_control_is_authenticated_ordered_and_raw_compatible() {
    let directory = TestDirectory::new("websocket-control");
    let project_path = directory.project_path();
    create_project(&project_path);
    let configured_token = "configured-token-0123456789abcdef";
    let presented_token = "presented-token-0123456789abcdef";
    let (daemon, web_address) = Daemon::start_web(&project_path, configured_token);
    let timeout = Duration::from_secs(1);
    let request = |token: &str| {
        tungstenite::http::Request::builder()
            .uri(format!("ws://{web_address}/v1/control"))
            .header("Host", web_address.to_string())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .header("Authorization", format!("Bearer {token}"))
            .body(())
            .unwrap()
    };
    let configure = |stream: &TcpStream| {
        stream.set_read_timeout(Some(timeout)).unwrap();
        stream.set_write_timeout(Some(timeout)).unwrap();
    };

    let unauthorized_stream = TcpStream::connect_timeout(&web_address, timeout).unwrap();
    configure(&unauthorized_stream);
    let unauthorized_request = request(presented_token);
    match tungstenite::client::client(unauthorized_request, unauthorized_stream) {
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            assert_eq!(
                response.status(),
                tungstenite::http::StatusCode::UNAUTHORIZED
            );
            let response = format!("{response:?}");
            assert!(!response.contains(configured_token));
            assert!(!response.contains(presented_token));
        }
        result => panic!("unauthorized WebSocket upgrade result: {result:?}"),
    }

    let websocket_stream = TcpStream::connect_timeout(&web_address, timeout).unwrap();
    configure(&websocket_stream);
    let websocket_request = request(configured_token);
    let (mut websocket, _) =
        tungstenite::client::client(websocket_request, websocket_stream).unwrap();
    let receive = |websocket: &mut tungstenite::WebSocket<TcpStream>| -> WireMessage {
        match websocket.read().unwrap() {
            tungstenite::Message::Text(text) => {
                assert!(text.as_str().ends_with('\n'));
                decode_line(text.as_str()).unwrap()
            }
            message => panic!("expected WebSocket text frame, got {message:?}"),
        }
    };
    let send = |websocket: &mut tungstenite::WebSocket<TcpStream>, message: &WireMessage| {
        websocket
            .send(tungstenite::Message::text(encode_line(message).unwrap()))
            .unwrap();
    };
    send(
        &mut websocket,
        &WireMessage::HandshakeRequest(HandshakeRequest {
            protocol: CURRENT_PROTOCOL_VERSION,
            build: "process-test".into(),
            client_type: ClientType::Integration,
            desired_role: Role::Operator,
            resume_cursor: None,
        }),
    );
    assert!(matches!(
        receive(&mut websocket),
        WireMessage::HandshakeResponse(response)
            if response.protocol == CURRENT_PROTOCOL_VERSION
                && response.granted_role == Role::Operator
    ));
    assert!(matches!(receive(&mut websocket), WireMessage::Snapshot(_)));

    let mut raw_session = TcpSession::new(
        ProtocolClient::new(ClientConfig::new(
            "process-test",
            ClientType::Integration,
            Role::Operator,
            "raw-process-client",
            project_id(),
        ))
        .unwrap(),
    );
    assert!(matches!(
        raw_session.connect(daemon.address, timeout).unwrap(),
        SessionEvent::Connected { .. }
    ));
    assert_eq!(raw_session.client().state(), &ConnectionState::Ready);

    send(
        &mut websocket,
        &command_version(
            CURRENT_PROTOCOL_VERSION,
            "web-cut",
            "web-cut-key",
            CommandPayload::Cut,
        ),
    );
    assert!(matches!(
        receive(&mut websocket),
        WireMessage::CommandResult(CommandResult::Accepted { id, revision: 1, .. })
            if id == "web-cut"
    ));
    let WireMessage::Event(event) = receive(&mut websocket) else {
        panic!("expected durable switcher event");
    };
    assert_eq!(event.cursor.revision, 1);
    assert!(matches!(
        event.payload,
        EventPayload::DesiredSwitcher { program, preview, .. }
            if program == input(2) && preview == input(1)
    ));
    let WireMessage::RuntimeEvent(runtime) = receive(&mut websocket) else {
        panic!("expected switcher runtime event");
    };
    assert_eq!(runtime.revision, 1);
    assert_eq!(runtime.generation, 1);
    assert_eq!(runtime.sequence, 1);
    assert!(matches!(
        runtime.event,
        RuntimeLifecycleEvent::Realized { domain, .. } if domain == "switcher"
    ));

    let status = ProcessCommand::new("/bin/kill")
        .args(["-TERM", &daemon.child.as_ref().unwrap().id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "SIGTERM command failed: {status}");
    assert!(matches!(
        receive(&mut websocket),
        WireMessage::Error(error) if error.error.code == "server_shutting_down"
    ));
    match websocket.read() {
        Ok(tungstenite::Message::Close(_))
        | Err(tungstenite::Error::ConnectionClosed)
        | Err(tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        )) => {}
        Err(tungstenite::Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
        }
        result => panic!("WebSocket did not terminate cleanly: {result:?}"),
    }
    let deadline = Instant::now() + timeout;
    loop {
        match raw_session
            .receive_cancellable(Duration::from_millis(25), || Instant::now() >= deadline)
            .unwrap()
        {
            SessionEvent::Disconnected {
                cause: DisconnectCause::ServerShutdown,
                ..
            } => break,
            SessionEvent::Disconnected { cause, .. } => {
                panic!("raw client disconnected unexpectedly: {cause:?}")
            }
            _ => assert!(Instant::now() < deadline, "raw client shutdown timed out"),
        }
    }
    drop(websocket);
    drop(raw_session);
    daemon.wait_success();

    let stored = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(stored.position().revision, 1);
    assert!(stored.idempotency_receipts().iter().any(|receipt| {
        receipt.key() == "web-cut-key"
            && receipt.command_id() == "web-cut"
            && matches!(
                receipt.outcome(),
                fm_persistence::ReceiptOutcome::Accepted { revision: 1, .. }
            )
    }));
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

fn create_rename_project(path: &Path) {
    let frame_rate = FrameRate::new(25, 1).unwrap();
    let mut project = Project::new(
        project_id(),
        "Rename Test",
        ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(16, 16).unwrap(),
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
    );
    for number in 1..=2 {
        project.add_input(Input {
            id: domain_input(number),
            name: format!("Input {number}"),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
    }
    assert!(project.set_input_audio_strip(
        domain_input(1),
        InputAudioStripState {
            gain: InputGainMilliDb::new(-1_000).unwrap(),
            ..InputAudioStripState::default()
        }
    ));
    assert!(project.set_input_audio_strip(
        domain_input(2),
        InputAudioStripState {
            gain: InputGainMilliDb::new(-2_000).unwrap(),
            ..InputAudioStripState::default()
        }
    ));
    project.set_main_mix(MainMix::new(domain_input(1), domain_input(2)));
    let stored = StoredProject::from_project(
        project,
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
    ProjectStore::new(path).unwrap().save(&stored).unwrap();
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
