//! Deterministic single-font text layout.
//!
//! # What this layout does
//!
//! * Maps each `char` to one glyph through the font's character map.
//! * Applies horizontal kerning whenever the font exposes a pair value for two
//!   adjacent glyphs.
//! * Breaks paragraphs on `'\n'` and greedily word-wraps on Unicode whitespace
//!   inside a pixel width, hard-breaking a single word that cannot fit.
//!
//! # What this layout deliberately does not do
//!
//! Grapheme-level shaping is out of scope: there is no bidirectional
//! reordering, no complex-script cluster shaping (Arabic joining, Indic
//! reordering), no mark positioning, and no OpenType feature application such
//! as ligatures or contextual alternates. Text is laid out in codepoint order,
//! left to right. Rendering right-to-left or complex scripts requires a shaper
//! (`HarfBuzz` or `rustybuzz`) that this crate does not depend on.
//!
//! # Determinism
//!
//! Horizontal advances and kerning are converted once from `f32` to
//! [`FIXED_ONE`]ths of a pixel with round-half-away-from-zero, then accumulated
//! in `i64`. Vertical metrics are rounded to whole pixels the same way. No
//! layout decision depends on accumulated floating-point state, so identical
//! input produces identical integer positions on every IEEE-754 platform.

use crate::font::ScaledFace;
use ab_glyph::{GlyphId, ScaleFont};

/// Sub-pixel denominator for horizontal accumulation: 1/64 px.
pub(crate) const FIXED_ONE: i64 = 64;

/// Clamp applied to every fixed-point accumulator so a hostile font with
/// absurd advances cannot overflow the pen.
const FIXED_LIMIT: i64 = 1 << 40;

/// [`FIXED_LIMIT`] as `f32`. A power of two, so the value is exact.
const FIXED_LIMIT_F32: f32 = 1_099_511_627_776.0;

/// One positioned glyph on a line. `x_fixed` is relative to the line origin and
/// already includes the kerning applied against the previous glyph.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LaidGlyph {
    pub id: GlyphId,
    pub x_fixed: i64,
    pub advance_fixed: i64,
}

/// One laid-out line. `width_fixed` is the advance width up to and including
/// the last non-whitespace glyph; trailing whitespace is excluded so alignment
/// does not drift.
#[derive(Clone, Debug, Default)]
pub(crate) struct LaidLine {
    pub glyphs: Vec<LaidGlyph>,
    pub width_fixed: i64,
}

/// A laid-out block of text plus the vertical metrics used to place it.
#[derive(Clone, Debug)]
pub(crate) struct LaidText {
    pub lines: Vec<LaidLine>,
    /// Baseline offset from the top of a line box, in whole pixels.
    pub ascent_px: i64,
    /// Baseline-to-baseline distance in whole pixels, at least 1.
    pub line_height_px: i64,
    /// Widest line in whole pixels.
    pub width_px: u32,
}

/// The text exceeded the caller's glyph budget for a single element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextTooLong;

/// Rounds a font metric to 1/64 px, half away from zero.
///
/// `f32 as i64` saturates in Rust and maps NaN to zero; the explicit clamp
/// keeps every accumulator inside [`FIXED_LIMIT`] regardless.
#[allow(clippy::cast_possible_truncation)]
fn to_fixed(value: f32) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    (value * 64.0)
        .round()
        .clamp(-FIXED_LIMIT_F32, FIXED_LIMIT_F32) as i64
}

/// Rounds a fixed-point x position to a whole pixel, half toward +infinity.
pub(crate) fn fixed_to_px(value: i64) -> i64 {
    value
        .saturating_add(FIXED_ONE / 2)
        .div_euclid(FIXED_ONE)
        .clamp(-FIXED_LIMIT, FIXED_LIMIT)
}

/// Rounds a pixel metric, half away from zero. Saturating cast as above.
#[allow(clippy::cast_possible_truncation)]
fn round_px(value: f32) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    value.round().clamp(-FIXED_LIMIT_F32, FIXED_LIMIT_F32) as i64
}

/// Lays out `text` for `face`.
///
/// `wrap_width_px` enables greedy word wrapping inside that pixel width;
/// `None` lays every paragraph out as one unbounded line (used by tickers,
/// which scroll horizontally instead of wrapping).
///
/// # Errors
///
/// Returns [`TextTooLong`] as soon as more than `max_glyphs` glyphs would be
/// laid out, before the work is done, so a hostile field value cannot force an
/// unbounded allocation.
pub(crate) fn layout_text(
    face: &ScaledFace<'_>,
    text: &str,
    wrap_width_px: Option<u32>,
    max_glyphs: usize,
) -> Result<LaidText, TextTooLong> {
    let mut layout = Layout::new(face, wrap_width_px, max_glyphs);
    for character in text.chars() {
        layout.push(character)?;
    }
    Ok(layout.finish())
}

struct Layout<'a, 'f> {
    face: &'a ScaledFace<'f>,
    max_fixed: Option<i64>,
    max_glyphs: usize,
    used_glyphs: usize,
    lines: Vec<LaidLine>,
    line: LaidLine,
    line_pen: i64,
    line_last: Option<GlyphId>,
    /// No glyph has been committed to the current line yet.
    fresh_line: bool,
    word: Vec<LaidGlyph>,
    word_pen: i64,
    word_last: Option<GlyphId>,
}

