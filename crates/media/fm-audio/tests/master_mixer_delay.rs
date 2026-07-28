#![allow(clippy::float_cmp)]

use std::num::NonZeroU128;

use fm_audio::{
    AudioBlock, AudioError, ChannelMapping, ChannelMappingRoute, ClippingPolicy, Gain, InputState,
    MAX_MASTER_MIXER_DELAY_BYTES, MAX_SAMPLE_DELAY_SAMPLES, MAX_SAMPLES_PER_BLOCK, MasterMixer,
    MasterMixerDelayError, PlanarAudioSource, SourceGain,
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

fn format(layout: ChannelLayout) -> AudioFormat {
    AudioFormat {
        sample_rate: SampleRate::new(48_000).unwrap(),
        sample_format: SampleFormat::F32,
        channels: layout,
    }
}

fn mono_format() -> AudioFormat {
    format(ChannelLayout::new(vec![Channel::Mono]).unwrap())
}

fn timing(sequence: u64, samples: usize) -> MediaTiming {
    let samples = u64::try_from(samples).unwrap();
    let start = sequence.checked_mul(samples).unwrap();
    let end = start.checked_add(samples).unwrap();
    let start_nanos = start.checked_mul(1_000_000_000).unwrap() / 48_000;
    let end_nanos = end.checked_mul(1_000_000_000).unwrap() / 48_000;
    MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(start).unwrap()),
            TimeBase::new(1, 48_000).unwrap(),
        ),
        NormalizedTimestamp::from_nanos(i64::try_from(start_nanos).unwrap()),
        NormalizedDuration::from_nanos(end_nanos - start_nanos).unwrap(),
        ClockDomainId::new(NonZeroU128::new(9).unwrap()),
        SequenceNumber::new(sequence),
    )
    .unwrap()
}

fn legacy_block(format: &AudioFormat, planes: Vec<Vec<f32>>) -> AudioBlock {
    AudioBlock::from_planar(format.clone(), planes).unwrap()
}

fn timed_block(format: &AudioFormat, sequence: u64, planes: Vec<Vec<f32>>) -> fm_frame::AudioBlock {
    let samples = planes[0].len();
    fm_frame::AudioBlock::new(
        timing(sequence, samples),
        format.sample_rate,
        format.channels.clone(),
        planes,
    )
    .unwrap()
}

fn mono_mixer(delay_samples: usize) -> (MasterMixer, InputId) {
    let format = mono_format();
    let id = input(1);
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    mixer.set_clipping_policy(ClippingPolicy::Allow);
    mixer
        .add_input(
            id,
            format.clone(),
            ChannelMapping::identity(format.channels).unwrap(),
            InputState::default(),
        )
        .unwrap();
    mixer.set_input_delay(id, delay_samples).unwrap();
    (mixer, id)
}

#[test]
fn legacy_delay_is_exact_across_arbitrary_boundaries_and_omitted_tail() {
    let format = mono_format();
    let (mut mixer, id) = mono_mixer(3);
    let first = legacy_block(&format, vec![vec![1.0, 2.0]]);
    let second = legacy_block(&format, vec![vec![3.0, 4.0, 5.0, 6.0, 7.0]]);

    assert_eq!(
        mixer.mix(2, &[(id, &first)], None).unwrap().block.plane(0),
        Some(&[0.0, 0.0][..])
    );
    assert_eq!(
        mixer.mix(5, &[(id, &second)], None).unwrap().block.plane(0),
        Some(&[0.0, 1.0, 2.0, 3.0, 4.0][..])
    );
    assert_eq!(
        mixer.mix(3, &[], None).unwrap().block.plane(0),
        Some(&[5.0, 6.0, 7.0][..])
    );
    assert_eq!(
        mixer.mix(3, &[], None).unwrap().block.plane(0),
        Some(&[0.0, 0.0, 0.0][..])
    );
}

