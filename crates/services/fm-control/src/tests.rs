use std::{error::Error, fmt, num::NonZeroU128, sync::mpsc::TryRecvError};

use fm_auth::{Policy, Principal, Role, SessionId, UserId};
use fm_clock::ClockDomainId;
use fm_engine::{Engine, ShowState};
use fm_protocol::{
    CommandMessage, CommandPayload, CommandResult, EngineIdentity, EventCursor,
    ManualTransitionKind, ManualTransitionPosition, ProtocolVersion, RuntimeLifecycleEvent,
    ServerIdentity, WireInputId, WireMessage,
};
use fm_types::{FrameRate, InputId};

use super::*;

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn principal(role: Role) -> Principal {
    Principal::authenticated(
        UserId::new("user").unwrap(),
        SessionId::new("session").unwrap(),
        [role],
    )
}

fn service(retained_events: usize, subscriber_queue: usize) -> ControlService {
    let show = ShowState::new(
        "show",
        vec![input(1), input(2), input(3)],
        input(1),
        input(2),
    )
    .unwrap();
    let engine = Engine::new(
        show,
        FrameRate::new(60, 1).unwrap(),
        ClockDomainId::new(NonZeroU128::new(1).unwrap()),
    );
    ControlService::new(
        engine,
        Policy::production(),
        "engine-a",
        "log-a",
        ControlLimits {
            retained_events,
            max_subscribers: 4,
            subscriber_queue,
        },
    )
}

fn command(id: &str, key: &str, payload: CommandPayload) -> CommandMessage {
    CommandMessage {
        protocol: ProtocolVersion::new(1, 0),
        id: id.to_owned(),
        idempotency_key: key.to_owned(),
        expected_revision: None,
        deadline_ms: None,
        payload,
    }
}

fn prepared<A>(outcome: PrepareSubmitOutcome<'_, A>) -> PreparedSubmission<'_, A> {
    match outcome {
        PrepareSubmitOutcome::Prepared(prepared) => prepared,
        PrepareSubmitOutcome::Replayed(_) => panic!("submission should require a commit"),
    }
}

fn server_identity() -> ServerIdentity {
    ServerIdentity {
        engine_id: "engine-a".to_owned(),
        project_id: "project-a".to_owned(),
        state_epoch: 1,
        log_id: "log-a".to_owned(),
    }
}

#[test]
fn authorization_runs_before_detailed_command_validation() {
    let mut control = service(8, 8);
    let denied = control
        .submit(
            &principal(Role::Viewer),
            command(
                "denied",
                "denied-key",
                CommandPayload::Fade { duration_frames: 0 },
            ),
            0,
        )
        .unwrap();
    let CommandResult::Rejected { code, .. } = denied.output.result else {
        panic!("viewer command should be rejected");
    };
    assert_eq!(code, "permission_denied");

    let invalid = control
        .submit(
            &principal(Role::Operator),
            command(
                "invalid",
                "invalid-key",
                CommandPayload::Fade { duration_frames: 0 },
            ),
            0,
        )
        .unwrap();
    let CommandResult::Rejected { code, .. } = invalid.output.result else {
        panic!("zero fade should be rejected");
    };
    assert_eq!(code, "invalid_command");
}

#[test]
fn manual_transition_is_authorized_durable_reversible_and_replay_safe() {
    let mut control = service(16, 8);
    let denied = control
        .submit(
            &principal(Role::Viewer),
            command(
                "denied-manual",
                "denied-manual-key",
                CommandPayload::StartManualTransition {
                    kind: ManualTransitionKind::Fade,
                },
            ),
            0,
        )
        .unwrap();
    assert!(matches!(
        denied.output.result,
        CommandResult::Rejected { ref code, .. } if code == "permission_denied"
    ));
    assert_eq!(control.diagnostics().current_revision, 0);

    let commands = [
        (
            "manual-start",
            CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::Wipe,
            },
        ),
        (
            "manual-forward",
            CommandPayload::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(8_000).unwrap(),
            },
        ),
        (
            "manual-reverse",
            CommandPayload::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(2_500).unwrap(),
            },
        ),
    ];
    for (id, payload) in commands {
        let message = command(id, id, payload);
        let accepted = control
            .submit(&principal(Role::Operator), message.clone(), 0)
            .unwrap();
        assert!(matches!(
            accepted.output.result,
            CommandResult::Accepted { .. }
        ));
        control.tick(&server_identity()).unwrap();
        let replay = control
            .submit(&principal(Role::Operator), message, 0)
            .unwrap();
        assert!(replay.replayed);
        assert!(replay.output.events.is_empty());
    }
    assert_eq!(
        control
            .engine
            .realized_switcher()
            .t_bar()
            .unwrap()
            .position()
            .basis_points(),
        2_500
    );
    assert!(control.idle_engine_snapshot().is_ok());

    control
        .submit(
            &principal(Role::Operator),
            command(
                "manual-cancel",
                "manual-cancel",
                CommandPayload::CancelManualTransition,
            ),
            0,
        )
        .unwrap();
    control.tick(&server_identity()).unwrap();
    assert!(control.engine.realized_switcher().t_bar().is_none());
    assert_eq!(control.engine.realized_switcher().program(), input(1));
    assert_eq!(control.diagnostics().current_revision, 4);
}