impl<'a, 'f> Layout<'a, 'f> {
    fn new(face: &'a ScaledFace<'f>, wrap_width_px: Option<u32>, max_glyphs: usize) -> Self {
        Self {
            face,
            max_fixed: wrap_width_px.map(|width| i64::from(width) * FIXED_ONE),
            max_glyphs,
            used_glyphs: 0,
            lines: Vec::new(),
            line: LaidLine::default(),
            line_pen: 0,
            line_last: None,
            fresh_line: true,
            word: Vec::new(),
            word_pen: 0,
            word_last: None,
        }
    }

    fn advance(&self, id: GlyphId) -> i64 {
        to_fixed(self.face.h_advance(id))
    }

    fn kern(&self, previous: Option<GlyphId>, id: GlyphId) -> i64 {
        previous.map_or(0, |previous| to_fixed(self.face.kern(previous, id)))
    }

    fn push(&mut self, character: char) -> Result<(), TextTooLong> {
        if character == '\n' {
            self.commit_word();
            self.flush_line();
            return Ok(());
        }
        // Control characters other than the newline above carry no layout
        // meaning here and are dropped rather than rendered as .notdef boxes.
        if character.is_control() {
            return Ok(());
        }
        if character.is_whitespace() {
            self.commit_word();
            let id = self.face.glyph_id(character);
            let x = self.line_pen.saturating_add(self.kern(self.line_last, id));
            self.line_pen = x
                .saturating_add(self.advance(id))
                .clamp(-FIXED_LIMIT, FIXED_LIMIT);
            self.line_last = Some(id);
            return Ok(());
        }

        self.used_glyphs += 1;
        if self.used_glyphs > self.max_glyphs {
            return Err(TextTooLong);
        }
        let id = self.face.glyph_id(character);
        let x = self.word_pen.saturating_add(self.kern(self.word_last, id));
        let advance = self.advance(id);
        self.word.push(LaidGlyph {
            id,
            x_fixed: x,
            advance_fixed: advance,
        });
        self.word_pen = x.saturating_add(advance).clamp(-FIXED_LIMIT, FIXED_LIMIT);
        self.word_last = Some(id);
        Ok(())
    }

    fn commit_word(&mut self) {
        let word = core::mem::take(&mut self.word);
        self.word_pen = 0;
        self.word_last = None;
        let Some(first) = word.first() else {
            return;
        };
        let word_width = word
            .last()
            .map_or(0, |last| last.x_fixed.saturating_add(last.advance_fixed));

        if let Some(max) = self.max_fixed {
            let kern = self.pending_kern(first.id);
            if !self.fresh_line
                && self
                    .line_pen
                    .saturating_add(kern)
                    .saturating_add(word_width)
                    > max
            {
                self.flush_line();
            }
            if word_width > max {
                self.commit_word_broken(&word, max);
                return;
            }
        }
        let origin = self.line_pen.saturating_add(self.pending_kern(first.id));
        self.emit_word(&word, 0, origin);
    }

    /// Kerning between the last glyph already on the line and the first glyph
    /// of the pending word. A word starting a line is never kerned.
    fn pending_kern(&self, first: GlyphId) -> i64 {
        if self.fresh_line {
            0
        } else {
            self.kern(self.line_last, first)
        }
    }

    /// Places a word wider than the wrap width by breaking between glyphs.
    /// Always makes progress: at least one glyph lands on each emitted line.
    fn commit_word_broken(&mut self, word: &[LaidGlyph], max: i64) {
        if !self.fresh_line {
            self.flush_line();
        }
        let mut start = 0;
        let mut origin = 0;
        for (index, glyph) in word.iter().enumerate() {
            let end = glyph
                .x_fixed
                .saturating_add(glyph.advance_fixed)
                .saturating_sub(origin);
            if index > start && end > max {
                self.emit_word(&word[start..index], origin, 0);
                self.flush_line();
                start = index;
                origin = glyph.x_fixed;
            }
        }
        self.emit_word(&word[start..], origin, 0);
    }

    /// Copies `word` onto the current line, shifted so that `origin` maps to
    /// `destination`.
    fn emit_word(&mut self, word: &[LaidGlyph], origin: i64, destination: i64) {
        let Some(last) = word.last().copied() else {
            return;
        };
        for glyph in word {
            self.line.glyphs.push(LaidGlyph {
                x_fixed: destination.saturating_add(glyph.x_fixed.saturating_sub(origin)),
                ..*glyph
            });
        }
        self.line_pen = destination
            .saturating_add(last.x_fixed.saturating_sub(origin))
            .saturating_add(last.advance_fixed)
            .clamp(-FIXED_LIMIT, FIXED_LIMIT);
        self.line.width_fixed = self.line_pen;
        self.line_last = Some(last.id);
        self.fresh_line = false;
    }

    fn flush_line(&mut self) {
        self.lines.push(core::mem::take(&mut self.line));
        self.line_pen = 0;
        self.line_last = None;
        self.fresh_line = true;
    }

    fn finish(mut self) -> LaidText {
        self.commit_word();
        self.flush_line();

        let ascent_px = round_px(self.face.ascent());
        let descent_px = round_px(self.face.descent());
        let line_gap_px = round_px(self.face.line_gap());
        let line_height_px = ascent_px
            .saturating_sub(descent_px)
            .saturating_add(line_gap_px)
            .max(1);
        let width_px = self
            .lines
            .iter()
            .map(|line| fixed_to_px(line.width_fixed))
            .max()
            .unwrap_or(0);
        LaidText {
            lines: self.lines,
            ascent_px,
            line_height_px,
            width_px: u32::try_from(width_px).unwrap_or(u32::MAX),
        }
    }
}