#[test]
fn zero_delay_preserves_all_master_mix_api_results() {
    let format = mono_format();
    let legacy = legacy_block(&format, vec![vec![0.25, -0.5]]);
    let canonical = timed_block(&format, 0, vec![vec![0.25, -0.5]]);

    let (mut legacy_mixer, id) = mono_mixer(0);
    assert_eq!(
        legacy_mixer
            .mix(2, &[(id, &legacy)], None)
            .unwrap()
            .block
            .plane(0),
        Some(&[0.25, -0.5][..])
    );

    let (mut timed_mixer, id) = mono_mixer(0);
    assert_eq!(
        timed_mixer
            .mix_timed(timing(0, 2), 2, &[(id, &canonical)], None)
            .unwrap()
            .block
            .plane(0),
        Some(&[0.25, -0.5][..])
    );

    let (mut source_gain_mixer, id) = mono_mixer(0);
    assert_eq!(
        source_gain_mixer
            .mix_timed_with_source_gains(
                timing(0, 2),
                2,
                &[(id, &canonical, SourceGain::UNITY)],
                &[],
            )
            .unwrap()
            .block
            .plane(0),
        Some(&[0.25, -0.5][..])
    );

    let (mut planar_mixer, id) = mono_mixer(0);
    let planes = vec![vec![0.25, -0.5]];
    let mut output = vec![vec![9.0; 4]];
    planar_mixer
        .mix_planar_timed_into(
            timing(0, 2),
            2,
            &[PlanarAudioSource {
                input: id,
                sample_rate: format.sample_rate,
                channel_layout: &format.channels,
                planes: &planes,
                samples: 2,
                source_gain: SourceGain::UNITY,
            }],
            &[],
            &mut output,
        )
        .unwrap();
    assert_eq!(output, vec![vec![0.25, -0.5, 9.0, 9.0]]);
}

#[test]
fn timed_and_preallocated_planar_apis_share_exact_delay_semantics() {
    let format = mono_format();
    let canonical = timed_block(&format, 0, vec![vec![1.0, 0.0, 0.0]]);
    let (mut timed_mixer, id) = mono_mixer(2);
    assert_eq!(
        timed_mixer
            .mix_timed(timing(0, 3), 3, &[(id, &canonical)], None)
            .unwrap()
            .block
            .plane(0),
        Some(&[0.0, 0.0, 1.0][..])
    );

    let (mut planar_mixer, id) = mono_mixer(2);
    let planes = vec![vec![1.0, 0.0, 0.0]];
    let mut output = vec![vec![9.0; 5]];
    let pointer = output[0].as_ptr();
    planar_mixer
        .mix_planar_timed_into(
            timing(0, 3),
            3,
            &[PlanarAudioSource {
                input: id,
                sample_rate: format.sample_rate,
                channel_layout: &format.channels,
                planes: &planes,
                samples: 3,
                source_gain: SourceGain::UNITY,
            }],
            &[],
            &mut output,
        )
        .unwrap();
    assert_eq!(output[0].as_ptr(), pointer);
    assert_eq!(output, vec![vec![0.0, 0.0, 1.0, 9.0, 9.0]]);
}

#[test]
fn aliases_own_independent_delay_histories() {
    let format = mono_format();
    let source = input(1);
    let alias = input(2);
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    mixer.set_clipping_policy(ClippingPolicy::Allow);
    mixer
        .add_input(
            source,
            format.clone(),
            ChannelMapping::identity(format.channels.clone()).unwrap(),
            InputState::default(),
        )
        .unwrap();
    mixer.set_input_delay(source, 1).unwrap();
    mixer
        .add_input_alias(alias, source, InputState::default())
        .unwrap();
    mixer.set_input_delay(alias, 3).unwrap();
    let source_block = legacy_block(&format, vec![vec![1.0, 0.0, 0.0, 0.0]]);
    let alias_block = legacy_block(&format, vec![vec![2.0, 0.0, 0.0, 0.0]]);

    let output = mixer
        .mix(4, &[(source, &source_block), (alias, &alias_block)], None)
        .unwrap();

    assert_eq!(output.block.plane(0), Some(&[0.0, 1.0, 0.0, 2.0][..]));
}

