//! Authority-side binding between automation bindings and the command plane.
//!
//! Every command an automation source produces is an ordinary
//! [`CommandMessage`] submitted through [`ControlService::submit`], so
//! authorization, idempotency keys, revision expectations and deadlines apply
//! to automation exactly as they apply to an operator. There is no bypass path.
//!
//! Automation holds no authority of its own. A binding stamps the *identity*
//! that armed it, never a frozen set of roles, and the authority that identity
//! currently holds is resolved again at every emission through an
//! [`AuthorityResolver`]. A session that has ended is refused before submission
//! and a role that has been downgraded is refused by the policy on the ordinary
//! command path, so a change to a person takes effect on the next command
//! rather than at the end of the show.
//!
//! An emitted idempotency key is a pure function of the action's identity --
//! who armed it, which binding, which occurrence, which intent -- so the same
//! logical action submitted twice is suppressed by the authority instead of
//! putting a second cut on air.
//!
//! This module owns no clock: `fm-automation` never reads one, and every entry
//! point here takes an explicit millisecond timestamp that it forwards both to
//! the automation engines and to the authority.

use std::{collections::BTreeMap, error::Error, fmt};

use fm_auth::{Principal, SessionId, UserId};
use fm_automation::{
    AutomationEvent, Chord, CommandIntent, ConditionContext, GoEngine, GoError, IntentBuffer,
    ProgrammedGo, ScheduleError, ScheduleId, ScheduleKind, ScheduleSet, Shortcut, ShortcutError,
    ShortcutRegistry, Trigger, TriggerEngine, TriggerError,
};
use fm_command::IdempotencyKey;
use fm_protocol::{CURRENT_PROTOCOL_VERSION, CommandMessage, CommandPayload, WireInputId};

use crate::{AuthorizationHook, CommandSubmission, ControlError, ControlService};

/// Registration and per-tick emission caps for one automation plane.
///
/// Every cap is enforced as an explicit refusal. Nothing here grows without a
/// bound and nothing is dropped without being reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomationLimits {
    pub max_shortcuts: usize,
    pub max_triggers: usize,
    /// Trigger actions awaiting their delay. One event arms at most one action
    /// per registered trigger, so the queue never exceeds this value plus
    /// [`Self::max_triggers`].
    pub max_pending_trigger_actions: usize,
    pub max_schedules: usize,
    pub max_go_programs: usize,
    pub max_active_go_runs: usize,
    /// Commands one [`AutomationPlane::tick`] may submit. Operator key presses
    /// do not consume this budget.
    pub max_commands_per_tick: usize,
    /// Key presses one frame may submit. A press does not consume the
    /// automation budget -- the operator must stay ahead of automation, which
    /// is why this bound is the larger of the two -- but it is still bounded,
    /// so a stuck or auto-repeating key cannot submit transitions without
    /// limit. The allowance is replenished by [`AutomationPlane::tick`], which
    /// a daemon calls once per frame.
    pub max_presses_per_frame: usize,
    /// The staleness budget applied to a binding that does not set its own
    /// [`AutomationRequest::deadline_after_due_ms`], measured from the moment
    /// the action was due. Without it a break armed without an explicit budget
    /// would fire arbitrarily late.
    pub default_deadline_after_due_ms: u64,
}

impl Default for AutomationLimits {
    fn default() -> Self {
        Self {
            max_shortcuts: 512,
            max_triggers: 256,
            max_pending_trigger_actions: 1_024,
            max_schedules: 256,
            max_go_programs: 256,
            max_active_go_runs: 16,
            max_commands_per_tick: 32,
            max_presses_per_frame: 64,
            default_deadline_after_due_ms: 1_000,
        }
    }
}

/// The bounded resource a refusal refers to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationResource {
    Shortcuts,
    Triggers,
    PendingTriggerActions,
    Schedules,
    GoPrograms,
    ActiveGoRuns,
    Presses,
}

impl AutomationResource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shortcuts => "shortcuts",
            Self::Triggers => "triggers",
            Self::PendingTriggerActions => "pending trigger actions",
            Self::Schedules => "schedules",
            Self::GoPrograms => "programmed GO lists",
            Self::ActiveGoRuns => "active programmed GO runs",
            Self::Presses => "key presses in one frame",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutomationError {
    /// A bound refused the operation. No partial registration took place.
    LimitReached {
        resource: AutomationResource,
        limit: usize,
    },
    /// No shortcut binding resolves the pressed chord in the given scope.
    UnboundChord,
    Shortcut(ShortcutError),
    Trigger(TriggerError),
    Schedule(ScheduleError),
    Go(GoError),
    Control(ControlError),
}

