use core::fmt;
use std::borrow::Cow;

use fm_frame::{
    AlphaMode, ColorPrimaries, CpuVideoFrame, MatrixCoefficients, MediaTiming, PixelFormat,
    SignalRange, TransferFunction, VideoFrameMetadata,
};
#[cfg(test)]
use fm_frame::{ChromaLocation, ColorMetadata};
use fm_gpu::{
    NativeContext, NativeFullscreenPipeline, NativeFullscreenPipelineOptions, NativeGpuError,
    NativeSourceExtentPolicy, NativeSubmittedSurfaceFrame, NativeSurfaceFrame, NativeTexture,
    ShaderDescriptor, ShaderLanguage, ShaderSource, ShaderStage, TextureFormat,
};

use crate::working_video_frame_metadata;

const IMPORT_FRAGMENT_SHADER: &str = r"
struct ImportUniform {
    transfer: u32,
    primaries: u32,
    padding_1: u32,
    padding_2: u32,
};

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var unused_source_texture: texture_2d<f32>;
@group(0) @binding(2) var<uniform> import_uniform: ImportUniform;

fn srgb_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        return encoded / 12.92;
    }
    return pow((encoded + 0.055) / 1.055, 2.4);
}

fn bt709_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.081 {
        return encoded / 4.5;
    }
    return pow((encoded + 0.099) / 1.099, 1.0 / 0.45);
}

fn decode_transfer(encoded: f32) -> f32 {
    if import_uniform.transfer == 0u {
        return srgb_to_linear(encoded);
    }
    if import_uniform.transfer == 1u {
        return bt709_to_linear(encoded);
    }
    if import_uniform.transfer == 2u {
        return pow(encoded, 2.4);
    }
    return 0.0;
}

fn to_working_primaries(linear: vec3<f32>) -> vec3<f32> {
    if import_uniform.primaries == 2u {
        return linear;
    }

    var xyz = vec3<f32>(0.0);
    if import_uniform.primaries == 0u {
        xyz = vec3<f32>(
            0.4123908 * linear.r + 0.35758433 * linear.g + 0.1804808 * linear.b,
            0.212639 * linear.r + 0.71516865 * linear.g + 0.07219232 * linear.b,
            0.01933082 * linear.r + 0.11919478 * linear.g + 0.95053214 * linear.b,
        );
    } else if import_uniform.primaries == 1u {
        xyz = vec3<f32>(
            0.48657095 * linear.r + 0.2656677 * linear.g + 0.19821729 * linear.b,
            0.22897457 * linear.r + 0.69173855 * linear.g + 0.07928691 * linear.b,
            0.0 * linear.r + 0.04511338 * linear.g + 1.0439444 * linear.b,
        );
    } else {
        return vec3<f32>(0.0);
    }

    return vec3<f32>(
        1.7166512 * xyz.r + -0.35567078 * xyz.g + -0.2533663 * xyz.b,
        -0.6666843 * xyz.r + 1.6164812 * xyz.g + 0.01576855 * xyz.b,
        0.01763986 * xyz.r + -0.04277061 * xyz.g + 0.94210315 * xyz.b,
    );
}

@fragment
fn import_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let encoded = textureLoad(source_texture, vec2<i32>(position.xy), 0);
    if encoded.a == 0.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    let linear = vec3<f32>(
        decode_transfer(encoded.r),
        decode_transfer(encoded.g),
        decode_transfer(encoded.b),
    );
    var working = to_working_primaries(linear);
    working = max(working, vec3<f32>(0.0));
    return vec4<f32>(working * encoded.a, encoded.a);
}
";

const SDR_OUTPUT_FRAGMENT_SHADER: &str = r"
struct OutputViewport {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
};

@group(0) @binding(0) var working_texture: texture_2d<f32>;
@group(0) @binding(1) var unused_working_texture: texture_2d<f32>;
@group(0) @binding(2) var<uniform> viewport: OutputViewport;

fn srgb_from_linear(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        return 12.92 * linear;
    }
    return 1.055 * pow(linear, 1.0 / 2.4) - 0.055;
}

fn center_nearest(coordinate: u32, source_size: u32, destination_size: u32) -> u32 {
    // Native textures are limited to 8192 per axis, so these products fit u32.
    let numerator = (2u * coordinate + 1u) * source_size;
    let denominator = 2u * destination_size;
    return min(numerator / denominator, source_size - 1u);
}

