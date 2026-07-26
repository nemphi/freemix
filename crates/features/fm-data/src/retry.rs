use std::{fmt, time::Duration};

/// Deterministic bounded exponential-backoff policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub multiplier: u32,
}

/// Invalid retry configuration or state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetryError {
    ZeroAttempts,
    ZeroMultiplier,
    InitialExceedsMaximum,
    Completed,
    AttemptsExhausted,
}

impl fmt::Display for RetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroAttempts => formatter.write_str("retry policy requires at least one attempt"),
            Self::ZeroMultiplier => {
                formatter.write_str("retry multiplier must be greater than zero")
            }
            Self::InitialExceedsMaximum => {
                formatter.write_str("initial backoff exceeds maximum backoff")
            }
            Self::Completed => formatter.write_str("retry operation is already complete"),
            Self::AttemptsExhausted => formatter.write_str("retry attempts are exhausted"),
        }
    }
}

impl std::error::Error for RetryError {}

impl RetryPolicy {
    /// Validates all policy invariants.
    ///
    /// # Errors
    ///
    /// Returns the applicable [`RetryError`] for an unusable policy.
    pub fn validate(self) -> Result<Self, RetryError> {
        if self.max_attempts == 0 {
            return Err(RetryError::ZeroAttempts);
        }
        if self.multiplier == 0 {
            return Err(RetryError::ZeroMultiplier);
        }
        if self.initial_backoff > self.max_backoff {
            return Err(RetryError::InitialExceedsMaximum);
        }
        Ok(self)
    }

    /// Returns the delay before `attempt`, where attempt 1 has no delay.
    #[must_use]
    pub fn delay_before(self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let exponent = attempt - 2;
        let factor = self.multiplier.saturating_pow(exponent);
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// Recorded result of one attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetryOutcome {
    Succeeded,
    Failed(String),
}

/// Auditable retry record with the delay that preceded the attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryRecord {
    pub attempt: u32,
    pub delay_before: Duration,
    pub outcome: RetryOutcome,
}

/// State machine that records retry outcomes in attempt order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryState {
    policy: RetryPolicy,
    records: Vec<RetryRecord>,
    complete: bool,
}

impl RetryState {
    /// Creates retry state after validating `policy`.
    ///
    /// # Errors
    ///
    /// Returns an error when `policy` is invalid.
    pub fn new(policy: RetryPolicy) -> Result<Self, RetryError> {
        Ok(Self {
            policy: policy.validate()?,
            records: Vec::new(),
            complete: false,
        })
    }

    #[must_use]
    pub fn records(&self) -> &[RetryRecord] {
        &self.records
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub fn next_delay(&self) -> Option<Duration> {
        let attempt = u32::try_from(self.records.len()).ok()?.checked_add(1)?;
        (attempt <= self.policy.max_attempts).then(|| self.policy.delay_before(attempt))
    }

    /// Appends a failed attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation is complete or has no attempt left.
    pub fn record_failure(&mut self, message: impl Into<String>) -> Result<(), RetryError> {
        let attempt = self.next_attempt()?;
        self.records.push(RetryRecord {
            attempt,
            delay_before: self.policy.delay_before(attempt),
            outcome: RetryOutcome::Failed(message.into()),
        });
        if attempt == self.policy.max_attempts {
            self.complete = true;
        }
        Ok(())
    }

    /// Appends a successful attempt and completes the operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation is complete or has no attempt left.
    pub fn record_success(&mut self) -> Result<(), RetryError> {
        let attempt = self.next_attempt()?;
        self.records.push(RetryRecord {
            attempt,
            delay_before: self.policy.delay_before(attempt),
            outcome: RetryOutcome::Succeeded,
        });
        self.complete = true;
        Ok(())
    }

    fn next_attempt(&self) -> Result<u32, RetryError> {
        if self.complete {
            return Err(RetryError::Completed);
        }
        let attempt = u32::try_from(self.records.len())
            .map_err(|_| RetryError::AttemptsExhausted)?
            .checked_add(1)
            .ok_or(RetryError::AttemptsExhausted)?;
        if attempt > self.policy.max_attempts {
            return Err(RetryError::AttemptsExhausted);
        }
        Ok(attempt)
    }
}
