use core::fmt;

use fm_color::{ColorError, LinearFrame, LinearRgba};

/// Largest denominator accepted by the Fade-to-Black planning primitive.
///
/// Each numerator and denominator remains exactly representable by the `f32`
/// arithmetic used by the CPU oracle and WGSL implementation.
pub const MAX_FADE_TO_BLACK_DENOMINATOR: u32 = u16::MAX as u32;

/// An exact rational position in the closed Fade-to-Black interval `[0, 1]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackPosition {
    numerator: u16,
    denominator: u16,
}

impl FadeToBlackPosition {
    pub const LIVE: Self = Self {
        numerator: 0,
        denominator: 1,
    };
    pub const BLACK: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    /// Validates an exact bounded Fade-to-Black position.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero or oversized denominator, or a numerator
    /// beyond the opaque-black endpoint.
    #[allow(clippy::cast_possible_truncation)]
    pub const fn compile(numerator: u32, denominator: u32) -> Result<Self, FadeToBlackPlanError> {
        if denominator == 0 {
            return Err(FadeToBlackPlanError::ZeroDenominator);
        }
        if denominator > MAX_FADE_TO_BLACK_DENOMINATOR {
            return Err(FadeToBlackPlanError::DenominatorLimit {
                denominator,
                maximum: MAX_FADE_TO_BLACK_DENOMINATOR,
            });
        }
        if numerator > denominator {
            return Err(FadeToBlackPlanError::PositionOutOfRange {
                numerator,
                denominator,
            });
        }
        Ok(Self {
            numerator: numerator as u16,
            denominator: denominator as u16,
        })
    }

    #[must_use]
    pub const fn numerator(self) -> u16 {
        self.numerator
    }

    #[must_use]
    pub const fn denominator(self) -> u16 {
        self.denominator
    }

    fn as_f32(self) -> f32 {
        f32::from(self.numerator) / f32::from(self.denominator)
    }
}

/// One deterministic Fade-to-Black application plan.
///
/// `progress` interpolates from `start` to `end`. Equal endpoints hold the
/// current FTB position, and a start greater than the end reverses toward live.
/// This is compositor work only: the plan contains no clock, audio, routing, or
/// operator state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackPlan {
    start: FadeToBlackPosition,
    end: FadeToBlackPosition,
    progress: FadeToBlackPosition,
}

impl FadeToBlackPlan {
    #[must_use]
    pub const fn new(
        start: FadeToBlackPosition,
        end: FadeToBlackPosition,
        progress: FadeToBlackPosition,
    ) -> Self {
        Self {
            start,
            end,
            progress,
        }
    }

    #[must_use]
    pub const fn start(self) -> FadeToBlackPosition {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> FadeToBlackPosition {
        self.end
    }

    #[must_use]
    pub const fn progress(self) -> FadeToBlackPosition {
        self.progress
    }

    pub(crate) fn resolved_position(self) -> f32 {
        if self.progress.numerator == 0 || self.start == self.end {
            return self.start.as_f32();
        }
        if self.progress.numerator == self.progress.denominator {
            return self.end.as_f32();
        }
        let start = self.start.as_f32();
        (self.end.as_f32() - start).mul_add(self.progress.as_f32(), start)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeToBlackPlanError {
    ZeroDenominator,
    DenominatorLimit { denominator: u32, maximum: u32 },
    PositionOutOfRange { numerator: u32, denominator: u32 },
}

impl fmt::Display for FadeToBlackPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDenominator => {
                formatter.write_str("Fade-to-Black denominator must be nonzero")
            }
            Self::DenominatorLimit {
                denominator,
                maximum,
            } => write!(
                formatter,
                "Fade-to-Black denominator {denominator} exceeds {maximum}"
            ),
            Self::PositionOutOfRange {
                numerator,
                denominator,
            } => write!(
                formatter,
                "Fade-to-Black position {numerator}/{denominator} exceeds opaque black"
            ),
        }
    }
}

impl std::error::Error for FadeToBlackPlanError {}