@fragment
fn output_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let target_pixel = vec2<u32>(position.xy);
    let viewport_end = vec2<u32>(viewport.x + viewport.width, viewport.y + viewport.height);
    if target_pixel.x < viewport.x || target_pixel.y < viewport.y ||
        target_pixel.x >= viewport_end.x || target_pixel.y >= viewport_end.y {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let source_size = textureDimensions(working_texture);
    let local = target_pixel - vec2<u32>(viewport.x, viewport.y);
    let source_pixel = vec2<u32>(
        center_nearest(local.x, source_size.x, viewport.width),
        center_nearest(local.y, source_size.y, viewport.height),
    );
    let premultiplied = textureLoad(working_texture, vec2<i32>(source_pixel), 0);
    let rec709 = vec3<f32>(
        1.660491 * premultiplied.r - 0.587641 * premultiplied.g - 0.072850 * premultiplied.b,
        -0.124550 * premultiplied.r + 1.132900 * premultiplied.g - 0.008349 * premultiplied.b,
        -0.018151 * premultiplied.r - 0.100579 * premultiplied.g + 1.118730 * premultiplied.b,
    );
    let bounded = clamp(rec709, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(
        srgb_from_linear(bounded.r),
        srgb_from_linear(bounded.g),
        srgb_from_linear(bounded.b),
        1.0,
    );
}
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputViewport {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn aspect_fit_viewport(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> OutputViewport {
    debug_assert!(source_width > 0 && source_height > 0);
    debug_assert!(target_width > 0 && target_height > 0);

    let target_is_narrower = u64::from(target_width) * u64::from(source_height)
        <= u64::from(target_height) * u64::from(source_width);
    let (width, height) = if target_is_narrower {
        (
            target_width,
            u32::try_from(
                ((u64::from(target_width) * u64::from(source_height)) / u64::from(source_width))
                    .max(1),
            )
            .expect("fitted height cannot exceed target height"),
        )
    } else {
        (
            u32::try_from(
                ((u64::from(target_height) * u64::from(source_width)) / u64::from(source_height))
                    .max(1),
            )
            .expect("fitted width cannot exceed target width"),
            target_height,
        )
    };
    OutputViewport {
        x: (target_width - width) / 2,
        y: (target_height - height) / 2,
        width,
        height,
    }
}

fn encode_viewport(viewport: OutputViewport) -> [u8; 16] {
    let mut bytes = [0; 16];
    for (chunk, value) in bytes.chunks_exact_mut(size_of::<u32>()).zip([
        viewport.x,
        viewport.y,
        viewport.width,
        viewport.height,
    ]) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Validation or GPU failures while importing a portable CPU frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeImportError {
    UnsupportedPixelFormat(PixelFormat),
    MissingMetadata,
    UnsupportedMetadata { actual: VideoFrameMetadata },
    PlaneCountMismatch { expected: usize, actual: usize },
    PlaneStrideTooSmall { minimum: usize, actual: usize },
    PlaneLengthMismatch { expected: usize, actual: usize },
    LayoutOverflow,
    Gpu(NativeGpuError),
}

impl fmt::Display for NativeImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPixelFormat(format) => {
                write!(formatter, "native import does not support {format:?}")
            }
            Self::MissingMetadata => formatter.write_str("native import requires color metadata"),
            Self::UnsupportedMetadata { actual } => write!(
                formatter,
                "native import metadata {actual:?} is outside the supported opaque full-range identity-matrix SDR RGB contract"
            ),
            Self::PlaneCountMismatch { expected, actual } => write!(
                formatter,
                "native import has {actual} planes; expected {expected}"
            ),
            Self::PlaneStrideTooSmall { minimum, actual } => write!(
                formatter,
                "native import stride {actual} is smaller than {minimum}"
            ),
            Self::PlaneLengthMismatch { expected, actual } => write!(
                formatter,
                "native import plane contains {actual} bytes; expected {expected}"
            ),
            Self::LayoutOverflow => formatter.write_str("native import layout arithmetic overflow"),
            Self::Gpu(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpu(error) => Some(error),
            _ => None,
        }
    }
}

impl From<NativeGpuError> for NativeImportError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

/// A canonical GPU-resident premultiplied linear-light BT.2020 frame.
pub struct NativeWorkingFrame {
    texture: NativeTexture,
    timing: MediaTiming,
    metadata: VideoFrameMetadata,
}

impl NativeWorkingFrame {
    #[must_use]
    pub const fn texture(&self) -> &NativeTexture {
        &self.texture
    }

    #[must_use]
    pub const fn timing(&self) -> MediaTiming {
        self.timing
    }

    #[must_use]
    pub const fn metadata(&self) -> VideoFrameMetadata {
        self.metadata
    }
}

/// Color-owned native importer containing only its compiled GPU pipeline.
pub struct NativeImportNormalizer {
    pipeline: NativeFullscreenPipeline,
}

impl NativeImportNormalizer {
    /// Compiles the import shader on an existing context.
    ///
    /// # Errors
    ///
    /// Returns a mapped GPU shader or pipeline validation error.
    pub async fn new(context: &NativeContext) -> Result<Self, NativeImportError> {
        let pipeline = context
            .create_fullscreen_pipeline_for_format(
                ShaderDescriptor::new(
                    "fm-color native import normalization",
                    ShaderStage::Fragment,
                    ShaderLanguage::Wgsl,
                    "import_fragment",
                    ShaderSource::Text(IMPORT_FRAGMENT_SHADER.to_owned()),
                ),
                TextureFormat::Rgba16Float,
            )
            .await?;
        Ok(Self { pipeline })
    }

    /// Validates and normalizes one labeled RGBA8 or BGRA8 CPU frame.
    ///
    /// Validation completes before upload, allocation, or submission. The
    /// source is uploaded once and remains GPU-resident through normalization.
    ///
    /// # Errors
    ///
    /// Returns a typed input contract failure or mapped GPU error.
    pub async fn normalize(
        &self,
        context: &NativeContext,
        source: &CpuVideoFrame,
    ) -> Result<NativeWorkingFrame, NativeImportError> {
        let validated = validate_source(source)?;
        let upload_bytes = rgba_upload_bytes(&validated);
        let uploaded = context
            .upload_rgba8(
                validated.width,
                validated.height,
                validated.stride,
                &upload_bytes,
            )
            .await?;
        let texture = context
            .create_rgba16_float_render_target(validated.width, validated.height)
            .await?;
        let uniform = encode_uniform(validated.transfer, validated.primaries);
        context
            .submit_fullscreen(&self.pipeline, &uploaded, &uploaded, &texture, &uniform)
            .await?;
        Ok(NativeWorkingFrame {
            texture,
            timing: source.timing(),
            metadata: working_video_frame_metadata(),
        })
    }
}

/// Failures while creating a format-specific native SDR output transform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeSdrOutputTransformError {
    UnsupportedTargetFormat(TextureFormat),
    Gpu(NativeGpuError),
}

impl fmt::Display for NativeSdrOutputTransformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTargetFormat(format) => write!(
                formatter,
                "native SDR output does not support target format {format:?}"
            ),
            Self::Gpu(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeSdrOutputTransformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpu(error) => Some(error),
            Self::UnsupportedTargetFormat(_) => None,
        }
    }
}

