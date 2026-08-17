use crate::CommandIntent;
use core::fmt;
use fm_scheduler::{ActionError, ActionId, ActionQueue, FrameNumber};
use std::collections::HashMap;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScheduleId(String);

impl ScheduleId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ScheduleId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleKind {
    Once,
    Every { interval_ms: u64 },
}

#[derive(Clone, Debug, PartialEq)]
struct ScheduledIntent<C> {
    id: ScheduleId,
    kind: ScheduleKind,
    intent: CommandIntent<C>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScheduleFire<C> {
    pub id: ScheduleId,
    /// The anchored boundary for this occurrence, not the poll time.
    pub occurrence_ms: u64,
    pub observed_at_ms: u64,
    /// Recurrences skipped to avoid an unbounded late catch-up burst.
    pub missed_occurrences: u64,
    pub intent: CommandIntent<C>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleError {
    DuplicateId(ScheduleId),
    UnknownId(ScheduleId),
    ZeroInterval,
    TimestampOverflow,
    Scheduler(ActionError),
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "schedule {} already exists", id.as_str()),
            Self::UnknownId(id) => write!(formatter, "schedule {} does not exist", id.as_str()),
            Self::ZeroInterval => formatter.write_str("schedule interval must be nonzero"),
            Self::TimestampOverflow => formatter.write_str("schedule timestamp overflowed"),
            Self::Scheduler(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ScheduleError {}

impl From<ActionError> for ScheduleError {
    fn from(value: ActionError) -> Self {
        Self::Scheduler(value)
    }
}

#[derive(Clone, Debug)]
pub struct ScheduleSet<C> {
    queue: ActionQueue<ScheduledIntent<C>>,
    actions: HashMap<ScheduleId, ActionId>,
}

impl<C> Default for ScheduleSet<C> {
    fn default() -> Self {
        Self {
            queue: ActionQueue::default(),
            actions: HashMap::new(),
        }
    }
}

impl<C: Clone> ScheduleSet<C> {
    /// Schedules an intent at an absolute caller timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate IDs, zero intervals, or exhausted IDs.
    pub fn schedule_at(
        &mut self,
        id: impl Into<ScheduleId>,
        at_ms: u64,
        kind: ScheduleKind,
        intent: CommandIntent<C>,
    ) -> Result<(), ScheduleError> {
        let id = id.into();
        if self.actions.contains_key(&id) {
            return Err(ScheduleError::DuplicateId(id));
        }
        if matches!(kind, ScheduleKind::Every { interval_ms: 0 }) {
            return Err(ScheduleError::ZeroInterval);
        }
        let action_id = self.queue.schedule(
            FrameNumber::new(at_ms),
            ScheduledIntent {
                id: id.clone(),
                kind,
                intent,
            },
        )?;
        self.actions.insert(id, action_id);
        Ok(())
    }

    /// Starts a relative timer from an explicit caller timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when `now_ms + delay_ms` overflows or scheduling fails.
    pub fn start_timer(
        &mut self,
        id: impl Into<ScheduleId>,
        now_ms: u64,
        delay_ms: u64,
        intent: CommandIntent<C>,
    ) -> Result<(), ScheduleError> {
        let at_ms = now_ms
            .checked_add(delay_ms)
            .ok_or(ScheduleError::TimestampOverflow)?;
        self.schedule_at(id, at_ms, ScheduleKind::Once, intent)
    }

    pub fn cancel(&mut self, id: &ScheduleId) -> Option<CommandIntent<C>> {
        let action_id = self.actions.remove(id)?;
        self.queue
            .cancel(action_id)
            .map(|entry| entry.action.intent)
    }

    /// Drains due schedules. Repeating schedules remain anchored to their
    /// original boundaries and emit at most once per poll.
    ///
    /// # Errors
    ///
    /// Returns an error if calculating or scheduling a future boundary fails.
    pub fn poll(&mut self, now_ms: u64) -> Result<Vec<ScheduleFire<C>>, ScheduleError> {
        let due = self.queue.drain_due(FrameNumber::new(now_ms));
        let mut fired = Vec::with_capacity(due.len());
        for scheduled in due {
            let action = scheduled.action;
            self.actions.remove(&action.id);
            let occurrence_ms = scheduled.target_frame.get();
            let mut missed_occurrences = 0;
            if let ScheduleKind::Every { interval_ms } = action.kind {
                let elapsed = now_ms - occurrence_ms;
                let intervals = elapsed
                    .checked_div(interval_ms)
                    .and_then(|value| value.checked_add(1))
                    .ok_or(ScheduleError::TimestampOverflow)?;
                missed_occurrences = intervals - 1;
                let advance = intervals
                    .checked_mul(interval_ms)
                    .ok_or(ScheduleError::TimestampOverflow)?;
                let next = occurrence_ms
                    .checked_add(advance)
                    .ok_or(ScheduleError::TimestampOverflow)?;
                let action_id = self.queue.schedule(
                    FrameNumber::new(next),
                    ScheduledIntent {
                        id: action.id.clone(),
                        kind: action.kind,
                        intent: action.intent.clone(),
                    },
                )?;
                self.actions.insert(action.id.clone(), action_id);
            }
            fired.push(ScheduleFire {
                id: action.id,
                occurrence_ms,
                observed_at_ms: now_ms,
                missed_occurrences,
                intent: action.intent,
            });
        }
        Ok(fired)
    }

    #[must_use]
    pub fn contains(&self, id: &ScheduleId) -> bool {
        self.actions.contains_key(id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}
