use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use crate::{CameraId, KeyRequirement, LayerId, LayerKind, SetId, Shot, TalentId, VirtualSetScene};

pub const MAX_LAYERS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingKind {
    Background,
    Foreground,
    Talent,
    Key,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    TooManyLayers {
        actual: usize,
        maximum: usize,
    },
    DuplicateLayerId(LayerId),
    DuplicateTalentId(TalentId),
    DuplicateCameraId(CameraId),
    DuplicateZOrder {
        z_order: i32,
        first: LayerId,
        second: LayerId,
    },
    NonNormalizedGeometry(LayerId),
    DegenerateGeometry(LayerId),
    UnknownLayerTalent {
        layer_id: LayerId,
        talent_id: TalentId,
    },
    InvalidCameraPosition(CameraId),
    InvalidCameraZoom(CameraId),
    ShotSetMismatch {
        expected: SetId,
        actual: SetId,
    },
    MissingCamera(CameraId),
    MissingBinding {
        layer_id: LayerId,
        kind: BindingKind,
    },
    MissingKeyBinding(TalentId),
    DuplicateBinding {
        layer_id: LayerId,
        kind: BindingKind,
    },
    DuplicateKeyBinding(TalentId),
    UnexpectedBinding {
        layer_id: LayerId,
        kind: BindingKind,
    },
    UnexpectedKeyBinding(TalentId),
    BindingTalentMismatch {
        layer_id: LayerId,
        expected: TalentId,
        actual: TalentId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    pub(crate) fn single(error: ValidationError) -> Self {
        Self(vec![error])
    }

    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "virtual-set validation failed with {} error(s)",
            self.len()
        )
    }
}

impl std::error::Error for ValidationErrors {}

/// Checks scene identity, geometry, layer ordering, talents, and camera presets.
///
/// # Errors
///
/// Returns every scene-level validation error that is found.
pub fn validate_scene(scene: &VirtualSetScene) -> Result<(), ValidationErrors> {
    let mut errors = Vec::new();

    if scene.layers.len() > MAX_LAYERS {
        errors.push(ValidationError::TooManyLayers {
            actual: scene.layers.len(),
            maximum: MAX_LAYERS,
        });
    }

    let talents = collect_unique(
        scene.talents.iter().copied(),
        &mut errors,
        ValidationError::DuplicateTalentId,
    );
    let mut layer_ids = BTreeSet::new();
    let mut z_orders = BTreeMap::new();
    for layer in &scene.layers {
        if !layer_ids.insert(layer.id) {
            errors.push(ValidationError::DuplicateLayerId(layer.id));
        }
        if let Some(first) = z_orders.insert(layer.z_order, layer.id) {
            errors.push(ValidationError::DuplicateZOrder {
                z_order: layer.z_order,
                first,
                second: layer.id,
            });
        }
        if !layer.plane.is_normalized() {
            errors.push(ValidationError::NonNormalizedGeometry(layer.id));
        } else if layer.plane.is_degenerate() {
            errors.push(ValidationError::DegenerateGeometry(layer.id));
        }
        if let LayerKind::Talent { talent_id, .. } = layer.kind
            && !talents.contains(&talent_id)
        {
            errors.push(ValidationError::UnknownLayerTalent {
                layer_id: layer.id,
                talent_id,
            });
        }
    }

    let mut cameras = BTreeSet::new();
    for preset in &scene.camera_presets {
        if !cameras.insert(preset.id) {
            errors.push(ValidationError::DuplicateCameraId(preset.id));
        }
        if !preset.position.is_normalized() {
            errors.push(ValidationError::InvalidCameraPosition(preset.id));
        }
        if !preset.zoom.is_finite() || preset.zoom <= 0.0 {
            errors.push(ValidationError::InvalidCameraZoom(preset.id));
        }
    }

    finish(errors)
}

