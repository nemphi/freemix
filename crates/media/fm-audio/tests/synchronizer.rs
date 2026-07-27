#![allow(clippy::float_cmp)]

use std::num::NonZeroU128;

use fm_audio::{
    AudioCadenceOrigin, AudioSynchronizerError, AudioSynchronizerLimits, BufferLimit,
    ClockMappedAudioSynchronizer, MAX_SYNCHRONIZER_BLOCKS, MAX_SYNCHRONIZER_BYTES,
    MAX_SYNCHRONIZER_OUTPUT_SAMPLES, MAX_SYNCHRONIZER_SAMPLES, MasterAudioInterval,
    SynchronizerDiscontinuity, SynchronizerLimit,
};
use fm_clock::{ClockDomainId, ClockMapping, ClockSnapshot, ClockTime};
use fm_frame::{
    AudioBlock, Channel, ChannelLayout, ClockDomainId as FrameClockDomainId, MediaFlags,
    MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    SequenceNumber, TimeBase,
};
use fm_types::SampleRate;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

fn nonzero(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn mapping(source_anchor_nanos: u64, master_anchor_nanos: u64, drift_ppb: i64) -> ClockMapping {
    ClockMapping::new(
        ClockSnapshot::new(
            ClockDomainId::new(nonzero(1)),
            ClockTime::from_nanos(source_anchor_nanos),
        ),
        ClockSnapshot::new(
            ClockDomainId::new(nonzero(2)),
            ClockTime::from_nanos(master_anchor_nanos),
        ),
        drift_ppb,
    )
    .unwrap()
}

fn mono() -> ChannelLayout {
    ChannelLayout::new(vec![Channel::Mono]).unwrap()
}

fn stereo() -> ChannelLayout {
    ChannelLayout::stereo()
}

fn rate(hertz: u32) -> SampleRate {
    SampleRate::new(hertz).unwrap()
}

fn origin(timestamp: i64, sample_index: u64) -> AudioCadenceOrigin {
    AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(timestamp), sample_index)
}

fn limits(
    blocks: usize,
    samples: usize,
    bytes: usize,
    output_samples: usize,
) -> AudioSynchronizerLimits {
    AudioSynchronizerLimits::new(blocks, samples, bytes, output_samples).unwrap()
}

fn duration(first_sample: u64, samples: u64, sample_rate: SampleRate) -> u64 {
    let start = u128::from(first_sample) * NANOS_PER_SECOND / u128::from(sample_rate.hertz());
    let end =
        u128::from(first_sample + samples) * NANOS_PER_SECOND / u128::from(sample_rate.hertz());
    u64::try_from(end - start).unwrap()
}

fn timestamp(origin: i64, sample: u64, sample_rate: SampleRate) -> i64 {
    let offset = u128::from(sample) * NANOS_PER_SECOND / u128::from(sample_rate.hertz());
    origin + i64::try_from(offset).unwrap()
}

fn coordinate_timestamp(
    origin_timestamp: i64,
    origin_sample: u64,
    sample: u64,
    sample_rate: SampleRate,
) -> i64 {
    let origin_boundary =
        u128::from(origin_sample) * NANOS_PER_SECOND / u128::from(sample_rate.hertz());
    let sample_boundary = u128::from(sample) * NANOS_PER_SECOND / u128::from(sample_rate.hertz());
    origin_timestamp + i64::try_from(sample_boundary - origin_boundary).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn block(
    sample_rate: SampleRate,
    layout: ChannelLayout,
    clock: u128,
    sequence: u64,
    pts: i64,
    duration_nanos: u64,
    planes: Vec<Vec<f32>>,
    flags: MediaFlags,
) -> AudioBlock {
    let timing = MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(0), TimeBase::new(1, 1).unwrap()),
        NormalizedTimestamp::from_nanos(pts),
        NormalizedDuration::from_nanos(duration_nanos).unwrap(),
        FrameClockDomainId::new(nonzero(clock)),
        SequenceNumber::new(sequence),
    )
    .unwrap()
    .with_flags(flags);
    AudioBlock::new(timing, sample_rate, layout, planes).unwrap()
}

