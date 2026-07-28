use fm_compositor::{
    AlphaMode, ChromaKey, CompositionPlan, CpuExecutionError, CpuSourceFrame, CropRect, Effect,
    Key, LumaKey, OutputInclusion, OutputTarget, PlanError, RectMask, Rgba8, Rotation,
    SafeAreaGuide, Scene, SceneError, SourceId, SourceLayer, Transform, TransitionError,
    TransitionKind, TransitionPlan, compile_scene, execute_cpu, execute_transition,
};
use fm_video::ImageFrame;

fn frame(width: u32, height: u32, pixels: &[Rgba8]) -> ImageFrame {
    ImageFrame::new(
        width,
        height,
        usize::try_from(width).unwrap() * 4,
        pixels.iter().flat_map(|pixel| pixel.to_bytes()).collect(),
    )
    .unwrap()
}

fn pixels(frame: &ImageFrame) -> Vec<Rgba8> {
    (0..frame.height())
        .flat_map(|y| (0..frame.width()).map(move |x| frame.pixel(x, y).unwrap()))
        .collect()
}

fn transform(x: i32, y: i32, width: u32, height: u32) -> Transform {
    Transform::new(x, y, width, height, Rotation::Deg0)
}

#[test]
fn ten_layers_compile_and_execute_in_stable_z_order() {
    let mut scene = Scene::new(10, 1, Rgba8::new(0, 0, 0, 255)).unwrap();
    let frames = (0_u8..10)
        .map(|value| frame(1, 1, &[Rgba8::new(value + 1, 0, 0, 255)]))
        .collect::<Vec<_>>();
    for index in (0_u32..10).rev() {
        let position = i32::try_from(index).unwrap();
        scene.push_layer(SourceLayer::new(
            SourceId::new(u64::from(index)),
            position,
            transform(position, 0, 1, 1),
        ));
    }

    let (plan, report) = compile_scene(&scene, OutputTarget::Program).unwrap();
    assert_eq!(plan.layers().len(), 10);
    assert_eq!(report.planned_layers(), 10);
    assert!(
        plan.layers()
            .windows(2)
            .all(|layers| layers[0].z() < layers[1].z())
    );
    let sources = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| CpuSourceFrame::new(SourceId::new(index as u64), frame))
        .collect::<Vec<_>>();
    let output = execute_cpu(&plan, &sources).unwrap();
    assert_eq!(
        pixels(&output),
        (1..=10)
            .map(|red| Rgba8::new(red, 0, 0, 255))
            .collect::<Vec<_>>()
    );
}

#[test]
fn crop_transform_mask_chroma_luma_order_and_opacity_have_exact_pixels() {
    let blue = Rgba8::new(0, 0, 255, 255);
    let mut scene = Scene::new(4, 1, blue).unwrap();
    scene.push_layer(
        SourceLayer::new(SourceId::new(1), 20, transform(1, 0, 2, 1))
            .with_crop(CropRect::new(1, 0, 2, 1))
            .with_mask(RectMask::new(1, 0, 1, 1))
            .with_key(Key::Chroma(ChromaKey::new(
                Rgba8::new(0, 255, 0, 255),
                0,
                0,
                0,
            ))),
    );
    scene.push_layer(
        SourceLayer::new(SourceId::new(2), 10, transform(0, 0, 1, 1))
            .with_key(Key::Luma(LumaKey::new(0, 0, false)))
            .with_opacity(128),
    );
    scene.push_layer(SourceLayer::new(
        SourceId::new(3),
        15,
        transform(3, 0, 1, 1),
    ));
    let first = frame(
        3,
        1,
        &[
            Rgba8::new(1, 1, 1, 255),
            Rgba8::new(0, 255, 0, 255),
            Rgba8::new(255, 0, 0, 255),
        ],
    );
    let white = frame(1, 1, &[Rgba8::new(255, 255, 255, 255)]);
    let yellow = frame(1, 1, &[Rgba8::new(255, 255, 0, 255)]);
    let (plan, _) = compile_scene(&scene, OutputTarget::Program).unwrap();
    let output = execute_cpu(
        &plan,
        &[
            CpuSourceFrame::new(SourceId::new(1), &first),
            CpuSourceFrame::new(SourceId::new(2), &white),
            CpuSourceFrame::new(SourceId::new(3), &yellow),
        ],
    )
    .unwrap();
    assert_eq!(
        pixels(&output),
        [
            Rgba8::new(128, 128, 255, 255),
            blue,
            Rgba8::new(255, 0, 0, 255),
            Rgba8::new(255, 255, 0, 255),
        ]
    );
}

