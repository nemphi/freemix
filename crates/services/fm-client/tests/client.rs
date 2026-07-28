use core::num::NonZeroU128;

use fm_client::{
    Client, ClientConfig, ClientError, CommandStatus, CommandUncertainty, ConnectionState,
    DEFAULT_COMPLETED_COMMAND_CAPACITY, Intake, MAX_COMPLETED_COMMAND_CAPACITY, Outbound, SyncMode,
};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, ClientType, CommandPayload, CommandResult,
    EngineIdentity, EventCursor, EventMessage, EventPayload, FadeToBlackPosition, FadeToBlackState,
    HandshakeOutcome, HandshakeResponse, MANUAL_TRANSITION_PROTOCOL_VERSION, ManualTransitionKind,
    ManualTransitionPosition, ManualTransitionState, ManualTransitionStatus, ProtocolVersion,
    ResumeCursor, Role, RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity,
    SnapshotMessage, SnapshotReason, WIPE_PROTOCOL_VERSION, WireInputId, WireMessage,
};
use fm_types::ProjectId;
use fm_ui_model::ManualTransitionStatus as ModelManualTransitionStatus;

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
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

fn config(capacity: usize) -> ClientConfig {
    let mut config = ClientConfig::new(
        vec![CURRENT_PROTOCOL_VERSION],
        "diagnostic 0.1",
        ClientType::Cli,
        Role::Operator,
        "diagnostic-a",
        project_id(),
    );
    config.outbound_capacity = capacity;
    config.initial_backoff_ms = 10;
    config.max_backoff_ms = 40;
    config
}

fn handshake(revision: u64, resume: Option<ResumeCursor>) -> HandshakeResponse {
    handshake_version(CURRENT_PROTOCOL_VERSION, revision, resume)
}

fn handshake_version(
    negotiated: ProtocolVersion,
    revision: u64,
    resume: Option<ResumeCursor>,
) -> HandshakeResponse {
    HandshakeResponse {
        negotiated,
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
        outcome: resume.map_or(
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
            |cursor| HandshakeOutcome::Resume { cursor },
        ),
    }
}

fn snapshot(revision: u64) -> SnapshotMessage {
    SnapshotMessage {
        engine: engine(),
        revision,
        show_name: "Show".to_owned(),
        inputs: vec![input(1), input(2)],
        desired_program: input(1),
        desired_preview: input(2),
        realized_program: input(1),
        realized_preview: input(2),
        desired_manual_transition: Some(ManualTransitionStatus::Inactive),
        realized_manual_transition: Some(ManualTransitionStatus::Inactive),
        desired_fade_to_black: Some(live_fade_to_black()),
        realized_fade_to_black: Some(live_fade_to_black()),
    }
}

fn live_fade_to_black() -> FadeToBlackState {
    fade_to_black(false, FadeToBlackPosition::LIVE.numerator())
}

fn fade_to_black(target_active: bool, numerator: u16) -> FadeToBlackState {
    FadeToBlackState {
        target_active,
        position: FadeToBlackPosition::new(numerator),
    }
}

fn connect_snapshot(client: &mut Client, revision: u64) {
    client.start_connect().unwrap();
    client.transport_connected().unwrap();
    client.accept_handshake(handshake(revision, None)).unwrap();
    client.apply_snapshot(snapshot(revision)).unwrap();
}

fn ready_client(capacity: usize) -> Client {
    let mut client = Client::new(config(capacity)).unwrap();
    connect_snapshot(&mut client, 4);
    client
}

fn active_manual(interval_start: u16, position: u16) -> ManualTransitionStatus {
    ManualTransitionStatus::Active(ManualTransitionState {
        kind: ManualTransitionKind::Wipe,
        from: input(1),
        to: input(2),
        interval_start: ManualTransitionPosition::new(interval_start).unwrap(),
        position: ManualTransitionPosition::new(position).unwrap(),
    })
}

