use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::{ShaderDescriptor, ShaderLanguage, ShaderSource, ShaderStage, TextureFormat};

const DIAGNOSTIC_WIDTH: u32 = 2;
const DIAGNOSTIC_HEIGHT: u32 = 2;
const RGBA_BYTES_PER_PIXEL: u32 = 4;
const FULLSCREEN_UNIFORM_SIZE: usize = 16;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(10);
const FULLSCREEN_TIMING_RING_SIZE: usize = 4;
const FULLSCREEN_TIMESTAMP_BYTES: u64 = 16;

/// Maximum number of fullscreen draws accepted by one native render pass.
pub const MAX_NATIVE_FULLSCREEN_DRAWS: usize = 64;

const FULLSCREEN_VERTEX_SHADER: &str = r"
@vertex
fn vertex_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[vertex_index], 0.0, 1.0);
}
";

const DIAGNOSTIC_FRAGMENT_SHADER: &str = r"
@fragment
fn fragment_main() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
";

static NEXT_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TEXTURE_ID: AtomicU64 = AtomicU64::new(1);

/// Native graphics APIs that may be selected for a native context.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeBackend {
    Vulkan,
    Metal,
    Dx12,
    Gl,
}

impl NativeBackend {
    const fn as_wgpu(self) -> wgpu::Backends {
        match self {
            Self::Vulkan => wgpu::Backends::VULKAN,
            Self::Metal => wgpu::Backends::METAL,
            Self::Dx12 => wgpu::Backends::DX12,
            Self::Gl => wgpu::Backends::GL,
        }
    }

    fn from_wgpu(backend: wgpu::Backend) -> Result<Self, NativeGpuError> {
        match backend {
            wgpu::Backend::Vulkan => Ok(Self::Vulkan),
            wgpu::Backend::Metal => Ok(Self::Metal),
            wgpu::Backend::Dx12 => Ok(Self::Dx12),
            wgpu::Backend::Gl => Ok(Self::Gl),
            other => Err(NativeGpuError::UnexpectedBackend(other.to_string())),
        }
    }
}

/// Portable diagnostic information reported by the selected native adapter.
///
/// This is identification only. It does not advertise external-memory
/// interoperability, presentation support, or backend certification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAdapterInfo {
    pub name: String,
    pub backend: NativeBackend,
}

/// Color spaces exposed by the native surface contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeSurfaceColorSpace {
    /// Standard dynamic range with sRGB primaries and transfer function.
    Srgb,
}

/// Presentation modes exposed by the native surface contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativePresentMode {
    /// Vsync-enabled first-in-first-out presentation.
    Fifo,
}

/// Window-compositor alpha modes exposed by the native surface contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeCompositeAlphaMode {
    /// Ignore target alpha and composite the window as opaque.
    Opaque,
}

/// Typed presentation capabilities supported by a surface-compatible adapter.
///
/// `opaque_sdr_formats` contains only non-sRGB texture formats which support
/// the explicit [`NativeSurfaceColorSpace::Srgb`] output color space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSurfaceCapabilities {
    pub opaque_sdr_formats: Vec<TextureFormat>,
    pub present_modes: Vec<NativePresentMode>,
    pub alpha_modes: Vec<NativeCompositeAlphaMode>,
}

impl NativeSurfaceCapabilities {
    /// Selects the minimal opaque SDR configuration, preferring BGRA8.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero extent or when the required explicit sRGB,
    /// FIFO, opaque-alpha contract is unavailable.
    pub fn select_opaque_sdr(
        &self,
        width: u32,
        height: u32,
    ) -> Result<NativeSurfaceConfiguration, NativeGpuError> {
        validate_surface_extent(width, height)?;
        let format = [TextureFormat::Bgra8Unorm, TextureFormat::Rgba8Unorm]
            .into_iter()
            .find(|format| self.opaque_sdr_formats.contains(format))
            .ok_or(NativeGpuError::OpaqueSdrSurfaceUnavailable)?;
        if !self.present_modes.contains(&NativePresentMode::Fifo)
            || !self.alpha_modes.contains(&NativeCompositeAlphaMode::Opaque)
        {
            return Err(NativeGpuError::OpaqueSdrSurfaceUnavailable);
        }
        Ok(NativeSurfaceConfiguration {
            width,
            height,
            format,
            color_space: NativeSurfaceColorSpace::Srgb,
            present_mode: NativePresentMode::Fifo,
            alpha_mode: NativeCompositeAlphaMode::Opaque,
        })
    }
}

/// Selected native surface configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NativeSurfaceConfiguration {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub color_space: NativeSurfaceColorSpace,
    pub present_mode: NativePresentMode,
    pub alpha_mode: NativeCompositeAlphaMode,
}

/// Tightly packed RGBA8 pixels produced by a diagnostic readback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticReadback {
    pub width: u32,
    pub height: u32,
    /// Number of bytes per row in `rgba`; GPU copy padding has been removed.
    pub stride: u32,
    pub rgba: Vec<u8>,
}

/// Tightly packed raw pixels read from an opaque native texture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTextureReadback {
    pub width: u32,
    pub height: u32,
    /// Number of bytes per row in `bytes`; GPU copy padding has been removed.
    pub stride: u32,
    pub format: TextureFormat,
    pub bytes: Vec<u8>,
}

/// Blend operation used by a native fullscreen pipeline.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NativeFullscreenBlend {
    /// Replaces the target with the fragment output.
    #[default]
    Replace,
    /// Composites premultiplied fragment output over the target.
    PremultipliedSourceOver,
}

/// Texture extent validation applied when submitting a fullscreen draw.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NativeSourceExtentPolicy {
    /// Both sources and the target must have identical extents.
    #[default]
    MatchTarget,
    /// Sources may have arbitrary extents independent of each other and the target.
    Independent,
}

/// Creation options for a native fullscreen pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeFullscreenPipelineOptions {
    pub target_format: TextureFormat,
    pub blend: NativeFullscreenBlend,
    pub uniform_size: usize,
    pub source_extent_policy: NativeSourceExtentPolicy,
}

impl NativeFullscreenPipelineOptions {
    /// Creates replacement-blend options with a 16-byte uniform and matching
    /// source/target extents.
    #[must_use]
    pub const fn new(target_format: TextureFormat) -> Self {
        Self {
            target_format,
            blend: NativeFullscreenBlend::Replace,
            uniform_size: FULLSCREEN_UNIFORM_SIZE,
            source_extent_policy: NativeSourceExtentPolicy::MatchTarget,
        }
    }
}

impl Default for NativeFullscreenPipelineOptions {
    fn default() -> Self {
        Self::new(TextureFormat::Rgba8Unorm)
    }
}

/// Target contents used at the start of a native fullscreen render pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NativeFullscreenLoadOp {
    /// Clears the target to transparent black before drawing.
    #[default]
    ClearTransparent,
    /// Clears the target to a normalized RGBA8 color before drawing.
    ClearRgba8([u8; 4]),
    /// Preserves existing target contents. The API does not track whether the
    /// target was initialized by an earlier upload, clear, or render pass.
    Load,
}

/// Availability of native GPU timestamps for fullscreen render passes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeFullscreenTimingSupport {
    Supported,
    Unsupported,
}

/// One completed fullscreen render-pass duration measured by GPU timestamps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeFullscreenTimingSample {
    duration_nanoseconds: f64,
}

impl NativeFullscreenTimingSample {
    /// Returns the finite, nonnegative GPU duration in nanoseconds.
    #[must_use]
    pub const fn duration_nanoseconds(self) -> f64 {
        self.duration_nanoseconds
    }
}

/// Snapshot of fullscreen GPU timestamp profiling.
///
/// Completed samples are drained when this snapshot is taken. Counters are
/// cumulative for the lifetime of the context and saturate at [`u64::MAX`].
#[derive(Clone, Debug, PartialEq)]
pub struct NativeFullscreenTimingTelemetry {
    pub support: NativeFullscreenTimingSupport,
    pub completed_samples: Vec<NativeFullscreenTimingSample>,
    pub pending_samples: usize,
    pub dropped_samples: u64,
    pub unavailable_samples: u64,
}

/// Failures while creating or exercising a native GPU context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeGpuError {
    NoBackendsSelected,
    SurfaceCreate(String),
    AdapterRequest(String),
    DeviceRequest(String),
    UnexpectedBackend(String),
    OpaqueSdrSurfaceUnavailable,
    UnsupportedSurfaceConfiguration,
    SurfaceNotConfigured,
    SurfaceFrameOutstanding,
    SurfacePoisoned,
    InvalidShaderDescriptor(String),
    ZeroTextureDimension {
        width: u32,
        height: u32,
    },
    TextureDimensionLimit {
        width: u32,
        height: u32,
        maximum: u32,
    },
    TextureSizeOverflow,
    UploadStrideTooSmall {
        minimum: usize,
        actual: usize,
    },
    UploadStrideTooLarge(usize),
    UploadLengthTooSmall {
        required: usize,
        actual: usize,
    },
    WrongContext,
    TextureNotRenderTarget,
    TextureNotSampleable,
    TextureDimensionMismatch,
    UnsupportedNativeTextureFormat(TextureFormat),
    TextureFormatMismatch {
        expected: TextureFormat,
        actual: TextureFormat,
    },
    SourceTextureFormatMismatch {
        first: TextureFormat,
        second: TextureFormat,
    },
    SourceTargetAliasing,
    UniformSize {
        expected: usize,
        actual: usize,
    },
    InvalidPipelineUniformSize {
        requested: usize,
        maximum: u64,
    },
    TooManyFullscreenDraws {
        maximum: usize,
        actual: usize,
    },
    Validation(String),
    Poll(String),
    Map(String),
    MapCallbackDropped,
    MapCallbackTimeout,
    ReadbackSizeOverflow,
    ReadbackStrideTooSmall {
        minimum: u32,
        actual: u32,
    },
    ReadbackLengthTooSmall {
        required: usize,
        actual: usize,
    },
}

