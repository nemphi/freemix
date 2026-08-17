use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
use freemixd::{ReadinessRecord, StatusReadinessRecord};

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

    fn start_with_diagnostic_stop_after(project: &Path, duration: &str) -> Self {
        let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
            .arg("serve")
            .arg(project)
            .args(["--diagnostic-stop-after", duration])
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

    fn start_with_status(project: &Path, token: &str, once: bool) -> (Self, SocketAddr) {
        Self::start_with_status_and_web(project, token, once, false)
    }

    /// `web` also starts the WebSocket gateway, whose bounded shutdown keeps the
    /// daemon winding down long enough to observe a readiness transition.
    fn start_with_status_and_web(
        project: &Path,
        token: &str,
        once: bool,
        web: bool,
    ) -> (Self, SocketAddr) {
        let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"));
        command
            .arg("serve")
            .arg(project)
            .args(["--listen", "127.0.0.1:0", "--status-listen", "127.0.0.1:0"])
            .env("FREEMIXD_STATUS_TOKEN", token);
        if once {
            command.arg("--once");
        }
        if web {
            command
                .args(["--web-listen", "127.0.0.1:0"])
                .env("FREEMIXD_WEB_TOKEN", token);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let expected = if web { 3 } else { 2 };
        let mut lines = read_startup_lines(&mut child, expected);
        let line = lines.remove(0);
        let status_line = lines.pop().unwrap();
        let readiness = line.parse::<ReadinessRecord>().unwrap_or_else(|error| {
            startup_failure(
                &mut child,
                vec![line.clone(), status_line.clone()],
                error.to_string(),
                None,
            )
        });
        let status = status_line
            .parse::<StatusReadinessRecord>()
            .unwrap_or_else(|error| {
                startup_failure(
                    &mut child,
                    vec![line.clone(), status_line.clone()],
                    error.to_string(),
                    None,
                )
            });
        (
            Self {
                child: Some(child),
                address: readiness.address,
                project_id: readiness.project_id,
            },
            status.address,
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
    let (sender, receiver) = mpsc::sync_channel::<std::io::Result<Vec<String>>>(1);
    let reader = std::thread::spawn(move || {
        let mut output = BufReader::new(stdout);
        let result = (|| {
            let mut lines = Vec::with_capacity(count);
            for _ in 0..count {
                let mut line = String::new();
                let read = output.read_line(&mut line).map_err(|error| {
                    std::io::Error::new(error.kind(), format!("{error}; stdout={lines:?}"))
                })?;
                if read == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("stdout ended after {lines:?}"),
                    ));
                }
                lines.push(line);
            }
            Ok(lines)
        })();
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    let result = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(error) => startup_failure(child, Vec::new(), error.to_string(), Some(reader)),
    };
    let lines = match result {
        Ok(lines) => lines,
        Err(error) => startup_failure(child, Vec::new(), error.to_string(), Some(reader)),
    };
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

const STATUS_TOKEN: &str = "status-token-0123456789abcdef0123456789";

/// Sends one raw HTTP/1.1 request and returns the whole response the listener
/// wrote before closing. Write failures are tolerated so a rejected request
/// still surfaces the daemon's response instead of a client-side panic.
fn status_request(address: SocketAddr, request: &[u8]) -> String {
    try_status_request(address, request).unwrap()
}

/// The same request, tolerating a refused connection so a probe loop can keep
/// running across a daemon that is on its way out.
fn try_status_request(address: SocketAddr, request: &[u8]) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let _ = stream.write_all(request);
    let _ = stream.flush();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    Some(String::from_utf8_lossy(&response).into_owned())
}

/// Request shapes a supervisor or a scanner actually sends: a HEAD probe, an
/// unsupported method, an absent route, a head past the cap, and a protected
/// route probed without credentials.
fn assert_status_request_shapes(status_address: SocketAddr) {
    // HEAD-default supervisors get the same headers a GET would, and no body.
    let probed = status_request(
        status_address,
        b"HEAD /healthz HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(probed.starts_with("HTTP/1.1 200 OK\r\n"), "{probed}");
    assert!(probed.contains("Content-Length: "), "{probed}");
    assert!(!probed.contains("check=healthz"), "{probed}");

    let unsupported = status_request(
        status_address,
        b"POST /healthz HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(
        unsupported.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{unsupported}"
    );

    let unknown = status_request(
        status_address,
        b"GET /metrics HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(
        unknown.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{unknown}"
    );

    let mut oversized = String::from("GET /healthz HTTP/1.1\r\nHost: status\r\n");
    while oversized.len() < 8 * 1024 {
        oversized.push_str("X-Padding: 0123456789012345678901234567890123456789\r\n");
    }
    oversized.push_str("\r\n");
    let rejected = status_request(status_address, oversized.as_bytes());
    assert!(
        rejected.starts_with("HTTP/1.1 413 Content Too Large\r\n"),
        "{rejected}"
    );
    assert!(
        rejected.contains("check=request\tstatus=too-large\tlimit_bytes=4096"),
        "{rejected}"
    );

    // An unauthenticated caller must not be able to confirm the protected route
    // exists by probing it with a method the route does not serve.
    let probed_route = status_request(
        status_address,
        b"POST /v1/support-bundle HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(
        probed_route.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{probed_route}"
    );
}

/// RFC 7235 makes the scheme case-insensitive; the token itself stays exact.
fn assert_bundle_authentication_scheme(status_address: SocketAddr) {
    let lowercase_scheme = status_request(
        status_address,
        format!(
            "GET /v1/support-bundle HTTP/1.1\r\nHost: status\r\nAuthorization: bEaReR   {STATUS_TOKEN}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(
        lowercase_scheme.starts_with("HTTP/1.1 200 OK\r\n"),
        "{lowercase_scheme}"
    );

    let mixed_case_token = status_request(
        status_address,
        format!(
            "GET /v1/support-bundle HTTP/1.1\r\nHost: status\r\nAuthorization: Bearer {}\r\n\r\n",
            STATUS_TOKEN.to_uppercase()
        )
        .as_bytes(),
    );
    assert!(
        mixed_case_token.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{mixed_case_token}"
    );
}

#[test]
fn status_listener_serves_health_readiness_and_guarded_support_bundle() {
    let directory = TestDirectory::new("status-listener");
    let project_path = directory.project_path();
    create_project(&project_path);

    let (daemon, status_address) = Daemon::start_with_status(&project_path, STATUS_TOKEN, false);

    let health = status_request(
        status_address,
        b"GET /healthz HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(health.starts_with("HTTP/1.1 200 OK\r\n"), "{health}");
    assert!(
        health.contains("FREEMIXD_STATUS\tv=1\tcheck=healthz\tstatus=live\t"),
        "{health}"
    );

    let ready = status_request(
        status_address,
        b"GET /readyz HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(ready.starts_with("HTTP/1.1 200 OK\r\n"), "{ready}");
    assert!(
        ready.contains(
            "check=readyz\tstatus=ready\treadiness=ready\thealth=healthy\tliveness=live\t"
        ),
        "{ready}"
    );

    assert_status_request_shapes(status_address);

    let anonymous = status_request(
        status_address,
        b"GET /v1/support-bundle HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(
        anonymous.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{anonymous}"
    );
    assert!(!anonymous.contains(STATUS_TOKEN), "{anonymous}");

    let wrong = status_request(
        status_address,
        b"GET /v1/support-bundle HTTP/1.1\r\nHost: status\r\nAuthorization: Bearer status-token-0123456789abcdef012345678\r\n\r\n",
    );
    assert!(
        wrong.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{wrong}"
    );

    let bundle = status_request(
        status_address,
        format!(
            "GET /v1/support-bundle HTTP/1.1\r\nHost: status\r\nAuthorization: Bearer {STATUS_TOKEN}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(bundle.starts_with("HTTP/1.1 200 OK\r\n"), "{bundle}");
    assert!(
        bundle.contains("check=support-bundle\tstatus=ok\t"),
        "{bundle}"
    );
    assert!(bundle.contains("\"schema\":\"fm-support-v1\""), "{bundle}");
    assert_bundle_authentication_scheme(status_address);
    assert!(!bundle.contains(STATUS_TOKEN), "{bundle}");
    assert!(
        !bundle.contains(&project_path.display().to_string()),
        "{bundle}"
    );
    assert!(!bundle.contains(&status_address.to_string()), "{bundle}");

    daemon.stop();
}

/// Liveness and readiness are answered by the accept thread, so connections
/// that occupy the listener without ever sending a request must not be able to
/// delay or fail a supervisor probe. Sixteen of forty probes failed here before
/// the accept thread stopped handing probe sockets to the request workers.
#[test]
fn status_listener_answers_probes_while_silent_connections_flood_it() {
    const HELD: usize = 50;
    const FLOOD_THREADS: usize = 4;
    const PROBES: usize = 40;

    let directory = TestDirectory::new("status-listener-flood");
    let project_path = directory.project_path();
    create_project(&project_path);

    let (daemon, status_address) = Daemon::start_with_status(&project_path, STATUS_TOKEN, true);

    let held: Vec<TcpStream> = (0..HELD)
        .filter_map(|_| TcpStream::connect_timeout(&status_address, Duration::from_secs(1)).ok())
        .collect();
    assert_eq!(held.len(), HELD, "could not open the silent connections");

    let flooding = Arc::new(AtomicBool::new(true));
    let threads: Vec<_> = (0..FLOOD_THREADS)
        .map(|_| {
            let flooding = Arc::clone(&flooding);
            std::thread::spawn(move || {
                let mut sockets = Vec::new();
                while flooding.load(Ordering::Acquire) {
                    if let Ok(socket) =
                        TcpStream::connect_timeout(&status_address, Duration::from_secs(1))
                    {
                        sockets.push(socket);
                    }
                    if sockets.len() >= 16 {
                        sockets.clear();
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        })
        .collect();

    let mut answered = 0_usize;
    let mut slowest = Duration::ZERO;
    let mut failure = String::new();
    for _ in 0..PROBES {
        let started = Instant::now();
        let health = status_request(
            status_address,
            b"GET /healthz HTTP/1.1\r\nHost: status\r\n\r\n",
        );
        slowest = slowest.max(started.elapsed());
        if health.starts_with("HTTP/1.1 200 OK\r\n") {
            answered += 1;
        } else if failure.is_empty() {
            failure = health;
        }
    }

    flooding.store(false, Ordering::Release);
    for thread in threads {
        thread.join().unwrap();
    }
    drop(held);

    assert_eq!(
        answered, PROBES,
        "flooded listener failed a probe: {failure}"
    );
    assert!(
        slowest < Duration::from_millis(250),
        "slowest probe under flood took {slowest:?}"
    );

    let mut client = daemon.connect();
    client.handshake(None);
    drop(client);
    daemon.wait_success();
}

/// The published readiness must track the daemon, not the value startup wrote.
///
/// SIGTERM moves the daemon from ready to draining while the status listener is
/// still up, which is exactly the interval a supervisor has to be able to tell
/// apart from a crash. The gateway is enabled so the daemon spends a bounded
/// but measurable interval winding its other subsystems down; probing across
/// the signal must observe `readiness=draining`, which a readiness frozen at
/// startup never reports.
#[cfg(unix)]
#[test]
fn readiness_reports_draining_after_a_cooperative_stop_request() {
    let directory = TestDirectory::new("status-listener-draining");
    let project_path = directory.project_path();
    create_project(&project_path);

    let (daemon, status_address) =
        Daemon::start_with_status_and_web(&project_path, STATUS_TOKEN, false, true);
    let ready = status_request(
        status_address,
        b"GET /readyz HTTP/1.1\r\nHost: status\r\n\r\n",
    );
    assert!(ready.contains("status=ready\treadiness=ready\t"), "{ready}");

    let probing = Arc::new(AtomicBool::new(true));
    let drained = Arc::new(AtomicBool::new(false));
    let probes: Vec<_> = (0..8)
        .map(|_| {
            let probing = Arc::clone(&probing);
            let drained = Arc::clone(&drained);
            std::thread::spawn(move || {
                while probing.load(Ordering::Acquire) {
                    let Some(readiness) = try_status_request(
                        status_address,
                        b"GET /readyz HTTP/1.1\r\nHost: status\r\n\r\n",
                    ) else {
                        continue;
                    };
                    if readiness.contains("status=not-ready\treadiness=draining\t") {
                        assert!(
                            readiness.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
                            "{readiness}"
                        );
                        drained.store(true, Ordering::Release);
                    }
                }
            })
        })
        .collect();

    let status = ProcessCommand::new("/bin/kill")
        .args(["-TERM", &daemon.child.as_ref().unwrap().id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "SIGTERM command failed: {status}");
    daemon.wait_success();

    probing.store(false, Ordering::Release);
    for probe in probes {
        probe.join().unwrap();
    }
    assert!(
        drained.load(Ordering::Acquire),
        "/readyz never reported the draining transition"
    );
}

#[test]
fn simulated_diagnostic_deadline_exits_cleanly() {
    let directory = TestDirectory::new("simulated-diagnostic-deadline");
    let project_path = directory.project_path();
    create_project(&project_path);

    Daemon::start_with_diagnostic_stop_after(&project_path, "50ms").wait_success();
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

    // The daemon was killed, not stopped: these revisions live in the journal
    // and only reach the manifest when the next daemon recovers them.
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

/// The whole point of the journal: a command the operator was told succeeded
/// must survive `SIGKILL`.
///
/// The daemon is killed with the acknowledgement already in the client's hand
/// and no chance to shut down, flush or checkpoint. The manifest is still at
/// revision 0 at that instant, so nothing here can pass because a file happened
/// to be written — the two revisions exist only as journal batches, and only
/// recovery can bring them back.
#[test]
fn acknowledged_commands_survive_sigkill_before_any_checkpoint() {
    let directory = TestDirectory::new("sigkill-durability");
    let project_path = directory.project_path();
    create_project(&project_path);
    let store = ProjectStore::new(&project_path).unwrap();

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    assert_eq!(client.handshake(None).current_revision, 0);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));

    client.send(&command(
        "killed-preview",
        "killed-preview-key",
        CommandPayload::SelectPreview { input: input(3) },
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 1, .. }
    ));
    client.send(&command(
        "killed-cut",
        "killed-cut-key",
        CommandPayload::Cut,
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 2, .. }
    ));

    // Both acknowledgements are in hand. Kill the process outright.
    daemon.stop();

    // Nothing acknowledged is in the manifest, so the recovery below cannot be
    // reading a per-command manifest rewrite.
    assert_eq!(store.load().unwrap().position().revision, 0);
    let crashed = store.scan_journal().unwrap();
    assert_eq!(crashed.batches().len(), 2);
    assert_eq!(crashed.checkpoint_sequence(), 0);
    assert!(crashed.observations().is_empty());

    let daemon = Daemon::start(&project_path);
    let mut restarted = daemon.connect();
    assert_eq!(restarted.handshake(None).current_revision, 2);
    assert!(matches!(restarted.receive(), WireMessage::Snapshot(_)));
    // The receipts came back too, so a client that retries after the crash is
    // answered from the original acceptance instead of cutting the show again.
    restarted.send(&command(
        "killed-cut-retry",
        "killed-cut-key",
        CommandPayload::Cut,
    ));
    assert!(matches!(
        restarted.next_result(),
        CommandResult::Accepted {
            id,
            revision: 2,
            ..
        } if id == "killed-cut"
    ));
    drop(restarted);
    daemon.wait_success();

    let persisted = store.load().unwrap();
    assert_eq!(persisted.position().revision, 2);
    assert_eq!(persisted.idempotency_receipts().len(), 2);
    // The cut swapped the preview selected at revision 1 onto program.
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(domain_input(3))
    );
    assert_eq!(
        persisted.runtime_routing().desired_program_id,
        Some(domain_input(3))
    );
}

/// Checkpoints must bound journal growth on their own, and recovery must land
/// on the exact state the daemon held, not merely a plausible one.
///
/// A run of `CHECKPOINTED_COMMANDS` commands is executed twice against
/// identical projects: once stopped cleanly, once killed and recovered. The two
/// projects must end byte-for-byte identical as `StoredProject` values —
/// revision, event sequence, frames rendered, runtime generation, clock time,
/// routing and every receipt.
#[test]
fn checkpoints_compact_the_journal_and_recovery_reproduces_the_exact_state() {
    /// One more than the daemon's checkpoint interval, so a checkpoint is
    /// forced mid-run without a clean shutdown.
    const CHECKPOINTED_COMMANDS: u32 = 65;

    fn cut_repeatedly(daemon: &Daemon, count: u32) {
        let mut client = daemon.connect();
        client.handshake(None);
        assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
        for revision in 1..=count {
            client.send(&command(
                &format!("cut-{revision}"),
                &format!("cut-key-{revision}"),
                CommandPayload::Cut,
            ));
            match client.next_result() {
                CommandResult::Accepted { revision: got, .. } => {
                    assert_eq!(got, u64::from(revision));
                }
                CommandResult::Rejected { code, message, .. } => {
                    panic!("command {revision} was rejected as {code}: {message}")
                }
            }
        }
    }

    let directory = TestDirectory::new("checkpoint-clean");
    let clean_path = directory.project_path();
    create_project(&clean_path);
    let clean_store = ProjectStore::new(&clean_path).unwrap();
    let daemon = Daemon::start(&clean_path);
    cut_repeatedly(&daemon, CHECKPOINTED_COMMANDS);
    daemon.wait_success();
    let clean = clean_store.load().unwrap();
    assert_eq!(clean.position().revision, u64::from(CHECKPOINTED_COMMANDS));

    let crash_directory = TestDirectory::new("checkpoint-crash");
    let crash_path = crash_directory.project_path();
    create_project(&crash_path);
    let crash_store = ProjectStore::new(&crash_path).unwrap();
    let daemon = Daemon::start(&crash_path);
    cut_repeatedly(&daemon, CHECKPOINTED_COMMANDS);
    daemon.stop();

    // The bounded checkpoint fired mid-run: the manifest advanced without a
    // clean shutdown, and only the commands after it were left to replay.
    let crashed = crash_store.load().unwrap();
    assert!(
        crashed.position().revision > 0 && crashed.position().revision < clean.position().revision,
        "a checkpoint must have advanced the manifest mid-run, found revision {}",
        crashed.position().revision
    );
    let scan = crash_store.scan_journal().unwrap();
    assert_eq!(scan.checkpoint_revision(), crashed.position().revision);
    assert_eq!(
        u64::from(CHECKPOINTED_COMMANDS) - crashed.position().revision,
        scan.batches().len() as u64,
        "every command after the checkpoint must still be in the journal"
    );

    let daemon = Daemon::start(&crash_path);
    let mut client = daemon.connect();
    assert_eq!(
        client.handshake(None).current_revision,
        u64::from(CHECKPOINTED_COMMANDS)
    );
    drop(client);
    daemon.wait_success();

    assert_eq!(crash_store.load().unwrap(), clean);
    // Checkpointing discarded what it applied instead of retaining it forever.
    let settled = crash_store.scan_journal().unwrap();
    assert!(settled.batches().is_empty());
    assert_eq!(
        settled.checkpoint_revision(),
        u64::from(CHECKPOINTED_COMMANDS)
    );
}

/// Work that is not durable is never acknowledged.
///
/// The journal is made unusable underneath a running daemon — the specific
/// fault stands in for a failing or full show disk. The commands that follow
/// must come back refused and retryable, the daemon must stay up and keep
/// serving, and no refused command may leave a trace: once the journal works
/// again the next command takes the very next revision.
#[test]
fn a_command_that_cannot_be_journalled_is_refused_and_the_daemon_keeps_serving() {
    let directory = TestDirectory::new("journal-append-failure");
    let project_path = directory.project_path();
    create_project(&project_path);
    let journal = project_path.join("journal");

    let daemon = Daemon::start_without_once(&project_path);
    let mut client = daemon.connect();
    client.handshake(None);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
    client.send(&command(
        "durable-cut",
        "durable-cut-key",
        CommandPayload::Cut,
    ));
    assert!(matches!(
        client.next_result(),
        CommandResult::Accepted { revision: 1, .. }
    ));

    let saved: Vec<(PathBuf, Vec<u8>)> = fs::read_dir(&journal)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert!(!saved.is_empty(), "the accepted command was journalled");
    fs::remove_dir_all(&journal).unwrap();
    fs::create_dir(&journal).unwrap();
    fs::create_dir(journal.join("journal.db")).unwrap();

    for id in ["refused-first", "refused-second"] {
        client.send(&command(id, &format!("{id}-key"), CommandPayload::Cut));
        match client.next_result() {
            CommandResult::Rejected {
                id: rejected,
                code,
                current_revision,
                retryable,
                ..
            } => {
                assert_eq!(rejected, id);
                assert_eq!(code, "unavailable");
                assert_eq!(current_revision, 1, "a refused command takes no revision");
                assert!(retryable);
            }
            CommandResult::Accepted { revision, .. } => {
                panic!("{id} must not be acknowledged, yet it took revision {revision}")
            }
        }
    }

    fs::remove_dir_all(&journal).unwrap();
    fs::create_dir(&journal).unwrap();
    for (path, bytes) in saved {
        fs::write(path, bytes).unwrap();
    }
    client.send(&command(
        "after-repair",
        "after-repair-key",
        CommandPayload::Cut,
    ));
    assert!(
        matches!(
            client.next_result(),
            CommandResult::Accepted { revision: 2, .. }
        ),
        "the refused commands left no gap in the revision or the journal"
    );
    drop(client);
    daemon.stop();

    let store = ProjectStore::new(&project_path).unwrap();
    let scan = store.scan_journal().unwrap();
    assert_eq!(
        scan.batches().len(),
        2,
        "only the accepted commands persist"
    );
    let daemon = Daemon::start(&project_path);
    let mut restarted = daemon.connect();
    assert_eq!(restarted.handshake(None).current_revision, 2);
    drop(restarted);
    daemon.wait_success();
    assert_eq!(store.load().unwrap().position().revision, 2);
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