impl fmt::Display for AutomationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitReached { resource, limit } => write!(
                formatter,
                "automation {} limit of {limit} reached",
                resource.as_str()
            ),
            Self::UnboundChord => formatter.write_str("no shortcut binds the pressed chord"),
            Self::Shortcut(error) => error.fmt(formatter),
            Self::Trigger(error) => error.fmt(formatter),
            Self::Schedule(error) => error.fmt(formatter),
            Self::Go(error) => error.fmt(formatter),
            Self::Control(error) => error.fmt(formatter),
        }
    }
}

impl Error for AutomationError {}

impl From<ShortcutError> for AutomationError {
    fn from(value: ShortcutError) -> Self {
        Self::Shortcut(value)
    }
}

impl From<TriggerError> for AutomationError {
    fn from(value: TriggerError) -> Self {
        Self::Trigger(value)
    }
}

impl From<ScheduleError> for AutomationError {
    fn from(value: ScheduleError) -> Self {
        Self::Schedule(value)
    }
}

impl From<GoError> for AutomationError {
    fn from(value: GoError) -> Self {
        Self::Go(value)
    }
}

impl From<ControlError> for AutomationError {
    fn from(value: ControlError) -> Self {
        Self::Control(value)
    }
}

/// The person and session a binding was armed by.
///
/// This is a reference to an identity, not a grant. It names who to ask; it
/// says nothing about what they may do, which is exactly why an armed binding
/// cannot outlive its arming principal's authority.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AutomationIdentity {
    user: UserId,
    session: SessionId,
}

impl AutomationIdentity {
    #[must_use]
    pub fn of(principal: &Principal) -> Self {
        Self {
            user: principal.user_id().clone(),
            session: principal.session_id().clone(),
        }
    }

    #[must_use]
    pub const fn user(&self) -> &UserId {
        &self.user
    }

    #[must_use]
    pub const fn session(&self) -> &SessionId {
        &self.session
    }
}

impl fmt::Display for AutomationIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.user, self.session)
    }
}

/// Resolves a stamped [`AutomationIdentity`] to the authority it holds *now*.
///
/// This is the seam a session store plugs into. There is no session store in
/// this repository yet, so the default [`ArmTimeAuthority`] answers from the
/// principal observed when the binding was armed -- the historical behaviour,
/// named for what it is rather than mistaken for a live lookup. A deployment
/// with sessions injects a resolver backed by them and a mid-show role change
/// or logout takes effect on the very next automation command.
pub trait AuthorityResolver {
    /// The identity's current authority, or `None` when it holds none: the
    /// session ended, or the user was removed.
    fn resolve(&self, identity: &AutomationIdentity) -> Option<Principal>;

    /// Observes the principal that armed a binding. A resolver backed by a live
    /// session store ignores this.
    fn observe(&mut self, _principal: &Principal) {}

    /// Drops any authority cached for an identity whose bindings are all gone.
    fn forget(&mut self, _identity: &AutomationIdentity) {}
}

/// The default resolver: the authority observed when a binding was armed.
///
/// It is a cache, not a session store, and it is bounded by the plane that owns
/// it -- an identity is forgotten as soon as its last binding is cancelled or
/// revoked. It can revoke an identity; it cannot notice a role change made
/// somewhere else, which is the reason [`AuthorityResolver`] is injectable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArmTimeAuthority {
    principals: BTreeMap<AutomationIdentity, Principal>,
}

impl ArmTimeAuthority {
    #[must_use]
    pub fn len(&self) -> usize {
        self.principals.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.principals.is_empty()
    }
}

impl AuthorityResolver for ArmTimeAuthority {
    fn resolve(&self, identity: &AutomationIdentity) -> Option<Principal> {
        self.principals.get(identity).cloned()
    }

    fn observe(&mut self, principal: &Principal) {
        self.principals
            .insert(AutomationIdentity::of(principal), principal.clone());
    }

    fn forget(&mut self, identity: &AutomationIdentity) {
        self.principals.remove(identity);
    }
}

/// One binding armed under a stamped identity's authority.
///
/// Shortcuts are absent by design: a press is authorized against the principal
/// that pressed it, so a shortcut binding carries no arming authority to
/// enumerate or revoke.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AutomationBinding {
    Trigger(String),
    Schedule(ScheduleId),
    GoRun(u64),
}