impl std::fmt::Display for NativeGpuError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(result) = format_surface_error(self, formatter) {
            return result;
        }
        match self {
            Self::NoBackendsSelected => formatter.write_str("no native GPU backends were selected"),
            Self::AdapterRequest(error) => {
                write!(formatter, "failed to request GPU adapter: {error}")
            }
            Self::DeviceRequest(error) => {
                write!(formatter, "failed to request GPU device: {error}")
            }
            Self::UnexpectedBackend(backend) => {
                write!(formatter, "adapter reported unexpected backend {backend}")
            }
            Self::InvalidShaderDescriptor(error) => {
                write!(formatter, "invalid fullscreen fragment shader: {error}")
            }
            Self::ZeroTextureDimension { width, height } => {
                write!(
                    formatter,
                    "texture dimensions must be nonzero, got {width}x{height}"
                )
            }
            Self::TextureDimensionLimit {
                width,
                height,
                maximum,
            } => write!(
                formatter,
                "texture dimensions {width}x{height} exceed the device limit {maximum}"
            ),
            Self::TextureSizeOverflow => formatter.write_str("texture dimensions overflow"),
            Self::UploadStrideTooSmall { minimum, actual } => write!(
                formatter,
                "RGBA8 upload stride {actual} is smaller than tight stride {minimum}"
            ),
            Self::UploadStrideTooLarge(stride) => {
                write!(
                    formatter,
                    "RGBA8 upload stride {stride} exceeds the native layout limit"
                )
            }
            Self::UploadLengthTooSmall { required, actual } => write!(
                formatter,
                "RGBA8 upload contains {actual} bytes but requires {required}"
            ),
            Self::WrongContext => {
                formatter.write_str("native GPU resource belongs to another context")
            }
            Self::TextureNotRenderTarget => {
                formatter.write_str("native GPU texture is not a render target")
            }
            Self::TextureNotSampleable => {
                formatter.write_str("native GPU source texture is not sampleable")
            }
            Self::TextureDimensionMismatch => {
                formatter.write_str("fullscreen source and target texture dimensions differ")
            }
            Self::UnsupportedNativeTextureFormat(format) => {
                write!(formatter, "native texture format {format:?} is unsupported")
            }
            Self::TextureFormatMismatch { expected, actual } => write!(
                formatter,
                "native texture format {actual:?} does not match expected format {expected:?}"
            ),
            Self::SourceTextureFormatMismatch { first, second } => write!(
                formatter,
                "fullscreen source texture formats differ: {first:?} and {second:?}"
            ),
            Self::SourceTargetAliasing => {
                formatter.write_str("fullscreen source texture aliases its render target")
            }
            Self::UniformSize { expected, actual } => write!(
                formatter,
                "fullscreen uniform contains {actual} bytes; expected exactly {expected} aligned bytes"
            ),
            Self::InvalidPipelineUniformSize { requested, maximum } => write!(
                formatter,
                "fullscreen pipeline uniform size {requested} is invalid; expected 1..={maximum} bytes"
            ),
            Self::TooManyFullscreenDraws { maximum, actual } => write!(
                formatter,
                "fullscreen pass contains {actual} draws; maximum is {maximum}"
            ),
            Self::Validation(error) => write!(formatter, "GPU validation failed: {error}"),
            Self::Poll(error) => write!(formatter, "failed while waiting for GPU work: {error}"),
            Self::Map(error) => write!(formatter, "failed to map GPU readback: {error}"),
            Self::MapCallbackDropped => formatter.write_str("GPU map callback did not complete"),
            Self::MapCallbackTimeout => formatter.write_str("GPU map callback timed out"),
            Self::ReadbackSizeOverflow => formatter.write_str("readback dimensions overflow"),
            Self::ReadbackStrideTooSmall { minimum, actual } => write!(
                formatter,
                "readback stride {actual} is smaller than tight stride {minimum}"
            ),
            Self::ReadbackLengthTooSmall { required, actual } => write!(
                formatter,
                "readback contains {actual} bytes but requires {required}"
            ),
            _ => unreachable!("surface errors returned above"),
        }
    }
}

fn format_surface_error(
    error: &NativeGpuError,
    formatter: &mut std::fmt::Formatter<'_>,
) -> Option<std::fmt::Result> {
    match error {
        NativeGpuError::SurfaceCreate(error) => Some(write!(
            formatter,
            "failed to create presentation surface: {error}"
        )),
        NativeGpuError::OpaqueSdrSurfaceUnavailable => Some(formatter.write_str(
            "surface does not support a non-sRGB BGRA8/RGBA8 format with explicit sRGB color space, FIFO presentation, and opaque alpha",
        )),
        NativeGpuError::UnsupportedSurfaceConfiguration => Some(
            formatter.write_str("surface configuration is not supported by this surface"),
        ),
        NativeGpuError::SurfaceNotConfigured => {
            Some(formatter.write_str("surface is not configured"))
        }
        NativeGpuError::SurfaceFrameOutstanding => {
            Some(formatter.write_str("a surface frame is already outstanding"))
        }
        NativeGpuError::SurfacePoisoned => Some(formatter.write_str(
            "surface is poisoned after a failed configuration and must be recreated",
        )),
        _ => None,
    }
}

impl std::error::Error for NativeGpuError {}

/// Opaque, context-bound GPU texture.
///
/// Textures are deliberately not cloneable and expose no `wgpu` handles.
pub struct NativeTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    context_id: u64,
    texture_id: u64,
    width: u32,
    height: u32,
    format: TextureFormat,
    usage: wgpu::TextureUsages,
}

/// Cloneable surface creator tied to one native instance, adapter, and context.
///
/// This factory owns no device or queue and cannot submit GPU work. It may be
/// retained on the macOS main thread while its [`NativeContext`] is moved to a
/// render worker.
#[derive(Clone)]
pub struct NativeSurfaceFactory {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    context_id: u64,
}

impl NativeSurfaceFactory {
    /// Creates one fresh surface from an owned safe window/display provider and
    /// queries its capabilities against the factory's original adapter.
    ///
    /// The resulting surface remains in the original context ownership domain,
    /// so existing textures and pipelines remain valid. On macOS this method
    /// must be called on the main thread.
    ///
    /// # Errors
    ///
    /// Returns the surface creation or opaque-SDR capability failure directly.
    /// No adapter/device recreation or retry is performed.
    ///
    /// # Panics
    ///
    /// On macOS, `wgpu` panics if this is called off the main thread.
    pub fn create_surface<W>(&self, window: W) -> Result<NativeSurface, NativeGpuError>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let surface = self
            .instance
            .create_surface(window)
            .map_err(|error| NativeGpuError::SurfaceCreate(error.to_string()))?;
        self.wrap_surface(surface)
    }

    fn wrap_surface(
        &self,
        surface: wgpu::Surface<'static>,
    ) -> Result<NativeSurface, NativeGpuError> {
        let capabilities = native_surface_capabilities(&surface.get_capabilities(&self.adapter));
        capabilities.select_opaque_sdr(1, 1)?;
        Ok(NativeSurface {
            surface,
            capabilities,
            state: NativeSurfaceState::Unconfigured,
            context_id: self.context_id,
            frame_outstanding: false,
        })
    }
}

/// Opaque presentation surface tied to the context returned beside it.
///
/// The owned window handle provider is retained internally for the lifetime of
/// the surface. Native handles and surface textures are never exposed.
pub struct NativeSurface {
    surface: wgpu::Surface<'static>,
    capabilities: NativeSurfaceCapabilities,
    state: NativeSurfaceState,
    context_id: u64,
    frame_outstanding: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeSurfaceState {
    Unconfigured,
    Configured(NativeSurfaceConfiguration),
    Poisoned,
}

/// Result of acquiring the next native surface frame.
pub enum NativeSurfaceAcquire<'surface> {
    Success(NativeSurfaceFrame<'surface>),
    Suboptimal(NativeSurfaceFrame<'surface>),
    Timeout,
    Outdated,
    Lost,
    Occluded,
    Validation(String),
}

/// Opaque acquired surface target which has not yet been submitted.
///
/// Dropping this value discards the frame. It can only be submitted through
/// [`NativeContext::submit_fullscreen_to_surface`] or
/// [`NativeContext::submit_fullscreen_pass_to_surface`].
pub struct NativeSurfaceFrame<'surface> {
    view: wgpu::TextureView,
    surface_texture: wgpu::SurfaceTexture,
    context_id: u64,
    configuration: NativeSurfaceConfiguration,
    _outstanding: OutstandingSurfaceFrame<'surface>,
}

impl NativeSurfaceFrame<'_> {
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.configuration.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.configuration.height
    }

    #[must_use]
    pub const fn format(&self) -> TextureFormat {
        self.configuration.format
    }
}

/// Opaque surface frame after one successful queue submission.
///
/// This value is only constructible by [`NativeContext`] and is consumed by
/// [`NativeContext::present`]. Dropping it discards the submitted frame.
pub struct NativeSubmittedSurfaceFrame<'surface> {
    frame: NativeSurfaceFrame<'surface>,
}

struct OutstandingSurfaceFrame<'surface> {
    outstanding: &'surface mut bool,
}

impl Drop for OutstandingSurfaceFrame<'_> {
    fn drop(&mut self) {
        *self.outstanding = false;
    }
}

impl NativeSurface {
    /// Returns the presentation capabilities captured for the paired adapter.
    #[must_use]
    pub const fn capabilities(&self) -> &NativeSurfaceCapabilities {
        &self.capabilities
    }

    /// Selects the preferred opaque SDR configuration for an extent.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero extent or unavailable required capability.
    pub fn select_opaque_sdr_configuration(
        &self,
        width: u32,
        height: u32,
    ) -> Result<NativeSurfaceConfiguration, NativeGpuError> {
        self.capabilities.select_opaque_sdr(width, height)
    }

    /// Configures or reconfigures this surface to a nonzero extent.
    ///
    /// # Errors
    ///
    /// Returns an error for the wrong context, an outstanding frame, an invalid
    /// extent, an unsupported configuration, or backend validation failure.
    /// Every error poisons this surface; subsequent configuration or acquisition
    /// returns [`NativeGpuError::SurfacePoisoned`] until the caller creates a
    /// replacement through [`NativeSurfaceFactory`].
    pub async fn configure(
        &mut self,
        context: &NativeContext,
        configuration: NativeSurfaceConfiguration,
    ) -> Result<(), NativeGpuError> {
        if let Err(error) = self.validate_configuration_request(context, configuration) {
            return finish_surface_configuration(&mut self.state, configuration, Err(error));
        }

        // Cancellation after backend dispatch must not expose the previous
        // configuration as usable.
        self.state = NativeSurfaceState::Poisoned;
        let result = self.dispatch_configuration(context, configuration).await;
        finish_surface_configuration(&mut self.state, configuration, result)
    }

