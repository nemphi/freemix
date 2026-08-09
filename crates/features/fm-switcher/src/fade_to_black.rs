use core::fmt;

/// Largest automatic Fade-to-Black duration accepted by the switcher.
pub const MAX_FADE_TO_BLACK_DURATION_FRAMES: u32 = 3_600;

/// Exact fixed denominator shared with the compositor FTB position contract.
pub const FADE_TO_BLACK_POSITION_DENOMINATOR: u32 = u16::MAX as u32;

/// An exact Fade-to-Black position in the closed live-to-black interval.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FadeToBlackPosition(u16);

impl FadeToBlackPosition {
    pub const LIVE: Self = Self(0);
    pub const BLACK: Self = Self(u16::MAX);

    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.0 as u32
    }

    #[must_use]
    pub const fn denominator(self) -> u32 {
        FADE_TO_BLACK_POSITION_DENOMINATOR
    }
}

/// Requested endpoint for an automatic Fade-to-Black move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeToBlackTarget {
    Live,
    Black,
}

impl FadeToBlackTarget {
    #[must_use]
    pub const fn from_active(active: bool) -> Self {
        if active { Self::Black } else { Self::Live }
    }

    #[must_use]
    pub const fn active(self) -> bool {
        matches!(self, Self::Black)
    }

    const fn position(self) -> FadeToBlackPosition {
        match self {
            Self::Live => FadeToBlackPosition::LIVE,
            Self::Black => FadeToBlackPosition::BLACK,
        }
    }
}

/// Exact FTB interval and trajectory progress for one media frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackFrame {
    interval_start: FadeToBlackPosition,
    interval_end: FadeToBlackPosition,
    target: FadeToBlackTarget,
    progress_start_numerator: u32,
    progress_end_numerator: u32,
    progress_denominator: u32,
}

impl FadeToBlackFrame {
    pub const LIVE: Self = Self {
        interval_start: FadeToBlackPosition::LIVE,
        interval_end: FadeToBlackPosition::LIVE,
        target: FadeToBlackTarget::Live,
        progress_start_numerator: 0,
        progress_end_numerator: 0,
        progress_denominator: 1,
    };

    #[must_use]
    pub const fn interval_start(self) -> FadeToBlackPosition {
        self.interval_start
    }

    #[must_use]
    pub const fn interval_end(self) -> FadeToBlackPosition {
        self.interval_end
    }

    #[must_use]
    pub const fn target(self) -> FadeToBlackTarget {
        self.target
    }

    #[must_use]
    pub const fn progress_start_numerator(self) -> u32 {
        self.progress_start_numerator
    }

    #[must_use]
    pub const fn progress_end_numerator(self) -> u32 {
        self.progress_end_numerator
    }

