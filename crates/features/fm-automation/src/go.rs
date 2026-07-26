use crate::CommandIntent;
use core::fmt;
use fm_command::{IdempotencyKey, MAX_TRANSACTION_COMMANDS};
use fm_scheduler::{ActionError, ActionId, ActionQueue, FrameNumber};
use fm_types::InputId;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub enum GoAction<C> {
    Intent(CommandIntent<C>),
    Delay(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedGoAction<C> {
    pub index: usize,
    pub offset_ms: u64,
    pub intent: CommandIntent<C>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GoPreview<C> {
    pub input: InputId,
    pub actions: Vec<PlannedGoAction<C>>,
    pub total_duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgrammedGo<C> {
    actions: Vec<PlannedGoAction<C>>,
    total_duration_ms: u64,
}

impl<C> ProgrammedGo<C> {
    /// Compiles delays into a previewable, bounded ordered action plan.
    ///
    /// # Errors
    ///
    /// Returns an error for no command actions, too many commands, or duration
    /// overflow.
    pub fn new(actions: impl IntoIterator<Item = GoAction<C>>) -> Result<Self, GoError> {
        let mut offset_ms = 0_u64;
        let mut planned = Vec::new();
        for action in actions {
            match action {
                GoAction::Delay(delay_ms) => {
                    offset_ms = offset_ms
                        .checked_add(delay_ms)
                        .ok_or(GoError::TimestampOverflow)?;
                }
                GoAction::Intent(intent) => {
                    if planned.len() == MAX_TRANSACTION_COMMANDS {
                        return Err(GoError::TooManyActions {
                            maximum: MAX_TRANSACTION_COMMANDS,
                        });
                    }
                    planned.push(PlannedGoAction {
                        index: planned.len(),
                        offset_ms,
                        intent,
                    });
                }
            }
        }
        if planned.is_empty() {
            return Err(GoError::Empty);
        }
        Ok(Self {
            actions: planned,
            total_duration_ms: offset_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct PendingGo<C> {
    run_id: u64,
    action: PlannedGoAction<C>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GoStart<C> {
    pub run_id: u64,
    pub replayed: bool,
    pub preview: GoPreview<C>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GoActionFire<C> {
    pub run_id: u64,
    pub scheduled_for_ms: u64,
    pub action: PlannedGoAction<C>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoError {
    Empty,
    TooManyActions { maximum: usize },
    UnknownInput(InputId),
    TimestampOverflow,
    RunSequenceExhausted,
    Scheduler(ActionError),
}

impl fmt::Display for GoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("programmed GO must contain an action"),
            Self::TooManyActions { maximum } => {
                write!(
                    formatter,
                    "programmed GO may contain at most {maximum} actions"
                )
            }
            Self::UnknownInput(input) => write!(formatter, "input {input} has no programmed GO"),
            Self::TimestampOverflow => formatter.write_str("programmed GO timestamp overflowed"),
            Self::RunSequenceExhausted => formatter.write_str("programmed GO run IDs exhausted"),
            Self::Scheduler(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GoError {}

impl From<ActionError> for GoError {
    fn from(value: ActionError) -> Self {
        Self::Scheduler(value)
    }
}

#[derive(Clone, Debug)]
pub struct GoEngine<C> {
    programs: HashMap<InputId, ProgrammedGo<C>>,
    queue: ActionQueue<PendingGo<C>>,
    action_ids: HashMap<u64, Vec<ActionId>>,
    active: HashSet<u64>,
    receipts: HashMap<IdempotencyKey, GoStart<C>>,
    next_run_id: u64,
}

impl<C> Default for GoEngine<C> {
    fn default() -> Self {
        Self {
            programs: HashMap::new(),
            queue: ActionQueue::default(),
            action_ids: HashMap::new(),
            active: HashSet::new(),
            receipts: HashMap::new(),
            next_run_id: 0,
        }
    }
}

impl<C: Clone> GoEngine<C> {
    pub fn program(&mut self, input: InputId, program: ProgrammedGo<C>) -> Option<ProgrammedGo<C>> {
        self.programs.insert(input, program)
    }

    #[must_use]
    pub fn preview(&self, input: InputId) -> Option<GoPreview<C>> {
        self.programs.get(&input).map(|program| GoPreview {
            input,
            actions: program.actions.clone(),
            total_duration_ms: program.total_duration_ms,
        })
    }

    /// Starts a programmed GO. Reusing an idempotency key returns the original
    /// receipt and never schedules the edge-triggered actions again.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown inputs, timestamp overflow, or exhausted
    /// scheduler/run identifiers.
    pub fn start(
        &mut self,
        input: InputId,
        idempotency_key: IdempotencyKey,
        now_ms: u64,
    ) -> Result<GoStart<C>, GoError> {
        if let Some(receipt) = self.receipts.get(&idempotency_key) {
            let mut replay = receipt.clone();
            replay.replayed = true;
            return Ok(replay);
        }
        let preview = self.preview(input).ok_or(GoError::UnknownInput(input))?;
        let run_id = self
            .next_run_id
            .checked_add(1)
            .ok_or(GoError::RunSequenceExhausted)?;
        let targets: Vec<_> = preview
            .actions
            .iter()
            .map(|action| {
                now_ms
                    .checked_add(action.offset_ms)
                    .ok_or(GoError::TimestampOverflow)
            })
            .collect::<Result<_, _>>()?;

        let mut scheduled = Vec::with_capacity(preview.actions.len());
        for (action, target) in preview.actions.iter().zip(targets) {
            match self.queue.schedule(
                FrameNumber::new(target),
                PendingGo {
                    run_id,
                    action: action.clone(),
                },
            ) {
                Ok(id) => scheduled.push(id),
                Err(error) => {
                    for id in scheduled {
                        self.queue.cancel(id);
                    }
                    return Err(error.into());
                }
            }
        }

        self.next_run_id = run_id;
        self.action_ids.insert(run_id, scheduled);
        self.active.insert(run_id);
        let receipt = GoStart {
            run_id,
            replayed: false,
            preview,
        };
        self.receipts.insert(idempotency_key, receipt.clone());
        Ok(receipt)
    }

    pub fn cancel(&mut self, run_id: u64) -> bool {
        if !self.active.remove(&run_id) {
            return false;
        }
        if let Some(ids) = self.action_ids.remove(&run_id) {
            for id in ids {
                self.queue.cancel(id);
            }
        }
        true
    }

    pub fn poll(&mut self, now_ms: u64) -> Vec<GoActionFire<C>> {
        let due = self.queue.drain_due(FrameNumber::new(now_ms));
        let mut fired = Vec::new();
        for scheduled in due {
            let pending = scheduled.action;
            if !self.active.contains(&pending.run_id) {
                continue;
            }
            if let Some(ids) = self.action_ids.get_mut(&pending.run_id) {
                ids.retain(|id| *id != scheduled.id);
                if ids.is_empty() {
                    self.action_ids.remove(&pending.run_id);
                    self.active.remove(&pending.run_id);
                }
            }
            fired.push(GoActionFire {
                run_id: pending.run_id,
                scheduled_for_ms: scheduled.target_frame.get(),
                action: pending.action,
            });
        }
        fired
    }

    #[must_use]
    pub fn is_active(&self, run_id: u64) -> bool {
        self.active.contains(&run_id)
    }
}