#[test]
fn muted_and_inactive_afv_strips_advance_without_contributing() {
    let format = mono_format();
    let muted = input(1);
    let afv = input(2);
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    mixer.set_clipping_policy(ClippingPolicy::Allow);
    for (id, strip_state) in [
        (
            muted,
            InputState {
                muted: true,
                ..InputState::default()
            },
        ),
        (
            afv,
            InputState {
                follow_video: true,
                ..InputState::default()
            },
        ),
    ] {
        mixer
            .add_input(
                id,
                format.clone(),
                ChannelMapping::identity(format.channels.clone()).unwrap(),
                strip_state,
            )
            .unwrap();
        mixer.set_input_delay(id, 2).unwrap();
    }
    let muted_block = legacy_block(&format, vec![vec![1.0, 0.0]]);
    let afv_block = legacy_block(&format, vec![vec![2.0, 0.0]]);
    assert_eq!(
        mixer
            .mix(2, &[(muted, &muted_block), (afv, &afv_block)], None,)
            .unwrap()
            .block
            .plane(0),
        Some(&[0.0, 0.0][..])
    );

    mixer
        .set_input_state(
            muted,
            InputState {
                ..InputState::default()
            },
            0,
        )
        .unwrap();
    assert_eq!(
        mixer.mix(2, &[], Some(afv)).unwrap().block.plane(0),
        Some(&[3.0, 0.0][..])
    );
}

#[test]
fn source_gain_envelope_applies_to_current_interval_after_delay() {
    let format = mono_format();
    let (mut mixer, id) = mono_mixer(2);
    let impulse = timed_block(&format, 0, vec![vec![1.0, 1.0]]);
    let silence = timed_block(&format, 1, vec![vec![0.0, 0.0]]);

    let first = mixer
        .mix_timed_with_source_gains(
            timing(0, 2),
            2,
            &[(id, &impulse, SourceGain::new(1, 0, 1).unwrap())],
            &[],
        )
        .unwrap();
    let second = mixer
        .mix_timed_with_source_gains(
            timing(1, 2),
            2,
            &[(id, &silence, SourceGain::new(0, 1, 1).unwrap())],
            &[],
        )
        .unwrap();

    assert_eq!(first.block.plane(0), Some(&[0.0, 0.0][..]));
    assert_eq!(second.block.plane(0), Some(&[0.5, 1.0][..]));
}

#[test]
fn changing_delay_resets_history_and_preserves_gain_ramp_contract() {
    let format = mono_format();
    let (mut mixer, id) = mono_mixer(2);
    let impulse = legacy_block(&format, vec![vec![1.0]]);
    assert_eq!(
        mixer
            .mix(1, &[(id, &impulse)], None)
            .unwrap()
            .block
            .plane(0),
        Some(&[0.0][..])
    );
    mixer.set_input_delay(id, 2).unwrap();
    let silence = legacy_block(&format, vec![vec![0.0]]);
    assert_eq!(
        mixer
            .mix(1, &[(id, &silence)], None)
            .unwrap()
            .block
            .plane(0),
        Some(&[0.0][..])
    );
    assert_eq!(
        mixer
            .mix(1, &[(id, &silence)], None)
            .unwrap()
            .block
            .plane(0),
        Some(&[1.0][..])
    );

    mixer.set_input_delay(id, 1).unwrap();
    mixer
        .set_input_state(
            id,
            InputState {
                gain: Gain::SILENCE,
                ..InputState::default()
            },
            2,
        )
        .unwrap();
    let unity = legacy_block(&format, vec![vec![1.0, 1.0]]);
    let output = mixer.mix(2, &[(id, &unity)], None).unwrap();

    assert_eq!(output.block.plane(0), Some(&[0.0, 0.0][..]));
    assert_eq!(mixer.current_linear_gain(id), Some(0.0));
}