    fn validate_configuration_request(
        &self,
        context: &NativeContext,
        configuration: NativeSurfaceConfiguration,
    ) -> Result<(), NativeGpuError> {
        validate_surface_state(self.frame_outstanding, self.state, false)?;
        validate_context_ownership(self.context_id, context.context_id)?;
        context.validate_texture_dimensions(
            configuration.width,
            configuration.height,
            configuration.format,
        )?;
        validate_surface_configuration(&self.capabilities, configuration)?;
        native_texture_format(configuration.format)?;
        Ok(())
    }

    async fn dispatch_configuration(
        &self,
        context: &NativeContext,
        configuration: NativeSurfaceConfiguration,
    ) -> Result<(), NativeGpuError> {
        let validation_scope = context
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);
        self.surface.configure(
            &context.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: native_texture_format(configuration.format)?,
                color_space: native_surface_color_space(configuration.color_space),
                width: configuration.width,
                height: configuration.height,
                desired_maximum_frame_latency: 2,
                present_mode: native_present_mode(configuration.present_mode),
                alpha_mode: native_alpha_mode(configuration.alpha_mode),
                view_formats: vec![],
            },
        );
        check_validation(validation_scope).await
    }

    /// Acquires an opaque frame or returns the backend's typed retry/recovery
    /// outcome.
    ///
    /// # Errors
    ///
    /// Returns an error for a wrong context, an unconfigured or poisoned
    /// surface, or a leaked outstanding frame. Backend validation is captured
    /// as [`NativeSurfaceAcquire::Validation`] rather than reaching `wgpu`'s
    /// default uncaptured-error handler.
    pub async fn acquire(
        &mut self,
        context: &NativeContext,
    ) -> Result<NativeSurfaceAcquire<'_>, NativeGpuError> {
        validate_surface_state(self.frame_outstanding, self.state, true)?;
        validate_context_ownership(self.context_id, context.context_id)?;
        let NativeSurfaceState::Configured(configuration) = self.state else {
            return Err(NativeGpuError::SurfaceNotConfigured);
        };
        let validation_scope = context
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let current = self.surface.get_current_texture();
        if let Some(error) = validation_scope.pop().await {
            return Ok(NativeSurfaceAcquire::Validation(error.to_string()));
        }
        let (surface_texture, suboptimal) = match current {
            wgpu::CurrentSurfaceTexture::Success(texture) => (texture, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
            wgpu::CurrentSurfaceTexture::Timeout => return Ok(NativeSurfaceAcquire::Timeout),
            wgpu::CurrentSurfaceTexture::Outdated => return Ok(NativeSurfaceAcquire::Outdated),
            wgpu::CurrentSurfaceTexture::Lost => return Ok(NativeSurfaceAcquire::Lost),
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(NativeSurfaceAcquire::Occluded),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Ok(NativeSurfaceAcquire::Validation(
                    "surface acquisition validation failed without backend detail".to_owned(),
                ));
            }
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.frame_outstanding = true;
        let frame = NativeSurfaceFrame {
            view,
            surface_texture,
            context_id: self.context_id,
            configuration,
            _outstanding: OutstandingSurfaceFrame {
                outstanding: &mut self.frame_outstanding,
            },
        };
        if suboptimal {
            Ok(NativeSurfaceAcquire::Suboptimal(frame))
        } else {
            Ok(NativeSurfaceAcquire::Success(frame))
        }
    }
}

impl NativeTexture {
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn format(&self) -> TextureFormat {
        self.format
    }
}

/// Opaque, context-bound fullscreen render pipeline.
///
/// The fragment stage is caller-supplied, while `fm-gpu` owns the vertex stage
/// and bind group contract. Pipelines are deliberately not cloneable.
pub struct NativeFullscreenPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    context_id: u64,
    target_format: TextureFormat,
    uniform_size: usize,
    uniform_binding_size: NonZeroU64,
    source_extent_policy: NativeSourceExtentPolicy,
}

/// One draw in a native fullscreen render pass.
pub struct NativeFullscreenDraw<'a> {
    pub pipeline: &'a NativeFullscreenPipeline,
    pub first: &'a NativeTexture,
    pub second: &'a NativeTexture,
    pub uniform: &'a [u8],
}

struct NativeRenderTarget<'target> {
    view: &'target wgpu::TextureView,
    context_id: u64,
    texture_id: Option<u64>,
    width: u32,
    height: u32,
    format: TextureFormat,
    usage: wgpu::TextureUsages,
}

enum FullscreenTimingProfiler {
    Unsupported,
    Supported {
        timestamp_period_nanoseconds: f64,
        state: Box<Mutex<FullscreenTimingState>>,
    },
}

struct FullscreenTimingState {
    slots: [FullscreenTimingSlot; FULLSCREEN_TIMING_RING_SIZE],
    accounting: FullscreenTimingAccounting,
}

struct FullscreenTimingSlot {
    query_set: wgpu::QuerySet,
    resolve_buffer: wgpu::Buffer,
    readback_buffer: wgpu::Buffer,
    status: FullscreenTimingSlotStatus,
}

enum FullscreenTimingSlotStatus {
    Available,
    Pending(mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>),
}

#[derive(Default)]
struct FullscreenTimingAccounting {
    completed: VecDeque<NativeFullscreenTimingSample>,
    dropped: u64,
    unavailable: u64,
}

struct FullscreenTimingReservation {
    query_set: wgpu::QuerySet,
    resolve_buffer: wgpu::Buffer,
    readback_buffer: wgpu::Buffer,
    map_sender: mpsc::SyncSender<Result<(), wgpu::BufferAsyncError>>,
}

enum FullscreenTimingHarvest {
    Pending,
    Completed(NativeFullscreenTimingSample),
    Unavailable,
}

impl FullscreenTimingProfiler {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, supported: bool) -> Self {
        if !supported {
            return Self::Unsupported;
        }

        let slots = std::array::from_fn(|_| FullscreenTimingSlot {
            query_set: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("fm-gpu fullscreen timestamp queries"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            resolve_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("fm-gpu fullscreen timestamp resolve"),
                size: FULLSCREEN_TIMESTAMP_BYTES,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("fm-gpu fullscreen timestamp readback"),
                size: FULLSCREEN_TIMESTAMP_BYTES,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            status: FullscreenTimingSlotStatus::Available,
        });
        Self::Supported {
            timestamp_period_nanoseconds: f64::from(queue.get_timestamp_period()),
            state: Box::new(Mutex::new(FullscreenTimingState {
                slots,
                accounting: FullscreenTimingAccounting::default(),
            })),
        }
    }

    fn reserve(&self, device: &wgpu::Device) -> Option<FullscreenTimingReservation> {
        self.harvest(device);
        let Self::Supported { state, .. } = self else {
            return None;
        };
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = state
            .slots
            .iter_mut()
            .find(|slot| matches!(slot.status, FullscreenTimingSlotStatus::Available))
        else {
            state.accounting.record_dropped();
            return None;
        };
        let (map_sender, map_receiver) = mpsc::sync_channel(1);
        slot.status = FullscreenTimingSlotStatus::Pending(map_receiver);
        Some(FullscreenTimingReservation {
            query_set: slot.query_set.clone(),
            resolve_buffer: slot.resolve_buffer.clone(),
            readback_buffer: slot.readback_buffer.clone(),
            map_sender,
        })
    }

    fn take_telemetry(&self, device: &wgpu::Device) -> NativeFullscreenTimingTelemetry {
        self.harvest(device);
        let Self::Supported { state, .. } = self else {
            return NativeFullscreenTimingTelemetry {
                support: NativeFullscreenTimingSupport::Unsupported,
                completed_samples: Vec::new(),
                pending_samples: 0,
                dropped_samples: 0,
                unavailable_samples: 0,
            };
        };
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending_samples = state
            .slots
            .iter()
            .filter(|slot| matches!(slot.status, FullscreenTimingSlotStatus::Pending(_)))
            .count();
        NativeFullscreenTimingTelemetry {
            support: NativeFullscreenTimingSupport::Supported,
            completed_samples: state.accounting.completed.drain(..).collect(),
            pending_samples,
            dropped_samples: state.accounting.dropped,
            unavailable_samples: state.accounting.unavailable,
        }
    }

    fn harvest(&self, device: &wgpu::Device) {
        let Self::Supported {
            timestamp_period_nanoseconds,
            state,
        } = self
        else {
            return;
        };
        let _ = device.poll(wgpu::PollType::Poll);
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for index in 0..state.slots.len() {
            let result = harvest_fullscreen_timing_slot(
                &mut state.slots[index],
                *timestamp_period_nanoseconds,
            );
            match result {
                FullscreenTimingHarvest::Pending => {}
                FullscreenTimingHarvest::Completed(sample) => {
                    state.accounting.record_completed(sample);
                }
                FullscreenTimingHarvest::Unavailable => state.accounting.record_unavailable(),
            }
        }
    }
}

impl FullscreenTimingAccounting {
    fn record_completed(&mut self, sample: NativeFullscreenTimingSample) {
        if self.completed.len() == FULLSCREEN_TIMING_RING_SIZE {
            self.record_dropped();
        } else {
            self.completed.push_back(sample);
        }
    }

    const fn record_dropped(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }

    const fn record_unavailable(&mut self) {
        self.unavailable = self.unavailable.saturating_add(1);
    }
}

fn harvest_fullscreen_timing_slot(
    slot: &mut FullscreenTimingSlot,
    timestamp_period_nanoseconds: f64,
) -> FullscreenTimingHarvest {
    let FullscreenTimingSlotStatus::Pending(receiver) = &slot.status else {
        return FullscreenTimingHarvest::Pending;
    };
    let map_result = match receiver.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return FullscreenTimingHarvest::Pending,
        Err(mpsc::TryRecvError::Disconnected) => {
            slot.readback_buffer.unmap();
            slot.status = FullscreenTimingSlotStatus::Available;
            return FullscreenTimingHarvest::Unavailable;
        }
    };

    let sample = map_result.ok().and_then(|()| {
        let mapped = slot.readback_buffer.get_mapped_range(..).ok()?;
        let sample = timestamp_sample_from_bytes(&mapped, timestamp_period_nanoseconds);
        drop(mapped);
        sample
    });
    slot.readback_buffer.unmap();
    slot.status = FullscreenTimingSlotStatus::Available;
    sample.map_or(
        FullscreenTimingHarvest::Unavailable,
        FullscreenTimingHarvest::Completed,
    )
}

