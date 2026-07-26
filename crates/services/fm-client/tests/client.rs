use core::num::NonZeroU128;

use fm_client::{
    Client, ClientConfig, ClientError, CommandStatus, ConnectionState, Intake, Outbound, SyncMode,
};
use fm_protocol::{
    CapabilityReportSummary, ClientType, CommandPayload, CommandResult, EngineIdentity,
    EventCursor, EventMessage, EventPayload, HandshakeOutcome, HandshakeResponse, ProtocolVersion,
    ResumeCursor, Role, RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity,
    SnapshotMessage, SnapshotReason, WireInputId, WireMessage,
};
use fm_types::ProjectId;

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
        vec![ProtocolVersion::new(1, 2)],
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
    HandshakeResponse {
        negotiated: ProtocolVersion::new(1, 1),
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
