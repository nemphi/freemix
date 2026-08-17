use std::{cell::RefCell, collections::BTreeMap, num::NonZeroU128, rc::Rc};

use fm_auth::{Policy, Role, SessionId, UserId};
use fm_automation::{
    AutomationEvent, Chord, CommandIntent, ConditionContext, EventFilter, GoAction, KeyStroke,
    Modifiers, ProgrammedGo, ScheduleId, ScheduleKind, Shortcut, ShortcutScope, Trigger,
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

const fn role_name(role: Role) -> &'static str {
    match role {
        Role::Viewer => "viewer",
        Role::Graphics => "graphics",
        Role::Audio => "audio",
        Role::Operator => "operator",
        Role::Admin => "admin",
    }
}

/// One person per role: two roles are two people, because one user and session
/// cannot simultaneously hold two different role sets.
fn principal(role: Role) -> Principal {
    Principal::authenticated(
        UserId::new(role_name(role)).unwrap(),
        SessionId::new("session").unwrap(),
        [role],
    )
}

/// The same person and session, holding a different role.
fn regraded(principal: &Principal, role: Role) -> Principal {
    Principal::authenticated(
        principal.user_id().clone(),
        principal.session_id().clone(),
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

fn fader(stream: &str, value: u128) -> CommandIntent<AutomationRequest> {
    CommandIntent::continuous(stream, AutomationRequest::new(select_preview(value)))
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

fn keys(tick: &AutomationTick) -> Vec<String> {
    tick.submitted
        .iter()
        .map(|entry| entry.idempotency_key.as_str().to_owned())
        .collect()
}

fn preview(control: &ControlService) -> InputId {
    control.snapshot().snapshot.desired_preview.to_domain()
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

/// The session store the [`AuthorityResolver`] seam exists for, standing in for
/// one this repository does not have yet. The test holds it alongside the plane
/// so a session can end mid-show, exactly as a real one would.
#[derive(Clone, Default)]
struct Sessions(Rc<RefCell<BTreeMap<AutomationIdentity, Principal>>>);

impl Sessions {
    fn set(&self, principal: &Principal) {
        self.0
            .borrow_mut()
            .insert(AutomationIdentity::of(principal), principal.clone());
    }

    fn end(&self, identity: &AutomationIdentity) {
        self.0.borrow_mut().remove(identity);
    }
}

impl AuthorityResolver for Sessions {
    fn resolve(&self, identity: &AutomationIdentity) -> Option<Principal> {
        self.0.borrow().get(identity).cloned()
    }

    fn observe(&mut self, principal: &Principal) {
        self.set(principal);
    }

    fn forget(&mut self, identity: &AutomationIdentity) {
        self.end(identity);
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

    let tick = plane.tick(&mut control, 100, &ConditionContext::new());
    assert_eq!(sources(&tick), ["schedule/operator", "schedule/viewer"]);
    assert!(tick.refusals.is_empty());
    assert!(tick.error.is_none());

    let operator_result = &tick.submitted[0].submission.output.result;
    assert!(matches!(operator_result, CommandResult::Accepted { .. }));
    assert_eq!(
        preview(&control),
        input(3),
        "the operator-armed schedule reached the engine"
    );

    let denied = &tick.submitted[1].submission.output.result;
    assert_eq!(rejection_code(denied), Some("permission_denied"));
    assert_eq!(
        preview(&control),
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
    assert_eq!(preview(&control), input(1));
}

/// An emitted idempotency key identifies the action, not the emission, so one
/// logical action submitted twice is replayed instead of going on air twice.
#[test]
fn one_action_submitted_twice_is_replayed_not_put_on_air_twice() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits::default());
    let operator = principal(Role::Operator);
    let source = WireInputId::from_domain(input(1));

    plane
        .program_go(
            source,
            ProgrammedGo::new([GoAction::Intent(request(3)), GoAction::Intent(request(2))])
                .unwrap(),
        )
        .unwrap();

    // One operator GO press, retried once.
    let first = plane.start_go(&operator, source, "press-1", 100).unwrap();
    let retry = plane.start_go(&operator, source, "press-1", 100).unwrap();
    assert_eq!(first, retry, "a retried press is the same run");
    assert_eq!(plane.active_go_run_len(), 1);

    let tick = plane.tick(&mut control, 100, &ConditionContext::new());
    assert_eq!(
        keys(&tick),
        ["auto/operator/go/1/0@100#0", "auto/operator/go/1/1@100#0"],
        "two actions, two keys -- not four"
    );
    assert!(
        tick.submitted
            .iter()
            .all(|entry| !entry.submission.replayed)
    );
    assert_eq!(preview(&control), input(2));

    // A retry that arrives after the run has finished is still the same run and
    // schedules nothing, so nothing fires a second time.
    assert_eq!(
        plane.start_go(&operator, source, "press-1", 400).unwrap(),
        first
    );
    let after = plane.tick(&mut control, 400, &ConditionContext::new());
    assert!(after.submitted.is_empty());
    assert!(after.refusals.is_empty());

    // The same press emitted twice: the second submission is replayed and the
    // engine state does not move again.
    plane
        .insert_shortcut(Shortcut {
            id: "take-three".to_owned(),
            scope: ShortcutScope::Global,
            chord: chord("T"),
            intent: request(3),
        })
        .unwrap();
    let pressed = plane
        .press(&mut control, &operator, None, &chord("T"), 500)
        .unwrap();
    let bounced = plane
        .press(&mut control, &operator, None, &chord("T"), 500)
        .unwrap();
    assert_eq!(pressed.idempotency_key, bounced.idempotency_key);
    assert!(!pressed.submission.replayed);
    assert!(
        bounced.submission.replayed,
        "a bouncing key does not cut twice"
    );
}

/// Continuous coalescing is last-value-wins within one stream of one binding
/// and never across bindings, and whatever it supersedes is reported.
#[test]
fn a_continuous_collision_across_bindings_reports_both_commands() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits::default());
    let operator = principal(Role::Operator);
    let viewer = principal(Role::Viewer);

    plane
        .schedule(&operator, "a", 100, ScheduleKind::Once, fader("tbar", 3))
        .unwrap();
    plane
        .schedule(&viewer, "b", 100, ScheduleKind::Once, fader("tbar", 1))
        .unwrap();

    let tick = plane.tick(&mut control, 100, &ConditionContext::new());
    assert_eq!(
        sources(&tick),
        ["schedule/a", "schedule/b"],
        "two bindings sharing a stream name are two commands, not one"
    );
    assert!(tick.refusals.is_empty());
    assert_eq!(
        preview(&control),
        input(3),
        "the operator's command reached the engine"
    );

    // Within one binding and one stream, the last value wins and the value it
    // superseded is reported rather than dropped.
    plane
        .insert_trigger(
            &operator,
            Trigger {
                intents: vec![fader("tbar", 1), fader("tbar", 2)],
                ..beat_trigger("sweep", 1)
            },
        )
        .unwrap();
    plane
        .ingest_event(&AutomationEvent::new("beat", 200))
        .unwrap();
    let tick = plane.tick(&mut control, 200, &ConditionContext::new());
    assert_eq!(sources(&tick), ["trigger/sweep"]);
    assert_eq!(preview(&control), input(2));
    assert_eq!(tick.refusals.len(), 1);
    assert_eq!(tick.refusals[0].reason, AutomationRefusalReason::Coalesced);
    assert_eq!(tick.refusals[0].request.payload, select_preview(1));
}

/// A binding stamps an identity, not a role set, so authority is whatever that
/// identity holds at the moment the command is emitted.
#[test]
fn authority_is_resolved_at_emission_not_frozen_at_arm_time() {
    let mut control = service();
    let sessions = Sessions::default();
    let mut plane = AutomationPlane::with_resolver(AutomationLimits::default(), sessions.clone());
    let operator = principal(Role::Operator);
    let identity = AutomationIdentity::of(&operator);

    plane
        .schedule(
            &operator,
            "break",
            100,
            ScheduleKind::Every { interval_ms: 100 },
            request(3),
        )
        .unwrap();
    assert_eq!(
        plane.bindings_for(&identity),
        [AutomationBinding::Schedule(ScheduleId::from("break"))]
    );

    let tick = plane.tick(&mut control, 100, &ConditionContext::new());
    assert!(tick.submitted[0].submission.is_accepted());
    assert_eq!(preview(&control), input(3));

    // The same person is downgraded mid-show. The armed schedule is refused on
    // the ordinary command path, with the code an operator command would get.
    sessions.set(&regraded(&operator, Role::Viewer));
    let tick = plane.tick(&mut control, 200, &ConditionContext::new());
    assert_eq!(
        rejection_code(&tick.submitted[0].submission.output.result),
        Some("permission_denied")
    );

    // The session ends. The binding is refused before it reaches the authority,
    // and the refusal carries the payload it did not submit.
    sessions.end(&identity);
    let tick = plane.tick(&mut control, 300, &ConditionContext::new());
    assert!(tick.submitted.is_empty());
    assert_eq!(tick.refusals.len(), 1);
    assert_eq!(
        tick.refusals[0].reason,
        AutomationRefusalReason::IdentityRevoked
    );
    assert_eq!(tick.refusals[0].identity, identity);
    assert_eq!(tick.refusals[0].request.payload, select_preview(3));

    // Revoking by identity cancels what that identity armed.
    assert_eq!(
        plane.revoke_identity(&identity),
        [AutomationBinding::Schedule(ScheduleId::from("break"))]
    );
    assert_eq!(plane.schedule_len(), 0);
    assert!(plane.bindings_for(&identity).is_empty());
    let tick = plane.tick(&mut control, 400, &ConditionContext::new());
    assert!(tick.submitted.is_empty());
    assert!(tick.refusals.is_empty());
}

/// Every bound refuses observably: nothing is queued without a limit and
/// nothing is dropped without being reported with enough to act on.
#[test]
fn bounds_refuse_a_flood_instead_of_queueing_or_dropping_it() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits {
        max_triggers: 1,
        max_pending_trigger_actions: 1,
        max_commands_per_tick: 4,
        max_presses_per_frame: 2,
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

    // A sponsor break planned in the same frame is refused first, because the
    // budget refuses from the least operator-driven end.
    plane
        .schedule(
            &operator,
            "sponsor-break",
            10,
            ScheduleKind::Once,
            request(2),
        )
        .unwrap();

    let tick = plane.tick(&mut control, 10, &ConditionContext::new());
    assert_eq!(tick.submitted.len(), 4);
    assert_eq!(tick.refusals.len(), 9);
    assert!(tick.error.is_none());
    assert!(
        tick.refusals
            .iter()
            .all(|refusal| refusal.reason == AutomationRefusalReason::TickBudget { limit: 4 })
    );
    assert_eq!(
        tick.refusals[0].source,
        AutomationSource::Schedule {
            id: "sponsor-break".to_owned()
        }
    );
    assert_eq!(
        plane.schedule_len(),
        0,
        "the fired one-shot schedule is gone from the plane"
    );
    assert_eq!(
        tick.refusals[0].request.payload,
        select_preview(2),
        "the refused break is handed back whole, so the caller can re-emit it"
    );
    assert_eq!(
        plane.pending_trigger_action_len(),
        0,
        "the refused commands are not requeued"
    );

    let settled = plane.tick(&mut control, 11, &ConditionContext::new());
    assert!(settled.submitted.is_empty());
    assert!(settled.refusals.is_empty());

    // Presses do not consume the automation budget, but they are bounded, and
    // the allowance is replenished once per frame.
    plane
        .insert_shortcut(Shortcut {
            id: "take-one".to_owned(),
            scope: ShortcutScope::Global,
            chord: chord("T"),
            intent: request(1),
        })
        .unwrap();
    plane
        .press(&mut control, &operator, None, &chord("T"), 12)
        .unwrap();
    plane
        .press(&mut control, &operator, None, &chord("T"), 13)
        .unwrap();
    assert_eq!(
        plane.press(&mut control, &operator, None, &chord("T"), 14),
        Err(AutomationError::LimitReached {
            resource: AutomationResource::Presses,
            limit: 2,
        })
    );
    plane.tick(&mut control, 15, &ConditionContext::new());
    plane
        .press(&mut control, &operator, None, &chord("T"), 16)
        .unwrap();
}

/// A stalled frame fires a recurring schedule once, and says how many
/// occurrences that cost.
#[test]
fn a_stalled_frame_reports_the_occurrences_it_skipped() {
    let mut control = service();
    let mut plane = AutomationPlane::new(AutomationLimits::default());
    let operator = principal(Role::Operator);

    plane
        .schedule(
            &operator,
            "break",
            100,
            ScheduleKind::Every { interval_ms: 100 },
            request(3),
        )
        .unwrap();
    let tick = plane.tick(&mut control, 100, &ConditionContext::new());
    assert!(tick.missed.is_empty());

    let tick = plane.tick(&mut control, 600, &ConditionContext::new());
    assert_eq!(tick.submitted.len(), 1, "no catch-up burst on air");
    assert_eq!(
        tick.missed,
        [AutomationMissedOccurrences {
            id: ScheduleId::from("break"),
            occurrence_ms: 200,
            missed: 4,
        }]
    );
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

    let tick = plane.tick(&mut control, 50, &ConditionContext::new());
    assert_eq!(
        sources(&tick),
        ["schedule/backdrop", "trigger/alpha", "trigger/beta"],
        "schedules precede triggers, and triggers keep registration order"
    );
    assert_eq!(
        preview(&control),
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