fn timestamp_sample_from_bytes(
    bytes: &[u8],
    timestamp_period_nanoseconds: f64,
) -> Option<NativeFullscreenTimingSample> {
    let beginning = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    let end = u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?);
    let ticks = end.checked_sub(beginning)?;
    let high_ticks = u32::try_from(ticks >> u32::BITS).ok()?;
    let low_ticks = u32::try_from(ticks & u64::from(u32::MAX)).ok()?;
    let ticks = f64::from(high_ticks) * 4_294_967_296.0 + f64::from(low_ticks);
    let duration_nanoseconds = ticks * timestamp_period_nanoseconds;
    (timestamp_period_nanoseconds.is_finite()
        && timestamp_period_nanoseconds > 0.0
        && duration_nanoseconds.is_finite()
        && duration_nanoseconds >= 0.0)
        .then_some(NativeFullscreenTimingSample {
            duration_nanoseconds,
        })
}

impl<'a> NativeFullscreenDraw<'a> {
    #[must_use]
    pub const fn new(
        pipeline: &'a NativeFullscreenPipeline,
        first: &'a NativeTexture,
        second: &'a NativeTexture,
        uniform: &'a [u8],
    ) -> Self {
        Self {
            pipeline,
            first,
            second,
            uniform,
        }
    }
}

/// Headless native `wgpu` context for GPU-resident execution and diagnostics.
///
/// Native API objects are intentionally not exposed. This context makes no
/// external-memory interoperability, presentation, or certification claim.
pub struct NativeContext {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_info: NativeAdapterInfo,
    context_id: u64,
    max_texture_dimension_2d: u32,
    fullscreen_timing: FullscreenTimingProfiler,
}