fn complete_cut(client: &mut Client, key: String) -> fm_protocol::CommandMessage {
    let command = client
        .queue_command(CommandPayload::Cut, key, Some(4), None)
        .unwrap();
    assert!(matches!(
        client.pop_outbound(),
        Some(Outbound::Command(queued)) if queued == command
    ));
    client
        .reconcile_result(CommandResult::Accepted {
            id: command.id.clone(),
            revision: 4,
            scheduled_frame: None,
        })
        .unwrap();
    command
}

fn runtime_event(
    revision: u64,
    generation: u64,
    sequence: u64,
    event: RuntimeLifecycleEvent,
) -> RuntimeEventMessage {
    RuntimeEventMessage {
        server: server(),
        revision,
        generation,
        sequence,
        event,
    }
}

#[test]
fn connect_snapshot_resume_and_reconnect_are_deterministic() {
    let mut client = Client::new(config(4)).unwrap();
    client.start_connect().unwrap();
    let first_hello = client.transport_connected().unwrap();
    assert_eq!(first_hello.resume_cursor, None);
    client.accept_handshake(handshake(4, None)).unwrap();
    assert_eq!(
        client.state(),
        &ConnectionState::Synchronizing {
            mode: SyncMode::Snapshot,
            target_revision: 4
        }
    );
    client.apply_snapshot(snapshot(4)).unwrap();
    assert_eq!(client.state(), &ConnectionState::Ready);

    let backoff = client.transport_disconnected();
    assert_eq!(backoff.attempt, 1);
    assert_eq!(backoff.delay_ms, 10);
    client.start_connect().unwrap();
    let reconnect_hello = client.transport_connected().unwrap();
    let cursor = reconnect_hello.resume_cursor.unwrap();
    assert_eq!(cursor.revision, 4);
    client.accept_handshake(handshake(5, Some(cursor))).unwrap();
    client
        .apply_event(EventMessage {
            cursor: EventCursor {
                engine: engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: Some(live_fade_to_black()),
            },
        })
        .unwrap();
    assert_eq!(client.state(), &ConnectionState::Ready);
    assert_eq!(
        client.model().view().unwrap().switcher.desired.program,
        input(2).to_domain()
    );

    assert_eq!(client.transport_disconnected().attempt, 1);
    assert_eq!(client.transport_disconnected().attempt, 2);
    assert_eq!(client.transport_disconnected().delay_ms, 40);
}

#[test]
fn protocol_1_5_requires_and_reduces_exact_fade_to_black_state() {
    let mut client = Client::new(config(4)).unwrap();
    client.start_connect().unwrap();
    client.transport_connected().unwrap();
    client.accept_handshake(handshake(4, None)).unwrap();

    let mut incomplete = snapshot(4);
    incomplete.desired_fade_to_black = None;
    assert!(matches!(
        client.apply_snapshot(incomplete),
        Err(ClientError::InvalidSnapshot(
            "protocol 1.5 snapshot omitted fade-to-black state"
        ))
    ));

    let mut initial = snapshot(4);
    initial.desired_fade_to_black = Some(fade_to_black(true, 40_000));
    initial.realized_fade_to_black = Some(fade_to_black(true, 20_000));
    client.apply_snapshot(initial).unwrap();
    let switcher = client.model().state().unwrap().switcher();
    assert_eq!(switcher.desired_fade_to_black, fade_to_black(true, 40_000));
    assert_eq!(switcher.realized_fade_to_black, fade_to_black(true, 20_000));

    let incomplete_event = EventMessage {
        cursor: EventCursor {
            engine: engine(),
            revision: 5,
        },
        payload: EventPayload::DesiredSwitcher {
            program: input(1),
            preview: input(2),
            manual_transition: Some(ManualTransitionStatus::Inactive),
            fade_to_black: None,
        },
    };
    assert!(matches!(
        client.apply_event(incomplete_event),
        Err(ClientError::InvalidSnapshot(
            "protocol 1.5 event omitted fade-to-black state"
        ))
    ));
    client
        .apply_event(EventMessage {
            cursor: EventCursor {
                engine: engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: Some(fade_to_black(false, 12_345)),
            },
        })
        .unwrap();
    assert_eq!(
        client
            .model()
            .state()
            .unwrap()
            .switcher()
            .desired_fade_to_black,
        fade_to_black(false, 12_345)
    );

    let incomplete_runtime = runtime_event(
        5,
        7,
        1,
        RuntimeLifecycleEvent::Realized {
            domain: "switcher".into(),
            manual_transition: Some(ManualTransitionStatus::Inactive),
            fade_to_black: None,
        },
    );
    assert!(matches!(
        client.apply_runtime_event(incomplete_runtime),
        Err(ClientError::InvalidSnapshot(
            "protocol 1.5 runtime event omitted fade-to-black state"
        ))
    ));
    client
        .apply_runtime_event(runtime_event(
            5,
            7,
            1,
            RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: Some(fade_to_black(false, 12_345)),
            },
        ))
        .unwrap();
    assert_eq!(
        client
            .model()
            .state()
            .unwrap()
            .switcher()
            .realized_fade_to_black,
        fade_to_black(false, 12_345)
    );
}

