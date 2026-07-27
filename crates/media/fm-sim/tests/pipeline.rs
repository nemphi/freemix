use fm_sim::{
    PipelineConfigError, RegistryError, RenderError, Rgba8, SimulatedPipeline, SimulatedSource,
    SourcePattern,
};
use fm_switcher::{ProgramFrame, TransitionKind};
use fm_types::InputId;
use std::num::NonZeroU128;

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn program(primary: InputId) -> ProgramFrame {
    ProgramFrame {
        primary,
        secondary: None,
        transition_kind: None,
        mix_numerator: 0,
        mix_denominator: 1,
        mix_start_numerator: 0,
        mix_end_numerator: 0,
    }
}

#[test]
fn registry_preserves_stable_ids_and_sorted_iteration() {
    let mut pipeline = SimulatedPipeline::new(4, 2).unwrap();
    let third = SimulatedSource::new(input(3), SourcePattern::Bars);
    let first = SimulatedSource::new(input(1), SourcePattern::Solid(Rgba8::new(1, 2, 3, 255)));

    pipeline.register(third).unwrap();
    pipeline.register(first).unwrap();

    assert_eq!(
        pipeline.inputs().collect::<Vec<_>>(),
        vec![input(1), input(3)]
    );
    assert_eq!(pipeline.source(input(3)), Some(&third));
    assert_eq!(
        pipeline.register(third),
        Err(RegistryError::DuplicateSource(input(3)))
    );
    assert_eq!(pipeline.remove(input(3)), Some(third));
    assert_eq!(pipeline.source(input(3)), None);
}

#[test]
fn pipeline_rejects_unbounded_dimensions() {
    assert!(matches!(
        SimulatedPipeline::new(0, 1),
        Err(PipelineConfigError::ZeroWidth)
    ));
    assert!(matches!(
        SimulatedPipeline::new(1, 0),
        Err(PipelineConfigError::ZeroHeight)
    ));
    assert!(matches!(
        SimulatedPipeline::new(SimulatedPipeline::MAX_WIDTH + 1, 1),
        Err(PipelineConfigError::DimensionsExceedLimit { .. })
    ));
}

#[test]
fn cut_program_frame_renders_only_primary_source() {
    let mut pipeline = SimulatedPipeline::new(2, 2).unwrap();
    let red = Rgba8::new(255, 0, 0, 255);
    pipeline
        .register(SimulatedSource::new(input(1), SourcePattern::Solid(red)))
        .unwrap();

    let output = pipeline.render(42, program(input(1))).unwrap();

    assert_eq!(output.pixel(0, 0), Some(red));
    assert_eq!(output.pixel(1, 1), Some(red));

    let identical = pipeline
        .render(
            43,
            ProgramFrame {
                primary: input(1),
                secondary: Some(input(1)),
                transition_kind: None,
                mix_numerator: u32::MAX,
                mix_denominator: 0,
                mix_start_numerator: u32::MAX,
                mix_end_numerator: u32::MAX,
            },
        )
        .unwrap();
    assert_eq!(identical.pixel(0, 0), Some(red));
}

#[test]
fn fade_renders_exact_endpoints_and_intermediate_mix() {
    let mut pipeline = SimulatedPipeline::new(1, 1).unwrap();
    let black = Rgba8::new(0, 0, 0, 255);
    let white = Rgba8::new(255, 255, 255, 255);
    pipeline
        .register(SimulatedSource::new(input(1), SourcePattern::Solid(black)))
        .unwrap();
    pipeline
        .register(SimulatedSource::new(input(2), SourcePattern::Solid(white)))
        .unwrap();
    let frame = |numerator| ProgramFrame {
        primary: input(1),
        secondary: Some(input(2)),
        transition_kind: Some(TransitionKind::Fade),
        mix_numerator: numerator,
        mix_denominator: 2,
        mix_start_numerator: numerator,
        mix_end_numerator: numerator,
    };

    assert_eq!(
        pipeline.render(0, frame(0)).unwrap().pixel(0, 0),
        Some(black)
    );
    assert_eq!(
        pipeline.render(0, frame(1)).unwrap().pixel(0, 0),
        Some(Rgba8::new(128, 128, 128, 255))
    );
    assert_eq!(
        pipeline.render(0, frame(2)).unwrap().pixel(0, 0),
        Some(white)
    );
}