impl From<NativeGpuError> for NativeSdrOutputTransformError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

fn sdr_output_pipeline_options(
    target_format: TextureFormat,
) -> Result<NativeFullscreenPipelineOptions, NativeSdrOutputTransformError> {
    if !matches!(
        target_format,
        TextureFormat::Rgba8Unorm | TextureFormat::Bgra8Unorm
    ) {
        return Err(NativeSdrOutputTransformError::UnsupportedTargetFormat(
            target_format,
        ));
    }
    Ok(NativeFullscreenPipelineOptions {
        source_extent_policy: NativeSourceExtentPolicy::Independent,
        ..NativeFullscreenPipelineOptions::new(target_format)
    })
}

fn validate_sdr_working_format(format: TextureFormat) -> Result<(), NativeGpuError> {
    if format != TextureFormat::Rgba16Float {
        return Err(NativeGpuError::TextureFormatMismatch {
            expected: TextureFormat::Rgba16Float,
            actual: format,
        });
    }
    Ok(())
}

/// GPU-resident transform from canonical working light to opaque SDR Program pixels.
///
/// The source contract is premultiplied linear-light BT.2020 in `Rgba16Float`.
/// Premultiplied RGB is matrix-transformed directly, explicitly flattening the
/// source over black. Output is straight, explicitly sRGB-encoded Rec.709 in
/// `Rgba8Unorm` or `Bgra8Unorm`; its alpha channel and all aspect-fit bars are
/// opaque. This narrow SDR diagnostic hard-clips linear Rec.709 to `[0, 1]`; it
/// is groundwork, not HDR output or tone mapping.
pub struct NativeSdrOutputTransform {
    pipeline: NativeFullscreenPipeline,
}

impl NativeSdrOutputTransform {
    /// Compiles the SDR Program output shader on an existing context.
    ///
    /// # Errors
    ///
    /// Returns a mapped GPU shader or pipeline validation error.
    pub async fn new(context: &NativeContext) -> Result<Self, NativeGpuError> {
        Self::create(
            context,
            NativeFullscreenPipelineOptions {
                source_extent_policy: NativeSourceExtentPolicy::Independent,
                ..NativeFullscreenPipelineOptions::new(TextureFormat::Rgba8Unorm)
            },
        )
        .await
    }

    /// Compiles the SDR Program output shader for an RGBA8 or native BGRA8 target.
    ///
    /// Both formats contain the same logical RGBA fragment output; BGRA only
    /// changes the target's storage channel order. Neither target format applies
    /// implicit sRGB conversion.
    ///
    /// # Errors
    ///
    /// Returns [`NativeSdrOutputTransformError::UnsupportedTargetFormat`] before
    /// pipeline creation for any format other than `Rgba8Unorm` or
    /// `Bgra8Unorm`, or a mapped GPU shader or pipeline validation error.
    pub async fn new_for_format(
        context: &NativeContext,
        target_format: TextureFormat,
    ) -> Result<Self, NativeSdrOutputTransformError> {
        let options = sdr_output_pipeline_options(target_format)?;
        Ok(Self::create(context, options).await?)
    }