/// One command an automation binding may produce, minus the identity that the
/// control plane mints for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationRequest {
    pub payload: CommandPayload,
    /// Carried verbatim into the command envelope's revision expectation, so a
    /// guarded automation command is refused with `revision_conflict` exactly
    /// like a guarded operator command.
    pub expected_revision: Option<u64>,
    /// Deadline measured from the moment the action was due, not from the tick
    /// that observed it, so a late tick refuses a stale command with
    /// `deadline_exceeded` instead of putting it on air. `None` takes
    /// [`AutomationLimits::default_deadline_after_due_ms`]; there is no way to
    /// arm a binding with no staleness boundary at all.
    pub deadline_after_due_ms: Option<u64>,
}

impl AutomationRequest {
    #[must_use]
    pub const fn new(payload: CommandPayload) -> Self {
        Self {
            payload,
            expected_revision: None,
            deadline_after_due_ms: None,
        }
    }

    #[must_use]
    pub const fn expecting(mut self, revision: u64) -> Self {
        self.expected_revision = Some(revision);
        self
    }

    #[must_use]
    pub const fn within_ms(mut self, millis: u64) -> Self {
        self.deadline_after_due_ms = Some(millis);
        self
    }
}

/// An automation request bound to the identity whose authority it will be
/// emitted under.
///
/// This type carries no authority. It names an identity, and the authority that
/// identity holds is resolved at emission, so nothing here can grant anything.
/// Only [`AutomationPlane`] constructs it and it never leaves the plane, but
/// that encapsulation is bookkeeping, not a security boundary: `Principal` is
/// constructible by any caller of this crate, so a forged principal with a
/// never-issued session is accepted here exactly as it is by
/// [`ControlService::submit`]. Integrity of the identities that reach this
/// module rests wholly on the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AutomationCommand {
    identity: AutomationIdentity,
    request: AutomationRequest,
}

/// The binding that produced one command.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AutomationSource {
    Shortcut { id: String },
    Trigger { id: String },
    Schedule { id: String },
    Go { run_id: u64, index: usize },
}

impl fmt::Display for AutomationSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shortcut { id } => write!(formatter, "shortcut/{id}"),
            Self::Trigger { id } => write!(formatter, "trigger/{id}"),
            Self::Schedule { id } => write!(formatter, "schedule/{id}"),
            Self::Go { run_id, index } => write!(formatter, "go/{run_id}/{index}"),
        }
    }
}

/// One automation-produced command that reached the authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationSubmission {
    pub source: AutomationSource,
    /// The timestamp the action was due at, which anchors its deadline.
    pub due_ms: u64,
    pub idempotency_key: IdempotencyKey,
    pub submission: CommandSubmission,
}

/// Why an automation-produced command never reached the authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationRefusalReason {
    /// The per-tick command budget was exhausted.
    TickBudget { limit: usize },
    /// A later intent for the same binding and the same continuous stream
    /// superseded this one. Coalescing never crosses bindings, so this is a
    /// deliberate last-value-wins on one stream of one binding.
    Coalesced,
    /// The arming identity no longer resolves to any authority. A downgraded
    /// role is not this: it still resolves, and the policy refuses the
    /// submission with `permission_denied` on the ordinary command path.
    IdentityRevoked,
    /// An earlier command in the same tick failed with
    /// [`AutomationTick::error`], so this one was not attempted.
    Aborted,
}

/// One automation-produced command that was not submitted.
///
/// The refusal carries the whole request rather than requeueing it. Requeueing
/// would re-order a stale automation command against the fresh operator input
/// of the next frame, and the deadline is anchored to `due_ms`, so a re-emitted
/// command would in any case have to be judged against the moment it was
/// originally due. Handing the caller the payload keeps that decision where the
/// show is: re-emit the sponsor break, or drop it and say so.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationRefusal {
    pub source: AutomationSource,
    pub due_ms: u64,
    pub identity: AutomationIdentity,
    pub request: AutomationRequest,
    pub reason: AutomationRefusalReason,
}

/// Recurrences of one schedule that a stalled frame skipped.
///
/// The schedule stays anchored to its original boundaries and fires once, so a
/// stall never produces a catch-up burst on air; this is how the caller learns
/// what the stall cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationMissedOccurrences {
    pub id: ScheduleId,
    /// The boundary that did fire.
    pub occurrence_ms: u64,
    pub missed: u64,
}

