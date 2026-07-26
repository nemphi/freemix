use core::fmt;

macro_rules! counter {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Returns the next value.
            ///
            /// # Errors
            ///
            /// Returns [`CounterOverflow`] when this counter is exhausted.
            pub fn checked_next(self) -> Result<Self, CounterOverflow> {
                self.0.checked_add(1).map(Self).ok_or(CounterOverflow)
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CounterOverflow;

impl fmt::Display for CounterOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("counter exhausted")
    }
}

impl std::error::Error for CounterOverflow {}

counter!(Revision);
counter!(StateEpoch);
counter!(RuntimeGeneration);
counter!(EventSequence);
counter!(RuntimeSequence);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_typed_and_checked() {
        assert_eq!(Revision::new(4).checked_next(), Ok(Revision::new(5)));
        assert_eq!(Revision::new(u64::MAX).checked_next(), Err(CounterOverflow));
        assert_eq!(RuntimeGeneration::new(9).to_string(), "9");
    }
}
