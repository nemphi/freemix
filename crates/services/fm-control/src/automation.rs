//! Authority-side binding between automation bindings and the command plane.
//!
//! Every command an automation source produces is an ordinary
//! [`CommandMessage`] submitted through [`ControlService::submit`], so
//! authorization, idempotency keys, revision expectations and deadlines apply
//! to automation exactly as they apply to an operator. There is no bypass path.
//!
//! Automation holds no authority of its own. A binding carries the
//! [`Principal`] that requested it and every emission is authorized against
//! that principal, so an automation source can never perform an action its
//! requester could not.
//!
//! This module owns no clock: `fm-automation` never reads one, and every entry
//! point here takes an explicit millisecond timestamp that it forwards both to
//! the automation engines and to the authority.

use std::{collections::HashMap, error::Error, fmt};

use fm_auth::Principal;
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
    /// are submitted directly and never consume this budget.
    pub max_commands_per_tick: usize,
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
    /// A programmed GO action fired for a run with no recorded authority.
    UnknownGoRun {
        run_id: u64,
    },
    EmissionSequenceExhausted,
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
            Self::UnknownGoRun { run_id } => write!(
                formatter,
                "programmed GO run {run_id} has no recorded principal"
            ),
            Self::EmissionSequenceExhausted => {
                formatter.write_str("automation emission sequence exhausted")
            }
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
    /// `deadline_exceeded` instead of putting it on air.
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

/// An automation request bound to the principal whose authority it carries.
///
/// Only [`AutomationPlane`] constructs this and it never leaves the plane, so a
/// binding can never be armed with authority its requester did not have.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AutomationCommand {
    principal: Principal,
    request: AutomationRequest,
}

/// The binding that produced one command.
#[derive(Clone, Debug, Eq, PartialEq)]
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

/// One automation-produced command the tick budget refused.
///
/// The command was neither submitted nor requeued; the refusal is the only
/// outcome, and it is always reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationRefusal {
    pub source: AutomationSource,
    pub due_ms: u64,
    pub limit: usize,
}

/// The complete outcome of one automation tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationTick {
    pub submitted: Vec<AutomationSubmission>,
    pub refusals: Vec<AutomationRefusal>,
}

#[derive(Clone, Debug)]
struct PlannedCommand {
    source: AutomationSource,
    due_ms: u64,
    command: AutomationCommand,
}

/// Shortcuts, triggers, schedules and programmed GO lists bound to one
/// authority.
///
/// A daemon drives exactly four entry points: [`Self::press`] for operator key
/// input, [`Self::ingest_event`] for observed events, [`Self::start_go`] for a
/// programmed GO, and [`Self::tick`] once per frame with the frame's timestamp.
/// Nothing else is required to put automation on air.
pub struct AutomationPlane {
    limits: AutomationLimits,
    shortcuts: ShortcutRegistry<AutomationRequest>,
    triggers: TriggerEngine<AutomationCommand>,
    schedules: ScheduleSet<AutomationCommand>,
    go: GoEngine<AutomationRequest>,
    go_run_principals: HashMap<u64, Principal>,
    sequence: u64,
}

impl AutomationPlane {
    #[must_use]
    pub fn new(limits: AutomationLimits) -> Self {
        Self {
            limits,
            shortcuts: ShortcutRegistry::default(),
            triggers: TriggerEngine::default(),
            schedules: ScheduleSet::default(),
            go: GoEngine::default(),
            go_run_principals: HashMap::new(),
            sequence: 0,
        }
    }

    #[must_use]
    pub const fn limits(&self) -> &AutomationLimits {
        &self.limits
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
        self.go_run_principals.len()
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

    /// Arms a trigger with the authority of the principal registering it.
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
        let Trigger {
            id,
            filter,
            delay_ms,
            conditions,
            intents,
        } = trigger;
        self.triggers.insert(Trigger {
            id,
            filter,
            delay_ms,
            conditions,
            intents: intents
                .into_iter()
                .map(|intent| intent.map(|request| stamp(principal, request)))
                .collect(),
        })?;
        Ok(())
    }

