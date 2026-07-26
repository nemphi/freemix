use std::{num::NonZeroU128, time::Duration};

use fm_types::InputId;
use fm_virtual_set::{
    BackgroundBinding, BindingKind, CameraId, CameraPreset, ForegroundBinding, KeyBinding,
    KeyRequirement, Layer, LayerId, LayerKind, MAX_LAYERS, NormalizedPlane, NormalizedPoint,
    RenderBinding, SetId, Shot, ShotBindings, ShotId, TalentBinding, TalentId, TransitionIntent,
    TransitionKind, ValidationError, VirtualSetScene, WipeDirection, compile, validate_scene,
    validate_shot,
};

fn nonzero(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn set_id(value: u128) -> SetId {
    SetId::new(nonzero(value))
}

fn shot_id(value: u128) -> ShotId {
    ShotId::new(nonzero(value))
}

fn layer_id(value: u128) -> LayerId {
    LayerId::new(nonzero(value))
}

fn talent_id(value: u128) -> TalentId {
    TalentId::new(nonzero(value))
}

fn camera_id(value: u128) -> CameraId {
    CameraId::new(nonzero(value))
}

fn input_id(value: u128) -> InputId {
    InputId::new(nonzero(value))
}

fn fixture() -> (VirtualSetScene, Shot) {
    let talent = talent_id(1);
    let scene = VirtualSetScene::new(
        set_id(1),
        vec![talent],
        vec![
            Layer::new(
                layer_id(1),
                -10,
                NormalizedPlane::rectangle(0.0, 0.0, 1.0, 1.0),
                LayerKind::Background,
            ),
            Layer::new(
                layer_id(2),
                0,
                NormalizedPlane::rectangle(0.25, 0.1, 0.75, 1.0),
                LayerKind::Talent {
                    talent_id: talent,
                    key: KeyRequirement::Required,
                },
            ),
            Layer::new(
                layer_id(3),
                10,
                NormalizedPlane::rectangle(0.0, 0.8, 1.0, 1.0),
                LayerKind::Foreground,
            ),
        ],
        vec![
            CameraPreset::new(camera_id(1), NormalizedPoint::new(0.5, 0.5), 1.0),
            CameraPreset::new(camera_id(2), NormalizedPoint::new(0.35, 0.4), 1.8),
        ],
    );
    let shot = Shot::new(
        shot_id(1),
        scene.id,
        camera_id(1),
        ShotBindings::new(
            vec![BackgroundBinding::new(layer_id(1), input_id(10))],
            vec![ForegroundBinding::new(layer_id(3), input_id(30))],
            vec![TalentBinding::new(layer_id(2), talent, input_id(20))],
            vec![KeyBinding::new(talent, input_id(21))],
        ),
        TransitionIntent::CUT,
    );
    (scene, shot)
}

#[test]
fn validates_scene_geometry_z_order_and_layer_limit() {
    let (mut scene, _) = fixture();
    scene.layers[1].plane = NormalizedPlane::rectangle(0.5, 0.5, 0.5, 0.9);
    scene.layers[2].z_order = scene.layers[0].z_order;
    let extra_layers = MAX_LAYERS + 1 - scene.layers.len();
    scene.layers.extend(
        (100_u128..)
            .zip(100_i32..)
            .take(extra_layers)
            .map(|(id, z_order)| {
                Layer::new(
                    layer_id(id),
                    z_order,
                    NormalizedPlane::rectangle(0.0, 0.0, 1.0, 1.0),
                    LayerKind::Background,
                )
            }),
    );

    let errors = validate_scene(&scene).unwrap_err();
    assert!(errors.errors().iter().any(|error| matches!(
        error,
        ValidationError::TooManyLayers { maximum, .. } if *maximum == MAX_LAYERS
    )));
    assert!(
        errors
            .errors()
            .contains(&ValidationError::DegenerateGeometry(layer_id(2)))
    );
    assert!(
        errors
            .errors()
            .iter()
            .any(|error| matches!(error, ValidationError::DuplicateZOrder { z_order: -10, .. }))
    );
}

#[test]
fn validates_missing_and_mismatched_shot_bindings() {
    let (scene, mut shot) = fixture();
    shot.bindings.backgrounds.clear();
    shot.bindings.talents[0].talent_id = talent_id(99);
    shot.bindings.keys.clear();

    let errors = validate_shot(&scene, &shot).unwrap_err();
    assert!(errors.errors().contains(&ValidationError::MissingBinding {
        layer_id: layer_id(1),
        kind: BindingKind::Background,
    }));
    assert!(
        errors
            .errors()
            .contains(&ValidationError::BindingTalentMismatch {
                layer_id: layer_id(2),
                expected: talent_id(1),
                actual: talent_id(99),
            })
    );
    assert!(
        errors
            .errors()
            .contains(&ValidationError::MissingKeyBinding(talent_id(1)))
    );
}

#[test]
fn selects_camera_position_and_zoom_presets() {
    let (scene, mut shot) = fixture();
    shot.use_camera(camera_id(2));

    let rendered = compile(&scene, &shot).unwrap();
    assert_eq!(rendered.camera.preset_id, camera_id(2));
    assert_eq!(rendered.camera.position, NormalizedPoint::new(0.35, 0.4));
    assert!((rendered.camera.zoom - 1.8).abs() < f32::EPSILON);
}

#[test]
fn replaces_talent_fill_without_replacing_key() {
    let (scene, mut shot) = fixture();
    assert_eq!(shot.replace_talent_source(talent_id(1), input_id(200)), 1);

    let rendered = compile(&scene, &shot).unwrap();
    assert!(matches!(
        rendered.layers[1].binding,
        RenderBinding::Talent {
            source,
            key_source: Some(key),
            ..
        } if source == input_id(200) && key == input_id(21)
    ));
}

#[test]
fn preserves_shot_transition_intent() {
    let (scene, mut shot) = fixture();
    shot.transition = TransitionIntent::new(
        TransitionKind::Wipe(WipeDirection::LeftToRight),
        Duration::from_millis(750),
    );

    assert_eq!(compile(&scene, &shot).unwrap().transition, shot.transition);
}

#[test]
fn render_description_is_deterministic_and_sorted_by_z_order() {
    let (mut scene, mut shot) = fixture();
    scene.layers.reverse();
    shot.bindings.backgrounds.reverse();
    shot.bindings.foregrounds.reverse();
    shot.bindings.talents.reverse();

    let first = compile(&scene, &shot).unwrap();
    let second = compile(&scene, &shot).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        first
            .layers
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>(),
        vec![layer_id(1), layer_id(2), layer_id(3)]
    );
    assert!(matches!(
        first.layers[0].binding,
        RenderBinding::Background { source } if source == input_id(10)
    ));
}
