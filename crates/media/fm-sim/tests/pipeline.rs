use fm_sim::{
    PipelineConfigError, RegistryError, RenderError, Rgba8, SimulatedPipeline, SimulatedSource,
    SourcePattern,
};
use fm_switcher::ProgramFrame;
use fm_types::InputId;
use std::num::NonZeroU128;

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn program(primary: InputId) -> ProgramFrame {
    ProgramFrame {
        primary,
        secondary: None,
        mix_numerator: 0,
        mix_denominator: 1,
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
        mix_numerator: numerator,
        mix_denominator: 2,
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
                mix_numerator: 1,
                mix_denominator: 2,
            }
        ),
        Err(RenderError::MissingSource { input: input(9) })
    );
}
