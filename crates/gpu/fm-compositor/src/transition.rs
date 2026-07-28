use core::fmt;

use fm_video::{BlendError, FrameError, ImageFrame, crossfade};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    Cut,
    Fade,
    /// Independently crossfades every premultiplied RGBA channel.
    AlphaFade,
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
    /// Compiles a Cut, Fade, `AlphaFade`, Wipe, or Slide at an exact rational progress value.
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
            TransitionKind::Cut
            | TransitionKind::Fade
            | TransitionKind::AlphaFade
            | TransitionKind::Wipe
            | TransitionKind::Slide => Ok(Self {
                kind,
                numerator,
                denominator,
            }),
            TransitionKind::Zoom | TransitionKind::Stinger => {
                Err(TransitionError::UnsupportedKind(kind))
            }
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

/// Executes a Cut, exact integer Fade/AlphaFade, horizontal Wipe, or horizontal Slide.
///
/// Cut is atomic and always returns `to`. Fade and `AlphaFade` return byte-identical endpoint
/// clones at zero and full progress through `fm-video`'s reference crossfade. `AlphaFade`
/// intentionally interpolates alpha together with the color channels for transparent content.
/// Wipe replaces columns from left to right, with its boundary at
/// `floor(width * numerator / denominator)`, and also returns byte-identical endpoints.
/// Slide moves Program left and Preview in from the right by the same exact integer offset.
///
/// # Errors
/// Returns an error if Fade, `AlphaFade`, Wipe, or Slide inputs have incompatible layouts.
pub fn execute_transition(
    plan: TransitionPlan,
    from: &ImageFrame,
    to: &ImageFrame,
) -> Result<ImageFrame, TransitionError> {
    match plan.kind {
        TransitionKind::Cut => Ok(to.clone()),
        TransitionKind::Fade | TransitionKind::AlphaFade => {
            Ok(crossfade(from, to, plan.numerator, plan.denominator)?)
        }
        TransitionKind::Wipe => wipe(from, to, plan.numerator, plan.denominator),
        TransitionKind::Slide => slide(from, to, plan.numerator, plan.denominator),
        TransitionKind::Zoom | TransitionKind::Stinger => {
            Err(TransitionError::UnsupportedKind(plan.kind))
        }
    }
}

pub(crate) fn wipe_boundary(width: u32, numerator: u32, denominator: u32) -> u32 {
    let boundary = u64::from(width) * u64::from(numerator) / u64::from(denominator);
    u32::try_from(boundary).expect("wipe boundary cannot exceed frame width")
}

fn wipe(
    from: &ImageFrame,
    to: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<ImageFrame, TransitionError> {
    validate_horizontal_inputs(from, to)?;
    if numerator == 0 {
        return Ok(from.clone());
    }
    if numerator == denominator {
        return Ok(to.clone());
    }

    let boundary = wipe_boundary(from.width(), numerator, denominator);
    let boundary_bytes = usize::try_from(boundary)
        .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
        .checked_mul(4)
        .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
    let mut pixels = from.pixels().to_vec();
    for (output_row, to_row) in pixels
        .chunks_exact_mut(from.stride())
        .zip(to.pixels().chunks_exact(to.stride()))
    {
        output_row[..boundary_bytes].copy_from_slice(&to_row[..boundary_bytes]);
    }
    Ok(
        ImageFrame::new(from.width(), from.height(), from.stride(), pixels)
            .map_err(BlendError::Frame)?,
    )
}

fn slide(
    from: &ImageFrame,
    to: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<ImageFrame, TransitionError> {
    validate_horizontal_inputs(from, to)?;
    if numerator == 0 {
        return Ok(from.clone());
    }
    if numerator == denominator {
        return Ok(to.clone());
    }

    let offset = wipe_boundary(from.width(), numerator, denominator);
    let offset_bytes = usize::try_from(offset)
        .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
        .checked_mul(4)
        .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
    let row_bytes = usize::try_from(from.width())
        .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
        .checked_mul(4)
        .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
    let remaining_bytes = row_bytes - offset_bytes;
    let mut pixels = from.pixels().to_vec();
    for ((output_row, from_row), to_row) in pixels
        .chunks_exact_mut(from.stride())
        .zip(from.pixels().chunks_exact(from.stride()))
        .zip(to.pixels().chunks_exact(to.stride()))
    {
        output_row[..remaining_bytes].copy_from_slice(&from_row[offset_bytes..row_bytes]);
        output_row[remaining_bytes..row_bytes].copy_from_slice(&to_row[..offset_bytes]);
    }
    Ok(
        ImageFrame::new(from.width(), from.height(), from.stride(), pixels)
            .map_err(BlendError::Frame)?,
    )
}

fn validate_horizontal_inputs(from: &ImageFrame, to: &ImageFrame) -> Result<(), BlendError> {
    if from.width() != to.width() {
        return Err(BlendError::WidthMismatch {
            left: from.width(),
            right: to.width(),
        });
    }
    if from.height() != to.height() {
        return Err(BlendError::HeightMismatch {
            left: from.height(),
            right: to.height(),
        });
    }
    if from.stride() != to.stride() {
        return Err(BlendError::StrideMismatch {
            left: from.stride(),
            right: to.stride(),
        });
    }
    Ok(())
}