impl NativeContext {
    /// Selects one of `backends` and requests a non-fallback adapter, device,
    /// and queue without creating a window or surface.
    ///
    /// # Errors
    ///
    /// Returns an error when no backend is selected or native adapter/device
    /// acquisition fails.
    pub async fn new(
        backends: impl IntoIterator<Item = NativeBackend>,
    ) -> Result<Self, NativeGpuError> {
        let backends = select_backends(backends)?;

        let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_descriptor.backends = backends;
        let instance = wgpu::Instance::new(instance_descriptor);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
                apply_limit_buckets: false,
            })
            .await
            .map_err(|error| NativeGpuError::AdapterRequest(error.to_string()))?;
        Self::from_instance_and_adapter(instance, adapter).await
    }

    /// Creates a surface from an owned safe window/display handle provider,
    /// then requests an adapter and device compatible with that surface.
    ///
    /// The returned surface and context form one ownership domain. On macOS,
    /// this constructor must be called on the main thread because Metal surface
    /// creation uses main-thread-only `AppKit` objects.
    ///
    /// # Errors
    ///
    /// Returns every surface, adapter, device, backend, or opaque-SDR
    /// capability failure directly. No fallback recreation loop is performed.
    ///
    /// # Panics
    ///
    /// On macOS, `wgpu` panics if this is called off the main thread.
    pub async fn new_with_surface<W>(
        backends: impl IntoIterator<Item = NativeBackend>,
        window: W,
    ) -> Result<(Self, NativeSurface), NativeGpuError>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let backends = select_backends(backends)?;
        let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_descriptor.backends = backends;
        let instance = wgpu::Instance::new(instance_descriptor);
        let surface = instance
            .create_surface(window)
            .map_err(|error| NativeGpuError::SurfaceCreate(error.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
                apply_limit_buckets: false,
            })
            .await
            .map_err(|error| NativeGpuError::AdapterRequest(error.to_string()))?;
        let context = Self::from_instance_and_adapter(instance, adapter).await?;
        let native_surface = context.surface_factory().wrap_surface(surface)?;
        Ok((context, native_surface))
    }

    async fn from_instance_and_adapter(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
    ) -> Result<Self, NativeGpuError> {
        let reported_info = adapter.get_info();
        let adapter_info = NativeAdapterInfo {
            name: reported_info.name,
            backend: NativeBackend::from_wgpu(reported_info.backend)?,
        };
        let timestamp_queries_supported =
            adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let required_features = if timestamp_queries_supported {
            wgpu::Features::TIMESTAMP_QUERY
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("fm-gpu native device"),
                required_features,
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await
            .map_err(|error| NativeGpuError::DeviceRequest(error.to_string()))?;
        let max_texture_dimension_2d = device.limits().max_texture_dimension_2d;
        let fullscreen_timing =
            FullscreenTimingProfiler::new(&device, &queue, timestamp_queries_supported);

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            adapter_info,
            context_id: NEXT_CONTEXT_ID.fetch_add(1, Ordering::Relaxed),
            max_texture_dimension_2d,
            fullscreen_timing,
        })
    }

    /// Returns portable identification reported by the selected adapter.
    #[must_use]
    pub const fn adapter_info(&self) -> &NativeAdapterInfo {
        &self.adapter_info
    }

    /// Nonblockingly harvests and drains completed fullscreen GPU timings.
    ///
    /// Unsupported adapters explicitly report [`NativeFullscreenTimingSupport::Unsupported`].
    /// Supported adapters may temporarily return no completed samples while GPU
    /// work is in flight. Profiling failures and ring pressure are reported by
    /// cumulative saturating counters and never fail fullscreen rendering.
    #[must_use]
    pub fn take_fullscreen_timing_telemetry(&self) -> NativeFullscreenTimingTelemetry {
        self.fullscreen_timing.take_telemetry(&self.device)
    }

    /// Returns a cloneable, non-submitting factory in this context's ownership
    /// domain for main-thread surface creation and recreation.
    #[must_use]
    pub fn surface_factory(&self) -> NativeSurfaceFactory {
        NativeSurfaceFactory {
            instance: self.instance.clone(),
            adapter: self.adapter.clone(),
            context_id: self.context_id,
        }
    }

    /// Uploads validated, row-strided RGBA8 pixels to a source texture.
    ///
    /// # Errors
    ///
    /// Returns an error for zero, overflowing, or unsupported dimensions, an
    /// invalid stride or source length, or backend validation failure.
    pub async fn upload_rgba8(
        &self,
        width: u32,
        height: u32,
        stride: usize,
        rgba: &[u8],
    ) -> Result<NativeTexture, NativeGpuError> {
        let layout = validate_upload(width, height, stride, rgba.len())?;
        self.validate_texture_dimensions(width, height, TextureFormat::Rgba8Unorm)?;
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture = self.create_texture(
            "fm-gpu RGBA8 upload",
            width,
            height,
            TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        )?;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba[..layout.required],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(layout.stride),
                rows_per_image: Some(height),
            },
            texture_extent(width, height),
        );
        check_validation(validation_scope).await?;
        Ok(self.wrap_texture(
            texture,
            width,
            height,
            TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        ))
    }

    /// Allocates an RGBA8 render target that remains GPU-resident and may be
    /// reused as a source by later fullscreen submissions.
    ///
    /// # Errors
    ///
    /// Returns an error for zero, overflowing, or unsupported dimensions, or
    /// backend validation failure.
    pub async fn create_rgba8_render_target(
        &self,
        width: u32,
        height: u32,
    ) -> Result<NativeTexture, NativeGpuError> {
        self.validate_texture_dimensions(width, height, TextureFormat::Rgba8Unorm)?;
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture = self.create_texture(
            "fm-gpu RGBA8 render target",
            width,
            height,
            TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        )?;
        check_validation(validation_scope).await?;
        Ok(self.wrap_texture(
            texture,
            width,
            height,
            TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        ))
    }

    /// Allocates an RGBA16-float render target that may be sampled by later
    /// fullscreen submissions.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid dimensions or backend validation failure.
    pub async fn create_rgba16_float_render_target(
        &self,
        width: u32,
        height: u32,
    ) -> Result<NativeTexture, NativeGpuError> {
        self.validate_texture_dimensions(width, height, TextureFormat::Rgba16Float)?;
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture = self.create_texture(
            "fm-gpu RGBA16-float render target",
            width,
            height,
            TextureFormat::Rgba16Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        )?;
        check_validation(validation_scope).await?;
        Ok(self.wrap_texture(
            texture,
            width,
            height,
            TextureFormat::Rgba16Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        ))
    }

    /// Creates a fullscreen pipeline using a compositor-supplied WGSL fragment
    /// stage and the private two-texture plus 16-byte uniform contract.
    ///
    /// Fragment WGSL bindings are group 0: texture 0, texture 1, and uniform
    /// buffer 2. Both textures are unfilterable RGBA8 or RGBA16-float 2D
    /// textures and must have the same format at submission.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-WGSL fragment descriptor or device shader/
    /// pipeline validation failure.
    pub async fn create_fullscreen_pipeline(
        &self,
        descriptor: ShaderDescriptor,
    ) -> Result<NativeFullscreenPipeline, NativeGpuError> {
        self.create_fullscreen_pipeline_for_format(descriptor, TextureFormat::Rgba8Unorm)
            .await
    }

    /// Creates a fullscreen pipeline for one of the native render-target
    /// formats supported by this contract.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported target format, invalid descriptor,
    /// or backend shader/pipeline validation failure.
    pub async fn create_fullscreen_pipeline_for_format(
        &self,
        descriptor: ShaderDescriptor,
        target_format: TextureFormat,
    ) -> Result<NativeFullscreenPipeline, NativeGpuError> {
        self.create_fullscreen_pipeline_with_options(
            descriptor,
            NativeFullscreenPipelineOptions::new(target_format),
        )
        .await
    }

    /// Creates a fullscreen pipeline with explicit target blending, uniform
    /// payload size, and source extent policy.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported target format, a zero or oversized
    /// uniform, an invalid descriptor, or backend shader/pipeline validation.
    pub async fn create_fullscreen_pipeline_with_options(
        &self,
        descriptor: ShaderDescriptor,
        options: NativeFullscreenPipelineOptions,
    ) -> Result<NativeFullscreenPipeline, NativeGpuError> {
        let native_target_format = native_texture_format(options.target_format)?;
        let uniform_binding_size = validate_pipeline_options(
            options,
            self.device.limits().max_uniform_buffer_binding_size,
        )?;
        descriptor
            .validate_contract()
            .map_err(|error| NativeGpuError::InvalidShaderDescriptor(error.to_string()))?;
        if descriptor.stage != ShaderStage::Fragment || descriptor.language != ShaderLanguage::Wgsl
        {
            return Err(NativeGpuError::InvalidShaderDescriptor(
                "expected a WGSL fragment stage".to_owned(),
            ));
        }
        let ShaderSource::Text(fragment_source) = descriptor.source else {
            return Err(NativeGpuError::InvalidShaderDescriptor(
                "expected text WGSL source".to_owned(),
            ));
        };

        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let vertex = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("fm-gpu fullscreen vertex shader"),
                source: wgpu::ShaderSource::Wgsl(FULLSCREEN_VERTEX_SHADER.into()),
            });
        let fragment = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(&descriptor.label),
                source: wgpu::ShaderSource::Wgsl(fragment_source.into()),
            });
        let bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("fm-gpu fullscreen bindings"),
                    entries: &[
                        texture_binding_layout(0),
                        texture_binding_layout(1),
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: Some(uniform_binding_size),
                            },
                            count: None,
                        },
                    ],
                });
        let pipeline_layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("fm-gpu fullscreen pipeline layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&descriptor.label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &vertex,
                    entry_point: Some("vertex_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &fragment,
                    entry_point: Some(&descriptor.entry_point),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: native_target_format,
                        blend: native_blend_state(options.blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        check_validation(validation_scope).await?;
        Ok(NativeFullscreenPipeline {
            pipeline,
            bind_group_layout,
            context_id: self.context_id,
            target_format: options.target_format,
            uniform_size: options.uniform_size,
            uniform_binding_size,
            source_extent_policy: options.source_extent_policy,
        })
    }

    /// Submits one GPU-resident fullscreen draw without polling or readback.
    ///
    /// # Errors
    ///
    /// Returns an error for wrong-context resources, source-target aliasing,
    /// incompatible texture dimensions or roles, a non-16-byte uniform, or
    /// backend validation failure.
    pub async fn submit_fullscreen(
        &self,
        pipeline: &NativeFullscreenPipeline,
        first: &NativeTexture,
        second: &NativeTexture,
        target: &NativeTexture,
        uniform: &[u8],
    ) -> Result<(), NativeGpuError> {
        let draw = NativeFullscreenDraw::new(pipeline, first, second, uniform);
        self.submit_fullscreen_pass(
            target,
            NativeFullscreenLoadOp::ClearTransparent,
            std::slice::from_ref(&draw),
        )
        .await
    }

    /// Submits a bounded draw list in one fullscreen render pass and one queue
    /// submission. All draws are validated before any GPU work is submitted.
    ///
    /// `NativeFullscreenLoadOp::Load` deliberately does not verify that the
    /// target has initialized contents.
    ///
    /// # Errors
    ///
    /// Returns an error when the draw bound is exceeded, any resource belongs
    /// to another context, texture usage/format/aliasing/extent is invalid, a
    /// uniform has the wrong exact size, or backend validation fails.
    pub async fn submit_fullscreen_pass(
        &self,
        target: &NativeTexture,
        load: NativeFullscreenLoadOp,
        draws: &[NativeFullscreenDraw<'_>],
    ) -> Result<(), NativeGpuError> {
        self.submit_fullscreen_pass_to_target(
            NativeRenderTarget {
                view: &target.view,
                context_id: target.context_id,
                texture_id: Some(target.texture_id),
                width: target.width,
                height: target.height,
                format: target.format,
                usage: target.usage,
            },
            load,
            draws,
        )
        .await
    }

    /// Submits one fullscreen draw directly into an acquired opaque surface
    /// target. The returned typestate is the only surface value accepted by
    /// [`Self::present`].
    ///
    /// # Errors
    ///
    /// Returns the same resource, format, extent, uniform, and validation
    /// failures as [`Self::submit_fullscreen`], including wrong-context frames.
    pub async fn submit_fullscreen_to_surface<'surface>(
        &self,
        frame: NativeSurfaceFrame<'surface>,
        pipeline: &NativeFullscreenPipeline,
        first: &NativeTexture,
        second: &NativeTexture,
        uniform: &[u8],
    ) -> Result<NativeSubmittedSurfaceFrame<'surface>, NativeGpuError> {
        let draw = NativeFullscreenDraw::new(pipeline, first, second, uniform);
        self.submit_fullscreen_pass_to_surface(
            frame,
            NativeFullscreenLoadOp::ClearTransparent,
            std::slice::from_ref(&draw),
        )
        .await
    }

    /// Submits a bounded fullscreen draw list directly into an acquired opaque
    /// surface target without exposing a native handle or performing readback.
    ///
    /// # Errors
    ///
    /// Returns the same validation failures as [`Self::submit_fullscreen_pass`].
    pub async fn submit_fullscreen_pass_to_surface<'surface>(
        &self,
        frame: NativeSurfaceFrame<'surface>,
        load: NativeFullscreenLoadOp,
        draws: &[NativeFullscreenDraw<'_>],
    ) -> Result<NativeSubmittedSurfaceFrame<'surface>, NativeGpuError> {
        self.submit_fullscreen_pass_to_target(
            NativeRenderTarget {
                view: &frame.view,
                context_id: frame.context_id,
                texture_id: None,
                width: frame.configuration.width,
                height: frame.configuration.height,
                format: frame.configuration.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            },
            load,
            draws,
        )
        .await?;
        Ok(NativeSubmittedSurfaceFrame { frame })
    }

    /// Dispatches presentation of a successfully submitted surface frame.
    ///
    /// `Ok(())` means that queue presentation dispatch passed `wgpu` validation.
    /// It does not mean the image reached physical scanout, nor does it confirm
    /// asynchronous status from the graphics backend or window compositor.
    ///
    /// # Errors
    ///
    /// Returns [`NativeGpuError::WrongContext`] for another context, or
    /// [`NativeGpuError::Validation`] when queue presentation dispatch fails
    /// validation. Validation is captured before `wgpu`'s default uncaptured-
    /// error handler can run.
    pub async fn present(
        &self,
        submitted: NativeSubmittedSurfaceFrame<'_>,
    ) -> Result<(), NativeGpuError> {
        validate_context_ownership(self.context_id, submitted.frame.context_id)?;
        let NativeSubmittedSurfaceFrame { frame } = submitted;
        let NativeSurfaceFrame {
            surface_texture,
            _outstanding: outstanding,
            ..
        } = frame;
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        self.queue.present(surface_texture);
        let result = check_validation(validation_scope).await;
        drop(outstanding);
        result
    }

    async fn submit_fullscreen_pass_to_target(
        &self,
        target: NativeRenderTarget<'_>,
        load: NativeFullscreenLoadOp,
        draws: &[NativeFullscreenDraw<'_>],
    ) -> Result<(), NativeGpuError> {
        self.validate_fullscreen_pass(&target, draws)?;
        let timing = self.fullscreen_timing.reserve(&self.device);
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let uniform_buffers = draws
            .iter()
            .map(|draw| {
                self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("fm-gpu fullscreen uniform"),
                    size: draw.pipeline.uniform_binding_size.get(),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
                    mapped_at_creation: false,
                })
            })
            .collect::<Vec<_>>();
        for (draw, uniform_buffer) in draws.iter().zip(&uniform_buffers) {
            self.queue.write_buffer(uniform_buffer, 0, draw.uniform);
        }
        let bind_groups = draws
            .iter()
            .zip(&uniform_buffers)
            .map(|(draw, uniform_buffer)| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("fm-gpu fullscreen bind group"),
                    layout: &draw.pipeline.bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&draw.first.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&draw.second.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: uniform_buffer.as_entire_binding(),
                        },
                    ],
                })
            })
            .collect::<Vec<_>>();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fm-gpu fullscreen commands"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fm-gpu fullscreen render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: match load {
                            NativeFullscreenLoadOp::ClearTransparent => {
                                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                            }
                            NativeFullscreenLoadOp::ClearRgba8([red, green, blue, alpha]) => {
                                wgpu::LoadOp::Clear(wgpu::Color {
                                    r: f64::from(red) / 255.0,
                                    g: f64::from(green) / 255.0,
                                    b: f64::from(blue) / 255.0,
                                    a: f64::from(alpha) / 255.0,
                                })
                            }
                            NativeFullscreenLoadOp::Load => wgpu::LoadOp::Load,
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                timestamp_writes: timing
                    .as_ref()
                    .map(|timing| wgpu::RenderPassTimestampWrites {
                        query_set: &timing.query_set,
                        beginning_of_pass_write_index: Some(0),
                        end_of_pass_write_index: Some(1),
                    }),
                ..Default::default()
            });
            for (draw, bindings) in draws.iter().zip(&bind_groups) {
                pass.set_pipeline(&draw.pipeline.pipeline);
                pass.set_bind_group(0, bindings, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        encode_fullscreen_timing_readback(&mut encoder, timing);
        self.queue.submit([encoder.finish()]);
        check_validation(validation_scope).await
    }

    /// Reads a native texture back into tightly packed raw bytes.
    /// GPU rows are dynamically aligned to 256 bytes and mapping is bounded by
    /// the same ten-second timeout as the original diagnostic render.
    ///
    /// # Errors
    ///
    /// Returns an error for a wrong-context texture, overflow, validation,
    /// polling, mapping, timeout, or malformed mapped layout.
    pub async fn readback(
        &self,
        texture: &NativeTexture,
    ) -> Result<NativeTextureReadback, NativeGpuError> {
        validate_context_ownership(self.context_id, texture.context_id)?;
        let bytes_per_pixel = u32::try_from(texture.format.bytes_per_texel())
            .map_err(|_| NativeGpuError::ReadbackSizeOverflow)?;
        let tight_stride = texture
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(NativeGpuError::ReadbackSizeOverflow)?;
        let padded_stride = aligned_bytes_per_row(tight_stride)?;
        let readback_size = u64::from(padded_stride)
            .checked_mul(u64::from(texture.height))
            .ok_or(NativeGpuError::ReadbackSizeOverflow)?;

        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fm-gpu texture readback"),
            size: readback_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fm-gpu readback commands"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_stride),
                    rows_per_image: Some(texture.height),
                },
            },
            texture_extent(texture.width, texture.height),
        );

        let submission = self.queue.submit([encoder.finish()]);
        let (map_sender, map_receiver) = mpsc::sync_channel(1);
        readback.map_async(wgpu::MapMode::Read, .., move |result| {
            let _ = map_sender.send(result);
        });
        let poll_result = self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(DIAGNOSTIC_TIMEOUT),
        });
        let map_result = map_receiver.recv_timeout(DIAGNOSTIC_TIMEOUT);
        check_validation(validation_scope).await?;
        poll_result.map_err(|error| NativeGpuError::Poll(error.to_string()))?;
        map_result
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => NativeGpuError::MapCallbackTimeout,
                mpsc::RecvTimeoutError::Disconnected => NativeGpuError::MapCallbackDropped,
            })?
            .map_err(|error| NativeGpuError::Map(error.to_string()))?;

        let mapped = readback
            .get_mapped_range(..)
            .map_err(|error| NativeGpuError::Map(error.to_string()))?;
        let bytes = unpack_rows(
            &mapped,
            texture.width,
            texture.height,
            bytes_per_pixel,
            padded_stride,
        )?;
        drop(mapped);
        readback.unmap();

        Ok(NativeTextureReadback {
            width: texture.width,
            height: texture.height,
            stride: tight_stride,
            format: texture.format,
            bytes,
        })
    }

    /// Reads an RGBA8 native texture back into tightly packed compatibility
    /// bytes.
    ///
    /// # Errors
    ///
    /// Returns a typed format mismatch before starting a GPU copy, or any raw
    /// readback error.
    pub async fn readback_rgba8(
        &self,
        texture: &NativeTexture,
    ) -> Result<DiagnosticReadback, NativeGpuError> {
        validate_context_ownership(self.context_id, texture.context_id)?;
        if texture.format != TextureFormat::Rgba8Unorm {
            return Err(NativeGpuError::TextureFormatMismatch {
                expected: TextureFormat::Rgba8Unorm,
                actual: texture.format,
            });
        }
        let raw = self.readback(texture).await?;
        Ok(DiagnosticReadback {
            width: raw.width,
            height: raw.height,
            stride: raw.stride,
            rgba: raw.bytes,
        })
    }

    /// Keeps the original 2x2 red draw/readback diagnostic available.
    ///
    /// # Errors
    ///
    /// Returns validation, submission polling, mapping, or layout errors.
    pub async fn diagnostic_readback(&self) -> Result<DiagnosticReadback, NativeGpuError> {
        let source = self
            .upload_rgba8(
                DIAGNOSTIC_WIDTH,
                DIAGNOSTIC_HEIGHT,
                (DIAGNOSTIC_WIDTH * RGBA_BYTES_PER_PIXEL) as usize,
                &[0; 16],
            )
            .await?;
        let target = self
            .create_rgba8_render_target(DIAGNOSTIC_WIDTH, DIAGNOSTIC_HEIGHT)
            .await?;
        let pipeline = self
            .create_fullscreen_pipeline(ShaderDescriptor::new(
                "fm-gpu diagnostic fragment shader",
                ShaderStage::Fragment,
                ShaderLanguage::Wgsl,
                "fragment_main",
                ShaderSource::Text(DIAGNOSTIC_FRAGMENT_SHADER.to_owned()),
            ))
            .await?;
        self.submit_fullscreen(&pipeline, &source, &source, &target, &[0; 16])
            .await?;
        self.readback_rgba8(&target).await
    }

    fn create_texture(
        &self,
        label: &str,
        width: u32,
        height: u32,
        format: TextureFormat,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture, NativeGpuError> {
        Ok(self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: texture_extent(width, height),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: native_texture_format(format)?,
            usage,
            view_formats: &[],
        }))
    }

    fn wrap_texture(
        &self,
        texture: wgpu::Texture,
        width: u32,
        height: u32,
        format: TextureFormat,
        usage: wgpu::TextureUsages,
    ) -> NativeTexture {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        NativeTexture {
            texture,
            view,
            context_id: self.context_id,
            texture_id: NEXT_TEXTURE_ID.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            format,
            usage,
        }
    }

    fn validate_texture_dimensions(
        &self,
        width: u32,
        height: u32,
        format: TextureFormat,
    ) -> Result<(), NativeGpuError> {
        validate_texture_dimensions(width, height, format)?;
        if width > self.max_texture_dimension_2d || height > self.max_texture_dimension_2d {
            return Err(NativeGpuError::TextureDimensionLimit {
                width,
                height,
                maximum: self.max_texture_dimension_2d,
            });
        }
        Ok(())
    }

    fn validate_fullscreen_pass(
        &self,
        target: &NativeRenderTarget<'_>,
        draws: &[NativeFullscreenDraw<'_>],
    ) -> Result<(), NativeGpuError> {
        validate_draw_count(draws.len())?;
        validate_context_ownership(self.context_id, target.context_id)?;
        if !target
            .usage
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(NativeGpuError::TextureNotRenderTarget);
        }
        native_texture_format(target.format)?;
        for draw in draws {
            self.validate_fullscreen_draw(target, draw)?;
        }
        Ok(())
    }

    fn validate_fullscreen_draw(
        &self,
        target: &NativeRenderTarget<'_>,
        draw: &NativeFullscreenDraw<'_>,
    ) -> Result<(), NativeGpuError> {
        let NativeFullscreenDraw {
            pipeline,
            first,
            second,
            uniform,
        } = draw;
        validate_context_ownership(self.context_id, pipeline.context_id)?;
        validate_context_ownership(self.context_id, first.context_id)?;
        validate_context_ownership(self.context_id, second.context_id)?;
        if pipeline.target_format != target.format {
            return Err(NativeGpuError::TextureFormatMismatch {
                expected: pipeline.target_format,
                actual: target.format,
            });
        }
        native_texture_format(first.format)?;
        native_texture_format(second.format)?;
        if !first.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING)
            || !second.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING)
        {
            return Err(NativeGpuError::TextureNotSampleable);
        }
        if first.format != second.format {
            return Err(NativeGpuError::SourceTextureFormatMismatch {
                first: first.format,
                second: second.format,
            });
        }
        if target.texture_id.is_some_and(|target_id| {
            first.texture_id == target_id || second.texture_id == target_id
        }) {
            return Err(NativeGpuError::SourceTargetAliasing);
        }
        if !source_extents_are_valid(
            pipeline.source_extent_policy,
            (first.width, first.height),
            (second.width, second.height),
            (target.width, target.height),
        ) {
            return Err(NativeGpuError::TextureDimensionMismatch);
        }
        if uniform.len() != pipeline.uniform_size {
            return Err(NativeGpuError::UniformSize {
                expected: pipeline.uniform_size,
                actual: uniform.len(),
            });
        }
        Ok(())
    }
}