    async fn create(
        context: &NativeContext,
        options: NativeFullscreenPipelineOptions,
    ) -> Result<Self, NativeGpuError> {
        let pipeline = context
            .create_fullscreen_pipeline_with_options(
                ShaderDescriptor::new(
                    "fm-color native SDR Program output",
                    ShaderStage::Fragment,
                    ShaderLanguage::Wgsl,
                    "output_fragment",
                    ShaderSource::Text(SDR_OUTPUT_FRAGMENT_SHADER.to_owned()),
                ),
                options,
            )
            .await?;
        Ok(Self { pipeline })
    }

    /// Writes one canonical working texture into its configured RGBA8 or BGRA8 target.
    ///
    /// This method submits GPU work only. It does not poll, map, or read either
    /// texture back to the CPU. `fm-gpu` validates resource ownership, target
    /// role and format, aliasing, and submission compatibility.
    ///
    /// # Errors
    ///
    /// Returns a format mismatch for a source other than `Rgba16Float`, or a
    /// mapped `fm-gpu` validation or submission error.
    pub async fn transform(
        &self,
        context: &NativeContext,
        working: &NativeTexture,
        target: &NativeTexture,
    ) -> Result<(), NativeGpuError> {
        validate_sdr_working_format(working.format())?;
        let viewport = aspect_fit_viewport(
            working.width(),
            working.height(),
            target.width(),
            target.height(),
        );
        context
            .submit_fullscreen(
                &self.pipeline,
                working,
                working,
                target,
                &encode_viewport(viewport),
            )
            .await
    }

    /// Writes one canonical working texture directly into an acquired surface frame.
    ///
    /// This consumes the acquired frame and submits GPU work without polling,
    /// mapping, or readback. The returned typestate is ready for
    /// [`NativeContext::present`].
    ///
    /// # Errors
    ///
    /// Returns a format mismatch for a source other than `Rgba16Float`, or a
    /// mapped `fm-gpu` surface submission error. On error, the consumed surface
    /// frame is discarded.
    pub async fn transform_to_surface<'surface>(
        &self,
        context: &NativeContext,
        working: &NativeTexture,
        frame: NativeSurfaceFrame<'surface>,
    ) -> Result<NativeSubmittedSurfaceFrame<'surface>, NativeGpuError> {
        validate_sdr_working_format(working.format())?;
        let viewport = aspect_fit_viewport(
            working.width(),
            working.height(),
            frame.width(),
            frame.height(),
        );
        context
            .submit_fullscreen_to_surface(
                frame,
                &self.pipeline,
                working,
                working,
                &encode_viewport(viewport),
            )
            .await
    }
}

struct ValidatedSource<'a> {
    width: u32,
    height: u32,
    stride: usize,
    active_row_bytes: usize,
    bytes: &'a [u8],
    format: PixelFormat,
    primaries: ColorPrimaries,
    transfer: TransferFunction,
}

fn validate_source(source: &CpuVideoFrame) -> Result<ValidatedSource<'_>, NativeImportError> {
    let payload = source.payload();
    if !matches!(payload.format(), PixelFormat::Rgba8 | PixelFormat::Bgra8) {
        return Err(NativeImportError::UnsupportedPixelFormat(payload.format()));
    }
    let actual_metadata = source
        .metadata()
        .ok_or(NativeImportError::MissingMetadata)?;
    if !is_supported_source_metadata(actual_metadata) {
        return Err(NativeImportError::UnsupportedMetadata {
            actual: actual_metadata,
        });
    }
    if payload.planes().len() != 1 {
        return Err(NativeImportError::PlaneCountMismatch {
            expected: 1,
            actual: payload.planes().len(),
        });
    }
    let dimensions = payload.dimensions();
    let plane = &payload.planes()[0];
    let active_row_bytes = validate_plane_layout(
        dimensions.width(),
        dimensions.height(),
        plane.stride(),
        plane.bytes().len(),
    )?;
    Ok(ValidatedSource {
        width: dimensions.width(),
        height: dimensions.height(),
        stride: plane.stride(),
        active_row_bytes,
        bytes: plane.bytes(),
        format: payload.format(),
        primaries: actual_metadata.color().primaries,
        transfer: actual_metadata.color().transfer,
    })
}

