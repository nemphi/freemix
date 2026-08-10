use core::fmt;

use fm_frame::{
    CpuVideoFrame, CpuVideoPayload, CpuVideoPlane, MediaTiming, PixelFormat, VideoDimensions,
    VideoPayloadError,
};
use fm_video::{
    CompositeError, CropError, FrameError, ImageFrame, Layer, Rgba8, TransformError,
    compose_layers, crop, draw_inset_rect_border, premultiply_alpha, transform_nearest,
};

use crate::{AlphaMode, CompositionPlan, Effect, Key, RectMask, SourceId};

#[derive(Clone, Copy, Debug)]
pub struct CpuSourceFrame<'a> {
    pub source: SourceId,
    pub frame: &'a ImageFrame,
}

impl<'a> CpuSourceFrame<'a> {
    #[must_use]
    pub const fn new(source: SourceId, frame: &'a ImageFrame) -> Self {
        Self { source, frame }
    }
}

#[derive(Debug)]
pub enum CpuExecutionError {
    MissingSource(SourceId),
    DuplicateSource(SourceId),
    UnsupportedEffect {
        layer: usize,
        effect: usize,
        name: String,
    },
    UnsupportedPixelFormat(PixelFormat),
    MissingPlane,
    Crop(CropError),
    Transform(TransformError),
    Composite(CompositeError),
    Frame(FrameError),
    VideoPayload(VideoPayloadError),
    Color(fm_color::ColorError),
}

