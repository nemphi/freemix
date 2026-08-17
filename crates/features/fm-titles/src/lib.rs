//! Dependency-light title scenes and a deterministic CPU renderer.
//!
//! # What is real
//!
//! * Templates, typed fields, optimistic scene updates, keyframe animation,
//!   clock and ticker runtime, and structural validation.
//! * Text rendering: glyph outlines are rasterized from caller-supplied font
//!   bytes through `ab_glyph`, with kerned advances, ascent-correct baselines,
//!   per-element pixel sizes, greedy word wrapping, horizontal and vertical
//!   alignment, anti-aliased coverage composited into premultiplied RGBA8, and
//!   clipping at the element box and the frame edge.
//! * Bounded work: output size, font size, characters per element, lines per
//!   element, glyphs per frame, and per-glyph rasterization area are capped,
//!   and every fill iterates its intersection with the canvas rather than the
//!   element's declared extent. Reaching a per-element cap degrades that
//!   element and is listed in [`RenderReport::degraded`] so the rest of the
//!   frame still goes to air; only a whole-frame cap is a [`RenderError`].
//!
//! # What is still missing
//!
//! * Grapheme-level shaping. Text is laid out in codepoint order: no bidi, no
//!   complex-script shaping, no ligatures or other OpenType features.
//! * Image decoding. Image elements still draw a placeholder and their assets
//!   are only reported; see [`REFERENCE_RENDERER_LIMITATIONS`].
//! * GPU rasterization, production effects, and any pipeline wiring. Nothing
//!   in the project schema instantiates these scenes yet.

mod animation;
mod font;
mod id;
mod model;
mod render;
mod runtime;
mod text;
mod validation;

pub use animation::{AnimatedProperty, AnimationTrack, Interpolation, Keyframe};
pub use font::{FontError, FontFace, MAX_FONT_BYTES};
pub use id::{ElementId, FieldId, TemplateId, TitleId};
pub use model::{
    Alignment, Bounds, Color, Element, ElementKind, FieldDefinition, FieldType, FieldValue,
    FontStyle, HorizontalAlignment, ImageSource, InstantiationError, Style, TitleScene,
    TitleTemplate, UpdateError, VerticalAlignment,
};
pub use render::{
    Degradation, DegradedElement, MAX_FONT_SIZE_PX, MAX_GLYPH_RASTER_DIMENSION,
    MAX_GLYPH_RASTER_PIXELS, MAX_GLYPHS_PER_ELEMENT, MAX_GLYPHS_PER_FRAME, MAX_OUTPUT_HEIGHT,
    MAX_OUTPUT_WIDTH, REFERENCE_RENDERER_LIMITATIONS, ReferenceRenderer, RenderError, RenderOutput,
    RenderReport,
};
pub use runtime::{
    ClockDirection, ClockFormat, ClockSpec, TickerDirection, TickerSpec, evaluate_clock,
    evaluate_ticker_position,
};
pub use validation::{
    AssetCatalog, MissingFont, MissingImage, ValidationError, ValidationReport, validate_scene,
    validate_template,
};
