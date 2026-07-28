use fm_compositor::{
    StingerBase, StingerFramePlan, StingerFrameRole, StingerPlanError, StingerRenderError,
    execute_stinger_frame,
};
use fm_video::{CompositeError, ImageFrame, Rgba8};

fn frame(width: u32, height: u32, pixels: &[Rgba8]) -> ImageFrame {
    let bytes = pixels
        .iter()
        .flat_map(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a])
        .collect();
    ImageFrame::new(width, height, usize::try_from(width).unwrap() * 4, bytes).unwrap()
}

#[test]
fn cut_point_selects_the_base_for_each_exact_media_frame() {
    let before = StingerFramePlan::compile(1, 4, 2).unwrap();
    let at_cut = StingerFramePlan::compile(2, 4, 2).unwrap();
    assert_eq!(before.base(), StingerBase::Program);
    assert_eq!(at_cut.base(), StingerBase::Preview);
    assert_eq!(before.frame_index(), 1);
    assert_eq!(before.frame_count(), 4);
    assert_eq!(before.cut_point_frame(), 2);
}

#[test]
fn transparent_and_partial_alpha_media_preserve_the_selected_base() {
    let program = frame(1, 1, &[Rgba8::new(255, 255, 255, 255)]);
    let preview = frame(1, 1, &[Rgba8::new(0, 0, 0, 255)]);
    let transparent = frame(1, 1, &[Rgba8::new(0, 0, 0, 0)]);
    let half_red = frame(1, 1, &[Rgba8::new(128, 0, 0, 128)]);

    let before = StingerFramePlan::compile(0, 2, 1).unwrap();
    assert_eq!(
        execute_stinger_frame(before, &program, &preview, &transparent).unwrap(),
        program
    );
    assert_eq!(
        execute_stinger_frame(before, &program, &preview, &half_red)
            .unwrap()
            .pixel(0, 0),
        Some(Rgba8::new(255, 127, 127, 255))
    );

    let at_cut = StingerFramePlan::compile(1, 2, 1).unwrap();
    assert_eq!(
        execute_stinger_frame(at_cut, &program, &preview, &transparent).unwrap(),
        preview
    );
    assert_eq!(
        execute_stinger_frame(at_cut, &program, &preview, &half_red)
            .unwrap()
            .pixel(0, 0),
        Some(Rgba8::new(128, 0, 0, 255))
    );
}

#[test]
fn plan_bounds_are_explicit() {
    assert_eq!(
        StingerFramePlan::compile(0, 0, 0),
        Err(StingerPlanError::EmptyMedia)
    );
    assert_eq!(
        StingerFramePlan::compile(2, 2, 1),
        Err(StingerPlanError::FrameOutOfRange {
            frame_index: 2,
            frame_count: 2,
        })
    );
    assert_eq!(
        StingerFramePlan::compile(0, 2, 3),
        Err(StingerPlanError::CutPointOutOfRange {
            cut_point_frame: 3,
            frame_count: 2,
        })
    );
    assert_eq!(
        StingerFramePlan::compile(0, 2, 2).unwrap().base(),
        StingerBase::Program
    );
}

#[test]
fn mismatched_or_straight_alpha_media_is_rejected() {
    let program = frame(1, 1, &[Rgba8::new(0, 0, 0, 255)]);
    let preview = frame(2, 1, &[Rgba8::new(0, 0, 0, 255); 2]);
    let media = frame(1, 1, &[Rgba8::new(0, 0, 0, 0)]);
    let plan = StingerFramePlan::compile(0, 1, 0).unwrap();
    assert_eq!(
        execute_stinger_frame(plan, &program, &preview, &media),
        Err(StingerRenderError::DimensionMismatch {
            role: StingerFrameRole::Preview,
            expected_width: 1,
            expected_height: 1,
            actual_width: 2,
            actual_height: 1,
        })
    );

    let preview = program.clone();
    let straight_alpha = frame(1, 1, &[Rgba8::new(255, 0, 0, 128)]);
    assert!(matches!(
        execute_stinger_frame(plan, &program, &preview, &straight_alpha),
        Err(StingerRenderError::Composite(
            CompositeError::NotPremultiplied { layer: Some(1), .. }
        ))
    ));
}