fn rgba_upload_bytes<'a>(source: &ValidatedSource<'a>) -> Cow<'a, [u8]> {
    if source.format == PixelFormat::Rgba8 {
        return Cow::Borrowed(source.bytes);
    }
    let mut rgba = source.bytes.to_vec();
    for row in rgba.chunks_exact_mut(source.stride) {
        for pixel in row[..source.active_row_bytes].chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    Cow::Owned(rgba)
}

fn validate_plane_layout(
    width: u32,
    height: u32,
    stride: usize,
    length: usize,
) -> Result<usize, NativeImportError> {
    let minimum = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(NativeImportError::LayoutOverflow)?;
    if stride < minimum {
        return Err(NativeImportError::PlaneStrideTooSmall {
            minimum,
            actual: stride,
        });
    }
    let expected = stride
        .checked_mul(usize::try_from(height).map_err(|_| NativeImportError::LayoutOverflow)?)
        .ok_or(NativeImportError::LayoutOverflow)?;
    if length != expected {
        return Err(NativeImportError::PlaneLengthMismatch {
            expected,
            actual: length,
        });
    }
    Ok(minimum)
}

fn is_supported_source_metadata(metadata: VideoFrameMetadata) -> bool {
    let color = metadata.color();
    matches!(
        color.primaries,
        ColorPrimaries::Bt709 | ColorPrimaries::DisplayP3 | ColorPrimaries::Bt2020
    ) && matches!(
        color.transfer,
        TransferFunction::Srgb | TransferFunction::Bt709 | TransferFunction::Bt1886
    ) && color.matrix == MatrixCoefficients::Identity
        && color.range == SignalRange::Full
        && metadata.alpha_mode() == Some(AlphaMode::Straight)
}

fn encode_uniform(transfer: TransferFunction, primaries: ColorPrimaries) -> [u8; 16] {
    let transfer = match transfer {
        TransferFunction::Srgb => 0_u32,
        TransferFunction::Bt709 => 1_u32,
        TransferFunction::Bt1886 => 2_u32,
        _ => unreachable!("validated native import transfer"),
    };
    let primaries = match primaries {
        ColorPrimaries::Bt709 => 0_u32,
        ColorPrimaries::DisplayP3 => 1_u32,
        ColorPrimaries::Bt2020 => 2_u32,
        ColorPrimaries::Bt601 => unreachable!("validated native import primaries"),
    };
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&transfer.to_le_bytes());
    bytes[4..8].copy_from_slice(&primaries.to_le_bytes());
    bytes
}

