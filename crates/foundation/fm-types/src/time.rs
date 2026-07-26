use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateError {
    ZeroNumerator,
    ZeroDenominator,
}

impl fmt::Display for RateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroNumerator => "rate numerator must be nonzero",
            Self::ZeroDenominator => "rate denominator must be nonzero",
        })
    }
}

impl std::error::Error for RateError {}

macro_rules! rational_rate {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name {
            numerator: u32,
            denominator: u32,
        }

        impl $name {
            /// Creates a normalized rational value.
            ///
            /// # Errors
            ///
            /// Returns [`RateError`] when either part that must be nonzero is zero.
            pub fn new(numerator: u32, denominator: u32) -> Result<Self, RateError> {
                if numerator == 0 {
                    return Err(RateError::ZeroNumerator);
                }
                if denominator == 0 {
                    return Err(RateError::ZeroDenominator);
                }
                let divisor = gcd(numerator, denominator);
                Ok(Self {
                    numerator: numerator / divisor,
                    denominator: denominator / divisor,
                })
            }

            #[must_use]
            pub const fn numerator(self) -> u32 {
                self.numerator
            }

            #[must_use]
            pub const fn denominator(self) -> u32 {
                self.denominator
            }
        }
    };
}

rational_rate!(FrameRate);
rational_rate!(TimeBase);

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaTimestamp(i64);

impl MediaTimestamp {
    #[must_use]
    pub const fn new(ticks: i64) -> Self {
        Self(ticks)
    }

    #[must_use]
    pub const fn ticks(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaDuration(u64);

impl MediaDuration {
    #[must_use]
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    #[must_use]
    pub const fn ticks(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timecode {
    hours: u8,
    minutes: u8,
    seconds: u8,
    frames: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimecodeError {
    HoursOutOfRange,
    MinutesOutOfRange,
    SecondsOutOfRange,
    FrameOutOfRange,
}

impl Timecode {
    /// Creates a timecode in a nominal integer frame-rate domain.
    ///
    /// # Errors
    ///
    /// Returns [`TimecodeError`] when a component is outside its valid range.
    pub fn new(
        hours: u8,
        minutes: u8,
        seconds: u8,
        frames: u8,
        nominal_frames_per_second: u8,
    ) -> Result<Self, TimecodeError> {
        if hours >= 24 {
            return Err(TimecodeError::HoursOutOfRange);
        }
        if minutes >= 60 {
            return Err(TimecodeError::MinutesOutOfRange);
        }
        if seconds >= 60 {
            return Err(TimecodeError::SecondsOutOfRange);
        }
        if frames >= nominal_frames_per_second {
            return Err(TimecodeError::FrameOutOfRange);
        }
        Ok(Self {
            hours,
            minutes,
            seconds,
            frames,
        })
    }

    #[must_use]
    pub const fn components(self) -> (u8, u8, u8, u8) {
        (self.hours, self.minutes, self.seconds, self.frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_rates_are_normalized() {
        let rate = FrameRate::new(60_000, 1_001).unwrap();
        assert_eq!((rate.numerator(), rate.denominator()), (60_000, 1_001));
        assert_eq!(
            FrameRate::new(50, 2).unwrap(),
            FrameRate::new(25, 1).unwrap()
        );
    }

    #[test]
    fn invalid_rates_and_timecodes_are_rejected() {
        assert_eq!(TimeBase::new(1, 0), Err(RateError::ZeroDenominator));
        assert_eq!(
            Timecode::new(0, 0, 0, 30, 30),
            Err(TimecodeError::FrameOutOfRange)
        );
    }
}
