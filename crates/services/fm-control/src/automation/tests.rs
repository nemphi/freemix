use std::num::NonZeroU128;

use fm_auth::{Policy, Role, SessionId, UserId};
use fm_automation::{
    AutomationEvent, Chord, CommandIntent, ConditionContext, EventFilter, KeyStroke, Modifiers,
    ScheduleKind, Shortcut, ShortcutScope, Trigger,
};
use fm_clock::ClockDomainId;
use fm_engine::{Engine, ShowState};
use fm_protocol::{CommandResult, WireInputId};
use fm_types::{FrameRate, InputId};

use super::*;
use crate::ControlLimits;

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn service() -> ControlService {
    let show = ShowState::new(
        "show",
        vec![
            (input(1), "One".to_owned()),
            (input(2), "Two".to_owned()),
            (input(3), "Three".to_owned()),
        ],
        input(1),
        input(2),
    )
    .unwrap();
    ControlService::new(
        Engine::new(
            show,
            FrameRate::new(60, 1).unwrap(),
            ClockDomainId::new(NonZeroU128::new(1).unwrap()),
        ),
        Policy::production(),
        "engine-a",
        "log-a",
        ControlLimits::default(),
    )
}

fn principal(role: Role) -> Principal {
    Principal::authenticated(
        UserId::new("user").unwrap(),
        SessionId::new("session").unwrap(),
        [role],
    )
}

fn select_preview(value: u128) -> CommandPayload {
    CommandPayload::SelectPreview {
        input: WireInputId::from_domain(input(value)),
    }
}

fn request(value: u128) -> CommandIntent<AutomationRequest> {
    CommandIntent::discrete(AutomationRequest::new(select_preview(value)))
}

fn chord(key: &str) -> Chord {
    Chord::new([KeyStroke::new(key, Modifiers::default())]).unwrap()
}

fn rejection_code(result: &CommandResult) -> Option<&str> {
    match result {
        CommandResult::Accepted { .. } => None,
        CommandResult::Rejected { code, .. } => Some(code.as_str()),
    }
}

fn sources(tick: &AutomationTick) -> Vec<String> {
    tick.submitted
        .iter()
        .map(|entry| entry.source.to_string())
        .collect()
}

fn beat_trigger(id: &str, value: u128) -> Trigger<AutomationRequest> {
    Trigger {
        id: id.to_owned(),
        filter: EventFilter::new("beat"),
        delay_ms: 0,
        conditions: Vec::new(),
        intents: vec![request(value)],
    }
}

/// An automation-produced command is authorized exactly like an operator
/// command, using the authority of the principal that requested it.
#[test]
fn automation_commands_are_authorized_as_their_requesting_principal() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits::default());
    let operator = principal(Role::Operator);
    let viewer = principal(Role::Viewer);

    plane
        .schedule(&operator, "operator", 100, ScheduleKind::Once, request(3))
        .unwrap();
    plane
        .schedule(&viewer, "viewer", 100, ScheduleKind::Once, request(1))
        .unwrap();

    let tick = plane
        .tick(&mut control, 100, &ConditionContext::new())
        .unwrap();
    assert_eq!(sources(&tick), ["schedule/operator", "schedule/viewer"]);
    assert!(tick.refusals.is_empty());

    let operator_result = &tick.submitted[0].submission.output.result;
    assert!(matches!(operator_result, CommandResult::Accepted { .. }));
    assert_eq!(
        tick.submitted[0].idempotency_key.as_str(),
        "auto/schedule/operator@100#1"
    );
    assert_eq!(
        control.snapshot().snapshot.desired_preview.to_domain(),
        input(3),
        "the operator-armed schedule reached the engine"
    );

    let denied = &tick.submitted[1].submission.output.result;
    assert_eq!(rejection_code(denied), Some("permission_denied"));
    assert_eq!(
        control.snapshot().snapshot.desired_preview.to_domain(),
        input(3),
        "the viewer-armed schedule changed nothing"
    );

    // The identical payload submitted directly by the same viewer is refused
    // with the identical code: automation shares the operator command path.
    let direct = control
        .submit(
            &viewer,
            CommandMessage {
                protocol: CURRENT_PROTOCOL_VERSION,
                id: "direct".to_owned(),
                idempotency_key: "direct".to_owned(),
                expected_revision: None,
                deadline_ms: None,
                payload: select_preview(1),
            },
            100,
        )
        .unwrap();
    assert_eq!(
        rejection_code(&direct.output.result),
        Some("permission_denied")
    );

    // A press is authorized against the principal that pressed it, never
    // against the principal that configured the binding.
    plane
        .insert_shortcut(Shortcut {
            id: "take-three".to_owned(),
            scope: ShortcutScope::Global,
            chord: chord("T"),
            intent: request(1),
        })
        .unwrap();
    let pressed = plane
        .press(&mut control, &viewer, None, &chord("T"), 200)
        .unwrap();
    assert_eq!(
        rejection_code(&pressed.submission.output.result),
        Some("permission_denied")
    );
    let pressed = plane
        .press(&mut control, &operator, None, &chord("T"), 200)
        .unwrap();
    assert!(matches!(
        pressed.submission.output.result,
        CommandResult::Accepted { .. }
    ));
    assert_eq!(
        control.snapshot().snapshot.desired_preview.to_domain(),
        input(1)
    );
}