/// The complete outcome of one automation tick.
///
/// Every command the frame planned is accounted for: it is in [`Self::submitted`]
/// or in [`Self::refusals`] with the reason it never reached the authority.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutomationTick {
    pub submitted: Vec<AutomationSubmission>,
    pub refusals: Vec<AutomationRefusal>,
    pub missed: Vec<AutomationMissedOccurrences>,
    /// The infrastructure error that stopped the frame, if one did. The work
    /// the frame had already done is still reported above it.
    pub error: Option<AutomationError>,
}

#[derive(Clone, Debug)]
struct PlannedCommand {
    source: AutomationSource,
    due_ms: u64,
    /// The intent's position within its firing binding. One trigger fire emits
    /// several intents sharing a source and a due time; this separates them
    /// without making the key depend on how many commands came before.
    intent_index: usize,
    command: AutomationCommand,
}

fn refuse(planned: PlannedCommand, reason: AutomationRefusalReason) -> AutomationRefusal {
    AutomationRefusal {
        source: planned.source,
        due_ms: planned.due_ms,
        identity: planned.command.identity,
        request: planned.command.request,
        reason,
    }
}

/// Shortcuts, triggers, schedules and programmed GO lists bound to one
/// authority.
///
/// A daemon drives exactly four entry points: [`Self::press`] for operator key
/// input, [`Self::ingest_event`] for observed events, [`Self::start_go`] for a
/// programmed GO, and [`Self::tick`] once per frame with the frame's timestamp.
/// Nothing else is required to put automation on air.
pub struct AutomationPlane<R = ArmTimeAuthority> {
    limits: AutomationLimits,
    resolver: R,
    shortcuts: ShortcutRegistry<AutomationRequest>,
    triggers: TriggerEngine<AutomationCommand>,
    schedules: ScheduleSet<AutomationCommand>,
    go: GoEngine<AutomationRequest>,
    go_runs: BTreeMap<u64, AutomationIdentity>,
    armed: BTreeMap<AutomationBinding, AutomationIdentity>,
    presses_this_frame: usize,
}

impl AutomationPlane<ArmTimeAuthority> {
    /// A plane that resolves authority from what it observed at arm time.
    #[must_use]
    pub fn new(limits: AutomationLimits) -> Self {
        Self::with_resolver(limits, ArmTimeAuthority::default())
    }
}

impl<R: AuthorityResolver> AutomationPlane<R> {
    /// A plane that resolves the authority of an armed identity through
    /// `resolver` at every emission.
    #[must_use]
    pub fn with_resolver(limits: AutomationLimits, resolver: R) -> Self {
        Self {
            limits,
            resolver,
            shortcuts: ShortcutRegistry::default(),
            triggers: TriggerEngine::default(),
            schedules: ScheduleSet::default(),
            go: GoEngine::default(),
            go_runs: BTreeMap::new(),
            armed: BTreeMap::new(),
            presses_this_frame: 0,
        }
    }

    #[must_use]
    pub const fn limits(&self) -> &AutomationLimits {
        &self.limits
    }

    #[must_use]
    pub const fn resolver(&self) -> &R {
        &self.resolver
    }

    #[must_use]
    pub fn shortcut_len(&self) -> usize {
        self.shortcuts.shortcuts().len()
    }

    #[must_use]
    pub fn trigger_len(&self) -> usize {
        self.triggers.registered_len()
    }

    #[must_use]
    pub fn pending_trigger_action_len(&self) -> usize {
        self.triggers.pending_len()
    }

    #[must_use]
    pub fn schedule_len(&self) -> usize {
        self.schedules.len()
    }

    #[must_use]
    pub fn active_go_run_len(&self) -> usize {
        self.go.active_run_len()
    }

    /// Every binding armed by one identity, in a stable order.
    #[must_use]
    pub fn bindings_for(&self, identity: &AutomationIdentity) -> Vec<AutomationBinding> {
        self.armed
            .iter()
            .filter(|(_, armed)| *armed == identity)
            .map(|(binding, _)| binding.clone())
            .collect()
    }

