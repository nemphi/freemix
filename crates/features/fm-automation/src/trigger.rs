use crate::{CommandIntent, Condition, ConditionContext, condition::conditions_match};
use core::fmt;
use fm_scheduler::{ActionError, ActionId, ActionQueue, FrameNumber};
use std::collections::HashMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationEvent {
    pub kind: String,
    pub source: Option<String>,
    pub fields: ConditionContext,
    /// Timestamp in a caller-defined millisecond domain.
    pub timestamp_ms: u64,
}

impl AutomationEvent {
    #[must_use]
    pub fn new(kind: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            kind: kind.into(),
            source: None,
            fields: ConditionContext::new(),
            timestamp_ms,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventFilter {
    pub kind: String,
    pub source: Option<String>,
    pub conditions: Vec<Condition>,
}

impl EventFilter {
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            source: None,
            conditions: Vec::new(),
        }
    }

    #[must_use]
    pub fn matches(&self, event: &AutomationEvent) -> bool {
        self.kind == event.kind
            && self
                .source
                .as_ref()
                .is_none_or(|source| event.source.as_ref() == Some(source))
            && conditions_match(&self.conditions, &event.fields)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trigger<C> {
    pub id: String,
    pub filter: EventFilter,
    pub delay_ms: u64,
    /// Conditions evaluated against current state when the delayed trigger fires.
    pub conditions: Vec<Condition>,
    pub intents: Vec<CommandIntent<C>>,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingTrigger<C> {
    trigger_id: String,
    event_timestamp_ms: u64,
    intents: Vec<CommandIntent<C>>,
    conditions: Vec<Condition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TriggerFire<C> {
    pub trigger_id: String,
    pub event_timestamp_ms: u64,
    pub scheduled_for_ms: u64,
    pub intents: Vec<CommandIntent<C>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TriggerError {
    DuplicateId(String),
    EmptyActions(String),
    TimestampOverflow { trigger_id: String },
    Scheduler(ActionError),
}

impl fmt::Display for TriggerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "trigger {id} already exists"),
            Self::EmptyActions(id) => write!(formatter, "trigger {id} has no actions"),
            Self::TimestampOverflow { trigger_id } => {
                write!(
                    formatter,
                    "trigger {trigger_id} delay exceeds timestamp range"
                )
            }
            Self::Scheduler(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TriggerError {}

impl From<ActionError> for TriggerError {
    fn from(value: ActionError) -> Self {
        Self::Scheduler(value)
    }
}

#[derive(Clone, Debug)]
pub struct TriggerEngine<C> {
    triggers: Vec<Trigger<C>>,
    pending: ActionQueue<PendingTrigger<C>>,
    pending_ids: HashMap<ActionId, String>,
}

impl<C> Default for TriggerEngine<C> {
    fn default() -> Self {
        Self {
            triggers: Vec::new(),
            pending: ActionQueue::default(),
            pending_ids: HashMap::new(),
        }
    }
}

impl<C: Clone> TriggerEngine<C> {
    /// Registers a trigger. Registration order is the tie-breaker for events
    /// that become due at the same caller timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate identifier or empty action list.
    pub fn insert(&mut self, trigger: Trigger<C>) -> Result<(), TriggerError> {
        if self.triggers.iter().any(|entry| entry.id == trigger.id) {
            return Err(TriggerError::DuplicateId(trigger.id));
        }
        if trigger.intents.is_empty() {
            return Err(TriggerError::EmptyActions(trigger.id));
        }
        self.triggers.push(trigger);
        Ok(())
    }

    /// Matches an event and schedules all corresponding trigger actions.
    ///
    /// # Errors
    ///
    /// Returns an error when a delay overflows or scheduler IDs are exhausted.
    pub fn ingest(&mut self, event: &AutomationEvent) -> Result<Vec<ActionId>, TriggerError> {
        let matches: Vec<_> = self
            .triggers
            .iter()
            .filter(|trigger| trigger.filter.matches(event))
            .map(|trigger| {
                let due = event
                    .timestamp_ms
                    .checked_add(trigger.delay_ms)
                    .ok_or_else(|| TriggerError::TimestampOverflow {
                        trigger_id: trigger.id.clone(),
                    })?;
                Ok((trigger, due))
            })
            .collect::<Result<_, TriggerError>>()?;
        let mut ids = Vec::new();
        for (trigger, due) in matches {
            let scheduled = self.pending.schedule(
                FrameNumber::new(due),
                PendingTrigger {
                    trigger_id: trigger.id.clone(),
                    event_timestamp_ms: event.timestamp_ms,
                    intents: trigger.intents.clone(),
                    conditions: trigger.conditions.clone(),
                },
            );
            let id = match scheduled {
                Ok(id) => id,
                Err(error) => {
                    for id in ids {
                        self.pending.cancel(id);
                        self.pending_ids.remove(&id);
                    }
                    return Err(error.into());
                }
            };
            self.pending_ids.insert(id, trigger.id.clone());
            ids.push(id);
        }
        Ok(ids)
    }

    /// Drains triggers due at or before `now_ms`, preserving due timestamp,
    /// event order, and trigger registration order.
    pub fn poll(&mut self, now_ms: u64, state: &ConditionContext) -> Vec<TriggerFire<C>> {
        self.pending
            .drain_due(FrameNumber::new(now_ms))
            .into_iter()
            .filter_map(|scheduled| {
                self.pending_ids.remove(&scheduled.id);
                conditions_match(&scheduled.action.conditions, state).then_some(TriggerFire {
                    trigger_id: scheduled.action.trigger_id,
                    event_timestamp_ms: scheduled.action.event_timestamp_ms,
                    scheduled_for_ms: scheduled.target_frame.get(),
                    intents: scheduled.action.intents,
                })
            })
            .collect()
    }

    pub fn cancel_action(&mut self, id: ActionId) -> bool {
        self.pending_ids.remove(&id);
        self.pending.cancel(id).is_some()
    }

    pub fn cancel_trigger(&mut self, trigger_id: &str) -> usize {
        let ids: Vec<_> = self
            .pending_ids
            .iter()
            .filter_map(|(id, pending_trigger)| (pending_trigger == trigger_id).then_some(*id))
            .collect();
        let count = ids.len();
        for id in ids {
            self.pending_ids.remove(&id);
            self.pending.cancel(id);
        }
        count
    }

    /// Unregisters a trigger and cancels every action it has already armed, so
    /// a revoked binding cannot fire from the pending queue afterwards.
    pub fn remove(&mut self, trigger_id: &str) -> bool {
        let before = self.triggers.len();
        self.triggers.retain(|trigger| trigger.id != trigger_id);
        if self.triggers.len() == before {
            return false;
        }
        self.cancel_trigger(trigger_id);
        true
    }

    /// Number of registered triggers, for caller-enforced registration bounds.
    #[must_use]
    pub fn registered_len(&self) -> usize {
        self.triggers.len()
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}