fn interval(
    start: i64,
    first_sample: u64,
    samples: u64,
    sample_rate: SampleRate,
) -> MasterAudioInterval {
    MasterAudioInterval::new(
        ClockDomainId::new(nonzero(2)),
        NormalizedTimestamp::from_nanos(start),
        NormalizedDuration::from_nanos(duration(first_sample, samples, sample_rate)).unwrap(),
    )
}

fn synchronizer(
    source_rate: SampleRate,
    output_rate: SampleRate,
    map: ClockMapping,
) -> ClockMappedAudioSynchronizer {
    synchronizer_at(source_rate, output_rate, map, origin(0, 0), origin(0, 0))
}

fn synchronizer_at(
    source_rate: SampleRate,
    output_rate: SampleRate,
    map: ClockMapping,
    source_origin: AudioCadenceOrigin,
    master_origin: AudioCadenceOrigin,
) -> ClockMappedAudioSynchronizer {
    ClockMappedAudioSynchronizer::new(
        source_rate,
        output_rate,
        mono(),
        map,
        source_origin,
        master_origin,
        limits(8, 128, 128 * 4, 64),
    )
    .unwrap()
}

fn assert_close(actual: f32, expected: f32, tolerance: f32) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} != {expected} within {tolerance}"
    );
}

#[test]
fn identity_render_is_exact_across_split_blocks() {
    let sample_rate = rate(48_000);
    let mut sync = synchronizer(sample_rate, sample_rate, mapping(0, 0, 0));
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        10,
        0,
        duration(0, 3, sample_rate),
        vec![vec![0.0, 1.0, 2.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        11,
        timestamp(0, 3, sample_rate),
        duration(3, 4, sample_rate),
        vec![vec![3.0, 4.0, 5.0, 6.0]],
        MediaFlags::NONE,
    ))
    .unwrap();

    let mut first = [-1.0; 5];
    sync.render_into(interval(0, 0, 5, sample_rate), &mut [&mut first])
        .unwrap();
    assert_eq!(first, [0.0, 1.0, 2.0, 3.0, 4.0]);

    let mut second = [-1.0; 2];
    sync.render_into(
        interval(timestamp(0, 5, sample_rate), 5, 2, sample_rate),
        &mut [&mut second],
    )
    .unwrap();
    assert_eq!(second, [5.0, 6.0]);
    let telemetry = sync.telemetry();
    assert_eq!(telemetry.accepted_blocks(), 2);
    assert_eq!(telemetry.rendered_intervals(), 2);
    assert_eq!(telemetry.rendered_samples(), 7);
    assert_eq!(telemetry.buffered_samples(), 0);
    assert_eq!(telemetry.peak_buffered_blocks(), 2);
}

#[test]
fn resamples_44_1_to_48_deterministically() {
    let source_rate = rate(44_100);
    let output_rate = rate(48_000);
    let mut sync = synchronizer(source_rate, output_rate, mapping(0, 0, 0));
    let source: Vec<f32> = (0_u8..16).map(f32::from).collect();
    sync.push(&block(
        source_rate,
        mono(),
        1,
        0,
        0,
        duration(0, 16, source_rate),
        vec![source],
        MediaFlags::NONE,
    ))
    .unwrap();

    let mut output = [-1.0; 12];
    sync.render_into(interval(0, 0, 12, output_rate), &mut [&mut output])
        .unwrap();
    for (index, actual) in output.into_iter().enumerate() {
        let expected = f32::from(u16::try_from(index).unwrap()) * 44_100.0 / 48_000.0;
        assert_close(actual, expected, 0.000_1);
    }
}

