use std::num::NonZeroU128;

use fm_audio::{
    AudioBlock, AudioError, Balance, ChannelMapping, ChannelMappingRoute, ChannelMeter,
    ClippingPolicy, Gain, InputState, MasterMixer, MasterOutput, PlanarAudioSource, SourceGain,
};
use fm_frame::{
    ClockDomainId, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    SequenceNumber,
};
use fm_types::{
    AudioFormat, Channel, ChannelLayout, InputId, MediaTimestamp, SampleFormat, SampleRate,
    TimeBase,
};

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn format(channels: ChannelLayout) -> AudioFormat {
    AudioFormat {
        sample_rate: SampleRate::new(48_000).unwrap(),
        sample_format: SampleFormat::F32,
        channels,
    }
}

fn mono_format() -> AudioFormat {
    format(ChannelLayout::new(vec![Channel::Mono]).unwrap())
}

fn stereo_format() -> AudioFormat {
    format(ChannelLayout::stereo())
}

fn block(format: &AudioFormat, planes: Vec<Vec<f32>>) -> AudioBlock {
    AudioBlock::from_planar(format.clone(), planes).unwrap()
}

fn planar_source<'a>(
    id: InputId,
    format: &'a AudioFormat,
    planes: &'a [Vec<f32>],
    samples: usize,
    source_gain: SourceGain,
) -> PlanarAudioSource<'a> {
    PlanarAudioSource {
        input: id,
        sample_rate: format.sample_rate,
        channel_layout: &format.channels,
        planes,
        samples,
        source_gain,
    }
}

fn add_input(mixer: &mut MasterMixer, id: InputId, format: &AudioFormat, state: InputState) {
    mixer
        .add_input(
            id,
            format.clone(),
            ChannelMapping::identity(format.channels.clone()).unwrap(),
            state,
        )
        .unwrap();
}

fn input_channels(output: &MasterOutput, id: InputId) -> &[ChannelMeter] {
    output
        .input_meters
        .iter()
        .find(|reading| reading.input == id)
        .unwrap()
        .meters
        .channels()
}

fn assert_close(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
}

fn assert_meter(meter: ChannelMeter, peak: f32, rms: f32) {
    assert_close(meter.peak, peak);
    assert_close(meter.rms, rms);
}

#[test]
fn planar_meter_length_errors_preserve_output_and_runtime_state() {
    let format = stereo_format();
    let id = input(1);
    let planes = vec![vec![1.0; 2]; 2];
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    add_input(&mut mixer, id, &format, InputState::default());
    mixer.set_input_delay(id, 1).unwrap();
    mixer
        .set_input_state(
            id,
            InputState {
                gain: Gain::SILENCE,
                balance: Balance::from_basis_points(10_000).unwrap(),
                ..InputState::default()
            },
            4,
        )
        .unwrap();
    let source = planar_source(id, &format, &planes, 2, SourceGain::UNITY);
    for (master_len, input_len) in [(0, 2), (2, 0)] {
        let mut output = vec![vec![9.0; 2]; 2];
        let mut master_meters = vec![ChannelMeter::default(); master_len];
        let mut input_meters = vec![ChannelMeter::default(); input_len];
        assert_eq!(
            mixer.mix_planar_timed_into_with_meters(
                timing(2),
                2,
                &[source],
                &[],
                &mut output,
                &mut master_meters,
                &mut input_meters,
            ),
            Err(AudioError::MeterCountMismatch {
                expected: 2,
                actual: 0,
            })
        );
        assert_eq!(output, vec![vec![9.0; 2]; 2]);
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        assert_eq!(mixer.current_normalized_balance(id), Some(0.0));
    }

    let mut output = vec![vec![0.0; 2]; 2];
    mixer
        .mix_planar_timed_into_with_meters(
            timing(2),
            2,
            &[source],
            &[],
            &mut output,
            &mut [ChannelMeter::default(); 2],
            &mut [ChannelMeter::default(); 2],
        )
        .unwrap();
    assert_eq!(output, [vec![0.0, 0.25], vec![0.0, 0.5]]);
    assert_eq!(mixer.current_linear_gain(id), Some(0.5));
    assert_eq!(mixer.current_normalized_balance(id), Some(0.5));
}

#[test]
fn shared_source_alias_meters_are_ordered_and_separate_from_the_master_sum() {
    let format = mono_format();
    let alias = input(1);
    let source_id = input(2);
    let silent = input(3);
    let source = block(&format, vec![vec![0.5; 4]]);
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    mixer.set_clipping_policy(ClippingPolicy::Allow);
    add_input(&mut mixer, source_id, &format, InputState::default());
    add_input(&mut mixer, silent, &format, InputState::default());
    mixer
        .add_input_alias(
            alias,
            source_id,
            InputState {
                gain: Gain::from_linear(0.5).unwrap(),
                ..InputState::default()
            },
        )
        .unwrap();

    let output = mixer
        .mix(4, &[(source_id, &source), (alias, &source)], None)
        .unwrap();

    assert_eq!(
        output
            .input_meters
            .iter()
            .map(|reading| reading.input)
            .collect::<Vec<_>>(),
        vec![alias, source_id, silent]
    );
    assert_meter(output.meters.channels()[0], 0.75, 0.75);
    assert_meter(input_channels(&output, alias)[0], 0.25, 0.25);
    assert_meter(input_channels(&output, source_id)[0], 0.5, 0.5);
    assert_meter(input_channels(&output, silent)[0], 0.0, 0.0);
}

