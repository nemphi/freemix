//! Backend-neutral scene planning and deterministic CPU reference composition.

mod cpu;
mod fade_to_black;
#[cfg(feature = "native-wgpu")]
mod native;
mod plan;
mod scene;
mod transition;

pub use cpu::{
    CpuColorSourceFrame, CpuExecutionError, CpuSourceFrame, execute_cpu, execute_cpu_frame,
    execute_cpu_with_color, image_from_cpu_frame,
};
pub use fade_to_black::{
    FadeToBlackPlan, FadeToBlackPlanError, FadeToBlackPosition, MAX_FADE_TO_BLACK_DENOMINATOR,
    execute_fade_to_black_cpu,
};
#[cfg(feature = "native-wgpu")]
pub use native::{
    MAX_NATIVE_TRANSFORM_DIMENSION, NativeCompositionError, NativeCompositionRenderer,
    NativeFadeToBlackError, NativeFadeToBlackRenderer, NativeSourceFrame, NativeTransitionError,
    NativeTransitionRenderer,
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