#[test]
fn absolute_44_1k_sample_one_uses_exact_source_and_master_endpoints() {
    let sample_rate = rate(44_100);
    let sample_index = 1;
    let sample_timestamp = timestamp(0, sample_index, sample_rate);
    assert_eq!(sample_timestamp, 22_675);
    assert_eq!(duration(sample_index, 1, sample_rate), 22_676);
    let cadence_origin = origin(sample_timestamp, sample_index);
    let mut sync = synchronizer_at(
        sample_rate,
        sample_rate,
        mapping(0, 0, 0),
        cadence_origin,
        cadence_origin,
    );

    let locally_rebased = block(
        sample_rate,
        mono(),
        1,
        0,
        sample_timestamp,
        duration(0, 1, sample_rate),
        vec![vec![1.0]],
        MediaFlags::NONE,
    );
    assert_eq!(
        sync.push(&locally_rebased),
        Err(AudioSynchronizerError::SourceDurationMismatch {
            expected_nanos: 22_676,
            actual_nanos: 22_675,
        })
    );
    assert_eq!(sync.telemetry().buffered_samples(), 0);
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        0,
        sample_timestamp,
        duration(sample_index, 2, sample_rate),
        vec![vec![1.0, 2.0]],
        MediaFlags::NONE,
    ))
    .unwrap();

    let wrong_interval = MasterAudioInterval::new(
        ClockDomainId::new(nonzero(2)),
        NormalizedTimestamp::from_nanos(sample_timestamp),
        NormalizedDuration::from_nanos(22_675).unwrap(),
    );
    let mut output = [9.0];
    assert_eq!(
        sync.render_into(wrong_interval, &mut [&mut output]),
        Err(AudioSynchronizerError::MasterDurationMismatch {
            expected_nanos: 22_676,
            actual_nanos: 22_675,
        })
    );
    assert_eq!(output, [9.0]);
    sync.render_into(
        interval(sample_timestamp, sample_index, 1, sample_rate),
        &mut [&mut output],
    )
    .unwrap();
    assert_eq!(output, [1.0]);
}

