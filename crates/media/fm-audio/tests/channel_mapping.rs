#![allow(clippy::float_cmp)]

use std::num::NonZeroU128;

use fm_audio::{
    ChannelMapping, ChannelMappingError, ChannelMappingRoute, ChannelMappingSide,
    MAX_CHANNEL_MAPPING_ROUTES, MAX_CHANNELS, MAX_SAMPLES_PER_BLOCK,
};
use fm_frame::{
    AudioBlock, Channel, ChannelLayout, ClockDomainId, MediaFlags, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SequenceNumber,
};
use fm_types::{MediaTimestamp, SampleRate, TimeBase};

fn layout(channels: Vec<Channel>) -> ChannelLayout {
    ChannelLayout::new(channels).unwrap()
}

fn timing() -> MediaTiming {
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(321), TimeBase::new(1, 48_000).unwrap()),
        NormalizedTimestamp::from_nanos(6_687_500),
        NormalizedDuration::from_nanos(62_500).unwrap(),
        ClockDomainId::new(NonZeroU128::new(9).unwrap()),
        SequenceNumber::new(17),
    )
    .unwrap()
    .with_flags(MediaFlags::DISCONTINUITY)
}

fn block(channel_layout: ChannelLayout, planes: Vec<Vec<f32>>) -> AudioBlock {
    AudioBlock::new(
        timing(),
        SampleRate::new(48_000).unwrap(),
        channel_layout,
        planes,
    )
    .unwrap()
}

#[test]
fn mono_identity_preserves_samples_and_timing() {
    let mono = layout(vec![Channel::Mono]);
    let mapping = ChannelMapping::matching(mono.clone(), mono.clone()).unwrap();
    let input = block(mono, vec![vec![0.25, -0.5, 1.0]]);

    let output = mapping.map(&input).unwrap();

    assert_eq!(output.timing(), input.timing());
    assert_eq!(output.sample_rate(), input.sample_rate());
    assert_eq!(output.channel_layout(), input.channel_layout());
    assert_eq!(output.sample_count(), input.sample_count());
    assert_eq!(output.planes(), input.planes());
}

#[test]
fn matching_labels_reorders_stereo_and_silences_unmapped_destination() {
    let source = layout(vec![Channel::Right, Channel::Left]);
    let destination = layout(vec![Channel::Left, Channel::Center, Channel::Right]);
    let mapping = ChannelMapping::matching(source.clone(), destination.clone()).unwrap();
    let input = block(source, vec![vec![3.0, 4.0], vec![1.0, 2.0]]);

    let output = mapping.map(&input).unwrap();

    assert_eq!(output.channel_layout(), &destination);
    assert_eq!(
        output.planes(),
        &[vec![1.0, 2.0], vec![0.0, 0.0], vec![3.0, 4.0]]
    );
}

#[test]
fn explicit_routes_duplicate_mono_and_downmix_stereo() {
    let mono = layout(vec![Channel::Mono]);
    let stereo = ChannelLayout::stereo();
    let duplicate = ChannelMapping::new(
        mono.clone(),
        stereo.clone(),
        vec![
            ChannelMappingRoute {
                source: Channel::Mono,
                destination: Channel::Left,
                coefficient: 1.0,
            },
            ChannelMappingRoute {
                source: Channel::Mono,
                destination: Channel::Right,
                coefficient: 0.5,
            },
        ],
    )
    .unwrap();
    let duplicated = duplicate
        .map(&block(mono.clone(), vec![vec![0.5, -1.0]]))
        .unwrap();
    assert_eq!(duplicated.planes(), &[vec![0.5, -1.0], vec![0.25, -0.5]]);

    let downmix = ChannelMapping::new(
        stereo.clone(),
        mono.clone(),
        vec![
            ChannelMappingRoute {
                source: Channel::Right,
                destination: Channel::Mono,
                coefficient: 0.25,
            },
            ChannelMappingRoute {
                source: Channel::Left,
                destination: Channel::Mono,
                coefficient: 0.75,
            },
        ],
    )
    .unwrap();
    let mixed = downmix
        .map(&block(stereo, vec![vec![1.0, -1.0], vec![0.0, 1.0]]))
        .unwrap();
    assert_eq!(mixed.planes(), &[vec![0.75, -0.5]]);
}

