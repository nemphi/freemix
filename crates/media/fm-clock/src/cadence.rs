use core::fmt;

use fm_types::FrameRate;

use crate::{ClockDuration, ClockTime};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CadenceError {
    ArithmeticOverflow,
    BeforeOrigin,
}

impl fmt::Display for CadenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArithmeticOverflow => "frame cadence arithmetic overflow",
            Self::BeforeOrigin => "clock time is before the frame cadence origin",
        })
    }
}

impl std::error::Error for CadenceError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameCadence {
    rate: FrameRate,
    origin: ClockTime,
}

impl FrameCadence {
    #[must_use]
    pub const fn new(rate: FrameRate, origin: ClockTime) -> Self {
        Self { rate, origin }
    }

    #[must_use]
    pub const fn rate(self) -> FrameRate {
        self.rate
    }

    #[must_use]
    pub const fn origin(self) -> ClockTime {
        self.origin
    }

    /// Returns the clock time at which `frame` begins.
    ///
    /// # Errors
    ///
    /// Returns [`CadenceError::ArithmeticOverflow`] if the frame position or
    /// origin sum is outside the representable timeline.
    pub fn time_of_frame(self, frame: u64) -> Result<ClockTime, CadenceError> {
        let offset = self.offset_of_frame(frame)?;
        self.origin
            .as_nanos()
            .checked_add(offset)
            .map(ClockTime::from_nanos)
            .ok_or(CadenceError::ArithmeticOverflow)
    }

    /// Returns the exact integer-nanosecond duration assigned to `frame`.
    ///
    /// Adjacent durations can differ by one nanosecond for fractional rates.
    ///
    /// # Errors
    ///
    /// Returns [`CadenceError::ArithmeticOverflow`] when the frame index or
    /// calculated positions cannot be represented.
    pub fn duration_of_frame(self, frame: u64) -> Result<ClockDuration, CadenceError> {
        let next = frame
            .checked_add(1)
            .ok_or(CadenceError::ArithmeticOverflow)?;
        let start = self.offset_of_frame(frame)?;
        let end = self.offset_of_frame(next)?;
        Ok(ClockDuration::from_nanos(end - start))
    }

    /// Finds the latest frame whose start is not later than `time`.
    ///
    /// # Errors
    ///
    /// Returns [`CadenceError::BeforeOrigin`] when `time` predates this
    /// cadence, or [`CadenceError::ArithmeticOverflow`] if the frame index
    /// cannot be represented.
    pub fn frame_at_or_before(self, time: ClockTime) -> Result<u64, CadenceError> {
        let elapsed = time
            .as_nanos()
            .checked_sub(self.origin.as_nanos())
            .ok_or(CadenceError::BeforeOrigin)?;
        let numerator = u128::from(elapsed)
            .checked_add(1)
            .and_then(|value| value.checked_mul(u128::from(self.rate.numerator())))
            .ok_or(CadenceError::ArithmeticOverflow)?;
        let denominator = u128::from(self.rate.denominator()).saturating_mul(NANOS_PER_SECOND);
        let frame = numerator
            .checked_sub(1)
            .ok_or(CadenceError::ArithmeticOverflow)?
            / denominator;
        u64::try_from(frame).map_err(|_| CadenceError::ArithmeticOverflow)
    }

    fn offset_of_frame(self, frame: u64) -> Result<u64, CadenceError> {
        let numerator = u128::from(frame)
            .checked_mul(u128::from(self.rate.denominator()))
            .and_then(|value| value.checked_mul(NANOS_PER_SECOND))
            .ok_or(CadenceError::ArithmeticOverflow)?;
        let nanos = numerator / u128::from(self.rate.numerator());
        u64::try_from(nanos).map_err(|_| CadenceError::ArithmeticOverflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntsc_60() -> FrameCadence {
        FrameCadence::new(
            FrameRate::new(60_000, 1_001).unwrap(),
            ClockTime::from_nanos(0),
        )
    }

    #[test]
    fn ntsc_cadence_alternates_integer_nanosecond_durations() {
        let cadence = ntsc_60();
        assert_eq!(cadence.time_of_frame(1).unwrap().as_nanos(), 16_683_333);
        assert_eq!(cadence.duration_of_frame(0).unwrap().as_nanos(), 16_683_333);
        assert_eq!(cadence.duration_of_frame(1).unwrap().as_nanos(), 16_683_333);
        assert_eq!(cadence.duration_of_frame(2).unwrap().as_nanos(), 16_683_334);
    }

    #[test]
    fn long_run_position_is_calculated_without_accumulated_drift() {
        let cadence = ntsc_60();
        let frames = 60_000 * 60 * 24;
        let expected = u128::from(frames) * 1_001 * NANOS_PER_SECOND / 60_000;
        assert_eq!(
            cadence.time_of_frame(frames).unwrap().as_nanos(),
            u64::try_from(expected).unwrap()
        );
    }

    #[test]
    fn frame_and_time_conversions_are_boundary_consistent() {
        let cadence = FrameCadence::new(
            FrameRate::new(24_000, 1_001).unwrap(),
            ClockTime::from_nanos(7),
        );
        for frame in [0, 1, 2, 10, 1_000, 1_000_000] {
            let time = cadence.time_of_frame(frame).unwrap();
            assert_eq!(cadence.frame_at_or_before(time).unwrap(), frame);
            if frame > 0 {
                assert_eq!(
                    cadence
                        .frame_at_or_before(ClockTime::from_nanos(time.as_nanos() - 1))
                        .unwrap(),
                    frame - 1
                );
            }
        }
    }

    #[test]
    fn time_before_origin_is_rejected() {
        let cadence = FrameCadence::new(FrameRate::new(25, 1).unwrap(), ClockTime::from_nanos(100));
        assert_eq!(
            cadence.frame_at_or_before(ClockTime::from_nanos(99)),
            Err(CadenceError::BeforeOrigin)
        );
    }
}