#[test]
fn wipe_propagates_with_authorization_idempotency_resume_and_exact_endpoints() {
    let mut control = service(8, 8);
    let denied = control
        .submit(
            &principal(Role::Viewer),
            command(
                "denied-wipe",
                "denied-wipe-key",
                CommandPayload::Wipe { duration_frames: 3 },
            ),
            0,
        )
        .unwrap();
    assert!(matches!(
        denied.output.result,
        CommandResult::Rejected { ref code, .. } if code == "permission_denied"
    ));
    assert_eq!(control.diagnostics().current_revision, 0);

    let identity = control.identity().clone();
    let message = command(
        "wipe",
        "wipe-key",
        CommandPayload::Wipe { duration_frames: 3 },
    );
    let accepted = control
        .submit(&principal(Role::Operator), message.clone(), 0)
        .unwrap();
    assert_eq!(
        accepted.output.result,
        CommandResult::Accepted {
            id: "wipe".to_owned(),
            revision: 1,
            scheduled_frame: Some(0),
        }
    );
    assert_eq!(accepted.output.events.len(), 1);

    let duplicate = control
        .submit(&principal(Role::Operator), message, 0)
        .unwrap();
    assert!(duplicate.replayed);
    assert_eq!(duplicate.output.result, accepted.output.result);
    assert!(duplicate.output.events.is_empty());
    assert_eq!(control.diagnostics().current_revision, 1);

    let ResumeDecision::Events(events) = control.resume(&EventCursor {
        engine: identity,
        revision: 0,
    }) else {
        panic!("the accepted wipe should be resumable");
    };
    assert_eq!(events, accepted.output.events);

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let tick = control.tick(&server_identity()).unwrap();
        assert_eq!(
            tick.frame
                .program
                .transition_kind
                .map(|kind| format!("{kind:?}")),
            Some("Wipe".to_owned())
        );
        assert_eq!(
            (
                tick.frame.program.mix_start_numerator,
                tick.frame.program.mix_end_numerator,
            ),
            (start, end)
        );
        assert_eq!(tick.runtime_events.len(), usize::from(end == 3));
    }

    let endpoint = control.tick(&server_identity()).unwrap();
    assert_eq!(endpoint.frame.program.primary, input(2));
    assert_eq!(endpoint.frame.program.secondary, None);
    assert_eq!(endpoint.frame.program.transition_kind, None);
    assert_eq!(control.diagnostics().current_revision, 1);
}

