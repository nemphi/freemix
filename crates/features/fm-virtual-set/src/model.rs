use std::time::Duration;

use fm_types::InputId;

use crate::{CameraId, LayerId, NormalizedPlane, NormalizedPoint, SetId, ShotId, TalentId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyRequirement {
    None,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerKind {
    Background,
    Foreground,
    Talent {
        talent_id: TalentId,
        key: KeyRequirement,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layer {
    pub id: LayerId,
    pub z_order: i32,
    pub plane: NormalizedPlane,
    pub kind: LayerKind,
}

impl Layer {
    #[must_use]
    pub const fn new(id: LayerId, z_order: i32, plane: NormalizedPlane, kind: LayerKind) -> Self {
        Self {
            id,
            z_order,
            plane,
            kind,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraPreset {
    pub id: CameraId,
    pub position: NormalizedPoint,
    pub zoom: f32,
}

impl CameraPreset {
    #[must_use]
    pub const fn new(id: CameraId, position: NormalizedPoint, zoom: f32) -> Self {
        Self { id, position, zoom }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VirtualSetScene {
    pub id: SetId,
    pub talents: Vec<TalentId>,
    pub layers: Vec<Layer>,
    pub camera_presets: Vec<CameraPreset>,
}

impl VirtualSetScene {
    #[must_use]
    pub const fn new(
        id: SetId,
        talents: Vec<TalentId>,
        layers: Vec<Layer>,
        camera_presets: Vec<CameraPreset>,
    ) -> Self {
        Self {
            id,
            talents,
            layers,
            camera_presets,
        }
    }

    #[must_use]
    pub fn camera_preset(&self, id: CameraId) -> Option<&CameraPreset> {
        self.camera_presets.iter().find(|preset| preset.id == id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackgroundBinding {
    pub layer_id: LayerId,
    pub source: InputId,
}

impl BackgroundBinding {
    #[must_use]
    pub const fn new(layer_id: LayerId, source: InputId) -> Self {
        Self { layer_id, source }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForegroundBinding {
    pub layer_id: LayerId,
    pub source: InputId,
}

impl ForegroundBinding {
    #[must_use]
    pub const fn new(layer_id: LayerId, source: InputId) -> Self {
        Self { layer_id, source }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TalentBinding {
    pub layer_id: LayerId,
    pub talent_id: TalentId,
    pub source: InputId,
}

impl TalentBinding {
    #[must_use]
    pub const fn new(layer_id: LayerId, talent_id: TalentId, source: InputId) -> Self {
        Self {
            layer_id,
            talent_id,
            source,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    pub talent_id: TalentId,
    pub source: InputId,
}

impl KeyBinding {
    #[must_use]
    pub const fn new(talent_id: TalentId, source: InputId) -> Self {
        Self { talent_id, source }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShotBindings {
    pub backgrounds: Vec<BackgroundBinding>,
    pub foregrounds: Vec<ForegroundBinding>,
    pub talents: Vec<TalentBinding>,
    pub keys: Vec<KeyBinding>,
}

impl ShotBindings {
    #[must_use]
    pub const fn new(
        backgrounds: Vec<BackgroundBinding>,
        foregrounds: Vec<ForegroundBinding>,
        talents: Vec<TalentBinding>,
        keys: Vec<KeyBinding>,
    ) -> Self {
        Self {
            backgrounds,
            foregrounds,
            talents,
            keys,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WipeDirection {
    LeftToRight,
    RightToLeft,
    TopToBottom,
    BottomToTop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    Cut,
    Dissolve,
    Wipe(WipeDirection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionIntent {
    pub kind: TransitionKind,
    pub duration: Duration,
}

impl TransitionIntent {
    pub const CUT: Self = Self::new(TransitionKind::Cut, Duration::ZERO);

    #[must_use]
    pub const fn new(kind: TransitionKind, duration: Duration) -> Self {
        Self { kind, duration }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shot {
    pub id: ShotId,
    pub set_id: SetId,
    pub camera_id: CameraId,
    pub bindings: ShotBindings,
    pub transition: TransitionIntent,
}

impl Shot {
    #[must_use]
    pub const fn new(
        id: ShotId,
        set_id: SetId,
        camera_id: CameraId,
        bindings: ShotBindings,
        transition: TransitionIntent,
    ) -> Self {
        Self {
            id,
            set_id,
            camera_id,
            bindings,
            transition,
        }
    }

    pub fn use_camera(&mut self, camera_id: CameraId) {
        self.camera_id = camera_id;
    }

    /// Replaces every fill source for a talent and returns the number changed.
    pub fn replace_talent_source(&mut self, talent_id: TalentId, source: InputId) -> usize {
        let mut replacements = 0;
        for binding in &mut self.bindings.talents {
            if binding.talent_id == talent_id {
                binding.source = source;
                replacements += 1;
            }
        }
        replacements
    }
}