fn encode_fullscreen_timing_readback(
    encoder: &mut wgpu::CommandEncoder,
    timing: Option<FullscreenTimingReservation>,
) {
    let Some(FullscreenTimingReservation {
        query_set,
        resolve_buffer,
        readback_buffer,
        map_sender,
    }) = timing
    else {
        return;
    };
    encoder.resolve_query_set(&query_set, 0..2, &resolve_buffer, 0);
    encoder.copy_buffer_to_buffer(
        &resolve_buffer,
        0,
        &readback_buffer,
        0,
        FULLSCREEN_TIMESTAMP_BYTES,
    );
    encoder.map_buffer_on_submit(&readback_buffer, wgpu::MapMode::Read, .., move |result| {
        let _ = map_sender.send(result);
    });
}

struct UploadLayout {
    stride: u32,
    required: usize,
}

fn validate_upload(
    width: u32,
    height: u32,
    stride: usize,
    length: usize,
) -> Result<UploadLayout, NativeGpuError> {
    validate_texture_dimensions(width, height, TextureFormat::Rgba8Unorm)?;
    let tight_stride = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(RGBA_BYTES_PER_PIXEL as usize))
        .ok_or(NativeGpuError::TextureSizeOverflow)?;
    if stride < tight_stride {
        return Err(NativeGpuError::UploadStrideTooSmall {
            minimum: tight_stride,
            actual: stride,
        });
    }
    let native_stride =
        u32::try_from(stride).map_err(|_| NativeGpuError::UploadStrideTooLarge(stride))?;
    let required = stride
        .checked_mul(usize::try_from(height).map_err(|_| NativeGpuError::TextureSizeOverflow)?)
        .ok_or(NativeGpuError::TextureSizeOverflow)?;
    if length < required {
        return Err(NativeGpuError::UploadLengthTooSmall {
            required,
            actual: length,
        });
    }
    Ok(UploadLayout {
        stride: native_stride,
        required,
    })
}

fn validate_texture_dimensions(
    width: u32,
    height: u32,
    format: TextureFormat,
) -> Result<(), NativeGpuError> {
    if width == 0 || height == 0 {
        return Err(NativeGpuError::ZeroTextureDimension { width, height });
    }
    width
        .checked_mul(
            u32::try_from(format.bytes_per_texel())
                .map_err(|_| NativeGpuError::TextureSizeOverflow)?,
        )
        .and_then(|stride| stride.checked_mul(height))
        .ok_or(NativeGpuError::TextureSizeOverflow)?;
    Ok(())
}

fn aligned_bytes_per_row(tight_stride: u32) -> Result<u32, NativeGpuError> {
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    tight_stride
        .checked_add(alignment - 1)
        .map(|value| value / alignment * alignment)
        .ok_or(NativeGpuError::ReadbackSizeOverflow)
}