#[test]
fn accepted_preparation_is_isolated_until_commit_and_projects_exactly() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    let before_diagnostics = control.diagnostics();
    let before_snapshot = control.snapshot().clone();
    let before_engine = control.engine.snapshot().unwrap();
    let message = command("prepared-cut", "prepared-cut-key", CommandPayload::Cut);

    let first = prepared(
        control
            .prepare_submit(&principal(Role::Operator), message.clone(), 0)
            .unwrap(),
    );
    assert!(first.submission().is_accepted());
    assert_eq!(first.idempotency_key().as_str(), "prepared-cut-key");
    assert_eq!(first.output().events.len(), 1);
    let first_output = first.output().clone();
    let projected = first.project(1).unwrap();
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
    first.abort();

    assert_eq!(control.diagnostics(), before_diagnostics);
    assert_eq!(control.snapshot(), &before_snapshot);
    assert_eq!(control.engine.snapshot().unwrap(), before_engine);
    assert!(control.log.is_empty());
    assert!(control.pending_runtime_actions.is_empty());
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));

    let second = prepared(
        control
            .prepare_submit(&principal(Role::Operator), message.clone(), 0)
            .unwrap(),
    );
    let prospective = second.submission().clone();
    assert_eq!(second.project(1).unwrap(), projected);
    let committed = second.commit().unwrap();

    assert_eq!(committed.output, first_output);
    assert_eq!(committed, prospective);
    assert_eq!(control.diagnostics().current_revision, 1);
    assert_eq!(control.log.len(), 1);
    assert_eq!(control.pending_runtime_actions.len(), 1);
    assert_eq!(control.snapshot().snapshot.revision, 1);
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Durable(ref event) if event == &committed.output.events[0]
    ));

    control.tick(&server_identity()).unwrap();
    assert_eq!(control.engine.snapshot().unwrap(), projected);

    let replay = control
        .prepare_submit(&principal(Role::Operator), message, 0)
        .unwrap();
    let PrepareSubmitOutcome::Replayed(replay) = replay else {
        panic!("durable duplicate must not create a prepared guard");
    };
    assert!(replay.replayed);
    assert!(replay.accepted.is_none());
    assert!(replay.output.events.is_empty());
    assert_eq!(replay.output.result, committed.output.result);
}

#[test]
fn rejected_preparation_and_drop_have_no_effects_but_commit_installs_receipt() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    let before_diagnostics = control.diagnostics();
    let before_snapshot = control.snapshot().clone();
    let before_engine = control.engine.snapshot().unwrap();
    let message = command(
        "prepared-rejection",
        "prepared-rejection-key",
        CommandPayload::SelectPreview {
            input: WireInputId::from_domain(input(99)),
        },
    );

    let rejected = prepared(
        control
            .prepare_submit(&principal(Role::Operator), message.clone(), 0)
            .unwrap(),
    );
    assert!(matches!(
        rejected.output().result,
        CommandResult::Rejected { ref code, .. } if code == "not_found"
    ));
    assert!(rejected.output().events.is_empty());
    assert_eq!(rejected.project(0).unwrap().receipts().len(), 1);
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
    drop(rejected);

    assert_eq!(control.diagnostics(), before_diagnostics);
    assert_eq!(control.snapshot(), &before_snapshot);
    assert_eq!(control.engine.snapshot().unwrap(), before_engine);
    assert!(control.log.is_empty());

    let rejected = prepared(
        control
            .prepare_submit(&principal(Role::Operator), message.clone(), 0)
            .unwrap(),
    );
    let prospective = rejected.submission().clone();
    let projected = rejected.project(0).unwrap();
    let committed = rejected.commit().unwrap();
    assert_eq!(committed, prospective);
    assert_eq!(control.engine.snapshot().unwrap(), projected);
    assert_eq!(control.diagnostics(), before_diagnostics);
    assert_eq!(control.snapshot(), &before_snapshot);
    assert!(control.log.is_empty());
    assert!(control.pending_runtime_actions.is_empty());
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));

    assert!(matches!(
        control
            .prepare_submit(&principal(Role::Operator), message, 0)
            .unwrap(),
        PrepareSubmitOutcome::Replayed(_)
    ));
}

