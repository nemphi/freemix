use core::fmt;

use fm_gpu::{
    NativeContext, NativeFullscreenBlend, NativeFullscreenDraw, NativeFullscreenLoadOp,
    NativeFullscreenPipeline, NativeFullscreenPipelineOptions, NativeGpuError,
    NativeSourceExtentPolicy, NativeTexture, ShaderDescriptor, ShaderLanguage, ShaderSource,
    ShaderStage, TextureFormat,
};
use fm_video::{CropRect, Rotation, Transform};

use crate::{
    CompositionPlan, FadeToBlackPlan, RectMask, SourceId, TransitionKind, TransitionPlan,
    transition::wipe_boundary,
};

/// Maximum width or height accepted for a native layer transform.
///
/// This keeps nearest-neighbor coordinate products within portable WGSL `u32`
/// arithmetic when combined with device-bounded source dimensions.
pub const MAX_NATIVE_TRANSFORM_DIMENSION: u32 = 16_384;

const COMPOSITION_UNIFORM_SIZE: usize = 80;

const COMPOSITION_FRAGMENT_SHADER: &str = r"
struct LayerUniform {
    translation_x: u32,
    translation_y: u32,
    scale_width: u32,
    scale_height: u32,
    rotation: u32,
    crop_x: u32,
    crop_y: u32,
    crop_width: u32,
    crop_height: u32,
    opacity: u32,
    visible: u32,
    rotated_width: u32,
    rotated_height: u32,
    mask_left: u32,
    mask_top: u32,
    mask_right: u32,
    mask_bottom: u32,
    mask_enabled: u32,
    mask_invert: u32,
    padding: u32,
};

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var unused_source_texture: texture_2d<f32>;
@group(0) @binding(2) var<uniform> layer: LayerUniform;

@fragment
fn composition_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    if layer.visible == 0u {
        return vec4<f32>(0.0);
    }

    let translated = vec2<i32>(position.xy) - vec2<i32>(
        bitcast<i32>(layer.translation_x),
        bitcast<i32>(layer.translation_y),
    );
    if translated.x < 0 || translated.y < 0 {
        return vec4<f32>(0.0);
    }
    let rotated = vec2<u32>(translated);
    if rotated.x >= layer.rotated_width || rotated.y >= layer.rotated_height {
        return vec4<f32>(0.0);
    }

    var scaled: vec2<u32>;
    switch layer.rotation {
        case 0u: {
            scaled = rotated;
        }
        case 1u: {
            scaled = vec2<u32>(rotated.y, layer.scale_height - 1u - rotated.x);
        }
        case 2u: {
            scaled = vec2<u32>(
                layer.scale_width - 1u - rotated.x,
                layer.scale_height - 1u - rotated.y,
            );
        }
        default: {
            scaled = vec2<u32>(layer.scale_width - 1u - rotated.y, rotated.x);
        }
    }

    let cropped_coordinates = vec2<u32>(
        scaled.x * layer.crop_width / layer.scale_width,
        scaled.y * layer.crop_height / layer.scale_height,
    );
    let inside_mask =
        cropped_coordinates.x >= layer.mask_left &&
        cropped_coordinates.y >= layer.mask_top &&
        cropped_coordinates.x < layer.mask_right &&
        cropped_coordinates.y < layer.mask_bottom;
    if layer.mask_enabled != 0u && inside_mask == (layer.mask_invert != 0u) {
        return vec4<f32>(0.0);
    }

    let source_coordinates =
        vec2<u32>(layer.crop_x, layer.crop_y) + cropped_coordinates;
    let source = textureLoad(source_texture, vec2<i32>(source_coordinates), 0);
    return source * (f32(layer.opacity) / 255.0);
}
";

const TRANSITION_FRAGMENT_SHADER: &str = r"
struct TransitionUniform {
    operation: u32,
    numerator: u32,
    denominator: u32,
    boundary: u32,
};

@group(0) @binding(0) var from_texture: texture_2d<f32>;
@group(0) @binding(1) var to_texture: texture_2d<f32>;
@group(0) @binding(2) var<uniform> transition: TransitionUniform;

@fragment
fn transition_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let coordinates = vec2<i32>(position.xy);
    let source = textureLoad(from_texture, coordinates, 0);
    let destination = textureLoad(to_texture, coordinates, 0);
    if transition.operation == 0u || transition.numerator == transition.denominator {
        return destination;
    }
    if transition.numerator == 0u {
        return source;
    }
    if transition.operation == 2u {
        if u32(position.x) < transition.boundary {
            return destination;
        }
        return source;
    }
    return mix(source, destination, f32(transition.numerator) / f32(transition.denominator));
}
";