#[test]
fn incompatible_handshake_is_terminal_until_configuration_changes() {
    let mut client = Client::new(config(2)).unwrap();
    client.start_connect().unwrap();
    client.transport_connected().unwrap();
    let mut incompatible = handshake(0, None);
    incompatible.negotiated = ProtocolVersion::new(2, 0);
    assert_eq!(
        client.accept_handshake(incompatible),
        Err(ClientError::IncompatibleProtocol(ProtocolVersion::new(
            2, 0
        )))
    );
    assert_eq!(
        client.state(),
        &ConnectionState::Incompatible {
            negotiated: ProtocolVersion::new(2, 0)
        }
    );
}

#[test]
fn completed_history_configuration_requires_a_finite_nonzero_bound() {
    assert_eq!(
        config(1).completed_command_capacity,
        DEFAULT_COMPLETED_COMMAND_CAPACITY
    );

    let mut zero = config(1);
    zero.completed_command_capacity = 0;
    assert!(matches!(
        Client::new(zero),
        Err(ClientError::InvalidConfig(
            "completed command capacity must be finite and between 1 and 65536"
        ))
    ));

    let mut unbounded = config(1);
    unbounded.completed_command_capacity = usize::MAX;
    assert!(unbounded.completed_command_capacity > MAX_COMPLETED_COMMAND_CAPACITY);
    assert!(matches!(
        Client::new(unbounded),
        Err(ClientError::InvalidConfig(
            "completed command capacity must be finite and between 1 and 65536"
        ))
    ));
}

#[test]
fn completed_history_stays_constant_and_reconnect_scan_is_bounded() {
    const RETAINED: usize = 32;
    const COMPLETED: usize = 4_096;

    let mut settings = config(1);
    settings.completed_command_capacity = RETAINED;
    let mut client = Client::new(settings).unwrap();
    connect_snapshot(&mut client, 4);

    for sequence in 1..=COMPLETED {
        complete_cut(&mut client, format!("stress-{sequence}"));
        assert!(client.retained_command_count() <= RETAINED);
    }
    assert_eq!(client.retained_command_count(), RETAINED);
    assert!(client.command("diagnostic-a:4064").is_none());
    assert!(matches!(
        client.command("diagnostic-a:4065").unwrap().status,
        CommandStatus::Completed(_)
    ));
    assert!(matches!(
        client.command("diagnostic-a:4096").unwrap().status,
        CommandStatus::Completed(_)
    ));

    let _ = client.transport_disconnected();
    client.start_connect().unwrap();
    let cursor = client.transport_connected().unwrap().resume_cursor.unwrap();
    client
        .accept_handshake(handshake_version(
            ProtocolVersion::new(1, 2),
            4,
            Some(cursor),
        ))
        .unwrap();
    assert_eq!(client.state(), &ConnectionState::Ready);
    assert_eq!(client.retained_command_count(), RETAINED);
    assert_eq!(
        client
            .command("diagnostic-a:4096")
            .unwrap()
            .command
            .protocol,
        CURRENT_PROTOCOL_VERSION
    );
}

