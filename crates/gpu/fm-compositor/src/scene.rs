use core::fmt;

use fm_frame::AlphaMode;
use fm_video::{CropRect, Rgba8, Transform};

/// Stable identifier used to resolve a source frame at execution time.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(pub u64);

impl SourceId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputTarget {
    Program,
    Preview,
    Record,
    Stream,
    Operator,
}

/// Per-output overlay routing flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputInclusion(u8);

impl OutputInclusion {
    pub const NONE: Self = Self(0);
    pub const PROGRAM: Self = Self(1 << 0);
    pub const PREVIEW: Self = Self(1 << 1);
    pub const RECORD: Self = Self(1 << 2);
    pub const STREAM: Self = Self(1 << 3);
    pub const OPERATOR: Self = Self(1 << 4);
    pub const ALL: Self = Self((1 << 5) - 1);
    const VALID_BITS: u8 = Self::ALL.0;

    /// Validates serialized inclusion flags.
    ///
    /// # Errors
    /// Returns an error if an unknown output bit is set.
    pub const fn from_bits(bits: u8) -> Result<Self, InclusionError> {
        if bits & !Self::VALID_BITS == 0 {
            Ok(Self(bits))
        } else {
            Err(InclusionError { bits })
        }
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn includes(self, target: OutputTarget) -> bool {
        self.contains(match target {
            OutputTarget::Program => Self::PROGRAM,
            OutputTarget::Preview => Self::PREVIEW,
            OutputTarget::Record => Self::RECORD,
            OutputTarget::Stream => Self::STREAM,
            OutputTarget::Operator => Self::OPERATOR,
        })
    }
}

impl core::ops::BitOr for OutputInclusion {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InclusionError {
    pub bits: u8,
}

impl fmt::Display for InclusionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "output inclusion contains unknown bits: {:#04x}",
            self.bits
        )
    }
}

impl std::error::Error for InclusionError {}

/// A rectangular source-space mask, applied after crop and before transform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RectMask {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub invert: bool,
}