const FADE_TO_BLACK_UNIFORM_SIZE: usize = 32;

const FADE_TO_BLACK_FRAGMENT_SHADER: &str = r"
struct FadeToBlackUniform {
    start_numerator: u32,
    start_denominator: u32,
    end_numerator: u32,
    end_denominator: u32,
    progress_numerator: u32,
    progress_denominator: u32,
    padding_0: u32,
    padding_1: u32,
};

@group(0) @binding(0) var program_texture: texture_2d<f32>;
@group(0) @binding(1) var unused_program_texture: texture_2d<f32>;
@group(0) @binding(2) var<uniform> ftb: FadeToBlackUniform;

fn ratio(numerator: u32, denominator: u32) -> f32 {
    return f32(numerator) / f32(denominator);
}

@fragment
fn fade_to_black_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let coordinates = vec2<i32>(position.xy);
    let source = textureLoad(program_texture, coordinates, 0);
    let start = ratio(ftb.start_numerator, ftb.start_denominator);
    let end = ratio(ftb.end_numerator, ftb.end_denominator);
    var fade_position = start;
    if ftb.progress_numerator == ftb.progress_denominator {
        fade_position = end;
    } else if ftb.progress_numerator != 0u {
        fade_position = mix(
            start,
            end,
            ratio(ftb.progress_numerator, ftb.progress_denominator),
        );
    }
    if fade_position <= 0.0 {
        return source;
    }
    if fade_position >= 1.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return mix(source, vec4<f32>(0.0, 0.0, 0.0, 1.0), fade_position);
}
";

/// Binds a plan source identifier to a canonical native working texture.
///
/// The texture must be `Rgba16Float` containing linear-light, premultiplied-alpha
/// working pixels. Import normalization has already canonicalized alpha, so the
/// native compositor never premultiplies again based on `PlanLayer::alpha_mode`.
#[derive(Clone, Copy)]
pub struct NativeSourceFrame<'a> {
    pub source: SourceId,
    pub texture: &'a NativeTexture,
}

impl<'a> NativeSourceFrame<'a> {
    #[must_use]
    pub const fn new(source: SourceId, texture: &'a NativeTexture) -> Self {
        Self { source, texture }
    }
}

/// Errors produced by native composition-plan execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCompositionError {
    MissingSource(SourceId),
    DuplicateSource(SourceId),
    SourceFormat {
        source: SourceId,
        actual: TextureFormat,
    },
    InvalidCrop {
        layer: usize,
        crop: CropRect,
        source_width: u32,
        source_height: u32,
    },
    InvalidTransformDimensions {
        layer: usize,
        width: u32,
        height: u32,
    },
    TransformDimensionLimit {
        layer: usize,
        width: u32,
        height: u32,
        maximum: u32,
    },
    UnsupportedKey {
        layer: usize,
    },
    UnsupportedEffect {
        layer: usize,
        effect: usize,
        name: String,
    },
    UnsupportedSafeAreas {
        count: usize,
    },
    Gpu(NativeGpuError),
}

impl fmt::Display for NativeCompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource(source) => write!(formatter, "source {} is missing", source.0),
            Self::DuplicateSource(source) => {
                write!(formatter, "source {} was supplied more than once", source.0)
            }
            Self::SourceFormat { source, actual } => write!(
                formatter,
                "source {} has native format {actual:?}; expected Rgba16Float",
                source.0
            ),
            Self::InvalidCrop {
                layer,
                crop,
                source_width,
                source_height,
            } => write!(
                formatter,
                "layer {layer} crop {crop:?} exceeds source bounds {source_width}x{source_height}"
            ),
            Self::InvalidTransformDimensions {
                layer,
                width,
                height,
            } => write!(
                formatter,
                "layer {layer} native transform dimensions must be nonzero, got {width}x{height}"
            ),
            Self::TransformDimensionLimit {
                layer,
                width,
                height,
                maximum,
            } => write!(
                formatter,
                "layer {layer} native transform dimensions {width}x{height} exceed {maximum}"
            ),
            Self::UnsupportedKey { layer } => {
                write!(
                    formatter,
                    "layer {layer} keys are unsupported by native composition"
                )
            }
            Self::UnsupportedEffect {
                layer,
                effect,
                name,
            } => write!(
                formatter,
                "layer {layer} effect {effect} ({name}) is unsupported by native composition"
            ),
            Self::UnsupportedSafeAreas { count } => write!(
                formatter,
                "native composition does not support {count} safe-area guides"
            ),
            Self::Gpu(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeCompositionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpu(error) => Some(error),
            _ => None,
        }
    }
}