#[test]
fn authorization_denial_is_staged_and_cached_only_by_commit() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    let before_diagnostics = control.diagnostics();
    let before_snapshot = control.snapshot().clone();
    let before_engine = control.engine.snapshot().unwrap();
    let message = command(
        "prepared-denial",
        "prepared-denial-key",
        CommandPayload::Cut,
    );
    let key = IdempotencyKey::new("prepared-denial-key");

    let denied = prepared(
        control
            .prepare_submit(&principal(Role::Viewer), message.clone(), 0)
            .unwrap(),
    );
    assert!(matches!(
        denied.output().result,
        CommandResult::Rejected { ref code, .. } if code == "permission_denied"
    ));
    assert_eq!(denied.project(0).unwrap(), before_engine);
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
    denied.abort();

    assert!(!control.authorization_denials.contains_key(&key));
    assert_eq!(control.diagnostics(), before_diagnostics);
    assert_eq!(control.snapshot(), &before_snapshot);
    assert_eq!(control.engine.snapshot().unwrap(), before_engine);

    let denied = prepared(
        control
            .prepare_submit(&principal(Role::Viewer), message, 0)
            .unwrap(),
    );
    let prospective = denied.submission().clone();
    let committed = denied.commit().unwrap();
    assert_eq!(committed, prospective);
    assert_eq!(
        control.authorization_denials.get(&key),
        Some(&committed.output.result)
    );
    assert_eq!(control.diagnostics(), before_diagnostics);
    assert_eq!(control.snapshot(), &before_snapshot);
    assert_eq!(control.engine.snapshot().unwrap(), before_engine);
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));

    let replayed = control
        .prepare_submit(
            &principal(Role::Operator),
            command("different", "prepared-denial-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    let PrepareSubmitOutcome::Replayed(replayed) = replayed else {
        panic!("committed denial must replay without a guard");
    };
    assert!(replayed.replayed);
    assert_eq!(replayed.output.result, committed.output.result);
}

#[test]
fn authorization_denial_replays_after_role_and_command_change_without_engine_mutation() {
    let mut control = service(8, 8);
    let before = control.snapshot().clone();
    let denied = control
        .submit(
            &principal(Role::Viewer),
            command(
                "denied",
                "denied-key",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(99)),
                },
            ),
            0,
        )
        .unwrap();

    let replayed = control
        .submit(
            &principal(Role::Operator),
            command("different-id", "denied-key", CommandPayload::Cut),
            0,
        )
        .unwrap();

    assert_eq!(replayed.output.result, denied.output.result);
    assert!(replayed.replayed);
    assert!(replayed.output.events.is_empty());
    assert!(replayed.accepted.is_none());
    assert_eq!(control.snapshot(), &before);
    assert_eq!(control.diagnostics().current_revision, 0);
}

#[test]
fn authorization_denial_cache_uses_the_retained_event_bound() {
    let mut control = service(2, 8);
    for key in ["one", "two", "three"] {
        control
            .submit(
                &principal(Role::Viewer),
                command(key, key, CommandPayload::Cut),
                0,
            )
            .unwrap();
    }

    assert_eq!(control.authorization_denials.len(), 2);
    assert!(
        !control
            .authorization_denials
            .contains_key(&IdempotencyKey::new("one"))
    );
}

#[test]
fn engine_controls_accepted_rejected_and_duplicate_behavior() {
    let mut control = service(8, 8);
    let operator = principal(Role::Operator);
    let accepted = control
        .submit(
            &operator,
            command(
                "select",
                "select-key",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(3)),
                },
            ),
            0,
        )
        .unwrap();
    assert!(accepted.is_accepted());
    assert!(!accepted.replayed);
    assert!(accepted.accepted.is_some());
    assert_eq!(accepted.output.events.len(), 1);

    let mut duplicate_message = command("other-id", "select-key", CommandPayload::Cut);
    duplicate_message.expected_revision = Some(99);
    let duplicate = control.submit(&operator, duplicate_message, 0).unwrap();
    assert!(duplicate.is_accepted());
    assert!(duplicate.replayed);
    assert!(duplicate.accepted.is_none());
    assert!(duplicate.output.events.is_empty());
    assert_eq!(duplicate.output.result, accepted.output.result);

    let rejected = control
        .submit(
            &operator,
            command(
                "unknown",
                "unknown-key",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(99)),
                },
            ),
            0,
        )
        .unwrap();
    let CommandResult::Rejected {
        code,
        current_revision,
        ..
    } = rejected.output.result
    else {
        panic!("unknown input should be rejected");
    };
    assert_eq!(code, "not_found");
    assert_eq!(current_revision, 1);
    assert_eq!(control.diagnostics().current_revision, 1);
}

