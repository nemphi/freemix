//! Portable GPU device, resource-pool, graph, and recovery contracts.
//!
//! This crate deliberately exposes no native handles and does not claim that
//! contract validation is equivalent to validation by a native GPU backend.

mod capability;
mod graph;
#[cfg(feature = "native-wgpu")]
mod native;
mod pool;
mod presentation;
mod resource;
mod shader;
mod simulated;

pub use capability::{AdapterProfile, DeviceFeature, DeviceLimits, DeviceProfile, ProfileError};
pub use graph::{
    GraphError, GraphResource, GraphResourceId, PassId, RenderGraph, RenderPassDescriptor,
    ResourceAccess, ResourceOrigin, ValidatedRenderGraph,
};
#[cfg(feature = "native-wgpu")]
pub use native::{
    DiagnosticReadback, MAX_NATIVE_FULLSCREEN_DRAWS, NativeAdapterInfo, NativeBackend,
    NativeCompositeAlphaMode, NativeContext, NativeFullscreenBlend, NativeFullscreenDraw,
    NativeFullscreenLoadOp, NativeFullscreenPipeline, NativeFullscreenPipelineOptions,
    NativeFullscreenTimingSample, NativeFullscreenTimingSupport, NativeFullscreenTimingTelemetry,
    NativeGpuError, NativePresentMode, NativeSourceExtentPolicy, NativeSubmittedSurfaceFrame,
    NativeSurface, NativeSurfaceAcquire, NativeSurfaceCapabilities, NativeSurfaceColorSpace,
    NativeSurfaceConfiguration, NativeSurfaceFactory, NativeSurfaceFrame, NativeTexture,
    NativeTextureReadback,
};
pub use pool::{
    BufferPool, FenceValue, LeaseId, PoolBudget, PoolError, PoolTelemetry, PooledLease, ResourceId,
    ResourcePool, TexturePool,
};
pub use presentation::{
    FrameDecision, FrameGeneration, FrameRejection, PresentationAction, PresentationExtent,
    PresentationFailure, PresentationFrame, PresentationLifecycle, PresentationState,
    PresentationTelemetry, ResizeGeneration, SurfaceAcquisition,
};
pub use resource::{
    BufferDescriptor, BufferUsage, DescriptorError, ResourceDescriptor, TextureDescriptor,
    TextureDimension, TextureFormat, TextureUsage,
};
pub use shader::{
    ContractShaderValidator, ShaderDescriptor, ShaderError, ShaderLanguage, ShaderSource,
    ShaderStage, ShaderValidationLevel, ShaderValidationMetadata, ShaderValidator, ValidatedShader,
};
pub use simulated::{
    DeviceError, DeviceState, ExternalLeaseError, RecoveryError, RecoveryPolicy, SimulatedBackend,
    SimulatedDevice,
};