impl From<NativeGpuError> for NativeCompositionError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

/// Native executor for compiled composition plans.
///
/// Inputs are canonical `Rgba16Float`, linear-light, premultiplied-alpha working
/// textures. `PlanLayer::alpha_mode` describes pre-normalization/CPU execution
/// and does not cause additional premultiplication in this executor.
pub struct NativeCompositionRenderer {
    pipeline: NativeFullscreenPipeline,
}

impl NativeCompositionRenderer {
    /// Compiles the native composition pipeline on `context`.
    ///
    /// # Errors
    ///
    /// Returns a mapped GPU shader or pipeline validation error.
    pub async fn new(context: &NativeContext) -> Result<Self, NativeCompositionError> {
        let pipeline = context
            .create_fullscreen_pipeline_with_options(
                ShaderDescriptor::new(
                    "fm-compositor native composition",
                    ShaderStage::Fragment,
                    ShaderLanguage::Wgsl,
                    "composition_fragment",
                    ShaderSource::Text(COMPOSITION_FRAGMENT_SHADER.to_owned()),
                ),
                NativeFullscreenPipelineOptions {
                    target_format: TextureFormat::Rgba16Float,
                    blend: NativeFullscreenBlend::PremultipliedSourceOver,
                    uniform_size: COMPOSITION_UNIFORM_SIZE,
                    source_extent_policy: NativeSourceExtentPolicy::Independent,
                },
            )
            .await?;
        Ok(Self { pipeline })
    }

    /// Renders `plan` into one GPU-resident `Rgba16Float` output texture.
    ///
    /// Source bindings must be unique canonical `Rgba16Float`, linear-light,
    /// premultiplied-alpha textures. Compositor-owned plan, source identifier,
    /// format, crop, and transform validation completes before output allocation;
    /// opaque GPU context ownership is validated by `fm-gpu` at submission.
    /// Layers are submitted in plan order as one bounded, premultiplied
    /// source-over render pass without readback.
    ///
    /// # Errors
    ///
    /// Returns a typed source, crop, transform, unsupported-work, or GPU error.
    pub async fn render(
        &self,
        context: &NativeContext,
        plan: &CompositionPlan,
        sources: &[NativeSourceFrame<'_>],
    ) -> Result<NativeTexture, NativeCompositionError> {
        let prepared = validate_composition(plan, sources)?;
        let output = context
            .create_rgba16_float_render_target(plan.width(), plan.height())
            .await?;
        let draws = prepared
            .iter()
            .map(|layer| {
                NativeFullscreenDraw::new(
                    &self.pipeline,
                    layer.texture,
                    layer.texture,
                    &layer.uniform,
                )
            })
            .collect::<Vec<_>>();
        let background = plan.background();
        context
            .submit_fullscreen_pass(
                &output,
                NativeFullscreenLoadOp::ClearRgba8([
                    background.r,
                    background.g,
                    background.b,
                    background.a,
                ]),
                &draws,
            )
            .await?;
        Ok(output)
    }
}

struct PreparedLayer<'a> {
    texture: &'a NativeTexture,
    uniform: [u8; COMPOSITION_UNIFORM_SIZE],
}

fn validate_composition<'a>(
    plan: &CompositionPlan,
    sources: &'a [NativeSourceFrame<'a>],
) -> Result<Vec<PreparedLayer<'a>>, NativeCompositionError> {
    validate_unique_native_sources(sources)?;
    for source in sources {
        if source.texture.format() != TextureFormat::Rgba16Float {
            return Err(NativeCompositionError::SourceFormat {
                source: source.source,
                actual: source.texture.format(),
            });
        }
    }
    validate_supported_work(plan)?;

    let mut prepared = Vec::with_capacity(plan.layers().len());
    for (layer_index, layer) in plan.layers().iter().enumerate() {
        let texture = sources
            .iter()
            .find(|candidate| candidate.source == layer.source())
            .ok_or(NativeCompositionError::MissingSource(layer.source()))?
            .texture;
        let crop = layer
            .crop()
            .unwrap_or(CropRect::new(0, 0, texture.width(), texture.height()));
        validate_native_crop(layer_index, crop, texture.width(), texture.height())?;
        validate_native_transform(layer_index, layer.transform())?;
        let visible = transform_is_visible(plan.width(), plan.height(), layer.transform());
        prepared.push(PreparedLayer {
            texture,
            uniform: encode_composition_uniform(
                layer.transform(),
                crop,
                layer.mask(),
                layer.opacity(),
                visible,
            ),
        });
    }
    Ok(prepared)
}