impl RectMask {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            invert: false,
        }
    }

    #[must_use]
    pub const fn inverted(mut self, invert: bool) -> Self {
        self.invert = invert;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChromaKey {
    pub color: Rgba8,
    pub tolerance: u8,
    pub softness: u8,
    pub spill: u8,
}

impl ChromaKey {
    #[must_use]
    pub const fn new(color: Rgba8, tolerance: u8, softness: u8, spill: u8) -> Self {
        Self {
            color,
            tolerance,
            softness,
            spill,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LumaKey {
    pub threshold: u8,
    pub softness: u8,
    pub invert: bool,
}

impl LumaKey {
    #[must_use]
    pub const fn new(threshold: u8, softness: u8, invert: bool) -> Self {
        Self {
            threshold,
            softness,
            invert,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Chroma(ChromaKey),
    Luma(LumaKey),
}

/// Backend-neutral effect descriptor. The reference executor only accepts the
/// named `passthrough` effect; production backends can bind other descriptors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Effect {
    name: String,
    parameters: Vec<i32>,
}

impl Effect {
    #[must_use]
    pub fn new(name: impl Into<String>, parameters: Vec<i32>) -> Self {
        Self {
            name: name.into(),
            parameters,
        }
    }

    #[must_use]
    pub fn passthrough() -> Self {
        Self::new("passthrough", Vec::new())
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn parameters(&self) -> &[i32] {
        &self.parameters
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLayer {
    source: SourceId,
    z: i32,
    transform: Transform,
    crop: Option<CropRect>,
    opacity: u8,
    enabled: bool,
    alpha_mode: AlphaMode,
    mask: Option<RectMask>,
    key: Option<Key>,
    effects: Vec<Effect>,
    overlay_inclusion: Option<OutputInclusion>,
}

impl SourceLayer {
    #[must_use]
    pub const fn new(source: SourceId, z: i32, transform: Transform) -> Self {
        Self {
            source,
            z,
            transform,
            crop: None,
            opacity: u8::MAX,
            enabled: true,
            alpha_mode: AlphaMode::Straight,
            mask: None,
            key: None,
            effects: Vec::new(),
            overlay_inclusion: None,
        }
    }

    #[must_use]
    pub const fn with_crop(mut self, crop: CropRect) -> Self {
        self.crop = Some(crop);
        self
    }

    #[must_use]
    pub const fn with_opacity(mut self, opacity: u8) -> Self {
        self.opacity = opacity;
        self
    }

    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub const fn with_alpha_mode(mut self, alpha_mode: AlphaMode) -> Self {
        self.alpha_mode = alpha_mode;
        self
    }

    #[must_use]
    pub const fn with_mask(mut self, mask: RectMask) -> Self {
        self.mask = Some(mask);
        self
    }

    #[must_use]
    pub const fn with_key(mut self, key: Key) -> Self {
        self.key = Some(key);
        self
    }

    #[must_use]
    pub fn with_effect(mut self, effect: Effect) -> Self {
        self.effects.push(effect);
        self
    }

    #[must_use]
    pub const fn as_overlay(mut self, inclusion: OutputInclusion) -> Self {
        self.overlay_inclusion = Some(inclusion);
        self
    }

    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn z(&self) -> i32 {
        self.z
    }

    #[must_use]
    pub const fn transform(&self) -> Transform {
        self.transform
    }

    #[must_use]
    pub const fn crop(&self) -> Option<CropRect> {
        self.crop
    }

    #[must_use]
    pub const fn opacity(&self) -> u8 {
        self.opacity
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }

    #[must_use]
    pub const fn mask(&self) -> Option<RectMask> {
        self.mask
    }

    #[must_use]
    pub const fn key(&self) -> Option<Key> {
        self.key
    }

    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    #[must_use]
    pub const fn overlay_inclusion(&self) -> Option<OutputInclusion> {
        self.overlay_inclusion
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafeAreaGuide {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub color: Rgba8,
}

impl SafeAreaGuide {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32, color: Rgba8) -> Self {
        Self {
            x,
            y,
            width,
            height,
            color,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scene {
    width: u32,
    height: u32,
    background: Rgba8,
    layers: Vec<SourceLayer>,
    safe_areas: Vec<SafeAreaGuide>,
}

impl Scene {
    pub const MAX_WIDTH: u32 = fm_frame::CpuVideoPayload::MAX_WIDTH;
    pub const MAX_HEIGHT: u32 = fm_frame::CpuVideoPayload::MAX_HEIGHT;

    /// Creates a scene after validating its bounded output and premultiplied background.
    ///
    /// # Errors
    /// Returns a typed error for zero, excessive, overflowing, or invalid output values.
    pub fn new(width: u32, height: u32, background: Rgba8) -> Result<Self, SceneError> {
        validate_dimensions(width, height)?;
        if !is_premultiplied(background) {
            return Err(SceneError::BackgroundNotPremultiplied(background));
        }
        Ok(Self {
            width,
            height,
            background,
            layers: Vec::new(),
            safe_areas: Vec::new(),
        })
    }

    pub fn push_layer(&mut self, layer: SourceLayer) {
        self.layers.push(layer);
    }

    pub fn push_safe_area(&mut self, guide: SafeAreaGuide) {
        self.safe_areas.push(guide);
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn background(&self) -> Rgba8 {
        self.background
    }

    #[must_use]
    pub fn layers(&self) -> &[SourceLayer] {
        &self.layers
    }

    #[must_use]
    pub fn safe_areas(&self) -> &[SafeAreaGuide] {
        &self.safe_areas
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneError {
    ZeroWidth,
    ZeroHeight,
    DimensionsTooLarge { width: u32, height: u32 },
    OutputTooLarge,
    BackgroundNotPremultiplied(Rgba8),
}

impl fmt::Display for SceneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => formatter.write_str("scene width must be nonzero"),
            Self::ZeroHeight => formatter.write_str("scene height must be nonzero"),
            Self::DimensionsTooLarge { width, height } => write!(
                formatter,
                "scene dimensions {width}x{height} exceed {}x{}",
                Scene::MAX_WIDTH,
                Scene::MAX_HEIGHT
            ),
            Self::OutputTooLarge => {
                formatter.write_str("scene output exceeds the frame byte limit")
            }
            Self::BackgroundNotPremultiplied(_) => {
                formatter.write_str("scene background is not premultiplied RGBA")
            }
        }
    }
}

impl std::error::Error for SceneError {}

fn validate_dimensions(width: u32, height: u32) -> Result<(), SceneError> {
    if width == 0 {
        return Err(SceneError::ZeroWidth);
    }
    if height == 0 {
        return Err(SceneError::ZeroHeight);
    }
    if width > Scene::MAX_WIDTH || height > Scene::MAX_HEIGHT {
        return Err(SceneError::DimensionsTooLarge { width, height });
    }
    let bytes = u64::from(width) * u64::from(height) * 4;
    if bytes > fm_video::ImageFrame::MAX_BUFFER_BYTES as u64 {
        return Err(SceneError::OutputTooLarge);
    }
    Ok(())
}

pub(crate) const fn is_premultiplied(pixel: Rgba8) -> bool {
    pixel.r <= pixel.a && pixel.g <= pixel.a && pixel.b <= pixel.a
}
