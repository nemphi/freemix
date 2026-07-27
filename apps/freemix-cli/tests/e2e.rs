use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientType, CommandMessage, CommandPayload, CommandResult,
    EngineIdentity, EventCursor, EventMessage, EventPayload, ProtocolVersion, Role,
    RuntimeDomainBoundary, RuntimeEventMessage, RuntimeLifecycleEvent, ServerHello, ServerIdentity,
    SnapshotMessage, WireInputId, WireMessage, decode_line, encode_line,
};

static TEST_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

    fn start_premature(kind: PrematureEvent) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_premature_event(&listener, kind));
        Self { address, worker }
    }

    fn start_old_without_wipe() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || serve_old_daemon_without_wipe(&listener));
        Self { address, worker }
    }

    fn address(&self) -> String {
        self.address.to_string()
    }

    fn finish(self) {
        self.worker.join().unwrap();
    }
}

#[derive(Clone, Copy)]
enum PrematureEvent {
    Durable,
    Runtime,
}

fn serve_remote_sessions(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let mut revision = 0;
    let mut fade_result_id = None;

    for session in 0..6 {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        assert_client_hello(read_message(&mut reader));
        write_handshake(&mut writer, &engine, revision);

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
            5 => {
                assert_command(&command, CommandPayload::Cut, "remote-fade", 0);
                write_message(
                    &mut writer,
                    &WireMessage::CommandResult(CommandResult::Accepted {
                        id: fade_result_id.clone().unwrap(),
                        revision,
                        scheduled_frame: Some(3),
                    }),
                );
                continue;
            }
            _ => unreachable!(),
        }

        revision += 1;
        let result = CommandResult::Accepted {
            id: command.id.clone(),
            revision,
            scheduled_frame: Some(revision),
        };
        if session == 3 {
            fade_result_id = Some(command.id.clone());
        }
        write_message(&mut writer, &WireMessage::CommandResult(result));
        write_command_events(&mut writer, &engine, revision, command.payload);
    }
}

fn serve_premature_event(listener: &TcpListener, kind: PrematureEvent) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_client_hello(read_message(&mut reader));
    write_handshake(&mut writer, &engine, 0);
    let WireMessage::Command(_) = read_message(&mut reader) else {
        panic!("expected remote command");
    };
    let message = match kind {
        PrematureEvent::Durable => WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision: 1,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
            },
        }),
        PrematureEvent::Runtime => WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(&engine),
            revision: 0,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
            },
        }),
    };
    write_message(&mut writer, &message);
}

fn serve_old_daemon_without_wipe(listener: &TcpListener) {
    let engine = EngineIdentity {
        engine_id: "project-42".into(),
        state_epoch: 1,
        log_id: "fake-remote-log".into(),
    };
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    assert_client_hello(read_message(&mut reader));
    write_handshake(&mut writer, &engine, 0);

    let mut unexpected = String::new();
    assert_eq!(reader.read_line(&mut unexpected).unwrap(), 0);
    assert!(unexpected.is_empty());
}

fn assert_client_hello(message: WireMessage) {
    let WireMessage::ClientHello(hello) = message else {
        panic!("expected client hello");
    };
    assert_eq!(hello.versions, vec![CURRENT_PROTOCOL_VERSION]);
    assert_eq!(hello.client_type, ClientType::Cli);
    assert_eq!(hello.desired_role, Role::Operator);
    assert_eq!(hello.cached_cursor, None);
}

fn write_handshake(writer: &mut TcpStream, engine: &EngineIdentity, revision: u64) {
    write_message(
        writer,
        &WireMessage::ServerHello(ServerHello {
            negotiated: ProtocolVersion::new(1, 0),
            granted_role: Role::Operator,
            permissions: vec!["switcher.write".into()],
            capabilities_digest: "fake-capabilities".into(),
            engine: engine.clone(),
            current_revision: revision,
            resume: false,
        }),
    );
    let preview = if revision == 0 { input(2) } else { input(1) };
    write_message(
        writer,
        &WireMessage::Snapshot(SnapshotMessage {
            engine: engine.clone(),
            revision,
            show_name: "Remote Contract".into(),
            inputs: vec![input(1), input(2)],
            desired_program: input(1),
            desired_preview: preview,
            realized_program: input(1),
            realized_preview: preview,
        }),
    );
}

