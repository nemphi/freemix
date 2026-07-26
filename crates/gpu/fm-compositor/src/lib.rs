//! Backend-neutral scene planning and deterministic CPU reference composition.

mod cpu;
#[cfg(feature = "native-wgpu")]
mod native;
mod plan;
mod scene;
mod transition;

pub use cpu::{
    CpuColorSourceFrame, CpuExecutionError, CpuSourceFrame, execute_cpu, execute_cpu_frame,
    execute_cpu_with_color, image_from_cpu_frame,
};
#[cfg(feature = "native-wgpu")]
pub use native::{
    MAX_NATIVE_TRANSFORM_DIMENSION, NativeCompositionError, NativeCompositionRenderer,
    NativeSourceFrame, NativeTransitionError, NativeTransitionRenderer,
};
pub use plan::{
    CompilationReport, CompositionPlan, PlanError, PlanLayer, ReportEntry, compile_scene,
};
pub use scene::{
    ChromaKey, Effect, InclusionError, Key, LumaKey, OutputInclusion, OutputTarget, RectMask,
    SafeAreaGuide, Scene, SceneError, SourceId, SourceLayer,
};
pub use transition::{TransitionError, TransitionKind, TransitionPlan, execute_transition};

pub use fm_frame::AlphaMode;
pub use fm_video::{CropRect, ImageFrame, Rgba8, Rotation, Transform};