/// Checks that a shot targets the scene and completely binds its layers.
///
/// # Errors
///
/// Returns every shot-level validation error that is found.
pub fn validate_shot(scene: &VirtualSetScene, shot: &Shot) -> Result<(), ValidationErrors> {
    let mut errors = Vec::new();

    if shot.set_id != scene.id {
        errors.push(ValidationError::ShotSetMismatch {
            expected: scene.id,
            actual: shot.set_id,
        });
    }
    if scene.camera_preset(shot.camera_id).is_none() {
        errors.push(ValidationError::MissingCamera(shot.camera_id));
    }

    let backgrounds = binding_layers(
        shot.bindings
            .backgrounds
            .iter()
            .map(|binding| binding.layer_id),
        BindingKind::Background,
        &mut errors,
    );
    let foregrounds = binding_layers(
        shot.bindings
            .foregrounds
            .iter()
            .map(|binding| binding.layer_id),
        BindingKind::Foreground,
        &mut errors,
    );
    let talent_bindings = binding_layers(
        shot.bindings.talents.iter().map(|binding| binding.layer_id),
        BindingKind::Talent,
        &mut errors,
    );
    let keys = collect_unique(
        shot.bindings.keys.iter().map(|binding| binding.talent_id),
        &mut errors,
        ValidationError::DuplicateKeyBinding,
    );

    let mut required_keys = BTreeSet::new();
    for layer in &scene.layers {
        match layer.kind {
            LayerKind::Background => {
                require_binding(layer.id, BindingKind::Background, &backgrounds, &mut errors);
            }
            LayerKind::Foreground => {
                require_binding(layer.id, BindingKind::Foreground, &foregrounds, &mut errors);
            }
            LayerKind::Talent { talent_id, key } => {
                require_binding(layer.id, BindingKind::Talent, &talent_bindings, &mut errors);
                if let Some(binding) = shot
                    .bindings
                    .talents
                    .iter()
                    .find(|binding| binding.layer_id == layer.id)
                    && binding.talent_id != talent_id
                {
                    errors.push(ValidationError::BindingTalentMismatch {
                        layer_id: layer.id,
                        expected: talent_id,
                        actual: binding.talent_id,
                    });
                }
                if key == KeyRequirement::Required {
                    required_keys.insert(talent_id);
                    if !keys.contains(&talent_id) {
                        errors.push(ValidationError::MissingKeyBinding(talent_id));
                    }
                }
            }
        }
    }

    reject_unexpected(&backgrounds, BindingKind::Background, scene, &mut errors);
    reject_unexpected(&foregrounds, BindingKind::Foreground, scene, &mut errors);
    reject_unexpected(&talent_bindings, BindingKind::Talent, scene, &mut errors);
    for talent_id in keys {
        if !required_keys.contains(&talent_id) {
            errors.push(ValidationError::UnexpectedKeyBinding(talent_id));
        }
    }

    finish(errors)
}

fn collect_unique<T: Copy + Ord>(
    values: impl Iterator<Item = T>,
    errors: &mut Vec<ValidationError>,
    duplicate: impl Fn(T) -> ValidationError,
) -> BTreeSet<T> {
    let mut unique = BTreeSet::new();
    for value in values {
        if !unique.insert(value) {
            errors.push(duplicate(value));
        }
    }
    unique
}

fn binding_layers(
    layers: impl Iterator<Item = LayerId>,
    kind: BindingKind,
    errors: &mut Vec<ValidationError>,
) -> BTreeSet<LayerId> {
    collect_unique(layers, errors, |layer_id| {
        ValidationError::DuplicateBinding { layer_id, kind }
    })
}

fn require_binding(
    layer_id: LayerId,
    kind: BindingKind,
    bindings: &BTreeSet<LayerId>,
    errors: &mut Vec<ValidationError>,
) {
    if !bindings.contains(&layer_id) {
        errors.push(ValidationError::MissingBinding { layer_id, kind });
    }
}

fn reject_unexpected(
    bindings: &BTreeSet<LayerId>,
    kind: BindingKind,
    scene: &VirtualSetScene,
    errors: &mut Vec<ValidationError>,
) {
    for &layer_id in bindings {
        let expected = scene.layers.iter().any(|layer| {
            layer.id == layer_id
                && matches!(
                    (kind, layer.kind),
                    (BindingKind::Background, LayerKind::Background)
                        | (BindingKind::Foreground, LayerKind::Foreground)
                        | (BindingKind::Talent, LayerKind::Talent { .. })
                )
        });
        if !expected {
            errors.push(ValidationError::UnexpectedBinding { layer_id, kind });
        }
    }
}

fn finish(errors: Vec<ValidationError>) -> Result<(), ValidationErrors> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors(errors))
    }
}