    #[must_use]
    pub const fn progress_denominator(self) -> u32 {
        self.progress_denominator
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackStarted {
    pub from: FadeToBlackPosition,
    pub target: FadeToBlackTarget,
    pub duration_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FadeToBlackRequest {
    Unchanged,
    Started(FadeToBlackStarted),
    Completed(FadeToBlackTarget),
}

/// Allocation-free result of advancing the FTB controller by one frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FadeToBlackAdvance {
    pub position_changed: Option<FadeToBlackPosition>,
    pub completed: Option<FadeToBlackTarget>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomaticFadeToBlack {
    start: FadeToBlackPosition,
    target: FadeToBlackTarget,
    duration_frames: u32,
    elapsed_frames: u32,
}

impl AutomaticFadeToBlack {
    fn position_at(self, elapsed_frames: u32) -> FadeToBlackPosition {
        let start = self.start.0;
        let target = self.target.position().0;
        let distance = start.abs_diff(target);
        let offset = u32::from(distance) * elapsed_frames / self.duration_frames;
        let offset = u16::try_from(offset).expect("interpolated FTB offset is within its endpoint");
        if target >= start {
            FadeToBlackPosition(start + offset)
        } else {
            FadeToBlackPosition(start - offset)
        }
    }

    fn frame(self) -> FadeToBlackFrame {
        let progress_end = self.elapsed_frames.saturating_add(1);
        let progress_end = if progress_end < self.duration_frames {
            progress_end
        } else {
            self.duration_frames
        };
        FadeToBlackFrame {
            interval_start: self.position_at(self.elapsed_frames),
            interval_end: self.position_at(progress_end),
            target: self.target,
            progress_start_numerator: self.elapsed_frames,
            progress_end_numerator: progress_end,
            progress_denominator: self.duration_frames,
        }
    }
}

/// Bounded deterministic automatic Fade-to-Black control state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackController {
    position: FadeToBlackPosition,
    target: FadeToBlackTarget,
    automatic: Option<AutomaticFadeToBlack>,
}

impl Default for FadeToBlackController {
    fn default() -> Self {
        Self {
            position: FadeToBlackPosition::LIVE,
            target: FadeToBlackTarget::Live,
            automatic: None,
        }
    }
}

impl FadeToBlackController {
    #[must_use]
    pub const fn settled(target: FadeToBlackTarget) -> Self {
        Self {
            position: target.position(),
            target,
            automatic: None,
        }
    }

    #[must_use]
    pub const fn position(self) -> FadeToBlackPosition {
        self.position
    }

    #[must_use]
    pub const fn target(self) -> FadeToBlackTarget {
        self.target
    }

    #[must_use]
    pub const fn is_automatic(self) -> bool {
        self.automatic.is_some()
    }

    #[must_use]
    pub fn frame(self) -> FadeToBlackFrame {
        match self.automatic {
            Some(automatic) => automatic.frame(),
            None => FadeToBlackFrame {
                interval_start: self.position,
                interval_end: self.position,
                target: self.target,
                progress_start_numerator: 0,
                progress_end_numerator: 0,
                progress_denominator: 1,
            },
        }
    }

    /// Starts or reverses an automatic move from the current exact position.
    ///
    /// Repeating the current target is idempotent and does not restart progress.
    ///
    /// # Errors
    ///
    /// Returns an error when `duration_frames` is zero or exceeds the bound.
    pub fn request(
        &mut self,
        target: FadeToBlackTarget,
        duration_frames: u32,
    ) -> Result<FadeToBlackRequest, FadeToBlackError> {
        validate_duration(duration_frames)?;
        if self.target == target {
            return Ok(FadeToBlackRequest::Unchanged);
        }
        if self.position == target.position() {
            self.target = target;
            self.automatic = None;
            return Ok(FadeToBlackRequest::Completed(target));
        }

        let started = FadeToBlackStarted {
            from: self.position,
            target,
            duration_frames,
        };
        self.target = target;
        self.automatic = Some(AutomaticFadeToBlack {
            start: self.position,
            target,
            duration_frames,
            elapsed_frames: 0,
        });
        Ok(FadeToBlackRequest::Started(started))
    }

    /// Advances exactly one frame without allocation.
    pub fn advance(&mut self) -> FadeToBlackAdvance {
        let Some(mut automatic) = self.automatic else {
            return FadeToBlackAdvance {
                position_changed: None,
                completed: None,
            };
        };
        let previous = self.position;
        automatic.elapsed_frames += 1;
        self.position = automatic.position_at(automatic.elapsed_frames);
        let completed = automatic.elapsed_frames >= automatic.duration_frames;
        if completed {
            self.position = automatic.target.position();
            self.automatic = None;
        } else {
            self.automatic = Some(automatic);
        }
        FadeToBlackAdvance {
            position_changed: if self.position == previous {
                None
            } else {
                Some(self.position)
            },
            completed: if completed {
                Some(automatic.target)
            } else {
                None
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FadeToBlackError {
    ZeroDuration,
    DurationLimit { duration_frames: u32, maximum: u32 },
}

impl fmt::Display for FadeToBlackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDuration => formatter.write_str("Fade-to-Black duration must be nonzero"),
            Self::DurationLimit {
                duration_frames,
                maximum,
            } => write!(
                formatter,
                "Fade-to-Black duration {duration_frames} exceeds {maximum} frames"
            ),
        }
    }
}

impl std::error::Error for FadeToBlackError {}

fn validate_duration(duration_frames: u32) -> Result<(), FadeToBlackError> {
    if duration_frames == 0 {
        Err(FadeToBlackError::ZeroDuration)
    } else if duration_frames > MAX_FADE_TO_BLACK_DURATION_FRAMES {
        Err(FadeToBlackError::DurationLimit {
            duration_frames,
            maximum: MAX_FADE_TO_BLACK_DURATION_FRAMES,
        })
    } else {
        Ok(())
    }
}
