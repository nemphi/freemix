use crate::{FrameError, ImageFrame, Rgba8};
use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlendError {
    ZeroDenominator,
    NumeratorExceedsDenominator { numerator: u32, denominator: u32 },
    WidthMismatch { left: u32, right: u32 },
    HeightMismatch { left: u32, right: u32 },
    StrideMismatch { left: usize, right: usize },
    Frame(FrameError),
}

impl fmt::Display for BlendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDenominator => formatter.write_str("blend denominator must be nonzero"),
            Self::NumeratorExceedsDenominator {
                numerator,
                denominator,
            } => write!(
                formatter,
                "blend numerator {numerator} exceeds denominator {denominator}"
            ),
            Self::WidthMismatch { left, right } => {
                write!(formatter, "frame widths differ: {left} and {right}")
            }
            Self::HeightMismatch { left, right } => {
                write!(formatter, "frame heights differ: {left} and {right}")
            }
            Self::StrideMismatch { left, right } => {
                write!(formatter, "frame strides differ: {left} and {right}")
            }
            Self::Frame(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BlendError {}

impl From<FrameError> for BlendError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

/// Crossfades two equal-format RGBA8 frames with rounded integer arithmetic.
///
/// `numerator == 0` returns `left`; `numerator == denominator` returns `right`.
///
/// # Errors
///
/// Returns a typed error for an invalid ratio, unequal dimensions or stride,
/// or an output allocation failure.
pub fn crossfade(
    left: &ImageFrame,
    right: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<ImageFrame, BlendError> {
    validate_inputs(left, right, numerator, denominator)?;
    if numerator == 0 {
        return Ok(left.clone());
    }
    if numerator == denominator {
        return Ok(right.clone());
    }

    let mut output = ImageFrame::new(
        left.width(),
        left.height(),
        left.stride(),
        vec![0; left.pixels().len()],
    )?;
    for y in 0..left.height() {
        for x in 0..left.width() {
            let (Some(first), Some(second)) = (left.pixel(x, y), right.pixel(x, y)) else {
                return Err(BlendError::Frame(FrameError::LayoutOverflow));
            };
            output.set_pixel(
                x,
                y,
                Rgba8::new(
                    blend_channel(first.r, second.r, numerator, denominator),
                    blend_channel(first.g, second.g, numerator, denominator),
                    blend_channel(first.b, second.b, numerator, denominator),
                    blend_channel(first.a, second.a, numerator, denominator),
                ),
            );
        }
    }
    Ok(output)
}

fn validate_inputs(
    left: &ImageFrame,
    right: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<(), BlendError> {
    if denominator == 0 {
        return Err(BlendError::ZeroDenominator);
    }
    if numerator > denominator {
        return Err(BlendError::NumeratorExceedsDenominator {
            numerator,
            denominator,
        });
    }
    if left.width() != right.width() {
        return Err(BlendError::WidthMismatch {
            left: left.width(),
            right: right.width(),
        });
    }
    if left.height() != right.height() {
        return Err(BlendError::HeightMismatch {
            left: left.height(),
            right: right.height(),
        });
    }
    if left.stride() != right.stride() {
        return Err(BlendError::StrideMismatch {
            left: left.stride(),
            right: right.stride(),
        });
    }
    Ok(())
}

fn blend_channel(left: u8, right: u8, numerator: u32, denominator: u32) -> u8 {
    let numerator = u64::from(numerator);
    let denominator = u64::from(denominator);
    let inverse = denominator - numerator;
    let value = u64::from(left) * inverse + u64::from(right) * numerator + denominator / 2;
    u8::try_from(value / denominator).unwrap_or(u8::MAX)
}