fn unpack_rows(
    source: &[u8],
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    padded_stride: u32,
) -> Result<Vec<u8>, NativeGpuError> {
    let tight_stride = width
        .checked_mul(bytes_per_pixel)
        .ok_or(NativeGpuError::ReadbackSizeOverflow)?;
    if padded_stride < tight_stride {
        return Err(NativeGpuError::ReadbackStrideTooSmall {
            minimum: tight_stride,
            actual: padded_stride,
        });
    }
    let required = usize::try_from(
        padded_stride
            .checked_mul(height)
            .ok_or(NativeGpuError::ReadbackSizeOverflow)?,
    )
    .map_err(|_| NativeGpuError::ReadbackSizeOverflow)?;
    if source.len() < required {
        return Err(NativeGpuError::ReadbackLengthTooSmall {
            required,
            actual: source.len(),
        });
    }
    let output_length = usize::try_from(
        tight_stride
            .checked_mul(height)
            .ok_or(NativeGpuError::ReadbackSizeOverflow)?,
    )
    .map_err(|_| NativeGpuError::ReadbackSizeOverflow)?;
    let tight_stride =
        usize::try_from(tight_stride).map_err(|_| NativeGpuError::ReadbackSizeOverflow)?;
    let padded_stride =
        usize::try_from(padded_stride).map_err(|_| NativeGpuError::ReadbackSizeOverflow)?;
    let mut output = Vec::with_capacity(output_length);
    for row in source[..required].chunks_exact(padded_stride) {
        output.extend_from_slice(&row[..tight_stride]);
    }
    Ok(output)
}

fn native_texture_format(format: TextureFormat) -> Result<wgpu::TextureFormat, NativeGpuError> {
    match format {
        TextureFormat::Rgba8Unorm => Ok(wgpu::TextureFormat::Rgba8Unorm),
        TextureFormat::Bgra8Unorm => Ok(wgpu::TextureFormat::Bgra8Unorm),
        TextureFormat::Rgba16Float => Ok(wgpu::TextureFormat::Rgba16Float),
        _ => Err(NativeGpuError::UnsupportedNativeTextureFormat(format)),
    }
}

fn select_backends(
    backends: impl IntoIterator<Item = NativeBackend>,
) -> Result<wgpu::Backends, NativeGpuError> {
    let backends = backends
        .into_iter()
        .fold(wgpu::Backends::empty(), |selected, backend| {
            selected | backend.as_wgpu()
        });
    if backends.is_empty() {
        Err(NativeGpuError::NoBackendsSelected)
    } else {
        Ok(backends)
    }
}

fn native_surface_capabilities(
    capabilities: &wgpu::SurfaceCapabilities,
) -> NativeSurfaceCapabilities {
    let supports_format = |format| {
        capabilities.format_capabilities.iter().any(|capability| {
            capability.format == format
                && capability
                    .color_spaces
                    .contains(wgpu::SurfaceColorSpaces::SRGB)
        })
    };
    let opaque_sdr_formats = [
        (wgpu::TextureFormat::Bgra8Unorm, TextureFormat::Bgra8Unorm),
        (wgpu::TextureFormat::Rgba8Unorm, TextureFormat::Rgba8Unorm),
    ]
    .into_iter()
    .filter_map(|(native, portable)| supports_format(native).then_some(portable))
    .collect();
    let present_modes = capabilities
        .present_modes
        .contains(&wgpu::PresentMode::Fifo)
        .then_some(NativePresentMode::Fifo)
        .into_iter()
        .collect();
    let alpha_modes = capabilities
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::Opaque)
        .then_some(NativeCompositeAlphaMode::Opaque)
        .into_iter()
        .collect();
    NativeSurfaceCapabilities {
        opaque_sdr_formats,
        present_modes,
        alpha_modes,
    }
}

fn validate_surface_extent(width: u32, height: u32) -> Result<(), NativeGpuError> {
    if width == 0 || height == 0 {
        Err(NativeGpuError::ZeroTextureDimension { width, height })
    } else {
        Ok(())
    }
}

fn validate_surface_configuration(
    capabilities: &NativeSurfaceCapabilities,
    configuration: NativeSurfaceConfiguration,
) -> Result<(), NativeGpuError> {
    validate_surface_extent(configuration.width, configuration.height)?;
    if capabilities
        .opaque_sdr_formats
        .contains(&configuration.format)
        && capabilities
            .present_modes
            .contains(&configuration.present_mode)
        && capabilities.alpha_modes.contains(&configuration.alpha_mode)
    {
        Ok(())
    } else {
        Err(NativeGpuError::UnsupportedSurfaceConfiguration)
    }
}

fn validate_surface_state(
    frame_outstanding: bool,
    state: NativeSurfaceState,
    require_configured: bool,
) -> Result<(), NativeGpuError> {
    if state == NativeSurfaceState::Poisoned {
        Err(NativeGpuError::SurfacePoisoned)
    } else if frame_outstanding {
        Err(NativeGpuError::SurfaceFrameOutstanding)
    } else if require_configured && state == NativeSurfaceState::Unconfigured {
        Err(NativeGpuError::SurfaceNotConfigured)
    } else {
        Ok(())
    }
}

fn finish_surface_configuration(
    state: &mut NativeSurfaceState,
    configuration: NativeSurfaceConfiguration,
    result: Result<(), NativeGpuError>,
) -> Result<(), NativeGpuError> {
    if result.is_ok() {
        *state = NativeSurfaceState::Configured(configuration);
    } else {
        *state = NativeSurfaceState::Poisoned;
    }
    result
}

fn validate_context_ownership(expected: u64, actual: u64) -> Result<(), NativeGpuError> {
    if expected == actual {
        Ok(())
    } else {
        Err(NativeGpuError::WrongContext)
    }
}

const fn native_surface_color_space(
    color_space: NativeSurfaceColorSpace,
) -> wgpu::SurfaceColorSpace {
    match color_space {
        NativeSurfaceColorSpace::Srgb => wgpu::SurfaceColorSpace::Srgb,
    }
}

const fn native_present_mode(present_mode: NativePresentMode) -> wgpu::PresentMode {
    match present_mode {
        NativePresentMode::Fifo => wgpu::PresentMode::Fifo,
    }
}

const fn native_alpha_mode(alpha_mode: NativeCompositeAlphaMode) -> wgpu::CompositeAlphaMode {
    match alpha_mode {
        NativeCompositeAlphaMode::Opaque => wgpu::CompositeAlphaMode::Opaque,
    }
}

fn validate_pipeline_options(
    options: NativeFullscreenPipelineOptions,
    maximum_uniform_size: u64,
) -> Result<NonZeroU64, NativeGpuError> {
    let uniform_size = u64::try_from(options.uniform_size).map_err(|_| {
        NativeGpuError::InvalidPipelineUniformSize {
            requested: options.uniform_size,
            maximum: maximum_uniform_size,
        }
    })?;
    if uniform_size > maximum_uniform_size {
        return Err(NativeGpuError::InvalidPipelineUniformSize {
            requested: options.uniform_size,
            maximum: maximum_uniform_size,
        });
    }
    NonZeroU64::new(uniform_size).ok_or(NativeGpuError::InvalidPipelineUniformSize {
        requested: options.uniform_size,
        maximum: maximum_uniform_size,
    })
}

fn validate_draw_count(draw_count: usize) -> Result<(), NativeGpuError> {
    if draw_count > MAX_NATIVE_FULLSCREEN_DRAWS {
        Err(NativeGpuError::TooManyFullscreenDraws {
            maximum: MAX_NATIVE_FULLSCREEN_DRAWS,
            actual: draw_count,
        })
    } else {
        Ok(())
    }
}

fn source_extents_are_valid(
    policy: NativeSourceExtentPolicy,
    first: (u32, u32),
    second: (u32, u32),
    target: (u32, u32),
) -> bool {
    match policy {
        NativeSourceExtentPolicy::MatchTarget => first == target && second == target,
        NativeSourceExtentPolicy::Independent => true,
    }
}

const fn native_blend_state(blend: NativeFullscreenBlend) -> Option<wgpu::BlendState> {
    match blend {
        NativeFullscreenBlend::Replace => None,
        NativeFullscreenBlend::PremultipliedSourceOver => Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        }),
    }
}

const fn texture_extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