#[test]
fn wipe_renders_exact_endpoints_and_floor_pixel_boundary() {
    let mut pipeline = SimulatedPipeline::new(5, 1).unwrap();
    let red = Rgba8::new(255, 0, 0, 255);
    let blue = Rgba8::new(0, 0, 255, 255);
    pipeline
        .register(SimulatedSource::new(input(1), SourcePattern::Solid(red)))
        .unwrap();
    pipeline
        .register(SimulatedSource::new(input(2), SourcePattern::Solid(blue)))
        .unwrap();
    let frame = |numerator| ProgramFrame {
        primary: input(1),
        secondary: Some(input(2)),
        transition_kind: Some(TransitionKind::Wipe),
        mix_numerator: numerator,
        mix_denominator: 2,
        mix_start_numerator: numerator,
        mix_end_numerator: numerator,
    };

    let start = pipeline.render(0, frame(0)).unwrap();
    assert!((0..5).all(|x| start.pixel(x, 0) == Some(red)));

    let half = pipeline.render(0, frame(1)).unwrap();
    assert_eq!(half.pixel(0, 0), Some(blue));
    assert_eq!(half.pixel(1, 0), Some(blue));
    assert!((2..5).all(|x| half.pixel(x, 0) == Some(red)));

    let end = pipeline.render(0, frame(2)).unwrap();
    assert!((0..5).all(|x| end.pixel(x, 0) == Some(blue)));
}

#[test]
fn pipeline_rejects_missing_and_unsupported_transition_kinds() {
    let mut pipeline = SimulatedPipeline::new(1, 1).unwrap();
    for value in [1, 2] {
        pipeline
            .register(SimulatedSource::new(
                input(value),
                SourcePattern::Solid(Rgba8::default()),
            ))
            .unwrap();
    }
    let frame = |transition_kind| ProgramFrame {
        primary: input(1),
        secondary: Some(input(2)),
        transition_kind,
        mix_numerator: 1,
        mix_denominator: 2,
        mix_start_numerator: 1,
        mix_end_numerator: 1,
    };

    assert_eq!(
        pipeline.render(0, frame(None)),
        Err(RenderError::MissingTransitionKind)
    );
    assert_eq!(
        pipeline.render(0, frame(Some(TransitionKind::Slide))),
        Err(RenderError::UnsupportedTransition(TransitionKind::Slide))
    );
}

#[test]
fn moving_bars_change_deterministically_with_frame_number() {
    let mut pipeline = SimulatedPipeline::new(14, 4).unwrap();
    pipeline
        .register(SimulatedSource::new(input(7), SourcePattern::Bars))
        .unwrap();

    let first = pipeline.render(10, program(input(7))).unwrap();
    let repeat = pipeline.render(10, program(input(7))).unwrap();
    let next = pipeline.render(11, program(input(7))).unwrap();

    assert_eq!(first, repeat);
    assert_ne!(first, next);
    assert_eq!(first.pixel(10, 3), Some(Rgba8::new(0, 0, 0, 255)));
    assert_eq!(next.pixel(11, 3), Some(Rgba8::new(0, 0, 0, 255)));
}

#[test]
fn missing_primary_and_secondary_sources_are_structured_errors() {
    let mut pipeline = SimulatedPipeline::new(1, 1).unwrap();
    pipeline
        .register(SimulatedSource::new(
            input(1),
            SourcePattern::Solid(Rgba8::default()),
        ))
        .unwrap();

    assert_eq!(
        pipeline.render(0, program(input(9))),
        Err(RenderError::MissingSource { input: input(9) })
    );
    assert_eq!(
        pipeline.render(
            0,
            ProgramFrame {
                primary: input(1),
                secondary: Some(input(9)),
                transition_kind: Some(TransitionKind::Fade),
                mix_numerator: 1,
                mix_denominator: 2,
                mix_start_numerator: 1,
                mix_end_numerator: 1,
            }
        ),
        Err(RenderError::MissingSource { input: input(9) })
    );
}