fn validate_unique_native_sources(
    sources: &[NativeSourceFrame<'_>],
) -> Result<(), NativeCompositionError> {
    for (index, source) in sources.iter().enumerate() {
        if sources[..index]
            .iter()
            .any(|candidate| candidate.source == source.source)
        {
            return Err(NativeCompositionError::DuplicateSource(source.source));
        }
    }
    Ok(())
}

fn validate_supported_work(plan: &CompositionPlan) -> Result<(), NativeCompositionError> {
    if !plan.safe_areas().is_empty() {
        return Err(NativeCompositionError::UnsupportedSafeAreas {
            count: plan.safe_areas().len(),
        });
    }
    for (layer_index, layer) in plan.layers().iter().enumerate() {
        if layer.key().is_some() {
            return Err(NativeCompositionError::UnsupportedKey { layer: layer_index });
        }
        for (effect_index, effect) in layer.effects().iter().enumerate() {
            if effect.name() != "passthrough" || !effect.parameters().is_empty() {
                return Err(NativeCompositionError::UnsupportedEffect {
                    layer: layer_index,
                    effect: effect_index,
                    name: effect.name().to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn validate_native_crop(
    layer: usize,
    crop: CropRect,
    source_width: u32,
    source_height: u32,
) -> Result<(), NativeCompositionError> {
    let valid = crop.width > 0
        && crop.height > 0
        && crop
            .x
            .checked_add(crop.width)
            .is_some_and(|right| right <= source_width)
        && crop
            .y
            .checked_add(crop.height)
            .is_some_and(|bottom| bottom <= source_height);
    if valid {
        Ok(())
    } else {
        Err(NativeCompositionError::InvalidCrop {
            layer,
            crop,
            source_width,
            source_height,
        })
    }
}

fn validate_native_transform(
    layer: usize,
    transform: Transform,
) -> Result<(), NativeCompositionError> {
    if transform.scale_width == 0 || transform.scale_height == 0 {
        return Err(NativeCompositionError::InvalidTransformDimensions {
            layer,
            width: transform.scale_width,
            height: transform.scale_height,
        });
    }
    if transform.scale_width > MAX_NATIVE_TRANSFORM_DIMENSION
        || transform.scale_height > MAX_NATIVE_TRANSFORM_DIMENSION
    {
        return Err(NativeCompositionError::TransformDimensionLimit {
            layer,
            width: transform.scale_width,
            height: transform.scale_height,
            maximum: MAX_NATIVE_TRANSFORM_DIMENSION,
        });
    }
    Ok(())
}

fn transform_is_visible(output_width: u32, output_height: u32, transform: Transform) -> bool {
    let (rotated_width, rotated_height) = rotated_dimensions(transform);
    let left = i64::from(transform.translation_x);
    let top = i64::from(transform.translation_y);
    let right = left + i64::from(rotated_width);
    let bottom = top + i64::from(rotated_height);
    left < i64::from(output_width) && top < i64::from(output_height) && right > 0 && bottom > 0
}

fn encode_composition_uniform(
    transform: Transform,
    crop: CropRect,
    mask: Option<RectMask>,
    opacity: u8,
    visible: bool,
) -> [u8; COMPOSITION_UNIFORM_SIZE] {
    let (rotated_width, rotated_height) = rotated_dimensions(transform);
    let rotation = match transform.rotation {
        Rotation::Deg0 => 0,
        Rotation::Deg90 => 1,
        Rotation::Deg180 => 2,
        Rotation::Deg270 => 3,
    };
    let (mask_left, mask_top, mask_right, mask_bottom, mask_enabled, mask_invert) =
        if let Some(mask) = mask {
            (
                mask.x,
                mask.y,
                mask.x
                    .checked_add(mask.width)
                    .expect("compiled mask right edge cannot overflow"),
                mask.y
                    .checked_add(mask.height)
                    .expect("compiled mask bottom edge cannot overflow"),
                1,
                u32::from(mask.invert),
            )
        } else {
            (0, 0, 0, 0, 0, 0)
        };
    let words = [
        transform.translation_x.cast_unsigned(),
        transform.translation_y.cast_unsigned(),
        transform.scale_width,
        transform.scale_height,
        rotation,
        crop.x,
        crop.y,
        crop.width,
        crop.height,
        u32::from(opacity),
        u32::from(visible),
        rotated_width,
        rotated_height,
        mask_left,
        mask_top,
        mask_right,
        mask_bottom,
        mask_enabled,
        mask_invert,
        0,
    ];
    let mut bytes = [0; COMPOSITION_UNIFORM_SIZE];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

const fn rotated_dimensions(transform: Transform) -> (u32, u32) {
    match transform.rotation {
        Rotation::Deg0 | Rotation::Deg180 => (transform.scale_width, transform.scale_height),
        Rotation::Deg90 | Rotation::Deg270 => (transform.scale_height, transform.scale_width),
    }
}

/// Errors produced by native Fade-to-Black application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeFadeToBlackError {
    SourceFormat { actual: TextureFormat },
    Gpu(NativeGpuError),
}

impl fmt::Display for NativeFadeToBlackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceFormat { actual } => write!(
                formatter,
                "Fade-to-Black source has native format {actual:?}; expected Rgba16Float"
            ),
            Self::Gpu(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeFadeToBlackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpu(error) => Some(error),
            Self::SourceFormat { .. } => None,
        }
    }
}

impl From<NativeGpuError> for NativeFadeToBlackError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

/// Native Fade-to-Black executor for an already composed Program texture.
pub struct NativeFadeToBlackRenderer {
    pipeline: NativeFullscreenPipeline,
}

impl NativeFadeToBlackRenderer {
    /// Compiles the explicit 32-byte FTB uniform and replacement pipeline.
    ///
    /// # Errors
    ///
    /// Returns a mapped GPU shader or pipeline validation error.
    pub async fn new(context: &NativeContext) -> Result<Self, NativeFadeToBlackError> {
        let pipeline = context
            .create_fullscreen_pipeline_with_options(
                ShaderDescriptor::new(
                    "fm-compositor native Fade-to-Black",
                    ShaderStage::Fragment,
                    ShaderLanguage::Wgsl,
                    "fade_to_black_fragment",
                    ShaderSource::Text(FADE_TO_BLACK_FRAGMENT_SHADER.to_owned()),
                ),
                NativeFullscreenPipelineOptions {
                    target_format: TextureFormat::Rgba16Float,
                    blend: NativeFullscreenBlend::Replace,
                    uniform_size: FADE_TO_BLACK_UNIFORM_SIZE,
                    source_extent_policy: NativeSourceExtentPolicy::MatchTarget,
                },
            )
            .await?;
        Ok(Self { pipeline })
    }

    /// Applies `plan` without color conversion, polling, or CPU readback.
    ///
    /// `program` must be the already composed canonical `Rgba16Float`,
    /// linear-light, premultiplied-alpha Program texture. The returned texture
    /// has the same dimensions and remains GPU-resident. This operation does
    /// not inspect or alter audio.
    ///
    /// # Errors
    ///
    /// Returns a source-format error before output allocation, or a mapped GPU
    /// resource, context, uniform, or validation error.
    pub async fn render(
        &self,
        context: &NativeContext,
        plan: FadeToBlackPlan,
        program: &NativeTexture,
    ) -> Result<NativeTexture, NativeFadeToBlackError> {
        validate_fade_to_black_format(program.format())?;
        let output = context
            .create_rgba16_float_render_target(program.width(), program.height())
            .await?;
        let uniform = encode_fade_to_black_uniform(plan);
        context
            .submit_fullscreen(&self.pipeline, program, program, &output, &uniform)
            .await?;
        Ok(output)
    }
}

fn validate_fade_to_black_format(format: TextureFormat) -> Result<(), NativeFadeToBlackError> {
    if format == TextureFormat::Rgba16Float {
        Ok(())
    } else {
        Err(NativeFadeToBlackError::SourceFormat { actual: format })
    }
}

fn encode_fade_to_black_uniform(plan: FadeToBlackPlan) -> [u8; FADE_TO_BLACK_UNIFORM_SIZE] {
    let words = [
        u32::from(plan.start().numerator()),
        u32::from(plan.start().denominator()),
        u32::from(plan.end().numerator()),
        u32::from(plan.end().denominator()),
        u32::from(plan.progress().numerator()),
        u32::from(plan.progress().denominator()),
        0,
        0,
    ];
    let mut bytes = [0; FADE_TO_BLACK_UNIFORM_SIZE];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// Errors produced by the native Cut/Fade/Wipe renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeTransitionError {
    WidthMismatch { from: u32, to: u32 },
    HeightMismatch { from: u32, to: u32 },
    Gpu(NativeGpuError),
}

impl fmt::Display for NativeTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WidthMismatch { from, to } => {
                write!(
                    formatter,
                    "native transition widths differ: {from} and {to}"
                )
            }
            Self::HeightMismatch { from, to } => {
                write!(
                    formatter,
                    "native transition heights differ: {from} and {to}"
                )
            }
            Self::Gpu(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeTransitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpu(error) => Some(error),
            _ => None,
        }
    }
}

impl From<NativeGpuError> for NativeTransitionError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

/// Native Cut/Fade/Wipe renderer containing only its compiled GPU pipeline.
pub struct NativeTransitionRenderer {
    pipeline: NativeFullscreenPipeline,
}

impl NativeTransitionRenderer {
    /// Compiles the compositor-owned transition fragment shader on `context`.
    /// No additional adapter or device is created.
    ///
    /// # Errors
    ///
    /// Returns a mapped GPU shader or pipeline validation error.
    pub async fn new(context: &NativeContext) -> Result<Self, NativeTransitionError> {
        let pipeline = context
            .create_fullscreen_pipeline_for_format(
                ShaderDescriptor::new(
                    "fm-compositor native transition",
                    ShaderStage::Fragment,
                    ShaderLanguage::Wgsl,
                    "transition_fragment",
                    ShaderSource::Text(TRANSITION_FRAGMENT_SHADER.to_owned()),
                ),
                TextureFormat::Rgba16Float,
            )
            .await?;
        Ok(Self { pipeline })
    }

    /// Submits a GPU-resident Cut, Fade, or Wipe and returns its native texture.
    /// This operation does not poll or read pixels back.
    ///
    /// # Errors
    ///
    /// Returns a dimension mismatch before allocation/submission, or a mapped
    /// GPU resource, context, alias, uniform, or validation error.
    pub async fn render(
        &self,
        context: &NativeContext,
        plan: TransitionPlan,
        from: &NativeTexture,
        to: &NativeTexture,
    ) -> Result<NativeTexture, NativeTransitionError> {
        validate_dimensions(from.width(), from.height(), to.width(), to.height())?;
        validate_format(from.format())?;
        validate_format(to.format())?;
        let output = context
            .create_rgba16_float_render_target(from.width(), from.height())
            .await?;
        let uniform = encode_uniform(plan, from.width());
        context
            .submit_fullscreen(&self.pipeline, from, to, &output, &uniform)
            .await?;
        Ok(output)
    }
}

fn validate_format(format: TextureFormat) -> Result<(), NativeTransitionError> {
    if format == TextureFormat::Rgba16Float {
        Ok(())
    } else {
        Err(NativeGpuError::TextureFormatMismatch {
            expected: TextureFormat::Rgba16Float,
            actual: format,
        }
        .into())
    }
}

fn validate_dimensions(
    from_width: u32,
    from_height: u32,
    to_width: u32,
    to_height: u32,
) -> Result<(), NativeTransitionError> {
    if from_width != to_width {
        return Err(NativeTransitionError::WidthMismatch {
            from: from_width,
            to: to_width,
        });
    }
    if from_height != to_height {
        return Err(NativeTransitionError::HeightMismatch {
            from: from_height,
            to: to_height,
        });
    }
    Ok(())
}

fn encode_uniform(plan: TransitionPlan, width: u32) -> [u8; 16] {
    let operation = match plan.kind() {
        TransitionKind::Cut => 0_u32,
        TransitionKind::Fade => 1_u32,
        TransitionKind::Wipe => 2_u32,
        TransitionKind::Slide | TransitionKind::Zoom | TransitionKind::Stinger => {
            unreachable!("TransitionPlan only compiles Cut, Fade, and Wipe")
        }
    };
    let boundary = if plan.kind() == TransitionKind::Wipe {
        wipe_boundary(width, plan.numerator(), plan.denominator())
    } else {
        0
    };
    let words = [operation, plan.numerator(), plan.denominator(), boundary];
    let mut bytes = [0; 16];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Effect, FadeToBlackPosition, Key, LumaKey, OutputTarget, RectMask, Rgba8, SafeAreaGuide,
        Scene, SourceLayer, compile_scene,
    };

    fn composition_plan(layer: SourceLayer) -> CompositionPlan {
        let mut scene = Scene::new(8, 6, Rgba8::new(0, 0, 0, 255)).unwrap();
        scene.push_layer(layer);
        compile_scene(&scene, OutputTarget::Program).unwrap().0
    }

    fn transform(x: i32, y: i32, width: u32, height: u32, rotation: Rotation) -> Transform {
        Transform::new(x, y, width, height, rotation)
    }

    fn uniform_words(uniform: [u8; COMPOSITION_UNIFORM_SIZE]) -> [u32; 20] {
        let mut words = [0; 20];
        for (word, bytes) in words.iter_mut().zip(uniform.chunks_exact(4)) {
            *word = u32::from_le_bytes(bytes.try_into().unwrap());
        }
        words
    }

    #[test]
    fn composition_uniform_is_twenty_little_endian_words_with_explicit_mask_bounds() {
        let transform = transform(-7, 9, 12, 5, Rotation::Deg90);
        let mask = RectMask::new(5, 6, 7, 8).inverted(true);
        let uniform =
            encode_composition_uniform(transform, CropRect::new(2, 3, 4, 6), Some(mask), 128, true);
        assert_eq!(uniform.len(), 80);
        assert_eq!(
            uniform_words(uniform),
            [
                (-7_i32).cast_unsigned(),
                9,
                12,
                5,
                1,
                2,
                3,
                4,
                6,
                128,
                1,
                5,
                12,
                5,
                6,
                12,
                14,
                1,
                1,
                0,
            ]
        );
    }

    #[test]
    fn native_crop_validation_uses_checked_source_bounds() {
        assert_eq!(
            validate_native_crop(2, CropRect::new(1, 2, 3, 4), 4, 6),
            Ok(())
        );
        let overflow = CropRect::new(u32::MAX, 0, 2, 1);
        assert_eq!(
            validate_native_crop(3, overflow, u32::MAX, 1),
            Err(NativeCompositionError::InvalidCrop {
                layer: 3,
                crop: overflow,
                source_width: u32::MAX,
                source_height: 1,
            })
        );
        let outside = CropRect::new(3, 0, 2, 1);
        assert_eq!(
            validate_native_crop(4, outside, 4, 1),
            Err(NativeCompositionError::InvalidCrop {
                layer: 4,
                crop: outside,
                source_width: 4,
                source_height: 1,
            })
        );
    }

    #[test]
    fn native_transform_bound_and_extreme_visibility_are_prevalidated() {
        let oversized = transform(0, 0, MAX_NATIVE_TRANSFORM_DIMENSION + 1, 1, Rotation::Deg0);
        assert_eq!(
            validate_native_transform(5, oversized),
            Err(NativeCompositionError::TransformDimensionLimit {
                layer: 5,
                width: MAX_NATIVE_TRANSFORM_DIMENSION + 1,
                height: 1,
                maximum: MAX_NATIVE_TRANSFORM_DIMENSION,
            })
        );

        for translation in [i32::MIN, i32::MAX] {
            let offscreen = transform(translation, translation, 1, 1, Rotation::Deg0);
            assert!(!transform_is_visible(8, 6, offscreen));
            let words = uniform_words(encode_composition_uniform(
                offscreen,
                CropRect::new(0, 0, 1, 1),
                None,
                255,
                false,
            ));
            assert_eq!(words[0], translation.cast_unsigned());
            assert_eq!(words[1], translation.cast_unsigned());
            assert_eq!(words[10], 0);
        }
        assert!(transform_is_visible(
            8,
            6,
            transform(-1, -1, 2, 2, Rotation::Deg0)
        ));
    }

    #[test]
    fn rect_masks_are_supported_while_other_native_plan_features_return_indexed_errors() {
        let source = SourceId::new(1);
        let base = transform(0, 0, 1, 1, Rotation::Deg0);

        let mask = composition_plan(
            SourceLayer::new(source, 0, base).with_mask(RectMask::new(0, 0, 1, 1)),
        );
        assert_eq!(validate_supported_work(&mask), Ok(()));

        let key = composition_plan(
            SourceLayer::new(source, 0, base).with_key(Key::Luma(LumaKey::new(1, 2, false))),
        );
        assert_eq!(
            validate_supported_work(&key),
            Err(NativeCompositionError::UnsupportedKey { layer: 0 })
        );

        let effect = composition_plan(
            SourceLayer::new(source, 0, base).with_effect(Effect::new("blur", vec![1])),
        );
        assert_eq!(
            validate_supported_work(&effect),
            Err(NativeCompositionError::UnsupportedEffect {
                layer: 0,
                effect: 0,
                name: "blur".to_owned(),
            })
        );

        let mut scene = Scene::new(2, 2, Rgba8::new(0, 0, 0, 255)).unwrap();
        scene.push_safe_area(SafeAreaGuide::new(
            0,
            0,
            1,
            1,
            Rgba8::new(255, 255, 255, 255),
        ));
        let safe_areas = compile_scene(&scene, OutputTarget::Operator).unwrap().0;
        assert_eq!(
            validate_supported_work(&safe_areas),
            Err(NativeCompositionError::UnsupportedSafeAreas { count: 1 })
        );
    }

    #[test]
    fn uniform_is_four_native_u32_words() {
        let fade = TransitionPlan::compile(TransitionKind::Fade, 3, 7).unwrap();
        let uniform = encode_uniform(fade, 11);
        assert_eq!(uniform.len(), 16);
        assert_eq!(u32::from_le_bytes(uniform[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(uniform[4..8].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(uniform[8..12].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(uniform[12..16].try_into().unwrap()), 0);

        let cut = TransitionPlan::compile(TransitionKind::Cut, 0, 1).unwrap();
        assert_eq!(
            u32::from_le_bytes(encode_uniform(cut, 11)[0..4].try_into().unwrap()),
            0
        );

        let wipe = TransitionPlan::compile(TransitionKind::Wipe, 1, 2).unwrap();
        let uniform = encode_uniform(wipe, 5);
        assert_eq!(u32::from_le_bytes(uniform[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(uniform[12..16].try_into().unwrap()), 2);
    }

    #[test]
    fn dimensions_are_validated_before_gpu_work() {
        assert_eq!(validate_dimensions(2, 2, 2, 2), Ok(()));
        assert_eq!(
            validate_dimensions(2, 2, 3, 2),
            Err(NativeTransitionError::WidthMismatch { from: 2, to: 3 })
        );
        assert_eq!(
            validate_dimensions(2, 2, 2, 3),
            Err(NativeTransitionError::HeightMismatch { from: 2, to: 3 })
        );
    }

    #[test]
    fn physical_format_must_be_canonical() {
        assert_eq!(validate_format(TextureFormat::Rgba16Float), Ok(()));
        assert_eq!(
            validate_format(TextureFormat::Rgba8Unorm),
            Err(NativeTransitionError::Gpu(
                NativeGpuError::TextureFormatMismatch {
                    expected: TextureFormat::Rgba16Float,
                    actual: TextureFormat::Rgba8Unorm,
                }
            ))
        );
    }

    #[test]
    fn fade_to_black_plan_has_an_explicit_bounded_uniform_layout() {
        let plan = FadeToBlackPlan::new(
            FadeToBlackPosition::compile(3, 4).unwrap(),
            FadeToBlackPosition::compile(1, 5).unwrap(),
            FadeToBlackPosition::compile(2, 3).unwrap(),
        );
        let uniform = encode_fade_to_black_uniform(plan);
        let mut words = [0_u32; 8];
        for (word, bytes) in words.iter_mut().zip(uniform.chunks_exact(4)) {
            *word = u32::from_le_bytes(bytes.try_into().unwrap());
        }
        assert_eq!(uniform.len(), FADE_TO_BLACK_UNIFORM_SIZE);
        assert_eq!(words, [3, 4, 1, 5, 2, 3, 0, 0]);
        assert_eq!(
            validate_fade_to_black_format(TextureFormat::Rgba16Float),
            Ok(())
        );
    }

    #[test]
    fn fade_to_black_rejects_noncanonical_native_inputs() {
        assert_eq!(
            validate_fade_to_black_format(TextureFormat::Rgba8Unorm),
            Err(NativeFadeToBlackError::SourceFormat {
                actual: TextureFormat::Rgba8Unorm,
            })
        );
        assert_eq!(
            validate_fade_to_black_format(TextureFormat::Rgba32Float),
            Err(NativeFadeToBlackError::SourceFormat {
                actual: TextureFormat::Rgba32Float,
            })
        );
    }
}