fn assert_command(
    command: &CommandMessage,
    payload: CommandPayload,
    key: &str,
    expected_revision: u64,
) {
    assert_eq!(command.protocol, ProtocolVersion::new(1, 0));
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
                program: input(1),
                preview: input(1),
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

#[test]
fn remote_commands_use_protocol_server_and_replay_duplicate_keys() {
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

    let duplicate = invoke(&[
        "remote-cut",
        &address,
        "--key",
        "remote-fade",
        "--expect",
        "0",
    ]);
    assert_success(&duplicate);
    assert_eq!(stdout(&duplicate), final_status);
    server.finish();
}

#[test]
fn remote_commands_reject_non_loopback_addresses_before_connecting() {
    let output = invoke(&["remote-status", "192.0.2.1:9123"]);
    assert_failure_contains(&output, "requires a loopback address");
}

#[test]
fn new_cli_does_not_send_wipe_to_an_old_daemon() {
    let server = FakeRemoteServer::start_old_without_wipe();
    let output = invoke(&[
        "remote-wipe",
        &server.address(),
        "3",
        "--key",
        "unsupported-wipe",
    ]);
    assert_failure_contains(
        &output,
        "command requires protocol 1.3, but the session negotiated 1.0",
    );
    server.finish();
}

#[test]
fn remote_commands_reject_durable_events_before_command_results() {
    assert_premature_event_rejected(PrematureEvent::Durable);
}

#[test]
fn remote_commands_reject_runtime_events_before_command_results() {
    assert_premature_event_rejected(PrematureEvent::Runtime);
}

fn assert_premature_event_rejected(kind: PrematureEvent) {
    let server = FakeRemoteServer::start_premature(kind);
    let output = invoke(&[
        "remote-cut",
        &server.address(),
        "--key",
        "premature-event",
        "--expect",
        "0",
    ]);
    assert_failure_contains(&output, "durable/runtime event before the command result");
    server.finish();
}

#[test]
fn complete_deterministic_mvp_contract() {
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
fn edited_schema_v4_configuration_survives_commands_restart_and_render() {
    let root = unique_test_root();
    let project = root.join("edited.freemix");
    let solid_image = root.join("solid.ppm");
    let bars_image = root.join("bars.ppm");
    fs::create_dir_all(&root).unwrap();
    let project_path = project.to_str().unwrap();
    assert_success(&invoke(&["new", project_path, "--name", "Edited V3"]));

    edit_schema_v4_configuration(&project);

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

fn edit_schema_v4_configuration(project: &Path) {
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
        r#""scenes": [{"id": 11, "name": "Edited scene", "background": {"red": 0, "green": 0, "blue": 0, "alpha": 255}, "layers": [{"name": "Source", "source": {"type": "input", "id": 1}, "enabled": true, "geometry": {"translation_x": 0, "translation_y": 0, "width": 1280, "height": 720, "rotation": "deg0"}, "crop": null, "opacity": 255, "z_order": 0}]}]"#,
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

#[test]
fn supported_legacy_manifest_is_migrated_before_cli_load() {
    let root = unique_test_root();
    let project = root.join("legacy.freemix");
    let image = root.join("legacy.ppm");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("project.json"),
        r#"{
  "schema_version": 2,
  "project_id": 42,
  "show_name": "Legacy CLI",
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
}
"#,
    )
    .unwrap();

    let migrated_status = status(&project);
    assert!(migrated_status.contains("project_id=42 show=\"Legacy CLI\""));
    assert_status(&migrated_status, 0, 0, 1, 1, 2, 2);
    let migrated = manifest(&project);
    assert!(migrated.starts_with("{\n  \"schema_version\": 4,"));
    assert!(migrated.contains(r#""type": "simulated""#));

    assert_success(&invoke(&[
        "render",
        project.to_str().unwrap(),
        image.to_str().unwrap(),
        "--width",
        "2",
        "--height",
        "1",
    ]));
    assert_solid_ppm(&image, 2, 1, [73, 151, 199]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema_v3_manifest_is_migrated_before_cli_load() {
    let root = unique_test_root();
    let project = root.join("v3.freemix");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("project.json"),
        include_str!("../../../crates/services/fm-persistence/tests/fixtures/schema-v3.json")
            .replace("\"revision\": 7", "\"revision\": 0")
            .replace("\"event_sequence\": 9", "\"event_sequence\": 0")
            .replace("\"frames_rendered\": 240", "\"frames_rendered\": 0")
            .replace("\"runtime_generation\": 3", "\"runtime_generation\": 0")
            .replace("\"clock_time_nanos\": 10000000", "\"clock_time_nanos\": 0"),
    )
    .unwrap();

    let migrated_status = status(&project);
    assert!(migrated_status.contains("show=\"Frozen V3 Scene\""));
    let migrated = manifest(&project);
    assert!(migrated.starts_with("{\n  \"schema_version\": 4,"));
    assert!(migrated.contains(r#""background": {"red": 0, "green": 0, "blue": 0, "alpha": 255}"#));
    assert!(migrated.contains(
        r#""geometry": {"translation_x": 0, "translation_y": 0, "width": 3840, "height": 2160, "rotation": "deg0"}"#
    ));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema_v1_manifest_is_rejected_without_mutation() {
    let root = unique_test_root();
    let project = root.join("v1.freemix");
    fs::create_dir_all(&project).unwrap();
    let original =
        include_str!("../../../crates/services/fm-persistence/tests/fixtures/schema-v1.json");
    fs::write(project.join("project.json"), original).unwrap();

    let result = invoke(&["status", project.to_str().unwrap()]);

    assert_failure_contains(&result, "unsupported schema 1; expected 4");
    assert_eq!(manifest(&project), original);
    fs::remove_dir_all(root).unwrap();
}

fn invoke(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_freemix-cli"))
        .args(arguments)
        .output()
        .unwrap()
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