#[test]
fn restored_engine_rejection_receipts_still_replay_after_authorization() {
    let show = ShowState::new(
        "show",
        vec![input(1), input(2), input(3)],
        input(1),
        input(2),
    )
    .unwrap();
    let mut engine = Engine::new(
        show,
        FrameRate::new(60, 1).unwrap(),
        ClockDomainId::new(NonZeroU128::new(1).unwrap()),
    );
    let mut original_message = command("original", "restored-key", CommandPayload::Cut);
    original_message.expected_revision = Some(1);
    let original = engine
        .execute(original_message.domain_envelope(EngineCommand::Cut), 0)
        .unwrap();
    let restored = Engine::restore(engine.snapshot().unwrap()).unwrap();
    let mut control = ControlService::new(
        restored,
        Policy::production(),
        "engine-a",
        "log-a",
        ControlLimits::default(),
    );

    let replayed = control
        .submit(
            &principal(Role::Operator),
            command(
                "different",
                "restored-key",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(99)),
                },
            ),
            0,
        )
        .unwrap();

    assert!(replayed.replayed);
    assert_eq!(replayed.output.result, command_result(&original.receipt));
    assert_eq!(control.diagnostics().current_revision, 0);
}

#[test]
fn command_result_is_represented_before_its_events() {
    let mut control = service(8, 8);
    let submission = control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    let messages = submission.output.into_wire_messages();
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], WireMessage::CommandResult(_)));
    let WireMessage::Event(event) = &messages[1] else {
        panic!("the durable event should follow the result");
    };
    assert_eq!(event.cursor.revision, 1);
    assert_eq!(
        event.payload,
        EventPayload::DesiredSwitcher {
            program: WireInputId::from_domain(input(2)),
            preview: WireInputId::from_domain(input(1)),
        }
    );
}

#[test]
fn resume_returns_all_contiguous_retained_revisions() {
    let mut control = service(8, 8);
    let operator = principal(Role::Operator);
    let identity = control.identity().clone();
    control
        .submit(
            &operator,
            command(
                "select",
                "key-1",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(3)),
                },
            ),
            0,
        )
        .unwrap();
    control
        .submit(&operator, command("cut", "key-2", CommandPayload::Cut), 0)
        .unwrap();

    let ResumeDecision::Events(events) = control.resume(&EventCursor {
        engine: identity,
        revision: 0,
    }) else {
        panic!("retained cursor should resume");
    };
    assert_eq!(
        events
            .iter()
            .map(|event| event.cursor.revision)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn wrong_identity_and_compacted_cursor_fall_back_to_snapshot() {
    let mut control = service(2, 8);
    let operator = principal(Role::Operator);
    let identity = control.identity().clone();
    for (id, key, target) in [
        ("one", "key-1", 3),
        ("two", "key-2", 2),
        ("three", "key-3", 3),
    ] {
        control
            .submit(
                &operator,
                command(
                    id,
                    key,
                    CommandPayload::SelectPreview {
                        input: WireInputId::from_domain(input(target)),
                    },
                ),
                0,
            )
            .unwrap();
    }

    assert!(matches!(
        control.resume(&EventCursor {
            engine: identity.clone(),
            revision: 0,
        }),
        ResumeDecision::Snapshot(_)
    ));
    let wrong_identity = EngineIdentity {
        log_id: "other-log".to_owned(),
        ..identity
    };
    assert!(matches!(
        control.resume(&EventCursor {
            engine: wrong_identity,
            revision: 2,
        }),
        ResumeDecision::Snapshot(_)
    ));
}

#[test]
fn retained_log_never_exceeds_its_bound() {
    let mut control = service(2, 8);
    let operator = principal(Role::Operator);
    for (id, target) in [("one", 3), ("two", 2), ("three", 3)] {
        control
            .submit(
                &operator,
                command(
                    id,
                    id,
                    CommandPayload::SelectPreview {
                        input: WireInputId::from_domain(input(target)),
                    },
                ),
                0,
            )
            .unwrap();
    }
    let diagnostics = control.diagnostics();
    assert_eq!(diagnostics.oldest_retained_revision, Some(2));
    assert_eq!(diagnostics.newest_retained_revision, Some(3));
}

#[test]
fn slow_subscriber_is_failed_and_removed() {
    let mut control = service(8, 1);
    let subscription = control.subscribe().unwrap();
    let operator = principal(Role::Operator);
    control
        .submit(
            &operator,
            command(
                "one",
                "key-1",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(3)),
                },
            ),
            0,
        )
        .unwrap();
    let second = control
        .submit(
            &operator,
            command(
                "two",
                "key-2",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(2)),
                },
            ),
            0,
        )
        .unwrap();

    assert_eq!(
        second.subscriber_failures,
        [SubscriberFailure {
            subscriber: subscription.id(),
            reason: SubscriberFailureReason::SlowClient,
        }]
    );
    assert_eq!(
        subscription.failure(),
        Some(SubscriberFailureReason::SlowClient)
    );
    assert_eq!(control.diagnostics().subscriber_count, 0);
}

