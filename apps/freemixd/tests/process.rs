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
    RestartPolicy, Rgba8, Rotation, Scene, SimulatedAudio, SimulatedInput, SimulatedVideo,
    SolidColor, SourceRef, StartupPolicy,
};
use fm_persistence::{ProjectPosition, ProjectStore, RuntimeRouting, StoredProject};
use fm_protocol::{
    ClientHello, ClientType, CommandMessage, CommandPayload, CommandResult, EventCursor,
    HandshakeOutcome, ProtocolVersion, Role, RuntimeLifecycleEvent, ServerHello, SnapshotReason,
    WireInputId, WireMessage, decode_line, encode_line,
};
use fm_types::{
    AudioFormat, BusId, ChannelLayout, ColorMetadata, FrameRate, InputId, OutputId, PixelFormat,
    ProjectId, SampleFormat, SampleRate, ScanMode, SceneId, VideoDimensions, VideoFormat,
};
use freemixd::ReadinessRecord;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const LEGACY_MAX_ID: u128 = 18_446_744_073_709_551_615;
const PROJECT_ID: u128 = LEGACY_MAX_ID + 42;
const INPUT_ID_BASE: u128 = LEGACY_MAX_ID + 100;

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

    fn handshake(&mut self, cursor: Option<EventCursor>) -> ServerHello {
        self.send(&WireMessage::ClientHello(ClientHello {
            versions: vec![ProtocolVersion::new(1, 0)],
            build: "process-test".into(),
            client_type: ClientType::Integration,
            desired_role: Role::Operator,
            cached_cursor: cursor,
        }));
        let WireMessage::ServerHello(hello) = self.receive() else {
            panic!("expected server hello");
        };
        hello
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
        RuntimeLifecycleEvent::Realized { domain } if domain == "switcher"
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
        vec![ProtocolVersion::new(1, 0)],
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
        vec![ProtocolVersion::new(1, 0)],
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
fn v2_project_migrates_and_serves() {
    let directory = TestDirectory::new("v2-migration");
    let project_path = directory.project_path();
    fs::create_dir(&project_path).unwrap();
    fs::write(
        project_path.join("project.json"),
        r#"{
  "schema_version": 2,
  "project_id": 9002,
  "show_name": "Legacy V2",
  "input_ids": [1, 2],
  "desired_program_id": 1,
  "realized_program_id": 1,
  "desired_preview_id": 2,
  "realized_preview_id": 2,
  "revision": 0,
  "state_epoch": 1,
  "event_sequence": 0,
  "frames_rendered": 0,
  "runtime_generation": 0,
  "clock_time_nanos": 0,
  "idempotency_receipts": []
}"#,
    )
    .unwrap();

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    assert_eq!(client.handshake(None).current_revision, 0);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
    drop(client);
    daemon.wait_success();

    let migrated = ProjectStore::new(project_path).unwrap().load().unwrap();
    assert_eq!(migrated.schema_version(), 4);
    assert_eq!(migrated.project().name(), "Legacy V2");
}

#[test]
fn v3_project_migrates_and_serves() {
    let directory = TestDirectory::new("v3-migration");
    let project_path = directory.project_path();
    fs::create_dir(&project_path).unwrap();
    fs::write(
        project_path.join("project.json"),
        include_str!("../../../crates/services/fm-persistence/tests/fixtures/schema-v3.json")
            .replace("\"revision\": 7", "\"revision\": 0")
            .replace("\"event_sequence\": 9", "\"event_sequence\": 0")
            .replace("\"frames_rendered\": 240", "\"frames_rendered\": 0")
            .replace("\"runtime_generation\": 3", "\"runtime_generation\": 0")
            .replace("\"clock_time_nanos\": 10000000", "\"clock_time_nanos\": 0"),
    )
    .unwrap();

    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    assert_eq!(client.handshake(None).current_revision, 0);
    assert!(matches!(client.receive(), WireMessage::Snapshot(_)));
    drop(client);
    daemon.wait_success();

    let migrated = ProjectStore::new(project_path).unwrap().load().unwrap();
    assert_eq!(migrated.schema_version(), 4);
    assert_eq!(migrated.project().name(), "Frozen V3 Scene");
    let scene = &migrated.project().scenes()[0];
    assert_eq!(scene.background, Rgba8::OPAQUE_BLACK);
    assert_eq!(
        scene.layers[0].geometry,
        LayerGeometry::new(0, 0, 3_840, 2_160, Rotation::Deg0)
    );
}

#[test]
fn v1_project_is_rejected_as_unsupported() {
    let directory = TestDirectory::new("v1-unsupported");
    let project_path = directory.project_path();
    fs::create_dir(&project_path).unwrap();
    fs::write(
        project_path.join("project.json"),
        include_str!("../../../crates/services/fm-persistence/tests/fixtures/schema-v1.json"),
    )
    .unwrap();

    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_freemixd"))
        .arg("serve")
        .arg(&project_path)
        .arg("--once")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unsupported schema 1; expected 4")
    );
}

#[test]
fn incompatible_handshake_returns_protocol_error() {
    let directory = TestDirectory::new("incompatible");
    let project_path = directory.project_path();
    create_project(&project_path);
    let daemon = Daemon::start(&project_path);
    let mut client = daemon.connect();
    client.send(&WireMessage::ClientHello(ClientHello {
        versions: vec![ProtocolVersion::new(2, 0)],
        build: "future".into(),
        client_type: ClientType::Integration,
        desired_role: Role::Operator,
        cached_cursor: None,
    }));
    assert!(matches!(
        client.receive(),
        WireMessage::Error(error) if error.error.code == "incompatible_version"
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
    WireMessage::Command(CommandMessage {
        protocol: ProtocolVersion::new(1, 0),
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