/// Every bound refuses observably: nothing is queued without a limit and
/// nothing is dropped without being reported.
#[test]
fn bounds_refuse_a_flood_instead_of_queueing_or_dropping_it() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits {
        max_triggers: 1,
        max_pending_trigger_actions: 1,
        max_commands_per_tick: 4,
        ..AutomationLimits::default()
    });
    let operator = principal(Role::Operator);

    let flood = Trigger {
        intents: (0..12).map(|_| request(3)).collect(),
        ..beat_trigger("flood", 3)
    };
    plane.insert_trigger(&operator, flood).unwrap();
    assert_eq!(
        plane.insert_trigger(&operator, beat_trigger("second", 1)),
        Err(AutomationError::LimitReached {
            resource: AutomationResource::Triggers,
            limit: 1,
        })
    );

    plane
        .ingest_event(&AutomationEvent::new("beat", 10))
        .unwrap();
    assert_eq!(plane.pending_trigger_action_len(), 1);
    assert_eq!(
        plane.ingest_event(&AutomationEvent::new("beat", 10)),
        Err(AutomationError::LimitReached {
            resource: AutomationResource::PendingTriggerActions,
            limit: 1,
        })
    );

    let tick = plane
        .tick(&mut control, 10, &ConditionContext::new())
        .unwrap();
    assert_eq!(tick.submitted.len(), 4);
    assert_eq!(tick.refusals.len(), 8);
    assert!(tick.refusals.iter().all(|refusal| refusal.limit == 4
        && refusal.source
            == AutomationSource::Trigger {
                id: "flood".to_owned()
            }));
    assert_eq!(
        plane.pending_trigger_action_len(),
        0,
        "the refused commands are not requeued"
    );

    let settled = plane
        .tick(&mut control, 11, &ConditionContext::new())
        .unwrap();
    assert!(settled.submitted.is_empty());
    assert!(settled.refusals.is_empty());
}

/// Bindings that match one input emit in a defined, stable order rather than
/// in map iteration order.
#[test]
fn bindings_matching_one_input_emit_in_a_defined_order() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits::default());
    let operator = principal(Role::Operator);

    plane
        .insert_trigger(&operator, beat_trigger("alpha", 3))
        .unwrap();
    plane
        .insert_trigger(&operator, beat_trigger("beta", 1))
        .unwrap();
    plane
        .schedule(&operator, "backdrop", 50, ScheduleKind::Once, request(2))
        .unwrap();
    plane
        .ingest_event(&AutomationEvent::new("beat", 50))
        .unwrap();

    let tick = plane
        .tick(&mut control, 50, &ConditionContext::new())
        .unwrap();
    assert_eq!(
        sources(&tick),
        ["schedule/backdrop", "trigger/alpha", "trigger/beta"],
        "schedules precede triggers, and triggers keep registration order"
    );
    assert_eq!(
        control.snapshot().snapshot.desired_preview.to_domain(),
        input(1),
        "the last binding in the defined order owns the resulting state"
    );
    assert!(tick.submitted.iter().all(|entry| matches!(
        entry.submission.output.result,
        CommandResult::Accepted { .. }
    )));
}

/// A pressed chord resolves to exactly one binding, and ambiguity is refused
/// at registration instead of being resolved at press time.
#[test]
fn shortcut_resolution_prefers_local_scope_and_refuses_ambiguity() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits::default());
    let operator = principal(Role::Operator);

    plane
        .insert_shortcut(Shortcut {
            id: "global-g".to_owned(),
            scope: ShortcutScope::Global,
            chord: chord("G"),
            intent: request(3),
        })
        .unwrap();
    assert!(matches!(
        plane.insert_shortcut(Shortcut {
            id: "local-g".to_owned(),
            scope: ShortcutScope::Local("audio".to_owned()),
            chord: chord("G"),
            intent: request(1),
        }),
        Err(AutomationError::Shortcut(_))
    ));

    for scope in ["switcher", "audio"] {
        plane
            .insert_shortcut(Shortcut {
                id: format!("{scope}-a"),
                scope: ShortcutScope::Local(scope.to_owned()),
                chord: chord("A"),
                intent: request(3),
            })
            .unwrap();
    }

    let pressed = plane
        .press(&mut control, &operator, Some("audio"), &chord("A"), 1)
        .unwrap();
    assert_eq!(
        pressed.source,
        AutomationSource::Shortcut {
            id: "audio-a".to_owned()
        },
        "the local binding for the pressed scope wins regardless of insert order"
    );

    let pressed = plane
        .press(&mut control, &operator, Some("audio"), &chord("G"), 2)
        .unwrap();
    assert_eq!(
        pressed.source,
        AutomationSource::Shortcut {
            id: "global-g".to_owned()
        },
        "a scope without a local binding falls back to the global one"
    );

    assert_eq!(
        plane.press(&mut control, &operator, None, &chord("A"), 3),
        Err(AutomationError::UnboundChord)
    );
}