#[test]
fn completed_eviction_preserves_all_unresolved_state_and_uncertainty() {
    let mut settings = config(2);
    settings.completed_command_capacity = 2;
    let mut client = Client::new(settings).unwrap();
    connect_snapshot(&mut client, 4);

    let wipe = client
        .queue_command(
            CommandPayload::Wipe {
                duration_frames: 45,
            },
            "unresolved-wipe",
            Some(4),
            None,
        )
        .unwrap();
    assert!(matches!(client.pop_outbound(), Some(Outbound::Command(_))));
    let preview = client
        .queue_command(
            CommandPayload::SelectPreview { input: input(1) },
            "unresolved-preview",
            Some(4),
            None,
        )
        .unwrap();
    assert!(matches!(client.pop_outbound(), Some(Outbound::Command(_))));

    for sequence in 0..128 {
        complete_cut(&mut client, format!("completed-{sequence}"));
    }
    assert_eq!(client.retained_command_count(), 4);
    assert_eq!(client.command(&wipe.id).unwrap().command, wipe);
    assert_eq!(
        client.command(&wipe.id).unwrap().status,
        CommandStatus::Sent
    );
    assert_eq!(client.command(&preview.id).unwrap().command, preview);
    assert_eq!(
        client.command(&preview.id).unwrap().status,
        CommandStatus::Sent
    );
    assert_eq!(client.model().pending_commands().len(), 2);
    assert_eq!(
        client.model().view().unwrap().switcher.desired.preview,
        input(1).to_domain()
    );

    let _ = client.transport_disconnected();
    client.start_connect().unwrap();
    let cursor = client.transport_connected().unwrap().resume_cursor.unwrap();
    client
        .accept_handshake(handshake_version(
            ProtocolVersion::new(1, 2),
            4,
            Some(cursor),
        ))
        .unwrap();
    assert_eq!(client.retained_command_count(), 4);
    assert_eq!(client.command(&wipe.id).unwrap().command, wipe);
    assert_eq!(
        client.command(&preview.id).unwrap().command.protocol,
        ProtocolVersion::new(1, 2)
    );
    assert_eq!(client.model().pending_commands().len(), 2);
    assert_eq!(
        client.queue_command(CommandPayload::Cut, "unresolved-wipe", Some(4), None),
        Err(ClientError::DuplicateIdempotencyKey(
            "unresolved-wipe".to_owned()
        ))
    );
}

#[test]
fn completion_order_evicts_command_and_local_key_index_together() {
    let mut settings = config(3);
    settings.completed_command_capacity = 2;
    let mut client = Client::new(settings).unwrap();
    connect_snapshot(&mut client, 4);

    let commands = ["first", "second", "third"].map(|key| {
        let command = client
            .queue_command(CommandPayload::Cut, key, Some(4), None)
            .unwrap();
        client.pop_outbound();
        command
    });
    client.retry_command(&commands[2].id).unwrap();
    assert_eq!(client.outbound_len(), 1);
    for index in [2, 0, 1] {
        client
            .reconcile_result(CommandResult::Accepted {
                id: commands[index].id.clone(),
                revision: 4,
                scheduled_frame: None,
            })
            .unwrap();
        if index == 2 {
            assert_eq!(client.outbound_len(), 0);
        }
    }

    assert!(client.command(&commands[2].id).is_none());
    assert!(client.command(&commands[0].id).is_some());
    assert!(client.command(&commands[1].id).is_some());
    assert_eq!(client.retained_command_count(), 2);
    // The bounded local index forgets the key; this does not make reuse safe
    // while the server may still retain its original receipt.
    let locally_accepted = client
        .queue_command(CommandPayload::Cut, "third", Some(4), None)
        .unwrap();
    assert_eq!(locally_accepted.id, "diagnostic-a:4");
    assert_eq!(
        client.queue_command(CommandPayload::Cut, "first", Some(4), None),
        Err(ClientError::DuplicateIdempotencyKey("first".to_owned()))
    );
}