#[cfg(test)]
const fn source_video_frame_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU128;

    use fm_frame::{
        ClockDomainId, CpuVideoPayload, CpuVideoPlane, MediaTimestamp, NormalizedDuration,
        NormalizedTimestamp, OriginalTimestamp, SequenceNumber, TimeBase, VideoDimensions,
    };

    use super::*;

    fn output_matrix(rgb: crate::Rgb) -> crate::Rgb {
        crate::Rgb::new(
            1.660_491 * rgb.r - 0.587_641 * rgb.g - 0.072_850 * rgb.b,
            -0.124_550 * rgb.r + 1.132_9 * rgb.g - 0.008_349 * rgb.b,
            -0.018_151 * rgb.r - 0.100_579 * rgb.g + 1.118_73 * rgb.b,
        )
    }

    fn source_pixel(
        viewport: OutputViewport,
        source_width: u32,
        source_height: u32,
        target_x: u32,
        target_y: u32,
    ) -> Option<(u32, u32)> {
        let viewport_end_x = viewport.x.checked_add(viewport.width)?;
        let viewport_end_y = viewport.y.checked_add(viewport.height)?;
        if target_x < viewport.x
            || target_y < viewport.y
            || target_x >= viewport_end_x
            || target_y >= viewport_end_y
        {
            return None;
        }
        Some((
            center_nearest(target_x - viewport.x, source_width, viewport.width),
            center_nearest(target_y - viewport.y, source_height, viewport.height),
        ))
    }

    fn center_nearest(coordinate: u32, source_size: u32, destination_size: u32) -> u32 {
        assert!(source_size > 0);
        assert!(destination_size > 0);
        assert!(coordinate < destination_size);

        let numerator = u128::from(coordinate)
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .and_then(|value| value.checked_mul(u128::from(source_size)))
            .expect("u32 center-nearest numerator fits u128");
        let denominator = u128::from(destination_size)
            .checked_mul(2)
            .expect("u32 center-nearest denominator fits u128");
        let bounded = (numerator / denominator).min(u128::from(source_size - 1));
        u32::try_from(bounded).expect("source coordinate is bounded to u32")
    }

    fn timing() -> MediaTiming {
        MediaTiming::new(
            OriginalTimestamp::new(MediaTimestamp::new(0), TimeBase::new(1, 1).unwrap()),
            NormalizedTimestamp::from_nanos(0),
            NormalizedDuration::from_nanos(1).unwrap(),
            ClockDomainId::new(NonZeroU128::new(1).unwrap()),
            SequenceNumber::new(0),
        )
        .unwrap()
    }

    fn frame(format: PixelFormat) -> CpuVideoFrame {
        let bytes_per_pixel = if format == PixelFormat::Rgba16Float {
            8
        } else {
            4
        };
        let payload = CpuVideoPayload::new(
            format,
            VideoDimensions::new(1, 1).unwrap(),
            vec![CpuVideoPlane::new(bytes_per_pixel, vec![0; bytes_per_pixel]).unwrap()],
        )
        .unwrap();
        CpuVideoFrame::new(timing(), payload)
    }

    #[test]
    fn input_layout_validation_is_typed() {
        assert_eq!(validate_plane_layout(2, 2, 8, 16), Ok(8));
        assert_eq!(validate_plane_layout(2, 2, 12, 24), Ok(8));
        assert_eq!(
            validate_plane_layout(2, 2, 7, 14),
            Err(NativeImportError::PlaneStrideTooSmall {
                minimum: 8,
                actual: 7,
            })
        );
        assert_eq!(
            validate_plane_layout(2, 2, 8, 15),
            Err(NativeImportError::PlaneLengthMismatch {
                expected: 16,
                actual: 15,
            })
        );
    }

    #[test]
    fn sdr_output_constructor_options_accept_only_rgba_and_bgra_unorm() {
        for format in [TextureFormat::Rgba8Unorm, TextureFormat::Bgra8Unorm] {
            let options = sdr_output_pipeline_options(format).unwrap();
            assert_eq!(options.target_format, format);
            assert_eq!(
                options.source_extent_policy,
                NativeSourceExtentPolicy::Independent
            );
        }

        for format in [
            TextureFormat::R8Unorm,
            TextureFormat::Rg8Unorm,
            TextureFormat::Rgba16Float,
            TextureFormat::Rgba32Float,
            TextureFormat::Depth32Float,
        ] {
            assert_eq!(
                sdr_output_pipeline_options(format),
                Err(NativeSdrOutputTransformError::UnsupportedTargetFormat(
                    format
                ))
            );
        }
    }

    #[test]
    fn sdr_output_source_format_validation_requires_canonical_half_float() {
        assert_eq!(
            validate_sdr_working_format(TextureFormat::Rgba16Float),
            Ok(())
        );
        for format in [
            TextureFormat::R8Unorm,
            TextureFormat::Rg8Unorm,
            TextureFormat::Rgba8Unorm,
            TextureFormat::Bgra8Unorm,
            TextureFormat::Rgba32Float,
            TextureFormat::Depth32Float,
        ] {
            assert_eq!(
                validate_sdr_working_format(format),
                Err(NativeGpuError::TextureFormatMismatch {
                    expected: TextureFormat::Rgba16Float,
                    actual: format,
                })
            );
        }
    }

    #[test]
    fn sdr_output_matrix_transfer_and_black_flatten_match_cpu_reference() {
        for sample in [
            crate::Rgb::new(0.0, 0.0, 0.0),
            crate::Rgb::new(1.0, 1.0, 1.0),
            crate::Rgb::new(1.0, 0.0, 0.0),
            crate::Rgb::new(0.0, 1.0, 0.0),
            crate::Rgb::new(0.0, 0.0, 1.0),
            crate::Rgb::new(0.15, 0.4, 0.8),
        ] {
            let expected =
                crate::convert_primaries(sample, ColorPrimaries::Bt2020, ColorPrimaries::Bt709)
                    .unwrap();
            let actual = output_matrix(sample);
            for (actual, expected) in [actual.r, actual.g, actual.b]
                .into_iter()
                .zip([expected.r, expected.g, expected.b])
            {
                assert!((actual - expected).abs() <= 2.0e-6);
            }
        }

        for linear in [0.0_f32, 0.003_130_8, 0.018, 0.214_041_14, 0.5, 1.0] {
            let shader_mirror = if linear <= 0.003_130_8 {
                12.92 * linear
            } else {
                1.055 * linear.powf(1.0 / 2.4) - 0.055
            };
            assert!((shader_mirror - crate::srgb_from_linear(linear)).abs() <= f32::EPSILON);
        }
        assert!((crate::srgb_from_linear(0.214_041_14) - 0.5).abs() <= 1.0e-7);

        let half_red = crate::convert_primaries(
            crate::Rgb::new(1.0, 0.0, 0.0),
            ColorPrimaries::Bt709,
            ColorPrimaries::Bt2020,
        )
        .unwrap()
        .map(|component| component * 0.5);
        let black_matted_linear = output_matrix(half_red).map(|linear| linear.clamp(0.0, 1.0));
        assert!((black_matted_linear.r - 0.5).abs() <= 2.0e-6);
        assert!(black_matted_linear.g <= 2.0e-6);
        assert!(black_matted_linear.b <= 2.0e-6);
        let black_matted = black_matted_linear.map(crate::srgb_from_linear);
        assert!((black_matted.r - crate::srgb_from_linear(0.5)).abs() <= 2.0e-6);
        assert!(black_matted.g <= 3.0e-5);
        assert!(black_matted.b <= 3.0e-5);

        let transparent = output_matrix(crate::Rgb::default())
            .map(|linear| crate::srgb_from_linear(linear.clamp(0.0, 1.0)));
        assert_eq!(transparent, crate::Rgb::default());
    }

    #[test]
    fn sdr_output_fit_policy_centers_opaque_bar_regions() {
        let letterbox = aspect_fit_viewport(4, 1, 6, 5);
        assert_eq!(
            letterbox,
            OutputViewport {
                x: 0,
                y: 2,
                width: 6,
                height: 1,
            }
        );
        assert_eq!(source_pixel(letterbox, 4, 1, 0, 0), None);
        assert_eq!(source_pixel(letterbox, 4, 1, 0, 2), Some((0, 0)));
        assert_eq!(source_pixel(letterbox, 4, 1, 1, 2), Some((1, 0)));
        assert_eq!(source_pixel(letterbox, 4, 1, 5, 2), Some((3, 0)));
        assert_eq!(source_pixel(letterbox, 4, 1, 5, 4), None);

        let pillarbox = aspect_fit_viewport(4, 1, 10, 1);
        assert_eq!(
            pillarbox,
            OutputViewport {
                x: 3,
                y: 0,
                width: 4,
                height: 1,
            }
        );
        assert_eq!(source_pixel(pillarbox, 4, 1, 2, 0), None);
        assert_eq!(source_pixel(pillarbox, 4, 1, 3, 0), Some((0, 0)));
        assert_eq!(source_pixel(pillarbox, 4, 1, 6, 0), Some((3, 0)));
        assert_eq!(source_pixel(pillarbox, 4, 1, 7, 0), None);

        assert_eq!(
            encode_viewport(pillarbox),
            [3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0]
        );
    }

    #[test]
    fn sdr_output_downscale_uses_center_aligned_nearest() {
        let viewport = aspect_fit_viewport(4, 1, 2, 1);
        assert_eq!(
            viewport,
            OutputViewport {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            }
        );
        assert_eq!(source_pixel(viewport, 4, 1, 0, 0), Some((1, 0)));
        assert_eq!(source_pixel(viewport, 4, 1, 1, 0), Some((3, 0)));
    }

    #[test]
    fn native_import_metadata_contract_is_exact() {
        assert_eq!(
            source_video_frame_metadata().color().primaries,
            ColorPrimaries::Bt709
        );
        assert_eq!(
            source_video_frame_metadata().alpha_mode(),
            Some(AlphaMode::Straight)
        );
        assert_eq!(
            working_video_frame_metadata(),
            VideoFrameMetadata::new(
                crate::working_color_metadata(),
                Some(AlphaMode::Premultiplied)
            )
        );

        let top_left = VideoFrameMetadata::new(
            ColorMetadata {
                chroma_location: ChromaLocation::TopLeft,
                ..source_video_frame_metadata().color()
            },
            Some(AlphaMode::Straight),
        );
        assert!(is_supported_source_metadata(top_left));

        let bt1886 = VideoFrameMetadata::new(
            ColorMetadata {
                transfer: TransferFunction::Bt1886,
                ..source_video_frame_metadata().color()
            },
            Some(AlphaMode::Straight),
        );
        assert!(is_supported_source_metadata(bt1886));
        let bt709 = VideoFrameMetadata::new(
            ColorMetadata {
                transfer: TransferFunction::Bt709,
                ..source_video_frame_metadata().color()
            },
            Some(AlphaMode::Straight),
        );
        assert!(is_supported_source_metadata(bt709));
        for primaries in [ColorPrimaries::DisplayP3, ColorPrimaries::Bt2020] {
            assert!(is_supported_source_metadata(VideoFrameMetadata::new(
                ColorMetadata {
                    primaries,
                    ..source_video_frame_metadata().color()
                },
                Some(AlphaMode::Straight),
            )));
        }
        assert_eq!(
            u32::from_le_bytes(
                encode_uniform(TransferFunction::Srgb, ColorPrimaries::Bt709)[..4]
                    .try_into()
                    .unwrap()
            ),
            0
        );
        assert_eq!(
            u32::from_le_bytes(
                encode_uniform(TransferFunction::Bt709, ColorPrimaries::Bt709)[..4]
                    .try_into()
                    .unwrap()
            ),
            1
        );
        assert_eq!(
            u32::from_le_bytes(
                encode_uniform(TransferFunction::Bt1886, ColorPrimaries::Bt709)[..4]
                    .try_into()
                    .unwrap()
            ),
            2
        );
        for (primaries, selector) in [
            (ColorPrimaries::Bt709, 0),
            (ColorPrimaries::DisplayP3, 1),
            (ColorPrimaries::Bt2020, 2),
        ] {
            assert_eq!(
                u32::from_le_bytes(
                    encode_uniform(TransferFunction::Srgb, primaries)[4..8]
                        .try_into()
                        .unwrap()
                ),
                selector
            );
        }
    }

    #[test]
    fn shader_transfer_paths_match_the_cpu_oracle() {
        for transfer in [
            TransferFunction::Srgb,
            TransferFunction::Bt709,
            TransferFunction::Bt1886,
        ] {
            let selector = u32::from_le_bytes(
                encode_uniform(transfer, ColorPrimaries::Bt709)[..4]
                    .try_into()
                    .unwrap(),
            );
            for encoded in [0.0_f32, 0.02, 0.040_45, 0.25, 0.75, 1.0] {
                let shader_mirror = match selector {
                    0 => crate::srgb_to_linear(encoded),
                    1 => crate::bt709_to_linear(encoded),
                    _ => encoded.powf(2.4),
                };
                let cpu = crate::decode_transfer(transfer, encoded).unwrap();
                assert!((shader_mirror - cpu).abs() <= f32::EPSILON);
            }
        }
    }

    #[test]
    fn source_validation_requires_supported_rgba_layout_and_attached_metadata() {
        assert!(matches!(
            validate_source(&frame(PixelFormat::Rgba8)),
            Err(NativeImportError::MissingMetadata)
        ));
        assert!(matches!(
            validate_source(&frame(PixelFormat::Rgba16Float)),
            Err(NativeImportError::UnsupportedPixelFormat(
                PixelFormat::Rgba16Float
            ))
        ));

        let valid = frame(PixelFormat::Rgba8)
            .with_metadata(source_video_frame_metadata())
            .unwrap();
        let validated = validate_source(&valid).unwrap();
        assert_eq!(
            (validated.width, validated.height, validated.stride),
            (1, 1, 4)
        );
        assert_eq!(validated.format, PixelFormat::Rgba8);
        assert_eq!(validated.primaries, ColorPrimaries::Bt709);

        let bgra = frame(PixelFormat::Bgra8)
            .with_metadata(source_video_frame_metadata())
            .unwrap();
        let validated = validate_source(&bgra).unwrap();
        assert_eq!(validated.format, PixelFormat::Bgra8);
        assert!(matches!(rgba_upload_bytes(&validated), Cow::Owned(_)));

        let bt1886_metadata = VideoFrameMetadata::new(
            ColorMetadata {
                transfer: TransferFunction::Bt1886,
                ..source_video_frame_metadata().color()
            },
            Some(AlphaMode::Straight),
        );
        let bt1886 = frame(PixelFormat::Rgba8)
            .with_metadata(bt1886_metadata)
            .unwrap();
        assert_eq!(
            validate_source(&bt1886).unwrap().transfer,
            TransferFunction::Bt1886
        );

        for color in [
            ColorMetadata {
                primaries: ColorPrimaries::Bt601,
                ..source_video_frame_metadata().color()
            },
            ColorMetadata {
                transfer: TransferFunction::Linear,
                ..source_video_frame_metadata().color()
            },
            ColorMetadata {
                range: SignalRange::Limited,
                ..source_video_frame_metadata().color()
            },
        ] {
            let unsupported = frame(PixelFormat::Rgba8)
                .with_metadata(VideoFrameMetadata::new(color, Some(AlphaMode::Straight)))
                .unwrap();
            assert!(matches!(
                validate_source(&unsupported),
                Err(NativeImportError::UnsupportedMetadata { .. })
            ));
        }

        let wrong_metadata = VideoFrameMetadata::new(
            source_video_frame_metadata().color(),
            Some(AlphaMode::Premultiplied),
        );
        let wrong = frame(PixelFormat::Rgba8)
            .with_metadata(wrong_metadata)
            .unwrap();
        assert!(matches!(
            validate_source(&wrong),
            Err(NativeImportError::UnsupportedMetadata { .. })
        ));
    }

    #[test]
    fn bgra_upload_swizzles_active_pixels_without_touching_row_padding() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93];
        let source = ValidatedSource {
            width: 2,
            height: 1,
            stride: 12,
            active_row_bytes: 8,
            bytes: &bytes,
            format: PixelFormat::Bgra8,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
        };
        assert_eq!(
            rgba_upload_bytes(&source).as_ref(),
            &[3, 2, 1, 4, 7, 6, 5, 8, 90, 91, 92, 93]
        );
        let rgba = ValidatedSource {
            format: PixelFormat::Rgba8,
            ..source
        };
        assert!(matches!(rgba_upload_bytes(&rgba), Cow::Borrowed(_)));
    }
}