#[test]
fn mute_solo_and_follow_video_gate_input_meters() {
    let format = mono_format();
    let audible = input(1);
    let muted = input(2);
    let follow_video = input(3);
    let not_soloed = input(4);
    let source = block(&format, vec![vec![0.25; 2]]);
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    for (id, state) in [
        (
            audible,
            InputState {
                soloed: true,
                ..InputState::default()
            },
        ),
        (
            muted,
            InputState {
                muted: true,
                soloed: true,
                ..InputState::default()
            },
        ),
        (
            follow_video,
            InputState {
                soloed: true,
                follow_video: true,
                ..InputState::default()
            },
        ),
        (not_soloed, InputState::default()),
    ] {
        add_input(&mut mixer, id, &format, state);
    }

    let output = mixer
        .mix(
            2,
            &[
                (audible, &source),
                (muted, &source),
                (follow_video, &source),
                (not_soloed, &source),
            ],
            Some(audible),
        )
        .unwrap();

    assert_meter(input_channels(&output, audible)[0], 0.25, 0.25);
    for id in [muted, follow_video, not_soloed] {
        assert_meter(input_channels(&output, id)[0], 0.0, 0.0);
    }
}

#[test]
fn planar_meter_slices_match_allocating_reference_in_stable_order() {
    let format = stereo_format();
    let first = input(1);
    let silent = input(2);
    let muted = input(3);
    let follow_video = input(4);
    let not_soloed = input(5);
    let planes = vec![vec![1.0; 4], vec![0.5; 4]];
    let mut reference = MasterMixer::new(format.clone()).unwrap();
    for (id, state) in [
        (
            follow_video,
            InputState {
                soloed: true,
                follow_video: true,
                ..InputState::default()
            },
        ),
        (
            silent,
            InputState {
                soloed: true,
                ..InputState::default()
            },
        ),
        (not_soloed, InputState::default()),
        (
            muted,
            InputState {
                muted: true,
                soloed: true,
                ..InputState::default()
            },
        ),
    ] {
        add_input(&mut reference, id, &format, state);
    }
    reference
        .add_input(
            first,
            format.clone(),
            ChannelMapping::new(
                format.channels.clone(),
                format.channels.clone(),
                vec![
                    ChannelMappingRoute {
                        source: Channel::Left,
                        destination: Channel::Right,
                        coefficient: 1.0,
                    },
                    ChannelMappingRoute {
                        source: Channel::Right,
                        destination: Channel::Left,
                        coefficient: 1.0,
                    },
                ],
            )
            .unwrap(),
            InputState {
                gain: Gain::from_linear(0.5).unwrap(),
                balance: Balance::from_basis_points(5_000).unwrap(),
                soloed: true,
                ..InputState::default()
            },
        )
        .unwrap();
    reference.set_input_delay(first, 1).unwrap();
    let mut planar = reference.clone();
    let source_gain = SourceGain::new(0, 1, 1).unwrap();
    let block = fm_frame::AudioBlock::new(
        timing(4),
        format.sample_rate,
        format.channels.clone(),
        planes.clone(),
    )
    .unwrap();
    let submitted = [first, muted, follow_video, not_soloed];
    let blocks = submitted.map(|id| (id, &block, source_gain));
    let expected = reference
        .mix_timed_with_source_gains(timing(4), 4, &blocks, &[first])
        .unwrap();
    let mut output = vec![vec![0.0; 4]; 2];
    let mut master_meters = [ChannelMeter::default(); 2];
    let mut input_meters = [ChannelMeter::default(); 10];
    let sources = submitted.map(|id| planar_source(id, &format, &planes, 4, source_gain));
    planar
        .mix_planar_timed_into_with_meters(
            timing(4),
            4,
            &sources,
            &[first],
            &mut output,
            &mut master_meters,
            &mut input_meters,
        )
        .unwrap();

    assert_eq!(output, expected.block.planes());
    assert_eq!(
        output,
        [
            vec![0.0, 0.0625, 0.09375, 0.125],
            vec![0.0, 0.25, 0.375, 0.5]
        ]
    );
    assert_eq!(master_meters, expected.meters.channels());
    assert_eq!(
        expected
            .input_meters
            .iter()
            .map(|reading| reading.input)
            .collect::<Vec<_>>(),
        [first, silent, muted, follow_video, not_soloed]
    );
    for (actual, expected) in input_meters.chunks_exact(2).zip(&expected.input_meters) {
        assert_eq!(actual, expected.meters.channels());
    }
    assert_meter(input_meters[0], 0.125, (0.028_320_312_5_f32 / 4.0).sqrt());
    assert_meter(input_meters[1], 0.5, (0.453_125_f32 / 4.0).sqrt());
    assert_eq!(&input_meters[2..], &[ChannelMeter::default(); 8]);
}

fn timing(samples: usize) -> MediaTiming {
    let duration_nanos = (samples as u64) * 1_000_000_000 / 48_000;
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(0), TimeBase::new(1, 48_000).unwrap()),
        NormalizedTimestamp::from_nanos(0),
        NormalizedDuration::from_nanos(duration_nanos).unwrap(),
        ClockDomainId::new(NonZeroU128::new(7).unwrap()),
        SequenceNumber::new(0),
    )
    .unwrap()
}