/// Applies Fade-to-Black to canonical premultiplied linear pixels.
///
/// The source is mixed directly toward opaque black `(0, 0, 0, 1)` with no
/// color-space conversion. RGB is attenuated while alpha approaches one.
/// Making the full endpoint opaque is deliberate output behavior: transparent
/// Program pixels cannot reveal a downstream surface while FTB is engaged.
/// Exact live and black endpoints bypass interpolation.
///
/// # Errors
///
/// Returns an error only if constructing the same-sized canonical output frame
/// fails.
pub fn execute_fade_to_black_cpu(
    plan: FadeToBlackPlan,
    source: &LinearFrame,
) -> Result<LinearFrame, ColorError> {
    let position = plan.resolved_position();
    if position <= 0.0 {
        return Ok(source.clone());
    }

    let pixels = if position >= 1.0 {
        vec![LinearRgba::new(0.0, 0.0, 0.0, 1.0); source.pixels().len()]
    } else {
        source
            .pixels()
            .iter()
            .copied()
            .map(|pixel| {
                LinearRgba::new(
                    mix(pixel.r, 0.0, position),
                    mix(pixel.g, 0.0, position),
                    mix(pixel.b, 0.0, position),
                    mix(pixel.a, 1.0, position),
                )
            })
            .collect()
    };
    LinearFrame::new(source.width(), source.height(), pixels)
}

fn mix(from: f32, to: f32, progress: f32) -> f32 {
    (to - from).mul_add(progress, from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(numerator: u32, denominator: u32) -> FadeToBlackPosition {
        FadeToBlackPosition::compile(numerator, denominator).unwrap()
    }

    fn frame() -> LinearFrame {
        LinearFrame::new(
            2,
            1,
            vec![
                LinearRgba::new(4.0, 1.5, 0.25, 0.5),
                LinearRgba::new(0.0, 0.0, 0.0, 0.0),
            ],
        )
        .unwrap()
    }

    #[test]
    fn exact_live_middle_and_black_pixels_cover_hdr_and_fractional_alpha() {
        let source = frame();
        let live = FadeToBlackPlan::new(
            FadeToBlackPosition::LIVE,
            FadeToBlackPosition::BLACK,
            position(0, 9),
        );
        assert_eq!(execute_fade_to_black_cpu(live, &source).unwrap(), source);

        let middle = FadeToBlackPlan::new(
            FadeToBlackPosition::LIVE,
            FadeToBlackPosition::BLACK,
            position(1, 2),
        );
        assert_eq!(
            execute_fade_to_black_cpu(middle, &source).unwrap().pixels(),
            &[
                LinearRgba::new(2.0, 0.75, 0.125, 0.75),
                LinearRgba::new(0.0, 0.0, 0.0, 0.5),
            ]
        );

        let black = FadeToBlackPlan::new(
            FadeToBlackPosition::LIVE,
            FadeToBlackPosition::BLACK,
            position(7, 7),
        );
        assert_eq!(
            execute_fade_to_black_cpu(black, &source).unwrap().pixels(),
            &[
                LinearRgba::new(0.0, 0.0, 0.0, 1.0),
                LinearRgba::new(0.0, 0.0, 0.0, 1.0),
            ]
        );
    }

    #[test]
    fn arbitrary_endpoints_support_reverse_and_hold() {
        let source = frame();
        let reverse = FadeToBlackPlan::new(position(3, 4), position(1, 4), position(1, 2));
        let reverse_output = execute_fade_to_black_cpu(reverse, &source).unwrap();
        assert_eq!(
            reverse_output.pixel(0, 0),
            Some(LinearRgba::new(2.0, 0.75, 0.125, 0.75))
        );

        let hold = FadeToBlackPlan::new(position(1, 4), position(1, 4), position(2, 3));
        let hold_output = execute_fade_to_black_cpu(hold, &source).unwrap();
        assert_eq!(
            hold_output.pixel(0, 0),
            Some(LinearRgba::new(3.0, 1.125, 0.1875, 0.625))
        );
    }

    #[test]
    fn positions_reject_unbounded_values() {
        assert_eq!(
            FadeToBlackPosition::compile(0, 0),
            Err(FadeToBlackPlanError::ZeroDenominator)
        );
        assert_eq!(
            FadeToBlackPosition::compile(2, 1),
            Err(FadeToBlackPlanError::PositionOutOfRange {
                numerator: 2,
                denominator: 1,
            })
        );
        assert_eq!(
            FadeToBlackPosition::compile(1, MAX_FADE_TO_BLACK_DENOMINATOR + 1),
            Err(FadeToBlackPlanError::DenominatorLimit {
                denominator: MAX_FADE_TO_BLACK_DENOMINATOR + 1,
                maximum: MAX_FADE_TO_BLACK_DENOMINATOR,
            })
        );
    }
}