#[test]
fn replayed_evicted_receipt_terminally_fails_sent_commands_and_forces_snapshot() {
    let mut settings = config(2);
    settings.completed_command_capacity = 2;
    let mut client = Client::new(settings).unwrap();
    connect_snapshot(&mut client, 4);

    let old = complete_cut(&mut client, "globally-reused-key".to_owned());
    complete_cut(&mut client, "evictor-a".to_owned());
    complete_cut(&mut client, "evictor-b".to_owned());
    assert!(client.command(&old.id).is_none());
    assert_eq!(client.retained_command_count(), 2);

    let collision = client
        .queue_command(
            CommandPayload::SelectPreview { input: input(1) },
            "globally-reused-key",
            Some(4),
            None,
        )
        .unwrap();
    let collateral = client
        .queue_command(CommandPayload::Cut, "still-unique", Some(4), None)
        .unwrap();
    assert!(matches!(client.pop_outbound(), Some(Outbound::Command(_))));
    assert!(matches!(client.pop_outbound(), Some(Outbound::Command(_))));
    assert_eq!(client.model().pending_commands().len(), 2);
    assert_eq!(
        client.model().view().unwrap().switcher.desired.preview,
        input(1).to_domain()
    );

    let affected = vec![collision.id.clone(), collateral.id.clone()];
    let error = client
        .reconcile_result(CommandResult::Accepted {
            id: old.id.clone(),
            revision: 4,
            scheduled_frame: None,
        })
        .unwrap_err();
    assert_eq!(
        error,
        ClientError::IdempotencyReplayCollision {
            received_command_id: old.id.clone(),
            affected_command_ids: affected.clone(),
        }
    );
    assert!(
        error
            .to_string()
            .contains("authoritative snapshot is required")
    );
    assert_eq!(
        client.state(),
        &ConnectionState::ResyncRequired {
            expected_revision: 5,
            received_revision: 4,
        }
    );
    assert_eq!(client.retained_command_count(), 2);
    assert_eq!(client.outbound_len(), 0);
    assert!(client.model().pending_commands().is_empty());
    assert_eq!(
        client.model().view().unwrap().switcher.desired.preview,
        input(2).to_domain()
    );
    for id in &affected {
        assert_eq!(
            client.command(id).unwrap().status,
            CommandStatus::TerminalUncertain(CommandUncertainty::IdempotencyReplayCollision {
                received_command_id: old.id.clone(),
            })
        );
    }

    client.start_connect().unwrap();
    assert_eq!(client.transport_connected().unwrap().resume_cursor, None);
    client.accept_handshake(handshake(4, None)).unwrap();
    client.apply_snapshot(snapshot(4)).unwrap();
    assert_eq!(client.state(), &ConnectionState::Ready);
    assert_eq!(client.retained_command_count(), 2);
    assert_eq!(
        client.retry_command(&collision.id),
        Err(ClientError::CommandTerminalUncertain(collision.id))
    );
}

#[test]
fn commands_have_monotonic_ids_and_results_reconcile_through_ui_model() {
    let mut client = ready_client(4);
    let first = client
        .queue_command(
            CommandPayload::SelectPreview { input: input(1) },
            "intent-a",
            Some(4),
            Some(500),
        )
        .unwrap();
    let second = client
        .queue_command(CommandPayload::Cut, "intent-b", None, None)
        .unwrap();
    assert_eq!(first.id, "diagnostic-a:1");
    assert_eq!(second.id, "diagnostic-a:2");
    assert_eq!(first.expected_revision, Some(4));
    assert_eq!(first.idempotency_key, "intent-a");
    assert_eq!(client.model().pending_commands().len(), 2);
    assert_eq!(
        client.model().view().unwrap().switcher.desired.preview,
        input(1).to_domain()
    );

    assert!(matches!(client.pop_outbound(), Some(Outbound::Command(_))));
    assert_eq!(
        client.command(&first.id).unwrap().status,
        CommandStatus::Sent
    );
    let result = CommandResult::Accepted {
        id: first.id.clone(),
        revision: 5,
        scheduled_frame: Some(9),
    };
    assert_eq!(
        client.reconcile_result(result.clone()).unwrap(),
        Intake::ResultReconciled
    );
    assert_eq!(
        client.command(&first.id).unwrap().status,
        CommandStatus::Completed(result.clone())
    );
    assert_eq!(
        client.reconcile_result(result).unwrap(),
        Intake::DuplicateResult
    );
}