const fn texture_binding_layout(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

async fn check_validation(scope: wgpu::ErrorScopeGuard) -> Result<(), NativeGpuError> {
    if let Some(error) = scope.pop().await {
        Err(NativeGpuError::Validation(error.to_string()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque_sdr_capabilities(formats: Vec<TextureFormat>) -> NativeSurfaceCapabilities {
        NativeSurfaceCapabilities {
            opaque_sdr_formats: formats,
            present_modes: vec![NativePresentMode::Fifo],
            alpha_modes: vec![NativeCompositeAlphaMode::Opaque],
        }
    }

    #[test]
    fn surface_selection_prefers_non_srgb_bgra8() {
        let capabilities =
            opaque_sdr_capabilities(vec![TextureFormat::Rgba8Unorm, TextureFormat::Bgra8Unorm]);
        let configuration = capabilities.select_opaque_sdr(1920, 1080).unwrap();

        assert_eq!(configuration.format, TextureFormat::Bgra8Unorm);
        assert_eq!(configuration.color_space, NativeSurfaceColorSpace::Srgb);
        assert_eq!(configuration.present_mode, NativePresentMode::Fifo);
        assert_eq!(configuration.alpha_mode, NativeCompositeAlphaMode::Opaque);
    }

    #[test]
    fn surface_selection_falls_back_to_rgba_and_rejects_missing_contracts() {
        let rgba = opaque_sdr_capabilities(vec![TextureFormat::Rgba8Unorm]);
        assert_eq!(
            rgba.select_opaque_sdr(1, 1).unwrap().format,
            TextureFormat::Rgba8Unorm
        );
        assert_eq!(
            rgba.select_opaque_sdr(0, 1),
            Err(NativeGpuError::ZeroTextureDimension {
                width: 0,
                height: 1
            })
        );

        let missing_opaque = NativeSurfaceCapabilities {
            alpha_modes: vec![],
            ..rgba
        };
        assert_eq!(
            missing_opaque.select_opaque_sdr(1, 1),
            Err(NativeGpuError::OpaqueSdrSurfaceUnavailable)
        );
    }

    #[test]
    fn surface_capabilities_require_explicit_srgb_and_map_bgra8() {
        let capabilities = wgpu::SurfaceCapabilities {
            formats: vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Rgba8Unorm,
            ],
            format_capabilities: vec![
                wgpu::SurfaceFormatCapabilities {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    color_spaces: wgpu::SurfaceColorSpaces::SRGB,
                },
                wgpu::SurfaceFormatCapabilities {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    color_spaces: wgpu::SurfaceColorSpaces::DISPLAY_P3,
                },
            ],
            present_modes: vec![wgpu::PresentMode::Fifo],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Opaque],
            usages: wgpu::TextureUsages::RENDER_ATTACHMENT,
        };

        let mapped = native_surface_capabilities(&capabilities);
        assert_eq!(mapped.opaque_sdr_formats, vec![TextureFormat::Bgra8Unorm]);
        assert_eq!(
            native_texture_format(TextureFormat::Bgra8Unorm),
            Ok(wgpu::TextureFormat::Bgra8Unorm)
        );
    }

    #[test]
    fn surface_guards_are_typed_and_ordered() {
        assert_eq!(
            validate_surface_state(false, NativeSurfaceState::Unconfigured, true),
            Err(NativeGpuError::SurfaceNotConfigured)
        );
        assert_eq!(
            validate_surface_state(true, NativeSurfaceState::Unconfigured, true),
            Err(NativeGpuError::SurfaceFrameOutstanding)
        );
        let configuration = opaque_sdr_capabilities(vec![TextureFormat::Bgra8Unorm])
            .select_opaque_sdr(1, 1)
            .unwrap();
        assert_eq!(
            validate_surface_state(false, NativeSurfaceState::Configured(configuration), true),
            Ok(())
        );
        assert_eq!(
            validate_surface_state(false, NativeSurfaceState::Unconfigured, false),
            Ok(())
        );
        assert_eq!(
            validate_surface_state(true, NativeSurfaceState::Poisoned, false),
            Err(NativeGpuError::SurfacePoisoned)
        );
    }

    #[test]
    fn every_failed_configuration_poisons_and_discards_stale_state() {
        let configuration = opaque_sdr_capabilities(vec![TextureFormat::Bgra8Unorm])
            .select_opaque_sdr(1920, 1080)
            .unwrap();
        let failures = [
            NativeGpuError::WrongContext,
            NativeGpuError::ZeroTextureDimension {
                width: 0,
                height: 1080,
            },
            NativeGpuError::UnsupportedSurfaceConfiguration,
            NativeGpuError::SurfaceFrameOutstanding,
            NativeGpuError::Validation("configure rejected".to_owned()),
        ];

        for failure in failures {
            let mut state = NativeSurfaceState::Configured(configuration);
            assert_eq!(
                finish_surface_configuration(&mut state, configuration, Err(failure.clone())),
                Err(failure)
            );
            assert_eq!(state, NativeSurfaceState::Poisoned);
            assert_eq!(
                validate_surface_state(false, state, true),
                Err(NativeGpuError::SurfacePoisoned)
            );
        }
    }

    #[test]
    fn outstanding_frame_guard_releases_on_drop() {
        let mut outstanding = true;
        drop(OutstandingSurfaceFrame {
            outstanding: &mut outstanding,
        });
        assert!(!outstanding);
    }

    #[test]
    fn surface_factory_is_cloneable_and_context_ownership_is_exact() {
        const fn assert_clone<T: Clone>() {}
        const fn assert_send<T: Send>() {}

        assert_clone::<NativeSurfaceFactory>();
        assert_send::<NativeContext>();
        assert_send::<NativeSurface>();
        assert_eq!(validate_context_ownership(7, 7), Ok(()));
        assert_eq!(
            validate_context_ownership(7, 8),
            Err(NativeGpuError::WrongContext)
        );
    }

    #[test]
    fn fullscreen_options_preserve_defaults_and_accept_layer_uniforms() {
        let defaults = NativeFullscreenPipelineOptions::default();
        assert_eq!(defaults.target_format, TextureFormat::Rgba8Unorm);
        assert_eq!(defaults.blend, NativeFullscreenBlend::Replace);
        assert_eq!(defaults.uniform_size, 16);
        assert_eq!(
            defaults.source_extent_policy,
            NativeSourceExtentPolicy::MatchTarget
        );
        assert!(native_blend_state(defaults.blend).is_none());

        let layer = NativeFullscreenPipelineOptions {
            blend: NativeFullscreenBlend::PremultipliedSourceOver,
            uniform_size: 64,
            source_extent_policy: NativeSourceExtentPolicy::Independent,
            ..defaults
        };
        assert_eq!(validate_pipeline_options(layer, 65_536).unwrap().get(), 64);
        assert!(native_blend_state(layer.blend).is_some());
    }

    #[test]
    fn fullscreen_limits_return_typed_errors() {
        let options = NativeFullscreenPipelineOptions {
            uniform_size: 0,
            ..NativeFullscreenPipelineOptions::default()
        };
        assert_eq!(
            validate_pipeline_options(options, 65_536),
            Err(NativeGpuError::InvalidPipelineUniformSize {
                requested: 0,
                maximum: 65_536,
            })
        );
        let options = NativeFullscreenPipelineOptions {
            uniform_size: 65_537,
            ..options
        };
        assert_eq!(
            validate_pipeline_options(options, 65_536),
            Err(NativeGpuError::InvalidPipelineUniformSize {
                requested: 65_537,
                maximum: 65_536,
            })
        );
        assert_eq!(validate_draw_count(MAX_NATIVE_FULLSCREEN_DRAWS), Ok(()));
        assert_eq!(
            validate_draw_count(MAX_NATIVE_FULLSCREEN_DRAWS + 1),
            Err(NativeGpuError::TooManyFullscreenDraws {
                maximum: MAX_NATIVE_FULLSCREEN_DRAWS,
                actual: MAX_NATIVE_FULLSCREEN_DRAWS + 1,
            })
        );
    }

    #[test]
    fn fullscreen_extent_policies_are_explicit() {
        assert!(source_extents_are_valid(
            NativeSourceExtentPolicy::MatchTarget,
            (1920, 1080),
            (1920, 1080),
            (1920, 1080),
        ));
        assert!(!source_extents_are_valid(
            NativeSourceExtentPolicy::MatchTarget,
            (1280, 720),
            (1920, 1080),
            (1920, 1080),
        ));
        assert!(source_extents_are_valid(
            NativeSourceExtentPolicy::Independent,
            (1280, 720),
            (640, 360),
            (1920, 1080),
        ));
    }

    #[test]
    fn upload_layout_accepts_padding_and_rejects_bad_inputs() {
        let layout = validate_upload(2, 2, 12, 24).unwrap();
        assert_eq!((layout.stride, layout.required), (12, 24));
        assert_eq!(
            validate_upload(0, 2, 8, 16).err(),
            Some(NativeGpuError::ZeroTextureDimension {
                width: 0,
                height: 2,
            })
        );
        assert_eq!(
            validate_upload(2, 1, 7, 8).err(),
            Some(NativeGpuError::UploadStrideTooSmall {
                minimum: 8,
                actual: 7,
            })
        );
        assert_eq!(
            validate_upload(2, 2, 8, 15).err(),
            Some(NativeGpuError::UploadLengthTooSmall {
                required: 16,
                actual: 15,
            })
        );
        assert_eq!(
            validate_upload(u32::MAX, 2, usize::MAX, usize::MAX).err(),
            Some(NativeGpuError::TextureSizeOverflow)
        );
    }

    #[test]
    fn readback_alignment_is_dynamic_and_checked() {
        assert_eq!(aligned_bytes_per_row(8), Ok(256));
        assert_eq!(aligned_bytes_per_row(256), Ok(256));
        assert_eq!(aligned_bytes_per_row(260), Ok(512));
        assert_eq!(
            aligned_bytes_per_row(u32::MAX),
            Err(NativeGpuError::ReadbackSizeOverflow)
        );
    }

    #[test]
    fn unpack_removes_padded_rows() {
        let mut source = vec![0; 512];
        source[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        source[256..264].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        assert_eq!(
            unpack_rows(&source, 2, 2, 4, 256).unwrap(),
            (1_u8..=16).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unpack_rejects_short_stride_source_and_overflow() {
        assert_eq!(
            unpack_rows(&[0; 8], 2, 1, 4, 7),
            Err(NativeGpuError::ReadbackStrideTooSmall {
                minimum: 8,
                actual: 7,
            })
        );
        assert_eq!(
            unpack_rows(&[0; 511], 2, 2, 4, 256),
            Err(NativeGpuError::ReadbackLengthTooSmall {
                required: 512,
                actual: 511,
            })
        );
        assert_eq!(
            unpack_rows(&[], u32::MAX, 1, 4, u32::MAX),
            Err(NativeGpuError::ReadbackSizeOverflow)
        );
    }

    #[test]
    fn fullscreen_timestamp_conversion_is_explicit_and_rejects_unavailable_values() {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&100_u64.to_le_bytes());
        bytes[8..].copy_from_slice(&104_u64.to_le_bytes());

        let sample = timestamp_sample_from_bytes(&bytes, 2.5).unwrap();
        assert!((sample.duration_nanoseconds() - 10.0).abs() < f64::EPSILON);
        assert!(sample.duration_nanoseconds().is_finite());

        bytes[8..].copy_from_slice(&99_u64.to_le_bytes());
        assert_eq!(timestamp_sample_from_bytes(&bytes, 2.5), None);
        assert_eq!(timestamp_sample_from_bytes(&bytes, 0.0), None);
        assert_eq!(timestamp_sample_from_bytes(&bytes, f64::NAN), None);
        assert_eq!(timestamp_sample_from_bytes(&bytes, f64::INFINITY), None);
        assert_eq!(timestamp_sample_from_bytes(&bytes[..15], 2.5), None);
    }

    #[test]
    fn fullscreen_timing_accounting_is_bounded_and_saturating() {
        let mut accounting = FullscreenTimingAccounting::default();
        let sample = NativeFullscreenTimingSample {
            duration_nanoseconds: 1.0,
        };
        for _ in 0..FULLSCREEN_TIMING_RING_SIZE {
            accounting.record_completed(sample);
        }
        accounting.record_completed(sample);

        assert_eq!(accounting.completed.len(), FULLSCREEN_TIMING_RING_SIZE);
        assert_eq!(accounting.dropped, 1);

        accounting.dropped = u64::MAX;
        accounting.unavailable = u64::MAX;
        accounting.record_dropped();
        accounting.record_unavailable();
        assert_eq!(accounting.dropped, u64::MAX);
        assert_eq!(accounting.unavailable, u64::MAX);
    }
}
