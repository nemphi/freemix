//! Deterministic clocks and clock-domain mapping for `FreeMix` media timelines.

mod cadence;
mod clock;
mod mapping;

pub use cadence::{CadenceError, FrameCadence};
pub use clock::{
    Clock, ClockDomainId, ClockDuration, ClockError, ClockSnapshot, ClockTime, Deadline,
    DeadlineStatus, ManualClock,
};
pub use mapping::{ClockMapping, DriftEstimator, MappingError};