#[test]
fn wipe_command_is_queued_with_its_exact_duration() {
    let mut client = ready_client(1);
    let command = client
        .queue_command(
            CommandPayload::Wipe {
                duration_frames: 45,
            },
            "wipe-45",
            Some(4),
            None,
        )
        .unwrap();

    assert_eq!(
        command.payload,
        CommandPayload::Wipe {
            duration_frames: 45,
        }
    );
    assert!(matches!(client.pop_outbound(), Some(Outbound::Command(queued)) if queued == command));
}

#[test]
fn new_client_does_not_send_wipe_after_negotiating_with_an_old_daemon() {
    let mut client = Client::new(config(1)).unwrap();
    client.start_connect().unwrap();
    client.transport_connected().unwrap();
    let mut old_handshake = handshake(4, None);
    old_handshake.negotiated = ProtocolVersion::new(1, 0);
    client.accept_handshake(old_handshake).unwrap();
    client.apply_snapshot(snapshot(4)).unwrap();

    assert_eq!(
        client.queue_command(
            CommandPayload::Wipe { duration_frames: 3 },
            "unsupported-wipe",
            Some(4),
            None,
        ),
        Err(ClientError::UnsupportedCommandVersion {
            negotiated: ProtocolVersion::new(1, 0),
            required: WIPE_PROTOCOL_VERSION,
        })
    );
    assert_eq!(client.outbound_len(), 0);
    assert!(client.command("diagnostic-a:1").is_none());

    let cut = client
        .queue_command(CommandPayload::Cut, "supported-cut", Some(4), None)
        .unwrap();
    assert_eq!(cut.id, "diagnostic-a:1");
    assert_eq!(cut.protocol, ProtocolVersion::new(1, 0));
}

#[test]
fn unresolved_manual_intent_blocks_fifo_after_reconnect_downgrade() {
    let mut client = ready_client(3);
    let manual = client
        .queue_command(
            CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::Fade,
            },
            "manual-start",
            Some(4),
            None,
        )
        .unwrap();
    assert!(matches!(
        client.pop_outbound(),
        Some(Outbound::Command(ref queued)) if queued == &manual
    ));
    client
        .queue_command(CommandPayload::Cut, "later-cut", Some(4), None)
        .unwrap();

    let _ = client.transport_disconnected();
    client.start_connect().unwrap();
    let cursor = client.transport_connected().unwrap().resume_cursor.unwrap();
    client
        .accept_handshake(handshake_version(WIPE_PROTOCOL_VERSION, 4, Some(cursor)))
        .unwrap();
    assert_eq!(client.state(), &ConnectionState::Ready);
    assert_eq!(
        manual.payload.minimum_protocol_version(),
        MANUAL_TRANSITION_PROTOCOL_VERSION
    );
    assert_eq!(
        client.command(&manual.id).unwrap().command.protocol,
        CURRENT_PROTOCOL_VERSION
    );
    assert_eq!(
        client.outbound_len(),
        1,
        "the later cut must remain queued behind the sent manual intent"
    );
    assert_eq!(
        client.pop_outbound(),
        None,
        "unsupported manual head must block the later cut"
    );
}

#[test]
fn event_gap_requests_snapshot_resync() {
    let mut client = ready_client(2);
    let error = client
        .apply_event(EventMessage {
            cursor: EventCursor {
                engine: engine(),
                revision: 6,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: Some(live_fade_to_black()),
            },
        })
        .unwrap_err();
    assert_eq!(
        error,
        ClientError::ResyncRequired {
            expected_revision: 5,
            received_revision: 6
        }
    );
    assert!(matches!(
        client.state(),
        ConnectionState::ResyncRequired { .. }
    ));
    client.start_connect().unwrap();
    assert_eq!(client.transport_connected().unwrap().resume_cursor, None);
}