#[test]
fn delay_budget_rejection_rolls_back_and_remove_reclaims_exact_bytes() {
    let layout = ChannelLayout::new(vec![
        Channel::Mono,
        Channel::Left,
        Channel::Right,
        Channel::Center,
        Channel::LowFrequency,
        Channel::LeftSurround,
        Channel::RightSurround,
    ])
    .unwrap();
    let format = format(layout);
    let mapping = ChannelMapping::identity(format.channels.clone()).unwrap();
    let bytes_per_strip =
        format.channels.channels().len() * MAX_SAMPLE_DELAY_SAMPLES * size_of::<f32>();
    let full_strips = MAX_MASTER_MIXER_DELAY_BYTES / bytes_per_strip;
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    for index in 0..full_strips {
        let id = input(u128::try_from(index + 1).unwrap());
        mixer
            .add_input(id, format.clone(), mapping.clone(), InputState::default())
            .unwrap();
        mixer.set_input_delay(id, MAX_SAMPLE_DELAY_SAMPLES).unwrap();
    }
    let retained = full_strips * bytes_per_strip;
    assert_eq!(mixer.retained_delay_bytes(), retained);
    let candidate = input(u128::try_from(full_strips + 1).unwrap());
    mixer
        .add_input(
            candidate,
            format.clone(),
            mapping.clone(),
            InputState::default(),
        )
        .unwrap();
    assert_eq!(
        mixer.set_input_delay(candidate, MAX_SAMPLE_DELAY_SAMPLES),
        Err(MasterMixerDelayError::BudgetExceeded {
            requested: bytes_per_strip,
            retained,
            maximum: MAX_MASTER_MIXER_DELAY_BYTES,
        })
    );
    assert_eq!(mixer.retained_delay_bytes(), retained);
    assert_eq!(mixer.input_delay_samples(candidate), Some(0));

    let removed = input(1);
    mixer.remove_input(removed).unwrap();
    assert_eq!(mixer.retained_delay_bytes(), retained - bytes_per_strip);
    mixer
        .set_input_delay(candidate, MAX_SAMPLE_DELAY_SAMPLES)
        .unwrap();
    assert_eq!(mixer.retained_delay_bytes(), retained);
}

#[test]
fn failed_validation_and_numeric_render_do_not_advance_delay_or_ramp() {
    let format = mono_format();
    let id = input(1);
    let maximum_gain = Gain::from_db(fm_audio::MAX_GAIN_DB).unwrap();
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    mixer.set_clipping_policy(ClippingPolicy::Allow);
    mixer
        .add_input(
            id,
            format.clone(),
            ChannelMapping::identity(format.channels.clone()).unwrap(),
            InputState {
                gain: maximum_gain,
                muted: true,
                ..InputState::default()
            },
        )
        .unwrap();
    mixer.set_input_delay(id, 1).unwrap();
    let maximum = legacy_block(&format, vec![vec![f32::MAX]]);
    mixer.mix(1, &[(id, &maximum)], None).unwrap();
    mixer
        .set_input_state(
            id,
            InputState {
                gain: Gain::SILENCE,
                ..InputState::default()
            },
            4,
        )
        .unwrap();
    let mut reference = mixer.clone();

    let invalid = vec![vec![f32::NAN]];
    let mut output = vec![vec![9.0]];
    assert!(matches!(
        mixer.mix_planar_timed_into(
            timing(0, 1),
            1,
            &[PlanarAudioSource {
                input: id,
                sample_rate: format.sample_rate,
                channel_layout: &format.channels,
                planes: &invalid,
                samples: 1,
                source_gain: SourceGain::UNITY,
            }],
            &[],
            &mut output,
        ),
        Err(AudioError::NonFiniteSample { .. })
    ));
    assert_eq!(output, vec![vec![9.0]]);
    let silence = legacy_block(&format, vec![vec![0.0]]);
    assert!(matches!(
        mixer.mix(1, &[(id, &silence)], None),
        Err(AudioError::NonFiniteSample { .. })
    ));
    assert_eq!(
        mixer.current_linear_gain(id),
        reference.current_linear_gain(id)
    );

    for candidate in [&mut mixer, &mut reference] {
        candidate
            .set_input_state(
                id,
                InputState {
                    gain: Gain::UNITY,
                    ..InputState::default()
                },
                0,
            )
            .unwrap();
    }
    assert_eq!(
        mixer.mix(1, &[(id, &silence)], None).unwrap(),
        reference.mix(1, &[(id, &silence)], None).unwrap()
    );
}

#[test]
fn copy_runtime_state_copies_delay_history_and_requires_matching_delay() {
    let format = mono_format();
    let id = input(1);
    let (mut source, _) = mono_mixer(2);
    let priming = legacy_block(&format, vec![vec![1.0, 2.0, 3.0]]);
    source.mix(3, &[(id, &priming)], None).unwrap();

    let (mut destination, _) = mono_mixer(2);
    destination.copy_runtime_state_from(&source).unwrap();
    let continuation = legacy_block(&format, vec![vec![4.0, 5.0]]);
    assert_eq!(
        destination.mix(2, &[(id, &continuation)], None).unwrap(),
        source.mix(2, &[(id, &continuation)], None).unwrap()
    );

    let (mut mismatched, _) = mono_mixer(1);
    assert_eq!(
        mismatched.copy_runtime_state_from(&source),
        Err(AudioError::FormatMismatch)
    );
    assert_eq!(mismatched.input_delay_samples(id), Some(1));
}