    /// Arms a schedule with the authority of the principal registering it.
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
        self.schedules.schedule_at(
            id,
            at_ms,
            kind,
            intent.map(|request| stamp(principal, request)),
        )?;
        Ok(())
    }

    pub fn cancel_schedule(&mut self, id: &ScheduleId) -> bool {
        self.schedules.cancel(id).is_some()
    }

    /// Installs the programmed GO list for one input.
    ///
    /// The list holds unstamped requests: a GO run is authorized against the
    /// principal that started it.
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

    /// Starts a programmed GO run under the authority of its starter.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::LimitReached`] at the concurrent-run cap, or
    /// an unknown-input or timestamp refusal from the GO engine.
    pub fn start_go(
        &mut self,
        principal: &Principal,
        input: WireInputId,
        now_ms: u64,
    ) -> Result<u64, AutomationError> {
        if self.go_run_principals.len() >= self.limits.max_active_go_runs {
            return Err(AutomationError::LimitReached {
                resource: AutomationResource::ActiveGoRuns,
                limit: self.limits.max_active_go_runs,
            });
        }
        let start = self.go.start(input.to_domain(), now_ms)?;
        self.go_run_principals
            .insert(start.run_id, principal.clone());
        Ok(start.run_id)
    }

    pub fn cancel_go(&mut self, run_id: u64) -> bool {
        self.go_run_principals.remove(&run_id);
        self.go.cancel(run_id)
    }

    /// Resolves a pressed chord and submits its command immediately.
    ///
    /// The press is authorized against `principal`, the principal that pressed
    /// it, so a shortcut configured by an administrator grants a viewer
    /// nothing. Presses bypass the per-tick budget by design: the budget exists
    /// to stop automation from starving the operator, not the operator.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationError::UnboundChord`] when no binding resolves, or
    /// an engine infrastructure error. A permission denial is a normal
    /// [`fm_protocol::CommandResult::Rejected`] inside the submission.
    pub fn press<A: AuthorizationHook>(
        &mut self,
        control: &mut ControlService<A>,
        principal: &Principal,
        scope: Option<&str>,
        chord: &Chord,
        now_ms: u64,
    ) -> Result<AutomationSubmission, AutomationError> {
        let shortcut = self
            .shortcuts
            .resolve(scope, chord)
            .ok_or(AutomationError::UnboundChord)?;
        let planned = PlannedCommand {
            source: AutomationSource::Shortcut {
                id: shortcut.id.clone(),
            },
            due_ms: now_ms,
            command: stamp(principal, shortcut.intent.clone().into_command()),
        };
        self.emit(control, planned, now_ms)
    }

    /// Submits every automation command due at `now_ms`.
    ///
    /// Sources are collected in a fixed order — schedules, then triggers, then
    /// programmed GO actions — and each engine drains its own queue in
    /// deterministic due-time then registration order, so the emitted sequence
    /// never depends on map iteration order. Later commands overwrite earlier
    /// ones on shared state, which is why the least operator-driven source is
    /// submitted first. Continuous intents coalesce last-intent-wins per stream
    /// and are submitted after the discrete ones in stream-name order.
    ///
    /// When the plan exceeds [`AutomationLimits::max_commands_per_tick`], the
    /// excess is refused from the least operator-driven end and reported in
    /// [`AutomationTick::refusals`]. Refused commands are not retried.
    ///
    /// # Errors
    ///
    /// Returns a schedule or engine infrastructure error. Permission denials,
    /// revision conflicts and exceeded deadlines are normal rejected results
    /// inside the returned submissions.
    pub fn tick<A: AuthorizationHook>(
        &mut self,
        control: &mut ControlService<A>,
        now_ms: u64,
        state: &ConditionContext,
    ) -> Result<AutomationTick, AutomationError> {
        let mut buffer = IntentBuffer::default();

        for fire in self.schedules.poll(now_ms)? {
            let source = AutomationSource::Schedule {
                id: fire.id.as_str().to_owned(),
            };
            let due_ms = fire.occurrence_ms;
            buffer.push(fire.intent.map(|command| PlannedCommand {
                source,
                due_ms,
                command,
            }));
        }

        for fire in self.triggers.poll(now_ms, state) {
            let due_ms = fire.scheduled_for_ms;
            for intent in fire.intents {
                let source = AutomationSource::Trigger {
                    id: fire.trigger_id.clone(),
                };
                buffer.push(intent.map(|command| PlannedCommand {
                    source,
                    due_ms,
                    command,
                }));
            }
        }

        for fire in self.go.poll(now_ms) {
            let principal = self
                .go_run_principals
                .get(&fire.run_id)
                .ok_or(AutomationError::UnknownGoRun {
                    run_id: fire.run_id,
                })?
                .clone();
            let source = AutomationSource::Go {
                run_id: fire.run_id,
                index: fire.action.index,
            };
            let due_ms = fire.scheduled_for_ms;
            buffer.push(fire.action.intent.map(|request| PlannedCommand {
                source,
                due_ms,
                command: AutomationCommand { principal, request },
            }));
        }
        self.go_run_principals
            .retain(|run_id, _| self.go.is_active(*run_id));

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

        let limit = self.limits.max_commands_per_tick;
        let overflow = plan.len().saturating_sub(limit);
        let refusals: Vec<_> = plan
            .drain(..overflow)
            .map(|planned| AutomationRefusal {
                source: planned.source,
                due_ms: planned.due_ms,
                limit,
            })
            .collect();

        let mut submitted = Vec::with_capacity(plan.len());
        for planned in plan {
            submitted.push(self.emit(control, planned, now_ms)?);
        }
        Ok(AutomationTick {
            submitted,
            refusals,
        })
    }

    fn emit<A: AuthorizationHook>(
        &mut self,
        control: &mut ControlService<A>,
        planned: PlannedCommand,
        now_ms: u64,
    ) -> Result<AutomationSubmission, AutomationError> {
        let PlannedCommand {
            source,
            due_ms,
            command,
        } = planned;
        let AutomationCommand { principal, request } = command;
        let AutomationRequest {
            payload,
            expected_revision,
            deadline_after_due_ms,
        } = request;

        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(AutomationError::EmissionSequenceExhausted)?;
        let key = format!("auto/{source}@{due_ms}#{}", self.sequence);
        let message = CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            id: key.clone(),
            idempotency_key: key.clone(),
            expected_revision,
            deadline_ms: deadline_after_due_ms.map(|budget| due_ms.saturating_add(budget)),
            payload,
        };
        let submission = control.submit(&principal, message, now_ms)?;
        Ok(AutomationSubmission {
            source,
            due_ms,
            idempotency_key: IdempotencyKey::new(key),
            submission,
        })
    }
}

fn stamp(principal: &Principal, request: AutomationRequest) -> AutomationCommand {
    AutomationCommand {
        principal: principal.clone(),
        request,
    }
}

#[cfg(test)]
mod tests;