#[test]
fn same_revision_runtime_realization_does_not_move_durable_cursor() {
    let mut client = ready_client(2);
    let cursor = client.last_applied_cursor().unwrap();

    assert_eq!(
        client
            .intake(WireMessage::RuntimeEvent(runtime_event(
                4,
                7,
                1,
                RuntimeLifecycleEvent::Realized {
                    domain: "switcher".to_owned(),
                    manual_transition: Some(ManualTransitionStatus::Inactive),
                    fade_to_black: Some(live_fade_to_black()),
                },
            )))
            .unwrap(),
        Intake::RuntimeEventObserved
    );

    assert_eq!(client.last_applied_cursor(), Some(cursor));
    assert_eq!(
        client
            .model()
            .state()
            .unwrap()
            .switcher()
            .runtime_generation,
        Some(7)
    );
}

#[test]
fn reconnecting_second_client_reduces_exact_desired_and_realized_manual_state() {
    let mut first = Client::new(config(4)).unwrap();
    first.start_connect().unwrap();
    first.transport_connected().unwrap();
    first.accept_handshake(handshake(4, None)).unwrap();
    let mut initial = snapshot(4);
    initial.desired_manual_transition = Some(active_manual(0, 6_250));
    initial.realized_manual_transition = Some(active_manual(6_250, 6_250));
    first.apply_snapshot(initial.clone()).unwrap();

    let mut second = Client::new(config(4)).unwrap();
    second.start_connect().unwrap();
    second.transport_connected().unwrap();
    second.accept_handshake(handshake(4, None)).unwrap();
    second.apply_snapshot(initial).unwrap();
    let switcher = second.model().state().unwrap().switcher();
    assert!(matches!(
        switcher.desired_manual_transition,
        ModelManualTransitionStatus::Active(state)
            if state.interval_start.basis_points() == 0
                && state.position.basis_points() == 6_250
    ));
    assert!(matches!(
        switcher.realized_manual_transition,
        ModelManualTransitionStatus::Active(state)
            if state.interval_start.basis_points() == 6_250
                && state.position.basis_points() == 6_250
    ));

    second
        .apply_event(EventMessage {
            cursor: EventCursor {
                engine: engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: Some(active_manual(0, 2_500)),
                fade_to_black: Some(live_fade_to_black()),
            },
        })
        .unwrap();
    second
        .apply_runtime_event(runtime_event(
            5,
            5,
            1,
            RuntimeLifecycleEvent::Realized {
                domain: "switcher".into(),
                manual_transition: Some(active_manual(2_500, 2_500)),
                fade_to_black: Some(live_fade_to_black()),
            },
        ))
        .unwrap();
    let switcher = second.model().state().unwrap().switcher();
    assert!(matches!(
        switcher.desired_manual_transition,
        ModelManualTransitionStatus::Active(state)
            if state.position.basis_points() == 2_500
    ));
    assert!(matches!(
        switcher.realized_manual_transition,
        ModelManualTransitionStatus::Active(state)
            if state.interval_start.basis_points() == 2_500
                && state.position.basis_points() == 2_500
    ));
}

#[test]
fn runtime_lifecycle_sequence_is_recorded_without_durable_reduction() {
    let mut client = ready_client(2);
    client
        .apply_runtime_event(runtime_event(4, 9, 1, RuntimeLifecycleEvent::Preparing))
        .unwrap();
    client
        .apply_runtime_event(runtime_event(
            4,
            9,
            2,
            RuntimeLifecycleEvent::Realized {
                domain: "audio".to_owned(),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: None,
            },
        ))
        .unwrap();

    assert_eq!(client.last_applied_cursor().unwrap().revision, 4);
    assert_eq!(
        client
            .model()
            .state()
            .unwrap()
            .switcher()
            .runtime_generation,
        None
    );
}