    /// Cancels every binding one identity armed and forgets its authority.
    ///
    /// This is the blunt instrument for "that person is off the show". The fine
    /// one is the resolver: an identity that stops resolving is refused at
    /// emission without anything having to be enumerated.
    pub fn revoke_identity(&mut self, identity: &AutomationIdentity) -> Vec<AutomationBinding> {
        let revoked = self.bindings_for(identity);
        for binding in &revoked {
            match binding {
                AutomationBinding::Trigger(id) => {
                    self.triggers.remove(id);
                }
                AutomationBinding::Schedule(id) => {
                    self.schedules.cancel(id);
                }
                AutomationBinding::GoRun(run_id) => {
                    self.go.cancel(*run_id);
                    self.go_runs.remove(run_id);
                }
            }
            self.armed.remove(binding);
        }
        self.resolver.forget(identity);
        revoked
    }

    /// Registers a shortcut binding.
    ///
    /// Shortcuts hold unstamped requests because a press is authorized against
    /// the principal that pressed it, never against the principal that
    /// configured the binding.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] at the shortcut cap, or a
    /// duplicate-identifier or chord-conflict refusal from the registry.
    pub fn insert_shortcut(
        &mut self,
        shortcut: Shortcut<AutomationRequest>,
    ) -> Result<(), AutomationError> {
        if self.shortcuts.shortcuts().len() >= self.limits.max_shortcuts {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::Shortcuts,
                limit: self.limits.max_shortcuts,
            });
        }
        self.shortcuts.insert(shortcut)?;
        Ok(())
    }

    pub fn remove_shortcut(&mut self, id: &str) -> bool {
        self.shortcuts.remove(id).is_some()
    }

    /// Arms a trigger under the identity of the principal registering it.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] at the trigger cap, or a
    /// duplicate-identifier or empty-action refusal from the trigger engine.
    pub fn insert_trigger(
        &mut self,
        principal: &Principal,
        trigger: Trigger<AutomationRequest>,
    ) -> Result<(), AutomationError> {
        if self.triggers.registered_len() >= self.limits.max_triggers {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::Triggers,
                limit: self.limits.max_triggers,
            });
        }
        let identity = AutomationIdentity::of(principal);
        let Trigger {
            id,
            filter,
            delay_ms,
            conditions,
            intents,
        } = trigger;
        let binding = AutomationBinding::Trigger(id.clone());
        self.triggers.insert(Trigger {
            id,
            filter,
            delay_ms,
            conditions,
            intents: intents
                .into_iter()
                .map(|intent| intent.map(|request| stamp(&identity, request)))
                .collect(),
        })?;
        self.arm(binding, principal, identity);
        Ok(())
    }

    /// Unregisters a trigger and cancels the actions it has already armed.
    pub fn remove_trigger(&mut self, id: &str) -> bool {
        if !self.triggers.remove(id) {
            return false;
        }
        self.release(&AutomationBinding::Trigger(id.to_owned()));
        true
    }

    /// Arms a schedule under the identity of the principal registering it.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] at the schedule cap, or a
    /// duplicate-identifier or zero-interval refusal from the schedule set.
    pub fn schedule(
        &mut self,
        principal: &Principal,
        id: impl Into<ScheduleId>,
        at_ms: u64,
        kind: ScheduleKind,
        intent: CommandIntent<AutomationRequest>,
    ) -> Result<(), AutomationError> {
        if self.schedules.len() >= self.limits.max_schedules {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::Schedules,
                limit: self.limits.max_schedules,
            });
        }
        let identity = AutomationIdentity::of(principal);
        let id = id.into();
        self.schedules.schedule_at(
            id.clone(),
            at_ms,
            kind,
            intent.map(|request| stamp(&identity, request)),
        )?;
        self.arm(AutomationBinding::Schedule(id), principal, identity);
        Ok(())
    }

    pub fn cancel_schedule(&mut self, id: &ScheduleId) -> bool {
        if self.schedules.cancel(id).is_none() {
            return false;
        }
        self.release(&AutomationBinding::Schedule(id.clone()));
        true
    }

    /// Installs the programmed GO list for one input.
    ///
    /// The list holds unstamped requests: a GO run is emitted under the
    /// identity that started it.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] when a new input would exceed
    /// the programmed-list cap. Replacing an existing list is always allowed.
    pub fn program_go(
        &mut self,
        input: WireInputId,
        program: ProgrammedGo<AutomationRequest>,
    ) -> Result<(), AutomationError> {
        let input = input.to_domain();
        if !self.go.has_program(input) && self.go.program_len() >= self.limits.max_go_programs {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::GoPrograms,
                limit: self.limits.max_go_programs,
            });
        }
        self.go.program(input, program);
        Ok(())
    }

    /// Matches one observed event against every armed trigger.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] when the pending queue is at
    /// its cap, so an event storm is refused instead of queued without bound.
    pub fn ingest_event(&mut self, event: &AutomationEvent) -> Result<(), AutomationError> {
        if self.triggers.pending_len() >= self.limits.max_pending_trigger_actions {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::PendingTriggerActions,
                limit: self.limits.max_pending_trigger_actions,
            });
        }
        self.triggers.ingest(event)?;
        Ok(())
    }

    /// Starts a programmed GO run under the identity of its starter.
    ///
    /// `idempotency_key` identifies the press, not the attempt: retrying one
    /// operator GO press returns the run the first attempt created, so the
    /// actions it emits keep their command identities and the authority
    /// suppresses the duplicates. A fresh key is a fresh run.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] at the concurrent-run cap --
    /// never for a retry of a run that already exists -- or an unknown-input or
    /// timestamp refusal from the GO engine.
    pub fn start_go(
        &mut self,
        principal: &Principal,
        input: WireInputId,
        idempotency_key: impl Into<IdempotencyKey>,
        now_ms: u64,
    ) -> Result<u64, AutomationError> {
        let idempotency_key = idempotency_key.into();
        if self.go.started(&idempotency_key).is_none()
            && self.go.active_run_len() >= self.limits.max_active_go_runs
        {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::ActiveGoRuns,
                limit: self.limits.max_active_go_runs,
            });
        }
        let start = self.go.start(input.to_domain(), idempotency_key, now_ms)?;
        if !start.replayed {
            let identity = AutomationIdentity::of(principal);
            self.go_runs.insert(start.run_id, identity.clone());
            self.arm(AutomationBinding::GoRun(start.run_id), principal, identity);
        }
        Ok(start.run_id)
    }

    pub fn cancel_go(&mut self, run_id: u64) -> bool {
        self.go_runs.remove(&run_id);
        self.release(&AutomationBinding::GoRun(run_id));
        self.go.cancel(run_id)
    }

    /// Resolves a pressed chord and submits its command immediately.
    ///
    /// The press is authorized against `principal`, the principal that pressed
    /// it, so a shortcut configured by an administrator grants a viewer
    /// nothing. Presses do not consume the per-tick automation budget -- that
    /// budget exists to stop automation from starving the operator, not the
    /// operator -- but they have a larger bound of their own, so a stuck key
    /// cannot submit transitions without limit.
    ///
    /// Two presses of one chord by one person in one millisecond are one
    /// action and share one idempotency key, so a bouncing key does not cut
    /// twice; two people pressing it do not collide, because the key names the
    /// presser.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::UnboundChord`] when no binding resolves,
    /// [`AutomationError::LimitReached`] when the frame's press allowance is
    /// spent, or an engine infrastructure error. A permission denial is a
    /// normal [`fm_protocol::CommandResult::Rejected`] inside the submission.
    pub fn press<A: AuthorizationHook>(
        &mut self,
        control: &mut ControlService<A>,
        principal: &Principal,
        scope: Option<&str>,
        chord: &Chord,
        now_ms: u64,
    ) -> Result<AutomationSubmission, AutomationError> {
        if self.presses_this_frame >= self.limits.max_presses_per_frame {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::Presses,
                limit: self.limits.max_presses_per_frame,
            });
        }
        let shortcut = self
            .shortcuts
            .resolve(scope, chord)
            .ok_or(AutomationError::UnboundChord)?;
        let planned = PlannedCommand {
            source: AutomationSource::Shortcut {
                id: shortcut.id.clone(),
            },
            due_ms: now_ms,
            intent_index: 0,
            command: AutomationCommand {
                identity: AutomationIdentity::of(principal),
                request: shortcut.intent.clone().into_command(),
            },
        };
        self.presses_this_frame += 1;
        Ok(self.emit(control, principal, &planned, now_ms)?)
    }

    /// Submits every automation command due at `now_ms`.
    ///
    /// Sources are collected in a fixed order -- schedules, then triggers, then
    /// programmed GO actions -- and each engine drains its own queue in
    /// deterministic due-time then registration order, so the emitted sequence
    /// never depends on map iteration order. Later commands overwrite earlier
    /// ones on shared state, which is why the least operator-driven source is
    /// submitted first. Continuous intents coalesce last-intent-wins per
    /// binding and stream -- never across bindings, where last-wins would
    /// destroy an unrelated command -- and are submitted after the discrete
    /// ones in binding then stream order.
    ///
    /// Every planned command is accounted for. What the budget refuses, what
    /// coalescing supersedes, and what a revoked identity loses all appear in
    /// [`AutomationTick::refusals`] carrying their payload. An infrastructure
    /// error stops the frame and is reported in [`AutomationTick::error`]
    /// alongside the work the frame had already done, rather than discarding
    /// commands that were already on air.
    ///
    /// This also replenishes the [`Self::press`] allowance, so the press bound
    /// is per frame.
    pub fn tick<A: AuthorizationHook>(
        &mut self,
        control: &mut ControlService<A>,
        now_ms: u64,
        state: &ConditionContext,
    ) -> AutomationTick {
        self.presses_this_frame = 0;
        let mut tick = AutomationTick::default();
        let (mut plan, completed) = self.collect(now_ms, state, &mut tick);

        let limit = self.limits.max_commands_per_tick;
        let overflow = plan.len().saturating_sub(limit);
        for planned in plan.drain(..overflow) {
            tick.refusals.push(refuse(
                planned,
                AutomationRefusalReason::TickBudget { limit },
            ));
        }

        let mut aborted = false;
        for planned in plan {
            if aborted {
                tick.refusals
                    .push(refuse(planned, AutomationRefusalReason::Aborted));
                continue;
            }
            let Some(principal) = self.resolver.resolve(&planned.command.identity) else {
                tick.refusals
                    .push(refuse(planned, AutomationRefusalReason::IdentityRevoked));
                continue;
            };
            match self.emit(control, &principal, &planned, now_ms) {
                Ok(submission) => tick.submitted.push(submission),
                Err(error) => {
                    tick.error = Some(AutomationError::Control(error));
                    tick.refusals
                        .push(refuse(planned, AutomationRefusalReason::Aborted));
                    aborted = true;
                }
            }
        }

        // Only once the frame has emitted: a binding that fired for the last
        // time still needed its arming authority to resolve while it did.
        for binding in completed {
            self.release(&binding);
        }
        tick
    }

    /// Drains every engine into one ordered plan, recording what coalescing
    /// superseded and what a stalled frame missed, and returning the bindings
    /// that have now fired for the last time.
    fn collect(
        &mut self,
        now_ms: u64,
        state: &ConditionContext,
        tick: &mut AutomationTick,
    ) -> (Vec<PlannedCommand>, Vec<AutomationBinding>) {
        let mut buffer: IntentBuffer<PlannedCommand, AutomationSource> = IntentBuffer::default();
        let mut coalesced = Vec::new();
        let mut completed = Vec::new();

        let fires = match self.schedules.poll(now_ms) {
            Ok(fires) => fires,
            Err(error) => {
                tick.error = Some(AutomationError::Schedule(error));
                Vec::new()
            }
        };
        for fire in fires {
            if fire.missed_occurrences > 0 {
                tick.missed.push(AutomationMissedOccurrences {
                    id: fire.id.clone(),
                    occurrence_ms: fire.occurrence_ms,
                    missed: fire.missed_occurrences,
                });
            }
            if !self.schedules.contains(&fire.id) {
                completed.push(AutomationBinding::Schedule(fire.id.clone()));
            }
            let source = AutomationSource::Schedule {
                id: fire.id.as_str().to_owned(),
            };
            let due_ms = fire.occurrence_ms;
            plan_intent(
                &mut buffer,
                &mut coalesced,
                source.clone(),
                fire.intent.map(|command| PlannedCommand {
                    source,
                    due_ms,
                    intent_index: 0,
                    command,
                }),
            );
        }

        for fire in self.triggers.poll(now_ms, state) {
            let due_ms = fire.scheduled_for_ms;
            for (intent_index, intent) in fire.intents.into_iter().enumerate() {
                let source = AutomationSource::Trigger {
                    id: fire.trigger_id.clone(),
                };
                plan_intent(
                    &mut buffer,
                    &mut coalesced,
                    source.clone(),
                    intent.map(|command| PlannedCommand {
                        source,
                        due_ms,
                        intent_index,
                        command,
                    }),
                );
            }
        }

        self.collect_go(now_ms, &mut buffer, &mut coalesced, &mut completed);

        tick.refusals.extend(
            coalesced
                .into_iter()
                .map(|planned| refuse(planned, AutomationRefusalReason::Coalesced)),
        );

        let mut plan: Vec<_> = buffer
            .drain_discrete()
            .into_iter()
            .map(CommandIntent::into_command)
            .collect();
        plan.extend(
            buffer
                .drain_continuous()
                .into_iter()
                .map(CommandIntent::into_command),
        );
        (plan, completed)
    }

    /// Plans the GO actions due at `now_ms` and retires the runs that have
    /// fired for the last time.
    ///
    /// A run whose identity is no longer recorded is skipped: only
    /// [`Self::revoke_identity`] removes one, and it cancels the run's pending
    /// actions in the same breath, so this is defence and not a path.
    fn collect_go(
        &mut self,
        now_ms: u64,
        buffer: &mut IntentBuffer<PlannedCommand, AutomationSource>,
        coalesced: &mut Vec<PlannedCommand>,
        completed: &mut Vec<AutomationBinding>,
    ) {
        for fire in self.go.poll(now_ms) {
            let Some(identity) = self.go_runs.get(&fire.run_id).cloned() else {
                continue;
            };
            let source = AutomationSource::Go {
                run_id: fire.run_id,
                index: fire.action.index,
            };
            let due_ms = fire.scheduled_for_ms;
            plan_intent(
                buffer,
                coalesced,
                source.clone(),
                fire.action.intent.map(|request| PlannedCommand {
                    source,
                    due_ms,
                    intent_index: 0,
                    command: AutomationCommand { identity, request },
                }),
            );
        }
        let finished: Vec<_> = self
            .go_runs
            .keys()
            .copied()
            .filter(|run_id| !self.go.is_active(*run_id))
            .collect();
        for run_id in finished {
            self.go_runs.remove(&run_id);
            completed.push(AutomationBinding::GoRun(run_id));
        }
    }

    /// Records that `binding` is armed under `identity`, caching the arm-time
    /// authority for a resolver that has no other source for it.
    fn arm(
        &mut self,
        binding: AutomationBinding,
        principal: &Principal,
        identity: AutomationIdentity,
    ) {
        self.resolver.observe(principal);
        self.armed.insert(binding, identity);
    }

    /// Drops a binding, and the identity's cached authority with it once no
    /// binding of that identity remains.
    fn release(&mut self, binding: &AutomationBinding) {
        let Some(identity) = self.armed.remove(binding) else {
            return;
        };
        if !self.armed.values().any(|armed| *armed == identity) {
            self.resolver.forget(&identity);
        }
    }

    fn emit<A: AuthorizationHook>(
        &self,
        control: &mut ControlService<A>,
        principal: &Principal,
        planned: &PlannedCommand,
        now_ms: u64,
    ) -> Result<AutomationSubmission, ControlError> {
        let PlannedCommand {
            source,
            due_ms,
            intent_index,
            command,
        } = planned;
        let AutomationCommand { identity, request } = command;

        // A pure function of the action's identity: who armed it, which
        // binding, which occurrence, which intent. Nothing about how many
        // commands came before, so the same action emitted twice is the same
        // key and the authority replays instead of cutting twice.
        let key = format!("auto/{}/{source}@{due_ms}#{intent_index}", identity.user());
        let budget = request
            .deadline_after_due_ms
            .unwrap_or(self.limits.default_deadline_after_due_ms);
        let message = CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            id: key.clone(),
            idempotency_key: key.clone(),
            expected_revision: request.expected_revision,
            deadline_ms: Some(due_ms.saturating_add(budget)),
            payload: request.payload.clone(),
        };
        let submission = control.submit(principal, message, now_ms)?;
        Ok(AutomationSubmission {
            source: source.clone(),
            due_ms: *due_ms,
            idempotency_key: IdempotencyKey::new(key),
            submission,
        })
    }
}

fn plan_intent(
    buffer: &mut IntentBuffer<PlannedCommand, AutomationSource>,
    coalesced: &mut Vec<PlannedCommand>,
    source: AutomationSource,
    intent: CommandIntent<PlannedCommand>,
) {
    if let Some(superseded) = buffer.push(source, intent) {
        coalesced.push(superseded);
    }
}

fn stamp(identity: &AutomationIdentity, request: AutomationRequest) -> AutomationCommand {
    AutomationCommand {
        identity: identity.clone(),
        request,
    }
}

#[cfg(test)]
mod tests;
