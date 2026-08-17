//! Owned font faces parsed from caller-supplied bytes.
//!
//! Fonts are always supplied explicitly: the caller reads a project asset and
//! hands the bytes to [`FontFace::from_bytes`]. This crate never discovers
//! system fonts, never falls back to another family, and never touches the
//! network. Unparsable or metrically unusable data is rejected with a typed
//! [`FontError`] instead of panicking later during layout.

use ab_glyph::{Font, FontVec, PxScale, PxScaleFont};
use core::fmt;

/// Largest accepted font file. Bounds the copy performed by parsing.
pub const MAX_FONT_BYTES: usize = 32 * 1024 * 1024;

/// A scaled view of a [`FontFace`] used by layout and rasterization.
pub(crate) type ScaledFace<'a> = PxScaleFont<&'a FontVec>;

/// Reasons caller-supplied font bytes are refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontError {
    /// The byte slice was empty.
    Empty,
    /// The byte slice exceeded [`MAX_FONT_BYTES`].
    TooLarge { size: usize, maximum: usize },
    /// The bytes are not a font this crate can parse.
    Unparsable,
    /// The font parsed but exposes metrics that cannot drive layout, such as a
    /// non-positive or non-finite ascent-to-descent height.
    UnusableMetrics,
}

impl fmt::Display for FontError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("font data is empty"),
            Self::TooLarge { size, maximum } => {
                write!(formatter, "font data is {size} bytes, maximum is {maximum}")
            }
            Self::Unparsable => formatter.write_str("font data could not be parsed"),
            Self::UnusableMetrics => formatter.write_str("font exposes unusable vertical metrics"),
        }
    }
}

impl std::error::Error for FontError {}

/// A parsed, owned font face.
///
/// Only the outline glyph source is used: colour glyph strikes (`CBDT`, `sbix`,
/// `SVG`) are ignored, and a face providing no outlines simply renders nothing.
pub struct FontFace {
    font: FontVec,
}

impl FontFace {
    /// Parses owned font bytes, taking face 0 of a collection.
    ///
    /// # Errors
    ///
    /// Returns [`FontError`] for empty, oversized, unparsable, or metrically
    /// unusable data. Hostile input never panics and never allocates beyond
    /// [`MAX_FONT_BYTES`].
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, FontError> {
        if bytes.is_empty() {
            return Err(FontError::Empty);
        }
        if bytes.len() > MAX_FONT_BYTES {
            return Err(FontError::TooLarge {
                size: bytes.len(),
                maximum: MAX_FONT_BYTES,
            });
        }
        let font = FontVec::try_from_vec(bytes).map_err(|_| FontError::Unparsable)?;
        let height = font.height_unscaled();
        let usable = height.is_finite()
            && height > 0.0
            && font.ascent_unscaled().is_finite()
            && font.descent_unscaled().is_finite()
            && font.line_gap_unscaled().is_finite();
        if !usable {
            return Err(FontError::UnusableMetrics);
        }
        Ok(Self { font })
    }

    /// Number of glyphs the face exposes.
    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.font.glyph_count()
    }

    /// Scales the face to a pixel height.
    ///
    /// `size_px` is the `ab_glyph` pixel scale: the distance from ascent to
    /// descent, not the em square. Callers clamp it to
    /// [`crate::MAX_FONT_SIZE_PX`] before reaching here, so the `u16`
    /// conversion below is lossless.
    pub(crate) fn scaled(&self, size_px: u32) -> ScaledFace<'_> {
        let size = u16::try_from(size_px).unwrap_or(u16::MAX);
        self.font.as_scaled(PxScale::from(f32::from(size)))
    }
}

impl fmt::Debug for FontFace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FontFace")
            .field("glyph_count", &self.font.glyph_count())
            .finish()
    }
}
