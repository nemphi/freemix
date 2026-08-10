use std::num::NonZeroU128;

use fm_audio::{
    AudioBlock, Balance, ChannelMapping, ChannelMeter, ClippingPolicy, Gain, InputState,
    MasterMixer, MasterOutput, SourceGain,
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
fn input_meter_applies_gain_and_balance_in_master_channel_order() {
    let format = stereo_format();
    let id = input(1);
    let source = block(&format, vec![vec![1.0; 4], vec![1.0; 4]]);
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    add_input(
        &mut mixer,
        id,
        &format,
        InputState {
            gain: Gain::from_linear(0.5).unwrap(),
            balance: Balance::from_basis_points(10_000).unwrap(),
            ..InputState::default()
        },
    );

    let output = mixer.mix(4, &[(id, &source)], None).unwrap();

    assert_meter(input_channels(&output, id)[0], 0.0, 0.0);
    assert_meter(input_channels(&output, id)[1], 0.5, 0.5);
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
fn delay_and_transition_source_gain_are_included_in_input_meter() {
    let format = mono_format();
    let id = input(1);
    let timing = timing(4);
    let source = fm_frame::AudioBlock::new(
        timing,
        format.sample_rate,
        format.channels.clone(),
        vec![vec![1.0; 4]],
    )
    .unwrap();
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    add_input(&mut mixer, id, &format, InputState::default());
    mixer.set_input_delay(id, 1).unwrap();

    let output = mixer
        .mix_timed_with_source_gains(
            timing,
            4,
            &[(id, &source, SourceGain::new(0, 1, 1).unwrap())],
            &[],
        )
        .unwrap();

    assert_eq!(output.block.planes()[0], [0.0, 0.5, 0.75, 1.0]);
    assert_meter(
        output.input_meters[0].meters.channels()[0],
        1.0,
        (1.812_5_f32 / 4.0).sqrt(),
    );
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
