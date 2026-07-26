use fm_types::InputId;

use crate::{
    BindingKind, CameraId, LayerId, NormalizedPlane, NormalizedPoint, SetId, Shot, ShotId,
    TalentId, TransitionIntent, ValidationError, ValidationErrors, VirtualSetScene, validate_scene,
    validate_shot,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderCamera {
    pub preset_id: CameraId,
    pub position: NormalizedPoint,
    pub zoom: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderBinding {
    Background {
        source: InputId,
    },
    Foreground {
        source: InputId,
    },
    Talent {
        talent_id: TalentId,
        source: InputId,
        key_source: Option<InputId>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderLayer {
    pub id: LayerId,
    pub z_order: i32,
    pub plane: NormalizedPlane,
    pub binding: RenderBinding,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderDescription {
    pub set_id: SetId,
    pub shot_id: ShotId,
    pub camera: RenderCamera,
    pub layers: Vec<RenderLayer>,
    pub transition: TransitionIntent,
}

/// Resolves a scene and shot into a stable, compositor-independent layer list.
///
/// # Errors
///
/// Returns all scene errors, or all shot errors when the scene is valid.
pub fn compile(
    scene: &VirtualSetScene,
    shot: &Shot,
) -> Result<RenderDescription, ValidationErrors> {
    validate_scene(scene)?;
    validate_shot(scene, shot)?;

    let preset = scene
        .camera_preset(shot.camera_id)
        .ok_or_else(|| ValidationErrors::single(ValidationError::MissingCamera(shot.camera_id)))?;
    let mut layers = scene
        .layers
        .iter()
        .map(|layer| {
            let binding = match layer.kind {
                crate::LayerKind::Background => {
                    let source = shot
                        .bindings
                        .backgrounds
                        .iter()
                        .find(|binding| binding.layer_id == layer.id)
                        .ok_or_else(|| missing_binding(layer.id, BindingKind::Background))?
                        .source;
                    RenderBinding::Background { source }
                }
                crate::LayerKind::Foreground => {
                    let source = shot
                        .bindings
                        .foregrounds
                        .iter()
                        .find(|binding| binding.layer_id == layer.id)
                        .ok_or_else(|| missing_binding(layer.id, BindingKind::Foreground))?
                        .source;
                    RenderBinding::Foreground { source }
                }
                crate::LayerKind::Talent { talent_id, .. } => {
                    let source = shot
                        .bindings
                        .talents
                        .iter()
                        .find(|binding| binding.layer_id == layer.id)
                        .ok_or_else(|| missing_binding(layer.id, BindingKind::Talent))?
                        .source;
                    let key_source = shot
                        .bindings
                        .keys
                        .iter()
                        .find(|binding| binding.talent_id == talent_id)
                        .map(|binding| binding.source);
                    RenderBinding::Talent {
                        talent_id,
                        source,
                        key_source,
                    }
                }
            };

            Ok(RenderLayer {
                id: layer.id,
                z_order: layer.z_order,
                plane: layer.plane,
                binding,
            })
        })
        .collect::<Result<Vec<_>, ValidationErrors>>()?;
    layers.sort_by_key(|layer| (layer.z_order, layer.id));

    Ok(RenderDescription {
        set_id: scene.id,
        shot_id: shot.id,
        camera: RenderCamera {
            preset_id: preset.id,
            position: preset.position,
            zoom: preset.zoom,
        },
        layers,
        transition: shot.transition,
    })
}

fn missing_binding(layer_id: LayerId, kind: BindingKind) -> ValidationErrors {
    ValidationErrors::single(ValidationError::MissingBinding { layer_id, kind })
}
