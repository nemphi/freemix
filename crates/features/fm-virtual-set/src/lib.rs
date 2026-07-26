//! Backend-neutral virtual-set scenes, shot bindings, and render descriptions.

mod geometry;
mod id;
mod model;
mod render;
mod validation;

pub use geometry::{NormalizedCorners, NormalizedPlane, NormalizedPoint};
pub use id::{CameraId, LayerId, SetId, ShotId, TalentId};
pub use model::{
    BackgroundBinding, CameraPreset, ForegroundBinding, KeyBinding, KeyRequirement, Layer,
    LayerKind, Shot, ShotBindings, TalentBinding, TransitionIntent, TransitionKind,
    VirtualSetScene, WipeDirection,
};
pub use render::{RenderBinding, RenderCamera, RenderDescription, RenderLayer, compile};
pub use validation::{
    BindingKind, MAX_LAYERS, ValidationError, ValidationErrors, validate_scene, validate_shot,
};
