use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use fm_client::{ClientError, CommandStatus, Intake, SessionEvent, SyncMode, TcpSessionError};
use fm_protocol::{
    CapabilityReportSummary, CommandPayload, CommandResult, EngineIdentity, EventCursor,
    EventMessage, EventPayload, HandshakeOutcome, HandshakeResponse, LineDecoder, ProtocolVersion,
    Role, RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity, SnapshotMessage,
    SnapshotReason, WireInputId, WireMessage, encode_line,
};
use fm_types::ProjectId;
use freemix_studio::{
    Command, ConnectionConfig, DaemonSupervisor, ExistingConfig, LifecycleState, ReadinessRecord,
    RestartPolicy, StudioConfig, StudioError, StudioRuntime, SupervisedConfig, SupervisorError,
    SupervisorState, parse_args,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PROJECT_VALUE: u128 = 18_446_744_073_709_551_657;
static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn project_id() -> ProjectId {
    ProjectId::new(NonZeroU128::new(PROJECT_VALUE).unwrap())
}

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
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
    HandshakeResponse {
        negotiated: ProtocolVersion::new(1, 2),
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
        inputs: vec![input(1), input(2)],
        desired_program: input(1),
        desired_preview: input(2),
        realized_program: input(1),
        realized_preview: input(2),
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
        },
    }
}

fn runtime_event(revision: u64) -> RuntimeEventMessage {
    RuntimeEventMessage {
        server: server(project_id()),
        revision,
        generation: 1,
        sequence: 1,
        event: RuntimeLifecycleEvent::Realized {
            domain: "switcher".to_owned(),
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
    }
}

fn spawn_server(run: impl FnOnce(TcpListener) + Send + 'static) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (address, thread::spawn(move || run(listener)))
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

fn serve_snapshot_then_resume(listener: &TcpListener) {
    let mut first = Peer::accept(listener);
    let WireMessage::HandshakeRequest(request) = first.receive() else {
        panic!("expected modern handshake request");
    };
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
    assert_eq!(heartbeat.last_applied.unwrap().revision, 4);
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

    runtime.send_heartbeat(1234).unwrap();
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
            "#!/bin/sh\nprintf 'FREEMIXD_READY\\tv=1\\taddress=127.0.0.1:32123\\tproject_id={PROJECT_VALUE}\\n'\nexit 7\n"
        ),
        "identity-change" => format!(
            "#!/bin/sh\nif test -e \"$2.count\"; then id=42; else id={PROJECT_VALUE}; : > \"$2.count\"; fi\nprintf 'FREEMIXD_READY\\tv=1\\taddress=127.0.0.1:32123\\tproject_id=%s\\n' \"$id\"\nIFS= read -r hold\n"
        ),
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
