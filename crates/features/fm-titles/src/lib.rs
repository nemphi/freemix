//! Dependency-light title scenes and a deterministic CPU reference renderer.
//!
//! The renderer is intended for tests, previews, and behavioral comparison. It
//! deliberately does not perform production font shaping or image decoding;
//! see [`REFERENCE_RENDERER_LIMITATIONS`] and [`RenderReport`].

mod animation;
mod id;
mod model;
mod render;
mod runtime;
mod validation;

pub use animation::{AnimatedProperty, AnimationTrack, Interpolation, Keyframe};
pub use id::{ElementId, FieldId, TemplateId, TitleId};
pub use model::{
    Alignment, Bounds, Color, Element, ElementKind, FieldDefinition, FieldType, FieldValue,
    FontStyle, HorizontalAlignment, ImageSource, InstantiationError, Style, TitleScene,
    TitleTemplate, UpdateError, VerticalAlignment,
};
pub use render::{
    REFERENCE_RENDERER_LIMITATIONS, ReferenceRenderer, RenderError, RenderOutput, RenderReport,
};
pub use runtime::{
    ClockDirection, ClockFormat, ClockSpec, TickerDirection, TickerSpec, evaluate_clock,
    evaluate_ticker_position,
};
pub use validation::{
    AssetCatalog, MissingFont, MissingImage, ValidationError, ValidationReport, validate_scene,
    validate_template,
};
