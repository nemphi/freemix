use crate::{CommandIntent, Condition, ConditionContext, condition::conditions_match};
use core::fmt;
use fm_command::{CommandEnvelope, Transaction, TransactionError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u16,
    pub delay_ms: u64,
}

impl RetryPolicy {
    #[must_use]
    pub const fn once() -> Self {
        Self {
            max_attempts: 1,
            delay_ms: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelPolicy {
    Immediate,
    FinishCurrentAttempt,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MacroDefinition<C> {
    pub id: String,
    intents: Transaction<CommandIntent<C>>,
    pub conditions: Vec<Condition>,
    pub retry: RetryPolicy,
    pub cancel_policy: CancelPolicy,
}

impl<C> MacroDefinition<C> {
    /// Creates a macro backed by `fm-command`'s bounded transaction type.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty/oversized command list or zero attempts.
    pub fn new(
        id: impl Into<String>,
        intents: impl IntoIterator<Item = CommandIntent<C>>,
        conditions: Vec<Condition>,
        retry: RetryPolicy,
        cancel_policy: CancelPolicy,
    ) -> Result<Self, MacroError> {
        if retry.max_attempts == 0 {
            return Err(MacroError::ZeroAttempts);
        }
        Ok(Self {
            id: id.into(),
            intents: Transaction::new(intents)?,
            conditions,
            retry,
            cancel_policy,
        })
    }

    #[must_use]
    pub const fn transaction(&self) -> &Transaction<CommandIntent<C>> {
        &self.intents
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MacroError {
    InvalidTransaction(TransactionError),
    ZeroAttempts,
    ConditionFailed,
    NotReady { ready_at_ms: u64 },
    NotRunning,
    TimestampOverflow,
}

impl fmt::Display for MacroError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransaction(error) => error.fmt(formatter),
            Self::ZeroAttempts => formatter.write_str("macro retry policy requires an attempt"),
            Self::ConditionFailed => formatter.write_str("macro conditions are not satisfied"),
            Self::NotReady { ready_at_ms } => {
                write!(formatter, "macro retry is not ready before {ready_at_ms}ms")
            }
            Self::NotRunning => formatter.write_str("macro is not running"),
            Self::TimestampOverflow => formatter.write_str("macro retry timestamp overflowed"),
        }
    }
}

impl std::error::Error for MacroError {}

impl From<TransactionError> for MacroError {
    fn from(value: TransactionError) -> Self {
        Self::InvalidTransaction(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptOutcome {
    Succeeded,
    Failed { retryable: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacroStatus {
    Running,
    WaitingRetry { ready_at_ms: u64 },
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MacroDispatch<C> {
    pub macro_id: String,
    pub run_id: String,
    pub attempt: u16,
    pub envelope: CommandEnvelope<Transaction<CommandIntent<C>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacroDecision {
    Succeeded,
    RetryAt(u64),
    Failed,
    Cancelled,
    CancellationPending,
}

#[derive(Clone, Debug)]
pub struct MacroRun<C> {
    definition: MacroDefinition<C>,
    run_id: String,
    attempt: u16,
    status: MacroStatus,
    cancel_requested: bool,
}

impl<C: Clone> MacroRun<C> {
    /// Starts a macro and returns its complete atomic command plan.
    ///
    /// # Errors
    ///
    /// Returns [`MacroError::ConditionFailed`] without creating a partial plan.
    pub fn start(
        definition: MacroDefinition<C>,
        run_id: impl Into<String>,
        context: &ConditionContext,
    ) -> Result<(Self, MacroDispatch<C>), MacroError> {
        if !conditions_match(&definition.conditions, context) {
            return Err(MacroError::ConditionFailed);
        }
        let run_id = run_id.into();
        let run = Self {
            definition,
            run_id,
            attempt: 1,
            status: MacroStatus::Running,
            cancel_requested: false,
        };
        let dispatch = run.dispatch();
        Ok((run, dispatch))
    }

    #[must_use]
    pub const fn status(&self) -> MacroStatus {
        self.status
    }

    #[must_use]
    pub const fn attempt(&self) -> u16 {
        self.attempt
    }

    pub fn cancel(&mut self) -> MacroDecision {
        match self.status {
            MacroStatus::Running
                if self.definition.cancel_policy == CancelPolicy::FinishCurrentAttempt =>
            {
                self.cancel_requested = true;
                MacroDecision::CancellationPending
            }
            MacroStatus::Succeeded | MacroStatus::Failed | MacroStatus::Cancelled => {
                MacroDecision::Cancelled
            }
            MacroStatus::Running | MacroStatus::WaitingRetry { .. } => {
                self.status = MacroStatus::Cancelled;
                MacroDecision::Cancelled
            }
        }
    }

    /// Records one atomic transaction outcome and decides retry/cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error unless an attempt is currently in flight.
    pub fn complete(
        &mut self,
        outcome: AttemptOutcome,
        now_ms: u64,
    ) -> Result<MacroDecision, MacroError> {
        if self.status != MacroStatus::Running {
            return Err(MacroError::NotRunning);
        }
        if self.cancel_requested {
            self.status = MacroStatus::Cancelled;
            return Ok(MacroDecision::Cancelled);
        }
        match outcome {
            AttemptOutcome::Succeeded => {
                self.status = MacroStatus::Succeeded;
                Ok(MacroDecision::Succeeded)
            }
            AttemptOutcome::Failed { retryable }
                if retryable && self.attempt < self.definition.retry.max_attempts =>
            {
                let ready_at_ms = now_ms
                    .checked_add(self.definition.retry.delay_ms)
                    .ok_or(MacroError::TimestampOverflow)?;
                self.status = MacroStatus::WaitingRetry { ready_at_ms };
                Ok(MacroDecision::RetryAt(ready_at_ms))
            }
            AttemptOutcome::Failed { .. } => {
                self.status = MacroStatus::Failed;
                Ok(MacroDecision::Failed)
            }
        }
    }

    /// Dispatches the next retry using a new idempotency key.
    ///
    /// # Errors
    ///
    /// Returns an error before the caller-provided retry boundary or when no
    /// retry is waiting.
    pub fn retry(&mut self, now_ms: u64) -> Result<MacroDispatch<C>, MacroError> {
        let MacroStatus::WaitingRetry { ready_at_ms } = self.status else {
            return Err(MacroError::NotRunning);
        };
        if now_ms < ready_at_ms {
            return Err(MacroError::NotReady { ready_at_ms });
        }
        self.attempt += 1;
        self.status = MacroStatus::Running;
        Ok(self.dispatch())
    }

    fn dispatch(&self) -> MacroDispatch<C> {
        let command_id = format!("{}:{}:{}", self.definition.id, self.run_id, self.attempt);
        MacroDispatch {
            macro_id: self.definition.id.clone(),
            run_id: self.run_id.clone(),
            attempt: self.attempt,
            envelope: CommandEnvelope::new(
                command_id.clone(),
                command_id,
                self.definition.intents.clone(),
            ),
        }
    }
}