#[test]
fn subscription_distinguishes_durable_and_runtime_events() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Durable(EventMessage { .. })
    ));

    control.tick(&server_identity()).unwrap();
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Runtime(RuntimeEventMessage { .. })
    ));
}

#[test]
fn ticks_do_not_change_durable_resume_history() {
    let mut control = service(8, 8);
    let identity = control.identity().clone();
    control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    control.tick(&server_identity()).unwrap();
    control.tick(&server_identity()).unwrap();

    let ResumeDecision::Events(events) = control.resume(&EventCursor {
        engine: identity,
        revision: 0,
    }) else {
        panic!("ticks must not create a durable resume gap");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cursor.revision, 1);
    assert_eq!(control.diagnostics().current_revision, 1);
}

#[test]
fn runtime_ordering_is_independent_per_generation() {
    let mut control = service(8, 8);
    let operator = principal(Role::Operator);
    let server = server_identity();
    control
        .submit(&operator, command("cut", "cut-key", CommandPayload::Cut), 0)
        .unwrap();
    let first = control.tick(&server).unwrap();
    control
        .submit(
            &operator,
            command(
                "preview",
                "preview-key",
                CommandPayload::SelectPreview {
                    input: WireInputId::from_domain(input(3)),
                },
            ),
            0,
        )
        .unwrap();
    let second = control.tick(&server).unwrap();

    assert_eq!(first.runtime_events[0].revision, 1);
    assert_eq!(first.runtime_events[0].generation, 1);
    assert_eq!(first.runtime_events[0].sequence, 1);
    assert_eq!(second.runtime_events[0].revision, 2);
    assert_eq!(second.runtime_events[0].generation, 2);
    assert_eq!(second.runtime_events[0].sequence, 1);
    assert_eq!(first.runtime_events[0].server, server);
    assert!(matches!(
        second.runtime_events[0].event,
        RuntimeLifecycleEvent::Realized { ref domain } if domain == "switcher"
    ));
}

#[test]
fn tick_realizes_the_command_on_its_exact_scheduled_frame() {
    let mut control = service(8, 8);
    assert_eq!(
        control.next_frame_deadline().unwrap(),
        fm_clock::ClockTime::ZERO
    );
    let submission = control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    let CommandResult::Accepted {
        scheduled_frame, ..
    } = submission.output.result
    else {
        panic!("cut should be accepted");
    };

    let tick = control.tick(&server_identity()).unwrap();
    assert_eq!(scheduled_frame, Some(tick.frame.frame.get()));
    assert_eq!(tick.frame.frame.get(), 0);
    assert_eq!(tick.frame.program.primary, input(2));
    assert_eq!(tick.frame.runtime_generation.get(), 1);
    assert_eq!(
        control.next_frame_deadline().unwrap(),
        fm_clock::ClockTime::from_nanos(16_666_666)
    );
    assert_eq!(tick.runtime_events.len(), 1);
    assert_eq!(
        control.snapshot().snapshot.realized_program.to_domain(),
        input(2)
    );
}