#[test]
fn invalid_layouts_routes_and_coefficients_are_rejected() {
    let mono = layout(vec![Channel::Mono]);
    let stereo = ChannelLayout::stereo();
    let duplicate_source = layout(vec![Channel::Left, Channel::Left]);
    assert_eq!(
        ChannelMapping::matching(duplicate_source, stereo.clone()),
        Err(ChannelMappingError::DuplicateLayoutChannel {
            side: ChannelMappingSide::Source,
            channel: Channel::Left,
        })
    );

    let missing = ChannelMappingRoute {
        source: Channel::Center,
        destination: Channel::Mono,
        coefficient: 1.0,
    };
    assert_eq!(
        ChannelMapping::new(stereo.clone(), mono.clone(), vec![missing]),
        Err(ChannelMappingError::UnknownRouteChannel {
            side: ChannelMappingSide::Source,
            channel: Channel::Center,
        })
    );
    let missing_destination = ChannelMappingRoute {
        source: Channel::Left,
        destination: Channel::Center,
        coefficient: 1.0,
    };
    assert_eq!(
        ChannelMapping::new(stereo.clone(), mono.clone(), vec![missing_destination]),
        Err(ChannelMappingError::UnknownRouteChannel {
            side: ChannelMappingSide::Destination,
            channel: Channel::Center,
        })
    );

    let route = ChannelMappingRoute {
        source: Channel::Left,
        destination: Channel::Mono,
        coefficient: 0.5,
    };
    assert_eq!(
        ChannelMapping::new(stereo.clone(), mono.clone(), vec![route, route]),
        Err(ChannelMappingError::DuplicateRoute {
            source: Channel::Left,
            destination: Channel::Mono,
        })
    );

    for coefficient in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(matches!(
            ChannelMapping::new(
                mono.clone(),
                mono.clone(),
                vec![ChannelMappingRoute {
                    source: Channel::Mono,
                    destination: Channel::Mono,
                    coefficient,
                }],
            ),
            Err(ChannelMappingError::InvalidCoefficient { route: 0, .. })
        ));
    }

    let too_many = vec![
        ChannelMappingRoute {
            source: Channel::Mono,
            destination: Channel::Mono,
            coefficient: 1.0,
        };
        MAX_CHANNEL_MAPPING_ROUTES + 1
    ];
    assert_eq!(
        ChannelMapping::new(mono.clone(), mono, too_many),
        Err(ChannelMappingError::TooManyRoutes {
            actual: MAX_CHANNEL_MAPPING_ROUTES + 1,
            maximum: MAX_CHANNEL_MAPPING_ROUTES,
        })
    );

    let too_wide = layout(vec![Channel::Mono; MAX_CHANNELS + 1]);
    assert_eq!(
        ChannelMapping::matching(too_wide, stereo),
        Err(ChannelMappingError::ChannelCountOutOfRange {
            side: ChannelMappingSide::Source,
            actual: MAX_CHANNELS + 1,
            maximum: MAX_CHANNELS,
        })
    );
}

#[test]
fn map_into_is_transactional_for_shape_input_and_numeric_failures() {
    let mono = layout(vec![Channel::Mono]);
    let mapping = ChannelMapping::matching(mono.clone(), mono.clone()).unwrap();
    let valid = block(mono.clone(), vec![vec![1.0, 2.0, 3.0]]);
    let mut output = vec![vec![9.0; 3]];

    let wrong_layout = block(layout(vec![Channel::Center]), vec![vec![1.0, 2.0, 3.0]]);
    assert_eq!(
        mapping.map_into(&wrong_layout, &mut output),
        Err(ChannelMappingError::SourceLayoutMismatch)
    );
    assert_eq!(output, vec![vec![9.0; 3]]);

    let malformed = block(mono, vec![vec![1.0, f32::NAN, 3.0]]);
    assert_eq!(
        mapping.map_into(&malformed, &mut output),
        Err(ChannelMappingError::NonFiniteInput {
            channel: 0,
            sample: 1,
        })
    );
    assert_eq!(output, vec![vec![9.0; 3]]);

    let mut short = vec![vec![8.0; 2]];
    assert_eq!(
        mapping.map_into(&valid, &mut short),
        Err(ChannelMappingError::OutputPlaneLengthMismatch {
            plane: 0,
            expected: 3,
            actual: 2,
        })
    );
    assert_eq!(short, vec![vec![8.0; 2]]);

    let mut extra_plane = vec![vec![6.0; 3], vec![6.0; 3]];
    assert_eq!(
        mapping.map_into(&valid, &mut extra_plane),
        Err(ChannelMappingError::OutputPlaneCountMismatch {
            expected: 1,
            actual: 2,
        })
    );
    assert_eq!(extra_plane, vec![vec![6.0; 3], vec![6.0; 3]]);

    let explosive = ChannelMapping::new(
        layout(vec![Channel::Mono]),
        layout(vec![Channel::Mono]),
        vec![ChannelMappingRoute {
            source: Channel::Mono,
            destination: Channel::Mono,
            coefficient: f32::MAX,
        }],
    )
    .unwrap();
    assert_eq!(
        explosive.map_into(&valid, &mut output),
        Err(ChannelMappingError::NonFiniteOutput {
            channel: 0,
            sample: 1,
        })
    );
    assert_eq!(output, vec![vec![9.0; 3]]);
}

#[test]
fn oversized_block_is_rejected_before_output_mutation() {
    let mono = layout(vec![Channel::Mono]);
    let mapping = ChannelMapping::matching(mono.clone(), mono.clone()).unwrap();
    let input = block(mono, vec![vec![0.0; MAX_SAMPLES_PER_BLOCK + 1]]);
    let mut output = vec![vec![7.0; MAX_SAMPLES_PER_BLOCK + 1]];

    assert_eq!(
        mapping.map_into(&input, &mut output),
        Err(ChannelMappingError::SampleCountOutOfRange {
            actual: MAX_SAMPLES_PER_BLOCK + 1,
            maximum: MAX_SAMPLES_PER_BLOCK,
        })
    );
    assert!(output[0].iter().all(|sample| *sample == 7.0));
}