#[test]
fn rectangular_mask_edges_and_crop_space_are_exact_across_layers() {
    let black = Rgba8::new(0, 0, 0, 255);
    let red = Rgba8::new(255, 0, 0, 255);
    let green = Rgba8::new(0, 255, 0, 255);
    let blue = Rgba8::new(0, 0, 255, 255);
    let source = frame(4, 1, &[Rgba8::new(255, 255, 255, 255), red, green, blue]);
    let mut scene = Scene::new(12, 1, black).unwrap();
    for (x, mask) in [
        (0, RectMask::new(1, 0, 1, 1)),
        (3, RectMask::new(1, 0, 1, 1).inverted(true)),
        (6, RectMask::new(3, 0, 1, 1)),
        (9, RectMask::new(0, 0, 3, 1)),
    ] {
        scene.push_layer(
            SourceLayer::new(SourceId::new(1), x, transform(x, 0, 3, 1))
                .with_crop(CropRect::new(1, 0, 3, 1))
                .with_mask(mask)
                .with_opacity(128),
        );
    }

    let (plan, _) = compile_scene(&scene, OutputTarget::Program).unwrap();
    let output = execute_cpu(&plan, &[CpuSourceFrame::new(SourceId::new(1), &source)]).unwrap();
    assert_eq!(
        pixels(&output),
        [
            black,
            Rgba8::new(0, 128, 0, 255),
            black,
            Rgba8::new(128, 0, 0, 255),
            black,
            Rgba8::new(0, 0, 128, 255),
            black,
            black,
            black,
            Rgba8::new(128, 0, 0, 255),
            Rgba8::new(0, 128, 0, 255),
            Rgba8::new(0, 0, 128, 255),
        ]
    );
}

#[test]
fn equal_z_scene_order_is_preserved_and_disabled_sources_are_not_resolved() {
    let mut scene = Scene::new(1, 1, Rgba8::new(0, 0, 0, 255)).unwrap();
    scene.push_layer(SourceLayer::new(SourceId::new(1), 5, transform(0, 0, 1, 1)));
    scene.push_layer(SourceLayer::new(SourceId::new(2), 5, transform(0, 0, 1, 1)));
    scene.push_layer(
        SourceLayer::new(SourceId::new(99), 100, transform(0, 0, 1, 1)).with_enabled(false),
    );
    let red = frame(1, 1, &[Rgba8::new(255, 0, 0, 255)]);
    let green = frame(1, 1, &[Rgba8::new(0, 255, 0, 255)]);
    let (plan, report) = compile_scene(&scene, OutputTarget::Program).unwrap();
    let output = execute_cpu(
        &plan,
        &[
            CpuSourceFrame::new(SourceId::new(1), &red),
            CpuSourceFrame::new(SourceId::new(2), &green),
        ],
    )
    .unwrap();
    assert_eq!(output.pixel(0, 0), Some(green.pixel(0, 0).unwrap()));
    assert_eq!(report.scene_layers(), 3);
    assert_eq!(report.planned_layers(), 2);
}

#[test]
fn overlay_flags_are_compiled_per_output() {
    let mut scene = Scene::new(1, 1, Rgba8::new(0, 0, 0, 255)).unwrap();
    scene.push_layer(
        SourceLayer::new(SourceId::new(1), 0, transform(0, 0, 1, 1))
            .as_overlay(OutputInclusion::PROGRAM | OutputInclusion::RECORD),
    );
    let red = frame(1, 1, &[Rgba8::new(255, 0, 0, 255)]);
    let (program, _) = compile_scene(&scene, OutputTarget::Program).unwrap();
    let (stream, _) = compile_scene(&scene, OutputTarget::Stream).unwrap();
    assert_eq!(
        execute_cpu(&program, &[CpuSourceFrame::new(SourceId::new(1), &red)])
            .unwrap()
            .pixel(0, 0),
        Some(Rgba8::new(255, 0, 0, 255))
    );
    assert_eq!(
        execute_cpu(&stream, &[]).unwrap().pixel(0, 0),
        Some(Rgba8::new(0, 0, 0, 255))
    );
}

#[test]
fn safe_areas_never_enter_program_but_render_in_operator_output() {
    let mut scene = Scene::new(3, 3, Rgba8::new(0, 0, 0, 255)).unwrap();
    scene.push_safe_area(SafeAreaGuide::new(1, 1, 2, 2, Rgba8::new(255, 255, 0, 255)));
    let (program, _) = compile_scene(&scene, OutputTarget::Program).unwrap();
    let (operator, _) = compile_scene(&scene, OutputTarget::Operator).unwrap();
    assert!(program.safe_areas().is_empty());
    assert_eq!(
        execute_cpu(&program, &[]).unwrap().pixel(1, 1),
        Some(Rgba8::new(0, 0, 0, 255))
    );
    assert_eq!(
        execute_cpu(&operator, &[]).unwrap().pixel(1, 1),
        Some(Rgba8::new(255, 255, 0, 255))
    );
}