impl fmt::Display for CpuExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource(source) => write!(formatter, "source {} is missing", source.0),
            Self::DuplicateSource(source) => {
                write!(formatter, "source {} was supplied more than once", source.0)
            }
            Self::UnsupportedEffect {
                layer,
                effect,
                name,
            } => write!(
                formatter,
                "layer {layer} effect {effect} ({name}) is unsupported by the CPU reference backend"
            ),
            Self::UnsupportedPixelFormat(format) => {
                write!(
                    formatter,
                    "CPU reference input format {format:?} is unsupported"
                )
            }
            Self::MissingPlane => formatter.write_str("CPU video payload has no packed plane"),
            Self::Crop(error) => error.fmt(formatter),
            Self::Transform(error) => error.fmt(formatter),
            Self::Composite(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::VideoPayload(error) => error.fmt(formatter),
            Self::Color(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CpuExecutionError {}

impl From<CropError> for CpuExecutionError {
    fn from(value: CropError) -> Self {
        Self::Crop(value)
    }
}

impl From<TransformError> for CpuExecutionError {
    fn from(value: TransformError) -> Self {
        Self::Transform(value)
    }
}

impl From<CompositeError> for CpuExecutionError {
    fn from(value: CompositeError) -> Self {
        Self::Composite(value)
    }
}

impl From<FrameError> for CpuExecutionError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

impl From<VideoPayloadError> for CpuExecutionError {
    fn from(value: VideoPayloadError) -> Self {
        Self::VideoPayload(value)
    }
}

impl From<fm_color::ColorError> for CpuExecutionError {
    fn from(value: fm_color::ColorError) -> Self {
        Self::Color(value)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CpuColorSourceFrame<'a> {
    pub source: SourceId,
    pub frame: &'a ImageFrame,
    pub pipeline: fm_color::ColorPipeline<'a>,
}

impl<'a> CpuColorSourceFrame<'a> {
    #[must_use]
    pub const fn new(
        source: SourceId,
        frame: &'a ImageFrame,
        pipeline: fm_color::ColorPipeline<'a>,
    ) -> Self {
        Self {
            source,
            frame,
            pipeline,
        }
    }
}

/// Executes a compiled plan using deterministic RGBA8 reference operations.
///
/// Source frames use the alpha representation declared by each layer. Missing
/// and duplicate source bindings fail instead of selecting a frame implicitly.
///
/// # Errors
/// Returns a typed source, effect, layout, crop, transform, or composition error.
pub fn execute_cpu(
    plan: &CompositionPlan,
    sources: &[CpuSourceFrame<'_>],
) -> Result<ImageFrame, CpuExecutionError> {
    validate_unique_sources(sources)?;
    let mut rendered = Vec::with_capacity(plan.layers().len());
    for (plan_index, layer) in plan.layers().iter().enumerate() {
        let source = sources
            .iter()
            .find(|candidate| candidate.source == layer.source())
            .ok_or(CpuExecutionError::MissingSource(layer.source()))?
            .frame;
        let mut frame = if let Some(rect) = layer.crop() {
            crop(source, rect)?
        } else {
            source.clone()
        };
        frame = apply_key_and_mask(&frame, layer.key(), layer.mask(), layer.alpha_mode())?;
        for (effect_index, effect) in layer.effects().iter().enumerate() {
            apply_effect(plan_index, effect_index, effect)?;
        }
        if layer.alpha_mode() == AlphaMode::Straight {
            frame = premultiply_alpha(&frame)?;
        }
        let transformed =
            transform_nearest(&frame, plan.width(), plan.height(), layer.transform())?;
        let transform = layer.transform();
        let (border_width, border_height) = match transform.rotation {
            fm_video::Rotation::Deg0 | fm_video::Rotation::Deg180 => {
                (transform.scale_width, transform.scale_height)
            }
            fm_video::Rotation::Deg90 | fm_video::Rotation::Deg270 => {
                (transform.scale_height, transform.scale_width)
            }
        };
        rendered.push(draw_inset_rect_border(
            &transformed,
            transform.translation_x,
            transform.translation_y,
            border_width,
            border_height,
            layer.inset_border_width(),
            fm_video::Rgba8::new(255, 255, 255, 255),
        )?);
    }

    let layers = rendered
        .iter()
        .zip(plan.layers())
        .map(|(frame, description)| Layer::new(frame, 0, 0, description.z(), description.opacity()))
        .collect::<Vec<_>>();
    let output = compose_layers(plan.width(), plan.height(), plan.background(), &layers)?;
    draw_safe_areas(output, plan)
}

/// Normalizes source colors through `fm-color` before reference composition.
///
/// The pipeline's output alpha mode must agree with the corresponding scene
/// layer's alpha mode. This seam keeps color policy out of the backend-neutral
/// plan while providing the canonical color implementation to CPU callers.
///
/// # Errors
/// Returns a typed color conversion or composition error.
pub fn execute_cpu_with_color(
    plan: &CompositionPlan,
    sources: &[CpuColorSourceFrame<'_>],
) -> Result<ImageFrame, CpuExecutionError> {
    let converted = sources
        .iter()
        .map(|source| {
            Ok((
                source.source,
                source.pipeline.convert_image(source.frame)?.image,
            ))
        })
        .collect::<Result<Vec<_>, CpuExecutionError>>()?;
    let bindings = converted
        .iter()
        .map(|(source, frame)| CpuSourceFrame::new(*source, frame))
        .collect::<Vec<_>>();
    execute_cpu(plan, &bindings)
}

/// Executes a plan against portable timed CPU frames and returns RGBA8 while
/// preserving caller-selected output timing.
///
/// # Errors
/// Returns a typed error for unsupported input formats or any composition failure.
pub fn execute_cpu_frame(
    plan: &CompositionPlan,
    sources: &[(SourceId, &CpuVideoFrame)],
    output_timing: MediaTiming,
) -> Result<CpuVideoFrame, CpuExecutionError> {
    let images = sources
        .iter()
        .map(|(source, frame)| Ok((*source, image_from_cpu_frame(frame)?)))
        .collect::<Result<Vec<_>, CpuExecutionError>>()?;
    let bindings = images
        .iter()
        .map(|(source, frame)| CpuSourceFrame::new(*source, frame))
        .collect::<Vec<_>>();
    let output = execute_cpu(plan, &bindings)?;
    let dimensions = VideoDimensions::new(output.width(), output.height())
        .ok_or(CpuExecutionError::Frame(FrameError::LayoutOverflow))?;
    let plane = CpuVideoPlane::new(output.stride(), output.pixels().to_vec())?;
    let payload = CpuVideoPayload::new(PixelFormat::Rgba8, dimensions, vec![plane])?;
    Ok(CpuVideoFrame::new(output_timing, payload))
}

/// Copies a portable packed RGBA8/BGRA8 frame into the reference image type.
///
/// # Errors
/// Returns an error for unsupported formats or malformed packed data.
pub fn image_from_cpu_frame(frame: &CpuVideoFrame) -> Result<ImageFrame, CpuExecutionError> {
    let payload = frame.payload();
    let plane = payload.plane(0).ok_or(CpuExecutionError::MissingPlane)?;
    let dimensions = payload.dimensions();
    let format = payload.format();
    if format != PixelFormat::Rgba8 && format != PixelFormat::Bgra8 {
        return Err(CpuExecutionError::UnsupportedPixelFormat(format));
    }
    if format == PixelFormat::Rgba8 {
        return Ok(ImageFrame::new(
            dimensions.width(),
            dimensions.height(),
            plane.stride(),
            plane.bytes().to_vec(),
        )?);
    }

    let mut bytes = plane.bytes().to_vec();
    for y in 0..usize::try_from(dimensions.height()).map_err(|_| FrameError::LayoutOverflow)? {
        let row = y
            .checked_mul(plane.stride())
            .ok_or(FrameError::LayoutOverflow)?;
        for x in 0..usize::try_from(dimensions.width()).map_err(|_| FrameError::LayoutOverflow)? {
            let offset = row
                .checked_add(x.checked_mul(4).ok_or(FrameError::LayoutOverflow)?)
                .ok_or(FrameError::LayoutOverflow)?;
            bytes.swap(offset, offset + 2);
        }
    }
    Ok(ImageFrame::new(
        dimensions.width(),
        dimensions.height(),
        plane.stride(),
        bytes,
    )?)
}

fn validate_unique_sources(sources: &[CpuSourceFrame<'_>]) -> Result<(), CpuExecutionError> {
    for (index, source) in sources.iter().enumerate() {
        if sources[..index]
            .iter()
            .any(|candidate| candidate.source == source.source)
        {
            return Err(CpuExecutionError::DuplicateSource(source.source));
        }
    }
    Ok(())
}

fn apply_effect(
    layer: usize,
    effect_index: usize,
    effect: &Effect,
) -> Result<(), CpuExecutionError> {
    if effect.name() == "passthrough" && effect.parameters().is_empty() {
        Ok(())
    } else {
        Err(CpuExecutionError::UnsupportedEffect {
            layer,
            effect: effect_index,
            name: effect.name().to_owned(),
        })
    }
}

fn apply_key_and_mask(
    source: &ImageFrame,
    key: Option<Key>,
    mask: Option<RectMask>,
    alpha_mode: AlphaMode,
) -> Result<ImageFrame, FrameError> {
    if key.is_none() && mask.is_none() {
        return Ok(source.clone());
    }
    let mut bytes = Vec::with_capacity(source.pixels().len());
    for y in 0..source.height() {
        for x in 0..source.width() {
            let pixel = source.pixel(x, y).ok_or(FrameError::LayoutOverflow)?;
            let key_alpha = key.map_or(u8::MAX, |key| key_factor(pixel, key));
            let mask_alpha = mask.map_or(u8::MAX, |mask| mask_factor(x, y, mask));
            let factor = multiply_u8(key_alpha, mask_alpha);
            let mut adjusted = apply_spill(pixel, key, key_alpha);
            match alpha_mode {
                AlphaMode::Straight => adjusted.a = multiply_u8(adjusted.a, factor),
                AlphaMode::Premultiplied => {
                    adjusted.r = multiply_u8(adjusted.r, factor);
                    adjusted.g = multiply_u8(adjusted.g, factor);
                    adjusted.b = multiply_u8(adjusted.b, factor);
                    adjusted.a = multiply_u8(adjusted.a, factor);
                }
            }
            bytes.extend_from_slice(&adjusted.to_bytes());
        }
    }
    ImageFrame::new(
        source.width(),
        source.height(),
        source.width() as usize * 4,
        bytes,
    )
}

fn key_factor(pixel: Rgba8, key: Key) -> u8 {
    match key {
        Key::Chroma(parameters) => {
            let distance = pixel
                .r
                .abs_diff(parameters.color.r)
                .max(pixel.g.abs_diff(parameters.color.g))
                .max(pixel.b.abs_diff(parameters.color.b));
            ramp(distance, parameters.tolerance, parameters.softness)
        }
        Key::Luma(parameters) => {
            let luma = u8::try_from(
                (u32::from(pixel.r) * 54
                    + u32::from(pixel.g) * 183
                    + u32::from(pixel.b) * 19
                    + 128)
                    / 256,
            )
            .unwrap_or(u8::MAX);
            let factor = ramp(luma, parameters.threshold, parameters.softness);
            if parameters.invert {
                u8::MAX - factor
            } else {
                factor
            }
        }
    }
}

fn ramp(value: u8, threshold: u8, softness: u8) -> u8 {
    if value <= threshold {
        return 0;
    }
    if softness == 0 {
        return u8::MAX;
    }
    let distance = value - threshold;
    if distance >= softness {
        u8::MAX
    } else {
        u8::try_from((u16::from(distance) * 255 + u16::from(softness) / 2) / u16::from(softness))
            .unwrap_or(u8::MAX)
    }
}

fn mask_factor(x: u32, y: u32, mask: RectMask) -> u8 {
    let inside = x >= mask.x
        && y >= mask.y
        && x < mask.x.saturating_add(mask.width)
        && y < mask.y.saturating_add(mask.height);
    if inside == mask.invert { 0 } else { u8::MAX }
}

fn apply_spill(pixel: Rgba8, key: Option<Key>, key_alpha: u8) -> Rgba8 {
    let Some(Key::Chroma(parameters)) = key else {
        return pixel;
    };
    if parameters.spill == 0 || key_alpha == u8::MAX {
        return pixel;
    }
    let amount = multiply_u8(parameters.spill, u8::MAX - key_alpha);
    let mut output = pixel;
    if parameters.color.g >= parameters.color.r && parameters.color.g >= parameters.color.b {
        let neutral = output.r.max(output.b);
        output.g = interpolate(output.g, neutral, amount);
    } else if parameters.color.b >= parameters.color.r {
        let neutral = output.r.max(output.g);
        output.b = interpolate(output.b, neutral, amount);
    } else {
        let neutral = output.g.max(output.b);
        output.r = interpolate(output.r, neutral, amount);
    }
    output
}

fn interpolate(from: u8, to: u8, amount: u8) -> u8 {
    let inverse = u16::from(u8::MAX - amount);
    let value = u16::from(from) * inverse + u16::from(to) * u16::from(amount) + 127;
    u8::try_from(value / 255).unwrap_or(u8::MAX)
}

fn multiply_u8(left: u8, right: u8) -> u8 {
    let product = u16::from(left) * u16::from(right) + 127;
    u8::try_from(product / 255).unwrap_or(u8::MAX)
}

fn draw_safe_areas(
    output: ImageFrame,
    plan: &CompositionPlan,
) -> Result<ImageFrame, CpuExecutionError> {
    if plan.safe_areas().is_empty() {
        return Ok(output);
    }
    let mut bytes = output.pixels().to_vec();
    for guide in plan.safe_areas() {
        let right = guide.x + guide.width - 1;
        let bottom = guide.y + guide.height - 1;
        for x in guide.x..=right {
            set_packed_pixel(&mut bytes, output.stride(), x, guide.y, guide.color)?;
            set_packed_pixel(&mut bytes, output.stride(), x, bottom, guide.color)?;
        }
        for y in guide.y..=bottom {
            set_packed_pixel(&mut bytes, output.stride(), guide.x, y, guide.color)?;
            set_packed_pixel(&mut bytes, output.stride(), right, y, guide.color)?;
        }
    }
    Ok(ImageFrame::new(
        output.width(),
        output.height(),
        output.stride(),
        bytes,
    )?)
}

fn set_packed_pixel(
    bytes: &mut [u8],
    stride: usize,
    x: u32,
    y: u32,
    pixel: Rgba8,
) -> Result<(), FrameError> {
    let offset = usize::try_from(y)
        .ok()
        .and_then(|row| row.checked_mul(stride))
        .and_then(|row| {
            usize::try_from(x)
                .ok()
                .and_then(|column| column.checked_mul(4))
                .and_then(|column| row.checked_add(column))
        })
        .ok_or(FrameError::LayoutOverflow)?;
    let destination = bytes
        .get_mut(offset..offset + 4)
        .ok_or(FrameError::LayoutOverflow)?;
    destination.copy_from_slice(&pixel.to_bytes());
    Ok(())
}
