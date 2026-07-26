use core::{fmt, num::NonZeroU128};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClockTime(u64);

impl ClockTime {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Adds a duration without wrapping the timeline.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::Overflow`] when the result exceeds `u64`
    /// nanoseconds.
    pub fn checked_add(self, duration: ClockDuration) -> Result<Self, ClockError> {
        self.0
            .checked_add(duration.0)
            .map(Self)
            .ok_or(ClockError::Overflow)
    }

    #[must_use]
    pub const fn duration_since(self, earlier: Self) -> Option<ClockDuration> {
        match self.0.checked_sub(earlier.0) {
            Some(nanos) => Some(ClockDuration(nanos)),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClockDuration(u64);

impl ClockDuration {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClockDomainId(NonZeroU128);

impl ClockDomainId {
    #[must_use]
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU128 {
        self.0
    }
}

impl From<NonZeroU128> for ClockDomainId {
    fn from(value: NonZeroU128) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ClockDomainId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSnapshot {
    domain: ClockDomainId,
    time: ClockTime,
}

impl ClockSnapshot {
    #[must_use]
    pub const fn new(domain: ClockDomainId, time: ClockTime) -> Self {
        Self { domain, time }
    }

    #[must_use]
    pub const fn domain(self) -> ClockDomainId {
        self.domain
    }

    #[must_use]
    pub const fn time(self) -> ClockTime {
        self.time
    }
}

pub trait Clock {
    #[must_use]
    fn snapshot(&self) -> ClockSnapshot;

    #[must_use]
    fn domain(&self) -> ClockDomainId {
        self.snapshot().domain()
    }

    #[must_use]
    fn now(&self) -> ClockTime {
        self.snapshot().time()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockError {
    Overflow,
    TimeRegression {
        current: ClockTime,
        attempted: ClockTime,
    },
}

impl fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => formatter.write_str("clock time overflow"),
            Self::TimeRegression { current, attempted } => write!(
                formatter,
                "clock cannot move backward from {}ns to {}ns",
                current.0, attempted.0
            ),
        }
    }
}

impl std::error::Error for ClockError {}

#[derive(Clone, Debug)]
pub struct ManualClock {
    snapshot: ClockSnapshot,
}

impl ManualClock {
    #[must_use]
    pub const fn new(domain: ClockDomainId, initial_time: ClockTime) -> Self {
        Self {
            snapshot: ClockSnapshot::new(domain, initial_time),
        }
    }

    /// Advances the clock by an exact duration.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::Overflow`] if the resulting time cannot be
    /// represented.
    pub fn advance(&mut self, duration: ClockDuration) -> Result<ClockSnapshot, ClockError> {
        let time = self.snapshot.time.checked_add(duration)?;
        self.snapshot = ClockSnapshot::new(self.snapshot.domain, time);
        Ok(self.snapshot)
    }

    /// Sets the current time while preserving monotonicity.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::TimeRegression`] if `time` is earlier than the
    /// current time.
    pub fn set(&mut self, time: ClockTime) -> Result<ClockSnapshot, ClockError> {
        if time < self.snapshot.time {
            return Err(ClockError::TimeRegression {
                current: self.snapshot.time,
                attempted: time,
            });
        }
        self.snapshot = ClockSnapshot::new(self.snapshot.domain, time);
        Ok(self.snapshot)
    }
}

impl Clock for ManualClock {
    fn snapshot(&self) -> ClockSnapshot {
        self.snapshot
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline(ClockTime);

impl Deadline {
    #[must_use]
    pub const fn at(time: ClockTime) -> Self {
        Self(time)
    }

    /// Creates a deadline relative to `now`.
    ///
    /// # Errors
    ///
    /// Returns [`ClockError::Overflow`] when the deadline is outside the
    /// representable timeline.
    pub fn after(now: ClockTime, duration: ClockDuration) -> Result<Self, ClockError> {
        now.checked_add(duration).map(Self)
    }

    #[must_use]
    pub const fn time(self) -> ClockTime {
        self.0
    }

    #[must_use]
    pub const fn status(self, now: ClockTime) -> DeadlineStatus {
        if now.0 < self.0.0 {
            DeadlineStatus::Pending(ClockDuration(self.0.0 - now.0))
        } else if now.0 == self.0.0 {
            DeadlineStatus::Due
        } else {
            DeadlineStatus::Missed(ClockDuration(now.0 - self.0.0))
        }
    }

    #[must_use]
    pub const fn is_due(self, now: ClockTime) -> bool {
        now.0 >= self.0.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineStatus {
    Pending(ClockDuration),
    Due,
    Missed(ClockDuration),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain() -> ClockDomainId {
        ClockDomainId::new(NonZeroU128::new(1).unwrap())
    }

    #[test]
    fn manual_clock_is_monotonic() {
        let mut clock = ManualClock::new(domain(), ClockTime::from_nanos(10));
        clock.advance(ClockDuration::from_nanos(5)).unwrap();
        assert_eq!(clock.snapshot().time().as_nanos(), 15);
        assert_eq!(
            clock.set(ClockTime::from_nanos(14)),
            Err(ClockError::TimeRegression {
                current: ClockTime::from_nanos(15),
                attempted: ClockTime::from_nanos(14),
            })
        );
        assert_eq!(clock.snapshot().time().as_nanos(), 15);
    }

    #[test]
    fn clock_overflow_does_not_mutate_clock() {
        let mut clock = ManualClock::new(domain(), ClockTime::from_nanos(u64::MAX));
        assert_eq!(
            clock.advance(ClockDuration::from_nanos(1)),
            Err(ClockError::Overflow)
        );
        assert_eq!(clock.snapshot().time().as_nanos(), u64::MAX);
    }

    #[test]
    fn deadline_reports_each_state() {
        let deadline = Deadline::at(ClockTime::from_nanos(20));
        assert_eq!(
            deadline.status(ClockTime::from_nanos(12)),
            DeadlineStatus::Pending(ClockDuration::from_nanos(8))
        );
        assert_eq!(
            deadline.status(ClockTime::from_nanos(20)),
            DeadlineStatus::Due
        );
        assert_eq!(
            deadline.status(ClockTime::from_nanos(23)),
            DeadlineStatus::Missed(ClockDuration::from_nanos(3))
        );
    }
}