#[test]
fn cut_fade_and_wipe_have_byte_exact_endpoints() {
    let left = frame(1, 1, &[Rgba8::new(1, 2, 3, 255)]);
    let right = frame(1, 1, &[Rgba8::new(200, 100, 50, 255)]);
    let cut = TransitionPlan::compile(TransitionKind::Cut, 0, 1).unwrap();
    assert_eq!(execute_transition(cut, &left, &right).unwrap(), right);

    let start = TransitionPlan::compile(TransitionKind::Fade, 0, 10).unwrap();
    let end = TransitionPlan::compile(TransitionKind::Fade, 10, 10).unwrap();
    assert_eq!(execute_transition(start, &left, &right).unwrap(), left);
    assert_eq!(execute_transition(end, &left, &right).unwrap(), right);
    let middle = TransitionPlan::compile(TransitionKind::Fade, 1, 2).unwrap();
    assert_eq!(
        execute_transition(middle, &left, &right)
            .unwrap()
            .pixel(0, 0),
        Some(Rgba8::new(101, 51, 27, 255))
    );

    let wipe_start = TransitionPlan::compile(TransitionKind::Wipe, 0, 10).unwrap();
    let wipe_end = TransitionPlan::compile(TransitionKind::Wipe, 10, 10).unwrap();
    assert_eq!(execute_transition(wipe_start, &left, &right).unwrap(), left);
    assert_eq!(execute_transition(wipe_end, &left, &right).unwrap(), right);
}

#[test]
fn wipe_uses_a_floor_boundary_for_odd_widths() {
    let from = frame(
        5,
        1,
        &[
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
            Rgba8::new(5, 0, 0, 255),
        ],
    );
    let to = frame(
        5,
        1,
        &[
            Rgba8::new(101, 0, 0, 255),
            Rgba8::new(102, 0, 0, 255),
            Rgba8::new(103, 0, 0, 255),
            Rgba8::new(104, 0, 0, 255),
            Rgba8::new(105, 0, 0, 255),
        ],
    );

    let half = TransitionPlan::compile(TransitionKind::Wipe, 1, 2).unwrap();
    assert_eq!(
        pixels(&execute_transition(half, &from, &to).unwrap()),
        [
            Rgba8::new(101, 0, 0, 255),
            Rgba8::new(102, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
            Rgba8::new(5, 0, 0, 255),
        ]
    );

    let two_thirds = TransitionPlan::compile(TransitionKind::Wipe, 2, 3).unwrap();
    assert_eq!(
        pixels(&execute_transition(two_thirds, &from, &to).unwrap()),
        [
            Rgba8::new(101, 0, 0, 255),
            Rgba8::new(102, 0, 0, 255),
            Rgba8::new(103, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
            Rgba8::new(5, 0, 0, 255),
        ]
    );
}

#[test]
fn malformed_and_unsupported_work_is_rejected_explicitly() {
    assert_eq!(
        Scene::new(0, 1, Rgba8::new(0, 0, 0, 0)),
        Err(SceneError::ZeroWidth)
    );
    assert!(matches!(
        Scene::new(1, 1, Rgba8::new(255, 0, 0, 1)),
        Err(SceneError::BackgroundNotPremultiplied(_))
    ));
    assert_eq!(
        TransitionPlan::compile(TransitionKind::Fade, 0, 0),
        Err(TransitionError::ZeroDenominator)
    );
    assert!(matches!(
        TransitionPlan::compile(TransitionKind::Slide, 0, 1),
        Err(TransitionError::UnsupportedKind(TransitionKind::Slide))
    ));

    let mut invalid = Scene::new(1, 1, Rgba8::new(0, 0, 0, 255)).unwrap();
    invalid.push_layer(SourceLayer::new(SourceId::new(1), 0, transform(0, 0, 0, 1)));
    assert_eq!(
        compile_scene(&invalid, OutputTarget::Program),
        Err(PlanError::ZeroTransformWidth { layer: 0 })
    );

    let mut excessive = Scene::new(1, 1, Rgba8::new(0, 0, 0, 255)).unwrap();
    for index in 0..=CompositionPlan::MAX_LAYERS {
        excessive.push_layer(SourceLayer::new(
            SourceId::new(index as u64),
            i32::try_from(index).unwrap(),
            transform(0, 0, 1, 1),
        ));
    }
    assert!(matches!(
        compile_scene(&excessive, OutputTarget::Program),
        Err(PlanError::TooManyLayers { .. })
    ));

    let mut effects = Scene::new(1, 1, Rgba8::new(0, 0, 0, 255)).unwrap();
    effects.push_layer(
        SourceLayer::new(SourceId::new(1), 0, transform(0, 0, 1, 1))
            .with_effect(Effect::new("gpu-only", vec![1])),
    );
    let source = frame(1, 1, &[Rgba8::new(0, 0, 0, 255)]);
    let (plan, _) = compile_scene(&effects, OutputTarget::Program).unwrap();
    assert!(matches!(
        execute_cpu(&plan, &[CpuSourceFrame::new(SourceId::new(1), &source)]),
        Err(CpuExecutionError::UnsupportedEffect { .. })
    ));
}

#[test]
fn compositor_alpha_import_is_the_canonical_type() {
    let alpha_mode: AlphaMode = fm_color::AlphaMode::Premultiplied;
    let layer =
        SourceLayer::new(SourceId::new(1), 0, transform(0, 0, 1, 1)).with_alpha_mode(alpha_mode);
    assert_eq!(layer.alpha_mode(), fm_frame::AlphaMode::Premultiplied);
}