#[test]
fn reset_rearms_independent_arbitrary_absolute_cadences() {
    let sample_rate = rate(44_100);
    let mut sync = synchronizer(sample_rate, sample_rate, mapping(0, 0, 0));
    let source_sample = 1_000_001;
    let master_sample = 9_000_007;
    let shared_timestamp = -5_000_000;
    let source_origin = origin(shared_timestamp, source_sample);
    let master_origin = origin(shared_timestamp, master_sample);
    sync.reset(source_origin, master_origin);
    assert_eq!(sync.source_origin(), source_origin);
    assert_eq!(sync.master_origin(), master_origin);

    sync.push(&block(
        sample_rate,
        mono(),
        1,
        40,
        shared_timestamp,
        duration(source_sample, 1, sample_rate),
        vec![vec![3.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        41,
        coordinate_timestamp(
            shared_timestamp,
            source_sample,
            source_sample + 1,
            sample_rate,
        ),
        duration(source_sample + 1, 2, sample_rate),
        vec![vec![4.0, 5.0]],
        MediaFlags::NONE,
    ))
    .unwrap();

    let mut first = [-1.0];
    sync.render_into(
        interval(shared_timestamp, master_sample, 1, sample_rate),
        &mut [&mut first],
    )
    .unwrap();
    assert_eq!(first, [3.0]);
    let mut rest = [-1.0; 2];
    sync.render_into(
        interval(
            coordinate_timestamp(
                shared_timestamp,
                master_sample,
                master_sample + 1,
                sample_rate,
            ),
            master_sample + 1,
            2,
            sample_rate,
        ),
        &mut [&mut rest],
    )
    .unwrap();
    assert_eq!(rest, [4.0, 5.0]);
    assert_eq!(sync.telemetry().resets(), 1);
}

#[test]
fn long_absolute_cadence_keeps_split_blocks_and_phase_exact() {
    let sample_rate = rate(44_100);
    let first_sample = u64::MAX - 16;
    let anchor_timestamp = -123_456;
    let cadence_origin = origin(anchor_timestamp, first_sample);
    let mut sync = synchronizer_at(
        sample_rate,
        sample_rate,
        mapping(0, 0, 0),
        cadence_origin,
        cadence_origin,
    );
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        70,
        anchor_timestamp,
        duration(first_sample, 4, sample_rate),
        vec![vec![0.0, 1.0, 2.0, 3.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        71,
        coordinate_timestamp(
            anchor_timestamp,
            first_sample,
            first_sample + 4,
            sample_rate,
        ),
        duration(first_sample + 4, 4, sample_rate),
        vec![vec![4.0, 5.0, 6.0, 7.0]],
        MediaFlags::NONE,
    ))
    .unwrap();

    let mut output = [-1.0; 8];
    sync.render_into(
        interval(anchor_timestamp, first_sample, 8, sample_rate),
        &mut [&mut output],
    )
    .unwrap();
    assert_eq!(output, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
    assert_eq!(sync.telemetry().buffered_samples(), 0);
}

#[test]
fn interpolation_phase_continues_across_a_block_seam() {
    let source_rate = rate(32_000);
    let output_rate = rate(48_000);
    let mut sync = synchronizer(source_rate, output_rate, mapping(0, 0, 0));
    sync.push(&block(
        source_rate,
        mono(),
        1,
        4,
        0,
        duration(0, 2, source_rate),
        vec![vec![0.0, 1.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    sync.push(&block(
        source_rate,
        mono(),
        1,
        5,
        timestamp(0, 2, source_rate),
        duration(2, 3, source_rate),
        vec![vec![2.0, 3.0, 4.0]],
        MediaFlags::NONE,
    ))
    .unwrap();

    let mut output = [-1.0; 4];
    sync.render_into(interval(0, 0, 4, output_rate), &mut [&mut output])
        .unwrap();
    for (actual, expected) in output.into_iter().zip([0.0, 2.0 / 3.0, 4.0 / 3.0, 2.0]) {
        assert_close(actual, expected, 0.000_1);
    }
}

#[test]
fn positive_and_negative_clock_offsets_select_the_mapped_samples() {
    let sample_rate = rate(10);
    let samples = vec![0.0, 1.0, 2.0, 3.0];
    let mut positive = synchronizer(sample_rate, sample_rate, mapping(200_000_000, 0, 0));
    positive
        .push(&block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 4, sample_rate),
            vec![samples.clone()],
            MediaFlags::NONE,
        ))
        .unwrap();
    let mut output = [-1.0];
    positive
        .render_into(interval(0, 0, 1, sample_rate), &mut [&mut output])
        .unwrap();
    assert_eq!(output, [2.0]);

    let mut negative = synchronizer_at(
        sample_rate,
        sample_rate,
        mapping(0, 200_000_000, 0),
        origin(-200_000_000, 0),
        origin(0, 0),
    );
    negative
        .push(&block(
            sample_rate,
            mono(),
            1,
            0,
            -200_000_000,
            duration(0, 4, sample_rate),
            vec![samples],
            MediaFlags::NONE,
        ))
        .unwrap();
    negative
        .render_into(interval(0, 0, 1, sample_rate), &mut [&mut output])
        .unwrap();
    assert_eq!(output, [0.0]);

    let mut positive_drift = synchronizer_at(
        sample_rate,
        sample_rate,
        mapping(0, 0, 1_000_000_000),
        origin(0, 0),
        origin(200_000_000, 0),
    );
    positive_drift
        .push(&block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 4, sample_rate),
            vec![vec![0.0, 1.0, 2.0, 3.0]],
            MediaFlags::NONE,
        ))
        .unwrap();
    positive_drift
        .render_into(interval(200_000_000, 0, 1, sample_rate), &mut [&mut output])
        .unwrap();
    assert_eq!(output, [1.0]);

    let mut negative_drift = synchronizer_at(
        sample_rate,
        sample_rate,
        mapping(0, 0, -500_000_000),
        origin(0, 0),
        origin(100_000_000, 0),
    );
    negative_drift
        .push(&block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 4, sample_rate),
            vec![vec![0.0, 1.0, 2.0, 3.0]],
            MediaFlags::NONE,
        ))
        .unwrap();
    negative_drift
        .render_into(interval(100_000_000, 0, 1, sample_rate), &mut [&mut output])
        .unwrap();
    assert_eq!(output, [2.0]);
}

#[test]
fn mapping_before_anchors_uses_floor_rounding() {
    let source_rate = rate(44_100);
    let output_rate = rate(1);
    let source_sample = 1;
    let source_origin = 989_999;
    let mut sync = synchronizer_at(
        source_rate,
        output_rate,
        mapping(1_000_000, 1_000_000, 500_000_000),
        origin(source_origin, source_sample),
        origin(999_999, 1),
    );
    sync.push(&block(
        source_rate,
        mono(),
        1,
        0,
        source_origin,
        duration(source_sample, 2, source_rate),
        vec![vec![0.0, 1.0]],
        MediaFlags::NONE,
    ))
    .unwrap();

    let mut output = [-1.0];
    sync.render_into(interval(999_999, 1, 1, output_rate), &mut [&mut output])
        .unwrap();
    assert_close(output[0], 10_000.0 / 22_676.0, 0.000_001);
}

#[test]
fn missing_lookahead_leaves_output_and_cursors_unchanged() {
    let source_rate = rate(10);
    let output_rate = rate(20);
    let mut sync = synchronizer_at(
        source_rate,
        output_rate,
        mapping(0, 0, 0),
        origin(0, 0),
        origin(50_000_000, 0),
    );
    sync.push(&block(
        source_rate,
        mono(),
        1,
        7,
        0,
        duration(0, 1, source_rate),
        vec![vec![0.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    let before = sync.telemetry();
    let mut output = [9.0];
    assert_eq!(
        sync.render_into(interval(50_000_000, 0, 1, output_rate), &mut [&mut output]),
        Err(AudioSynchronizerError::NeedMoreInput {
            required_sample: 1,
            buffered_end_sample: 1,
        })
    );
    assert_eq!(output, [9.0]);
    assert_eq!(
        sync.telemetry().buffered_samples(),
        before.buffered_samples()
    );

    sync.push(&block(
        source_rate,
        mono(),
        1,
        8,
        100_000_000,
        duration(1, 1, source_rate),
        vec![vec![1.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    sync.render_into(interval(50_000_000, 0, 1, output_rate), &mut [&mut output])
        .unwrap();
    assert_eq!(output, [0.5]);
    assert_eq!(sync.telemetry().failed_renders(), 1);
    assert_eq!(sync.telemetry().need_more_input(), 1);
}

#[test]
fn validates_every_configured_limit_dimension() {
    let cases = [
        (
            AudioSynchronizerLimits::new(0, 1, 4, 1),
            SynchronizerLimit::Blocks,
        ),
        (
            AudioSynchronizerLimits::new(MAX_SYNCHRONIZER_BLOCKS + 1, 1, 4, 1),
            SynchronizerLimit::Blocks,
        ),
        (
            AudioSynchronizerLimits::new(1, 0, 4, 1),
            SynchronizerLimit::Samples,
        ),
        (
            AudioSynchronizerLimits::new(1, MAX_SYNCHRONIZER_SAMPLES + 1, 4, 1),
            SynchronizerLimit::Samples,
        ),
        (
            AudioSynchronizerLimits::new(1, 1, 0, 1),
            SynchronizerLimit::Bytes,
        ),
        (
            AudioSynchronizerLimits::new(1, 1, MAX_SYNCHRONIZER_BYTES + 1, 1),
            SynchronizerLimit::Bytes,
        ),
        (
            AudioSynchronizerLimits::new(1, 1, 4, 0),
            SynchronizerLimit::OutputSamples,
        ),
        (
            AudioSynchronizerLimits::new(1, 1, 4, MAX_SYNCHRONIZER_OUTPUT_SAMPLES + 1),
            SynchronizerLimit::OutputSamples,
        ),
    ];
    for (result, expected_limit) in cases {
        assert!(matches!(
            result,
            Err(AudioSynchronizerError::InvalidLimit { limit, .. }) if limit == expected_limit
        ));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn rejects_configuration_format_domain_and_continuity_errors_transactionally() {
    assert!(matches!(
        AudioSynchronizerLimits::new(0, 1, 4, 1),
        Err(AudioSynchronizerError::InvalidLimit {
            limit: SynchronizerLimit::Blocks,
            ..
        })
    ));
    assert!(matches!(
        AudioSynchronizerLimits::new(MAX_SYNCHRONIZER_BLOCKS + 1, 1, 4, 1),
        Err(AudioSynchronizerError::InvalidLimit {
            limit: SynchronizerLimit::Blocks,
            ..
        })
    ));
    let high_rate = rate(AudioBlock::MAX_SAMPLE_RATE_HZ + 1);
    assert!(matches!(
        ClockMappedAudioSynchronizer::new(
            high_rate,
            rate(48_000),
            mono(),
            mapping(0, 0, 0),
            origin(0, 0),
            origin(0, 0),
            limits(1, 1, 4, 1),
        ),
        Err(AudioSynchronizerError::SourceRateOutOfRange(_))
    ));
    let duplicate = ChannelLayout::new(vec![Channel::Left, Channel::Left]).unwrap();
    assert!(matches!(
        ClockMappedAudioSynchronizer::new(
            rate(48_000),
            rate(48_000),
            duplicate,
            mapping(0, 0, 0),
            origin(0, 0),
            origin(0, 0),
            limits(1, 1, 8, 1),
        ),
        Err(AudioSynchronizerError::DuplicateChannel)
    ));
    assert!(matches!(
        ClockMappedAudioSynchronizer::new(
            rate(48_000),
            rate(48_000),
            stereo(),
            mapping(0, 0, 0),
            origin(0, 0),
            origin(0, 0),
            limits(1, 1, 4, 1),
        ),
        Err(AudioSynchronizerError::ByteCapacityTooSmall { .. })
    ));

    let sample_rate = rate(10);
    let mut sync = synchronizer(sample_rate, sample_rate, mapping(0, 0, 0));
    let rejected = [
        block(
            rate(20),
            mono(),
            1,
            0,
            0,
            duration(0, 1, rate(20)),
            vec![vec![0.0]],
            MediaFlags::NONE,
        ),
        block(
            sample_rate,
            stereo(),
            1,
            0,
            0,
            duration(0, 1, sample_rate),
            vec![vec![0.0], vec![0.0]],
            MediaFlags::NONE,
        ),
        block(
            sample_rate,
            mono(),
            9,
            0,
            0,
            duration(0, 1, sample_rate),
            vec![vec![0.0]],
            MediaFlags::NONE,
        ),
        block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 1, sample_rate),
            vec![vec![0.0]],
            MediaFlags::DISCONTINUITY,
        ),
        block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 1, sample_rate),
            vec![vec![0.0]],
            MediaFlags::CORRUPTED,
        ),
        block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 1, sample_rate),
            vec![vec![f32::NAN]],
            MediaFlags::NONE,
        ),
    ];
    for bad in &rejected {
        assert!(sync.push(bad).is_err());
        assert_eq!(sync.telemetry().buffered_samples(), 0);
    }

    let first = block(
        sample_rate,
        mono(),
        1,
        20,
        0,
        duration(0, 1, sample_rate),
        vec![vec![0.0]],
        MediaFlags::NONE,
    );
    sync.push(&first).unwrap();
    let wrong_sequence = block(
        sample_rate,
        mono(),
        1,
        22,
        100_000_000,
        duration(1, 1, sample_rate),
        vec![vec![1.0]],
        MediaFlags::NONE,
    );
    assert!(matches!(
        sync.push(&wrong_sequence),
        Err(AudioSynchronizerError::Discontinuity(
            SynchronizerDiscontinuity::Sequence { .. }
        ))
    ));
    let wrong_pts = block(
        sample_rate,
        mono(),
        1,
        21,
        100_000_001,
        duration(1, 1, sample_rate),
        vec![vec![1.0]],
        MediaFlags::NONE,
    );
    assert!(matches!(
        sync.push(&wrong_pts),
        Err(AudioSynchronizerError::Discontinuity(
            SynchronizerDiscontinuity::SourcePts { .. }
        ))
    ));
    let wrong_duration = block(
        sample_rate,
        mono(),
        1,
        21,
        100_000_000,
        duration(1, 1, sample_rate) + 1,
        vec![vec![1.0]],
        MediaFlags::NONE,
    );
    assert!(matches!(
        sync.push(&wrong_duration),
        Err(AudioSynchronizerError::SourceDurationMismatch { .. })
    ));
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        21,
        100_000_000,
        duration(1, 1, sample_rate),
        vec![vec![1.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    assert_eq!(sync.telemetry().buffered_samples(), 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn enforces_each_buffer_and_output_bound() {
    let sample_rate = rate(10);
    let one = || {
        block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 1, sample_rate),
            vec![vec![0.0]],
            MediaFlags::NONE,
        )
    };
    let mut by_blocks = ClockMappedAudioSynchronizer::new(
        sample_rate,
        sample_rate,
        mono(),
        mapping(0, 0, 0),
        origin(0, 0),
        origin(0, 0),
        limits(1, 8, 32, 4),
    )
    .unwrap();
    by_blocks.push(&one()).unwrap();
    assert!(matches!(
        by_blocks.push(&block(
            sample_rate,
            mono(),
            1,
            1,
            100_000_000,
            duration(1, 1, sample_rate),
            vec![vec![1.0]],
            MediaFlags::NONE,
        )),
        Err(AudioSynchronizerError::BufferOverflow {
            limit: BufferLimit::Blocks,
            ..
        })
    ));

    let mut by_samples = ClockMappedAudioSynchronizer::new(
        sample_rate,
        sample_rate,
        mono(),
        mapping(0, 0, 0),
        origin(0, 0),
        origin(0, 0),
        limits(4, 2, 32, 4),
    )
    .unwrap();
    assert!(matches!(
        by_samples.push(&block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 3, sample_rate),
            vec![vec![0.0; 3]],
            MediaFlags::NONE,
        )),
        Err(AudioSynchronizerError::BufferOverflow {
            limit: BufferLimit::Samples,
            ..
        })
    ));

    let mut by_bytes = ClockMappedAudioSynchronizer::new(
        sample_rate,
        sample_rate,
        mono(),
        mapping(0, 0, 0),
        origin(0, 0),
        origin(0, 0),
        limits(4, 8, 8, 4),
    )
    .unwrap();
    assert!(matches!(
        by_bytes.push(&block(
            sample_rate,
            mono(),
            1,
            0,
            0,
            duration(0, 3, sample_rate),
            vec![vec![0.0; 3]],
            MediaFlags::NONE,
        )),
        Err(AudioSynchronizerError::BufferOverflow {
            limit: BufferLimit::Bytes,
            ..
        })
    ));

    let mut output = [7.0; 5];
    assert!(matches!(
        by_blocks.render_into(interval(0, 0, 5, sample_rate), &mut [&mut output]),
        Err(AudioSynchronizerError::OutputSampleCountOutOfRange { .. })
    ));
    assert_eq!(output, [7.0; 5]);
    let mut left = [0.0];
    let mut right = [0.0; 2];
    assert!(matches!(
        by_blocks.render_into(interval(0, 0, 1, sample_rate), &mut [&mut left, &mut right]),
        Err(AudioSynchronizerError::OutputPlaneCountMismatch { .. })
    ));
    let mut stereo_sync = ClockMappedAudioSynchronizer::new(
        sample_rate,
        sample_rate,
        stereo(),
        mapping(0, 0, 0),
        origin(0, 0),
        origin(0, 0),
        limits(2, 4, 32, 4),
    )
    .unwrap();
    assert!(matches!(
        stereo_sync.render_into(interval(0, 0, 1, sample_rate), &mut [&mut left, &mut right]),
        Err(AudioSynchronizerError::OutputPlaneLengthMismatch { .. })
    ));
    let wrong_master = MasterAudioInterval::new(
        ClockDomainId::new(nonzero(99)),
        NormalizedTimestamp::from_nanos(0),
        NormalizedDuration::from_nanos(duration(0, 1, sample_rate)).unwrap(),
    );
    assert!(matches!(
        by_blocks.render_into(wrong_master, &mut [&mut left]),
        Err(AudioSynchronizerError::MasterClockMismatch { .. })
    ));
    let wrong_duration = MasterAudioInterval::new(
        ClockDomainId::new(nonzero(2)),
        NormalizedTimestamp::from_nanos(0),
        NormalizedDuration::from_nanos(200_000_000).unwrap(),
    );
    assert!(matches!(
        by_blocks.render_into(wrong_duration, &mut [&mut left]),
        Err(AudioSynchronizerError::MasterDurationMismatch { .. })
    ));
}

#[test]
fn master_pts_rejection_rolls_back_the_render_cursor() {
    let sample_rate = rate(10);
    let mut sync = synchronizer(sample_rate, sample_rate, mapping(0, 0, 0));
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        0,
        0,
        duration(0, 3, sample_rate),
        vec![vec![0.0, 1.0, 2.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    let mut output = [-1.0];
    sync.render_into(interval(0, 0, 1, sample_rate), &mut [&mut output])
        .unwrap();
    output[0] = -1.0;
    assert!(matches!(
        sync.render_into(interval(100_000_001, 1, 1, sample_rate), &mut [&mut output]),
        Err(AudioSynchronizerError::Discontinuity(
            SynchronizerDiscontinuity::MasterPts { .. }
        ))
    ));
    assert_eq!(output, [-1.0]);
    sync.render_into(interval(100_000_000, 1, 1, sample_rate), &mut [&mut output])
        .unwrap();
    assert_eq!(output, [1.0]);
}

#[test]
fn reset_rearms_continuity_and_extreme_arithmetic_is_transactional() {
    let sample_rate = rate(1);
    let mut sync = synchronizer(sample_rate, sample_rate, mapping(0, 0, 0));
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        1,
        0,
        duration(0, 2, sample_rate),
        vec![vec![1.0, 2.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    sync.reset(origin(-2_000_000_000, 0), origin(i64::MAX, 0));
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        99,
        -2_000_000_000,
        duration(0, 2, sample_rate),
        vec![vec![3.0, 4.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
    assert_eq!(sync.telemetry().resets(), 1);
    assert_eq!(sync.telemetry().buffered_samples(), 2);

    let mut output = [7.0];
    let extreme = MasterAudioInterval::new(
        ClockDomainId::new(nonzero(2)),
        NormalizedTimestamp::from_nanos(i64::MAX),
        NormalizedDuration::from_nanos(1_000_000_000).unwrap(),
    );
    assert_eq!(
        sync.render_into(extreme, &mut [&mut output]),
        Err(AudioSynchronizerError::ArithmeticOverflow)
    );
    assert_eq!(output, [7.0]);
    assert_eq!(sync.telemetry().buffered_samples(), 2);

    sync.reset(origin(0, 0), origin(0, 0));
    let maximum_sequence = block(
        sample_rate,
        mono(),
        1,
        u64::MAX,
        0,
        duration(0, 1, sample_rate),
        vec![vec![0.0]],
        MediaFlags::NONE,
    );
    assert_eq!(
        sync.push(&maximum_sequence),
        Err(AudioSynchronizerError::ArithmeticOverflow)
    );
    sync.push(&block(
        sample_rate,
        mono(),
        1,
        0,
        0,
        duration(0, 1, sample_rate),
        vec![vec![0.0]],
        MediaFlags::NONE,
    ))
    .unwrap();
}