#[test]
fn raw_multichannel_delay_precedes_channel_mapping() {
    let input_format = format(ChannelLayout::stereo());
    let output_format = mono_format();
    let id = input(1);
    let mapping = ChannelMapping::new(
        input_format.channels.clone(),
        output_format.channels.clone(),
        vec![
            ChannelMappingRoute {
                source: Channel::Left,
                destination: Channel::Mono,
                coefficient: 1.0,
            },
            ChannelMappingRoute {
                source: Channel::Right,
                destination: Channel::Mono,
                coefficient: -1.0,
            },
        ],
    )
    .unwrap();
    let mut mixer = MasterMixer::new(output_format).unwrap();
    mixer.set_clipping_policy(ClippingPolicy::Allow);
    mixer
        .add_input(id, input_format.clone(), mapping, InputState::default())
        .unwrap();
    mixer.set_input_delay(id, 2).unwrap();
    let block = legacy_block(
        &input_format,
        vec![vec![1.0, 2.0, 3.0], vec![10.0, 20.0, 30.0]],
    );

    assert_eq!(
        mixer.mix(3, &[(id, &block)], None).unwrap().block.plane(0),
        Some(&[0.0, 0.0, -9.0][..])
    );
    assert_eq!(
        mixer.mix(2, &[], None).unwrap().block.plane(0),
        Some(&[-18.0, -27.0][..])
    );
}

#[test]
fn delay_bounds_and_empty_legacy_intervals_are_exact() {
    let format = mono_format();
    let mut mixer = MasterMixer::new(format.clone()).unwrap();
    let id = input(1);
    mixer
        .add_input(
            id,
            format.clone(),
            ChannelMapping::identity(format.channels.clone()).unwrap(),
            InputState::default(),
        )
        .unwrap();
    assert_eq!(
        mixer.set_input_delay(id, MAX_SAMPLE_DELAY_SAMPLES + 1),
        Err(MasterMixerDelayError::SampleDelay(
            fm_audio::SampleDelayError::DelayOutOfRange {
                actual: MAX_SAMPLE_DELAY_SAMPLES + 1,
                maximum: MAX_SAMPLE_DELAY_SAMPLES,
            }
        ))
    );
    assert_eq!(mixer.retained_delay_bytes(), 0);
    assert_eq!(mixer.input_delay_samples(id), Some(0));
    assert_eq!(
        mixer.set_input_delay(input(2), 1),
        Err(MasterMixerDelayError::UnknownInput(input(2)))
    );
    let error = mixer
        .set_input_delay(id, MAX_SAMPLE_DELAY_SAMPLES + 1)
        .unwrap_err();
    assert!(
        std::error::Error::source(&error)
            .is_some_and(|source| source.to_string().contains("exceeds the limit"))
    );

    mixer.set_input_delay(id, MAX_SAMPLE_DELAY_SAMPLES).unwrap();
    let before = mixer.retained_delay_bytes();
    assert_eq!(
        mixer.mix(0, &[], None).unwrap().block.plane(0),
        Some(&[][..])
    );
    assert_eq!(mixer.retained_delay_bytes(), before);

    let mut samples = vec![0.0; MAX_SAMPLES_PER_BLOCK];
    samples[0] = 1.0;
    let maximum = legacy_block(&mono_format(), vec![samples]);
    let leading = mixer
        .mix(MAX_SAMPLES_PER_BLOCK, &[(id, &maximum)], None)
        .unwrap();
    assert!(
        leading
            .block
            .plane(0)
            .unwrap()
            .iter()
            .all(|sample| *sample == 0.0)
    );
    let tail = mixer.mix(MAX_SAMPLES_PER_BLOCK, &[], None).unwrap();
    assert_eq!(tail.block.plane(0).unwrap()[0], 1.0);
    assert!(
        tail.block.plane(0).unwrap()[1..]
            .iter()
            .all(|sample| *sample == 0.0)
    );
}