#[test]
fn shutdown_ticks_settle_fade_without_claiming_runtime_realization() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    control
        .submit(
            &principal(Role::Operator),
            command(
                "fade",
                "fade-key",
                CommandPayload::Fade { duration_frames: 3 },
            ),
            0,
        )
        .unwrap();
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Durable(_)
    ));

    for _ in 0..3 {
        let outcome = control.tick_for_shutdown(&server_identity()).unwrap();
        assert!(outcome.runtime_events.is_empty());
        assert!(outcome.subscriber_failures.is_empty());
        assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
    }

    assert!(control.idle_engine_snapshot().is_ok());
    assert_eq!(control.runtime_sequence, 0);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RealizerFailure;

impl fmt::Display for RealizerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("render output failed")
    }
}

impl Error for RealizerFailure {}

#[test]
fn successful_realizer_observes_exact_frame_before_publication() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Durable(_)
    ));

    let mut projected = control.engine.clone();
    let expected = projected.tick().unwrap();
    let outcome = control
        .tick_with_realizer(&server_identity(), |frame| {
            assert_eq!(frame, &expected);
            assert_eq!(frame.program, expected.program);
            assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
            Ok::<(), RealizerFailure>(())
        })
        .unwrap();

    assert_eq!(outcome.frame, expected);
    assert_eq!(outcome.runtime_events.len(), 1);
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Runtime(ref event) if event == &outcome.runtime_events[0]
    ));
}

#[test]
fn failed_realizer_is_fatal_without_false_publication_or_control_updates() {
    let mut control = service(8, 8);
    let subscription = control.subscribe().unwrap();
    control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    assert!(matches!(
        subscription.try_recv().unwrap(),
        LiveEvent::Durable(_)
    ));

    let before_snapshot = control.snapshot().clone();
    let before_pending = control.pending_runtime_actions.clone();
    let before_active_transition = control.active_transition;
    let before_runtime_generation = control.runtime_sequence_generation;
    let before_runtime_sequence = control.runtime_sequence;
    let mut projected = control.engine.clone();
    let expected = projected.tick().unwrap();

    let error = control
        .tick_with_realizer(&server_identity(), |frame| {
            assert_eq!(frame, &expected);
            Err(RealizerFailure)
        })
        .unwrap_err();

    assert_eq!(error.tick_error(), None);
    assert_eq!(error.realization_error(), Some(&RealizerFailure));
    assert_eq!(
        error.to_string(),
        "frame realization failed: render output failed"
    );
    assert!(error.source().is_some());
    assert_eq!(error.into_realization_error(), Some(RealizerFailure));
    assert_eq!(
        control.engine.runtime_generation(),
        expected.runtime_generation
    );
    assert_eq!(control.snapshot(), &before_snapshot);
    assert_eq!(control.pending_runtime_actions, before_pending);
    assert_eq!(control.active_transition, before_active_transition);
    assert_eq!(
        control.runtime_sequence_generation,
        before_runtime_generation
    );
    assert_eq!(control.runtime_sequence, before_runtime_sequence);
    assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(
        control.idle_engine_snapshot(),
        Err(SnapshotError::WorkInFlight)
    );
}

#[test]
fn idle_engine_snapshot_preserves_engine_idle_checks() {
    let mut control = service(8, 8);
    assert_eq!(
        control.idle_engine_snapshot().unwrap(),
        control.engine.snapshot().unwrap()
    );

    control
        .submit(
            &principal(Role::Operator),
            command("cut", "cut-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
    assert_eq!(
        control.idle_engine_snapshot(),
        Err(SnapshotError::WorkInFlight)
    );
}