#[test]
fn runtime_event_rejects_wrong_identity_and_revision_ahead() {
    let mut client = ready_client(2);
    let mut wrong_identity = runtime_event(4, 1, 1, RuntimeLifecycleEvent::Accepted);
    wrong_identity.server.engine_id = "engine-b".to_owned();
    assert_eq!(
        client.apply_runtime_event(wrong_identity.clone()),
        Err(ClientError::RuntimeIdentityMismatch {
            expected: Box::new(server()),
            received: Box::new(wrong_identity.server),
        })
    );
    assert_eq!(
        client.apply_runtime_event(runtime_event(5, 1, 1, RuntimeLifecycleEvent::Accepted,)),
        Err(ClientError::RuntimeRevisionAhead {
            current_revision: 4,
            received_revision: 5,
        })
    );
    assert_eq!(client.state(), &ConnectionState::Ready);
    assert_eq!(client.last_applied_cursor().unwrap().revision, 4);
}

#[test]
fn runtime_sequence_gap_does_not_request_durable_resync() {
    let mut client = ready_client(2);
    client
        .apply_runtime_event(runtime_event(4, 3, 1, RuntimeLifecycleEvent::Accepted))
        .unwrap();
    assert_eq!(
        client.apply_runtime_event(runtime_event(4, 3, 3, RuntimeLifecycleEvent::Preparing,)),
        Err(ClientError::RuntimeSequenceGap {
            generation: 3,
            expected_sequence: 2,
            received_sequence: 3,
        })
    );
    assert_eq!(client.state(), &ConnectionState::Ready);
    assert_eq!(client.last_applied_cursor().unwrap().revision, 4);

    client
        .apply_event(EventMessage {
            cursor: EventCursor {
                engine: engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: Some(live_fade_to_black()),
            },
        })
        .unwrap();
    assert_eq!(client.last_applied_cursor().unwrap().revision, 5);
}

#[test]
fn runtime_sequences_are_per_generation_and_reset_on_disconnect() {
    let mut client = ready_client(2);
    client
        .apply_runtime_event(runtime_event(4, 1, 1, RuntimeLifecycleEvent::Accepted))
        .unwrap();
    client
        .apply_runtime_event(runtime_event(4, 2, 1, RuntimeLifecycleEvent::Accepted))
        .unwrap();

    let _ = client.transport_disconnected();
    client.start_connect().unwrap();
    let cursor = client.transport_connected().unwrap().resume_cursor.unwrap();
    client.accept_handshake(handshake(4, Some(cursor))).unwrap();
    client
        .apply_runtime_event(runtime_event(4, 1, 1, RuntimeLifecycleEvent::Accepted))
        .unwrap();
}

#[test]
fn heartbeat_reports_only_the_last_applied_cursor() {
    let mut client = ready_client(2);
    let heartbeat = client.queue_heartbeat(1_234).unwrap();
    assert_eq!(heartbeat.server, server());
    assert_eq!(heartbeat.sequence, 1);
    assert_eq!(heartbeat.sent_at_ms, 1_234);
    let cursor = heartbeat.last_applied.unwrap();
    assert_eq!(cursor.server, server());
    assert_eq!(cursor.revision, 4);
    assert!(matches!(
        client.pop_outbound(),
        Some(Outbound::Heartbeat(_))
    ));
}

#[test]
fn outbound_queue_rejects_overflow_without_consuming_an_id() {
    let mut client = ready_client(1);
    client.queue_heartbeat(0).unwrap();
    assert_eq!(
        client.queue_command(CommandPayload::Cut, "full", Some(4), None),
        Err(ClientError::QueueFull { capacity: 1 })
    );
    client.pop_outbound();
    let command = client
        .queue_command(CommandPayload::Cut, "accepted", Some(4), None)
        .unwrap();
    assert_eq!(command.id, "diagnostic-a:1");
    assert_eq!(client.outbound_len(), 1);
}
