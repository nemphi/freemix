use core::fmt;

use fm_video::{BlendError, ImageFrame, crossfade};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    Cut,
    Fade,
    Wipe,
    Slide,
    Zoom,
    Stinger,
}

/// Validated transition work. Progress is retained as an exact rational value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionPlan {
    kind: TransitionKind,
    numerator: u32,
    denominator: u32,
}

impl TransitionPlan {
    /// Compiles a Cut or Fade at an exact rational progress value.
    ///
    /// # Errors
    /// Returns an error for a zero denominator, progress beyond the endpoint,
    /// or a transition not implemented in this phase.
    pub const fn compile(
        kind: TransitionKind,
        numerator: u32,
        denominator: u32,
    ) -> Result<Self, TransitionError> {
        if denominator == 0 {
            return Err(TransitionError::ZeroDenominator);
        }
        if numerator > denominator {
            return Err(TransitionError::ProgressOutOfRange {
                numerator,
                denominator,
            });
        }
        match kind {
            TransitionKind::Cut | TransitionKind::Fade => Ok(Self {
                kind,
                numerator,
                denominator,
            }),
            TransitionKind::Wipe
            | TransitionKind::Slide
            | TransitionKind::Zoom
            | TransitionKind::Stinger => Err(TransitionError::UnsupportedKind(kind)),
        }
    }

    #[must_use]
    pub const fn kind(self) -> TransitionKind {
        self.kind
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionError {
    ZeroDenominator,
    ProgressOutOfRange { numerator: u32, denominator: u32 },
    UnsupportedKind(TransitionKind),
    Blend(BlendError),
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDenominator => formatter.write_str("transition denominator must be nonzero"),
            Self::ProgressOutOfRange {
                numerator,
                denominator,
            } => write!(
                formatter,
                "transition progress {numerator}/{denominator} exceeds its endpoint"
            ),
            Self::UnsupportedKind(kind) => {
                write!(
                    formatter,
                    "transition {kind:?} is not supported in this phase"
                )
            }
            Self::Blend(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TransitionError {}

impl From<BlendError> for TransitionError {
    fn from(value: BlendError) -> Self {
        Self::Blend(value)
    }
}

/// Executes a Cut or exact integer Fade between equal-format frames.
///
/// Cut is atomic and always returns `to`. Fade returns byte-identical endpoint
/// clones at zero and full progress through `fm-video`'s reference crossfade.
///
/// # Errors
/// Returns an error if Fade inputs have incompatible layouts.
pub fn execute_transition(
    plan: TransitionPlan,
    from: &ImageFrame,
    to: &ImageFrame,
) -> Result<ImageFrame, TransitionError> {
    match plan.kind {
        TransitionKind::Cut => Ok(to.clone()),
        TransitionKind::Fade => Ok(crossfade(from, to, plan.numerator, plan.denominator)?),
        TransitionKind::Wipe
        | TransitionKind::Slide
        | TransitionKind::Zoom
        | TransitionKind::Stinger => Err(TransitionError::UnsupportedKind(plan.kind)),
    }
}
