use crate::{FrameDeadline, FrameNumber};
use core::fmt;
use fm_types::FrameRate;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacingError {
    DeadlineOverflow,
    FrameNumberExhausted,
}

impl fmt::Display for PacingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeadlineOverflow => "frame deadline exceeds the nanosecond timeline",
            Self::FrameNumberExhausted => "frame number space exhausted",
        })
    }
}

impl std::error::Error for PacingError {}

/// An exact rational frame timeline with a deterministic cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramePacer {
    frame_rate: FrameRate,
    origin_ns: u64,
    next_frame: FrameNumber,
}

impl FramePacer {
    #[must_use]
    pub const fn new(frame_rate: FrameRate, origin_ns: u64) -> Self {
        Self {
            frame_rate,
            origin_ns,
            next_frame: FrameNumber::new(0),
        }
    }

    /// Restores a pacer whose next frame is `next_frame`.
    ///
    /// # Errors
    ///
    /// Returns an error when the next frame's exact deadline cannot fit in the
    /// nanosecond timeline or the next frame cannot be consumed without
    /// exhausting the frame number space.
    pub fn restore(
        frame_rate: FrameRate,
        origin_ns: u64,
        next_frame: FrameNumber,
    ) -> Result<Self, PacingError> {
        let pacer = Self {
            frame_rate,
            origin_ns,
            next_frame,
        };
        pacer.next_deadline()?;
        next_frame
            .get()
            .checked_add(1)
            .ok_or(PacingError::FrameNumberExhausted)?;
        Ok(pacer)
    }

    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    #[must_use]
    pub const fn next_frame(&self) -> FrameNumber {
        self.next_frame
    }

    /// Computes the deadline of a frame without advancing the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`PacingError::DeadlineOverflow`] when the deadline cannot fit
    /// in the scheduler's nanosecond timeline.
    pub fn deadline_for(&self, frame: FrameNumber) -> Result<FrameDeadline, PacingError> {
        let offset =
            u128::from(frame.get()) * u128::from(self.frame_rate.denominator()) * NANOS_PER_SECOND
                / u128::from(self.frame_rate.numerator());
        let at_ns = u128::from(self.origin_ns)
            .checked_add(offset)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(PacingError::DeadlineOverflow)?;
        Ok(FrameDeadline { frame, at_ns })
    }

    /// Returns the next deadline without advancing the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`PacingError::DeadlineOverflow`] when the deadline cannot fit
    /// in the scheduler's nanosecond timeline.
    pub fn next_deadline(&self) -> Result<FrameDeadline, PacingError> {
        self.deadline_for(self.next_frame)
    }

    /// Advances by one frame and returns the consumed deadline.
    ///
    /// # Errors
    ///
    /// Returns an error when either the deadline or next frame number would
    /// overflow.
    pub fn advance(&mut self) -> Result<FrameDeadline, PacingError> {
        let deadline = self.next_deadline()?;
        let next = self
            .next_frame
            .get()
            .checked_add(1)
            .ok_or(PacingError::FrameNumberExhausted)?;
        self.next_frame = FrameNumber::new(next);
        Ok(deadline)
    }
}
