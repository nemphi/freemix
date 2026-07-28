#![allow(clippy::float_cmp)]

use core::num::{NonZeroU128, NonZeroUsize};

use fm_audio::{
    AudioCadenceOrigin, AudioSilenceSpan, AudioSynchronizerError, AudioSynchronizerLimits,
    ClockMappedAudioSynchronizer, ClockRecalibrationError, ClockRecalibrationPolicy,
    ClockRecalibrationUpdate, MasterAudioInterval,
};
use fm_clock::{ClockDomainId, ClockMapping, ClockSnapshot, ClockTime, MappingError};
use fm_frame::{
    AudioBlock, Channel, ChannelLayout, ClockDomainId as FrameClockDomainId, MediaTimestamp,
    MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp, SequenceNumber,
    TimeBase,
};
use fm_types::SampleRate;

const SOURCE_DOMAIN_VALUE: u128 = 1;
const MASTER_DOMAIN_VALUE: u128 = 2;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

fn domain(value: u128) -> ClockDomainId {
    ClockDomainId::new(NonZeroU128::new(value).unwrap())
}

fn observation(source_nanos: u64, master_nanos: u64) -> (ClockSnapshot, ClockSnapshot) {
    (
        ClockSnapshot::new(
            domain(SOURCE_DOMAIN_VALUE),
            ClockTime::from_nanos(source_nanos),
        ),
        ClockSnapshot::new(
            domain(MASTER_DOMAIN_VALUE),
            ClockTime::from_nanos(master_nanos),
        ),
    )
}

fn synchronizer(mapping: ClockMapping) -> ClockMappedAudioSynchronizer {
    let rate = SampleRate::new(48_000).unwrap();
    ClockMappedAudioSynchronizer::new(
        rate,
        rate,
        ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        mapping,
        AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
        AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
        AudioSynchronizerLimits::new(8, 128, 128 * size_of::<f32>(), 64).unwrap(),
    )
    .unwrap()
}

fn identity_mapping() -> ClockMapping {
    let (source, master) = observation(0, 0);
    ClockMapping::new(source, master, 0).unwrap()
}

fn configured_synchronizer(
    window: usize,
    minimum: usize,
    max_drift_ppm: u32,
    max_error_nanos: u64,
) -> ClockMappedAudioSynchronizer {
    let mut synchronizer = synchronizer(identity_mapping());
    synchronizer.configure_clock_recalibration(
        ClockRecalibrationPolicy::new(window, minimum, max_drift_ppm, max_error_nanos).unwrap(),
    );
    synchronizer
}

fn interval(start_nanos: i64, samples: u64, rate: u32) -> MasterAudioInterval {
    MasterAudioInterval::new(
        domain(MASTER_DOMAIN_VALUE),
        NormalizedTimestamp::from_nanos(start_nanos),
        NormalizedDuration::from_nanos(samples * NANOS_PER_SECOND / u64::from(rate)).unwrap(),
    )
}

fn ramp_block(samples: usize, rate: SampleRate) -> AudioBlock {
    let duration_nanos =
        u64::try_from(samples).unwrap() * NANOS_PER_SECOND / u64::from(rate.hertz());
    let timing = MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(0),
            TimeBase::new(1, rate.hertz()).unwrap(),
        ),
        NormalizedTimestamp::from_nanos(0),
        NormalizedDuration::from_nanos(duration_nanos).unwrap(),
        FrameClockDomainId::new(NonZeroU128::new(SOURCE_DOMAIN_VALUE).unwrap()),
        SequenceNumber::new(0),
    )
    .unwrap();
    AudioBlock::new(
        timing,
        rate,
        ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        vec![
            (0..samples)
                .map(|sample| f32::from(u16::try_from(sample).unwrap()))
                .collect(),
        ],
    )
    .unwrap()
}

fn run_synthetic_drift(drift_ppm: i64) -> ClockMappedAudioSynchronizer {
    let mut synchronizer = configured_synchronizer(32, 8, 500, 10_000_000);
    let jitter = [-2_000_i64, 1_000, 3_000, -1_000, 0, 2_000, -3_000];
    for index in 1..=240_u64 {
        let source_nanos = index * NANOS_PER_SECOND;
        let ideal_master =
            i128::from(source_nanos) * (1_000_000_i128 + i128::from(drift_ppm)) / 1_000_000;
        let master_nanos = u64::try_from(
            ideal_master + i128::from(jitter[usize::try_from(index).unwrap() % jitter.len()]),
        )
        .unwrap();
        let (source, master) = observation(source_nanos, master_nanos);
        synchronizer.observe_clock_pair(source, master).unwrap();
    }
    synchronizer
}

#[test]
fn long_positive_and_negative_drift_converge_with_deterministic_jitter() {
    for drift_ppm in [137_i64, -83] {
        let synchronizer = run_synthetic_drift(drift_ppm);
        let telemetry = synchronizer.clock_recalibration_telemetry();
        let expected_ppb = drift_ppm * 1_000;
        assert!(
            (telemetry.current_drift_ppb() - expected_ppb).abs() <= 500,
            "{} ppb did not converge to {expected_ppb} ppb",
            telemetry.current_drift_ppb()
        );
        assert_eq!(telemetry.observation_count(), 32);
        assert_eq!(telemetry.accepted_recalibrations(), 233);
        assert_eq!(telemetry.rejected_recalibrations(), 0);
        assert_eq!(telemetry.anchor_generation(), 233);
    }
}

#[test]
fn estimated_drift_is_clamped_and_outliers_are_rejected_transactionally() {
    let mut synchronizer = configured_synchronizer(4, 2, 50, 10_000_000);
    let first = observation(NANOS_PER_SECOND, NANOS_PER_SECOND);
    assert_eq!(
        synchronizer.observe_clock_pair(first.0, first.1).unwrap(),
        ClockRecalibrationUpdate::Collecting {
            observations: 1,
            required: 2,
        }
    );
    let second = observation(2 * NANOS_PER_SECOND, 2_000_200_000);
    assert_eq!(
        synchronizer.observe_clock_pair(second.0, second.1).unwrap(),
        ClockRecalibrationUpdate::Recalibrated {
            drift_ppb: 50_000,
            clamped: true,
        }
    );
    let mapping = synchronizer.mapping();
    let telemetry = synchronizer.clock_recalibration_telemetry();

    let outlier = observation(3 * NANOS_PER_SECOND, 5 * NANOS_PER_SECOND);
    assert!(matches!(
        synchronizer.observe_clock_pair(outlier.0, outlier.1),
        Err(ClockRecalibrationError::ObservationOutlier { .. })
    ));
    assert_eq!(synchronizer.mapping(), mapping);
    assert_eq!(
        synchronizer
            .clock_recalibration_telemetry()
            .observation_count(),
        telemetry.observation_count()
    );
    assert_eq!(
        synchronizer
            .clock_recalibration_telemetry()
            .rejected_recalibrations(),
        1
    );

    assert_eq!(
        synchronizer.observe_clock_pair(second.0, second.1),
        Err(ClockRecalibrationError::Mapping(
            MappingError::NonMonotonicSamples
        ))
    );
    assert_eq!(synchronizer.mapping(), mapping);
}

#[test]
fn recalibration_preserves_the_active_render_frontier_for_both_drift_signs() {
    for drift_ppm in [1_000_i64, -1_000] {
        let rate = SampleRate::new(1_000).unwrap();
        let mut synchronizer = ClockMappedAudioSynchronizer::new(
            rate,
            rate,
            ChannelLayout::new(vec![Channel::Mono]).unwrap(),
            identity_mapping(),
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            AudioSynchronizerLimits::new(4, 32, 32 * size_of::<f32>(), 8).unwrap(),
        )
        .unwrap();
        synchronizer.configure_clock_recalibration(
            ClockRecalibrationPolicy::new(4, 2, 2_000, 2_000_000).unwrap(),
        );
        synchronizer.push(&ramp_block(16, rate)).unwrap();

        let mut prefix = [0.0; 4];
        synchronizer
            .render_into(interval(0, 4, rate.hertz()), &mut [&mut prefix])
            .unwrap();
        assert_eq!(prefix, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(synchronizer.telemetry().buffered_samples(), 12);

        let first = observation(NANOS_PER_SECOND, NANOS_PER_SECOND);
        synchronizer.observe_clock_pair(first.0, first.1).unwrap();
        let stale_plan = synchronizer
            .plan_render(interval(4_000_000, 2, rate.hertz()), 2)
            .unwrap();
        let master_nanos = if drift_ppm > 0 {
            2_001_000_000
        } else {
            1_999_000_000
        };
        let second = observation(2 * NANOS_PER_SECOND, master_nanos);
        assert_eq!(
            synchronizer.observe_clock_pair(second.0, second.1).unwrap(),
            ClockRecalibrationUpdate::Recalibrated {
                drift_ppb: drift_ppm * 1_000,
                clamped: false,
            }
        );
        assert_eq!(
            synchronizer.preflight_commit_render(stale_plan),
            Err(AudioSynchronizerError::StaleRenderPlan)
        );

        let mut next = [0.0; 2];
        synchronizer
            .render_into(interval(4_000_000, 2, rate.hertz()), &mut [&mut next])
            .unwrap();
        assert_eq!(next[0], 4.0);
        if drift_ppm > 0 {
            assert!((next[1] - 4.999).abs() < 0.000_01, "{next:?}");
        } else {
            assert!((next[1] - 5.001_001).abs() < 0.000_01, "{next:?}");
        }

        let mut continued = [0.0; 2];
        synchronizer
            .render_into(interval(6_000_000, 2, rate.hertz()), &mut [&mut continued])
            .unwrap();
        assert!(continued[0] > next[1]);
    }
}

#[test]
fn extreme_raw_drift_is_clamped_symmetrically_before_mapping_construction() {
    let cases = [
        (observation(1, 1), observation(2, u64::MAX), 50_000),
        (
            observation(1, 1),
            observation(i64::MAX.cast_unsigned(), 2),
            -50_000,
        ),
    ];
    for (first, second, expected_drift_ppb) in cases {
        let mut synchronizer = configured_synchronizer(2, 2, 50, u64::MAX);
        synchronizer.observe_clock_pair(first.0, first.1).unwrap();
        assert_eq!(
            synchronizer.observe_clock_pair(second.0, second.1).unwrap(),
            ClockRecalibrationUpdate::Recalibrated {
                drift_ppb: expected_drift_ppb,
                clamped: true,
            }
        );
    }
}

#[test]
fn reset_declares_a_discontinuity_and_clears_the_observation_window() {
    let mut synchronizer = configured_synchronizer(8, 2, 500, 10_000_000);
    for index in 1..=3_u64 {
        let source_nanos = index * NANOS_PER_SECOND;
        let pair = observation(source_nanos, source_nanos + index * 125_000);
        synchronizer.observe_clock_pair(pair.0, pair.1).unwrap();
    }
    assert_eq!(
        synchronizer
            .clock_recalibration_telemetry()
            .observation_count(),
        3
    );

    let restarted = observation(100, 10_000);
    synchronizer
        .reset_clock_discontinuity(
            restarted.0,
            restarted.1,
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(100), 0),
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(10_000), 0),
        )
        .unwrap();
    assert_eq!(
        synchronizer
            .clock_recalibration_telemetry()
            .observation_count(),
        0
    );
    assert_eq!(
        synchronizer.mapping().map(restarted.0).unwrap(),
        restarted.1
    );
    let next = observation(1_000_100, 1_010_125);
    assert_eq!(
        synchronizer.observe_clock_pair(next.0, next.1).unwrap(),
        ClockRecalibrationUpdate::Collecting {
            observations: 1,
            required: 2,
        }
    );
}

#[test]
fn observation_window_stays_bounded_over_long_runs() {
    let mut synchronizer = configured_synchronizer(5, 2, 500, 10_000_000);
    for index in 1..=10_000_u64 {
        let nanos = index * 1_000_000;
        let pair = observation(nanos, nanos);
        synchronizer.observe_clock_pair(pair.0, pair.1).unwrap();
    }
    let telemetry = synchronizer.clock_recalibration_telemetry();
    assert_eq!(telemetry.observation_count(), 5);
    assert_eq!(telemetry.accepted_recalibrations(), 9_999);
    assert_eq!(telemetry.current_drift_ppb(), 0);
}

#[test]
fn arithmetic_failure_preserves_mapping_buffer_and_render_cursor_state() {
    let mut synchronizer = configured_synchronizer(4, 2, 500, u64::MAX);
    let sample_count = NonZeroUsize::new(8).unwrap();
    let duration = NormalizedDuration::from_nanos(166_666).unwrap();
    let timing = MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(0), TimeBase::new(1, 48_000).unwrap()),
        NormalizedTimestamp::from_nanos(0),
        duration,
        FrameClockDomainId::new(NonZeroU128::new(SOURCE_DOMAIN_VALUE).unwrap()),
        SequenceNumber::new(0),
    )
    .unwrap();
    synchronizer
        .push_silence_batch(&[AudioSilenceSpan::new(timing, sample_count)])
        .unwrap();
    let first = observation(NANOS_PER_SECOND, NANOS_PER_SECOND);
    synchronizer.observe_clock_pair(first.0, first.1).unwrap();

    let interval = MasterAudioInterval::new(
        domain(MASTER_DOMAIN_VALUE),
        NormalizedTimestamp::from_nanos(0),
        NormalizedDuration::from_nanos(20_833).unwrap(),
    );
    let plan = synchronizer.plan_render(interval, 1).unwrap();
    let mapping = synchronizer.mapping();
    let occupancy = synchronizer.telemetry();
    let observations = synchronizer
        .clock_recalibration_telemetry()
        .observation_count();

    let overflowing = observation(i64::MAX.cast_unsigned() + 1, i64::MAX.cast_unsigned() + 1);
    assert_eq!(
        synchronizer.observe_clock_pair(overflowing.0, overflowing.1),
        Err(ClockRecalibrationError::Mapping(
            MappingError::ArithmeticOverflow
        ))
    );
    assert_eq!(synchronizer.mapping(), mapping);
    assert_eq!(synchronizer.telemetry(), occupancy);
    assert_eq!(
        synchronizer
            .clock_recalibration_telemetry()
            .observation_count(),
        observations
    );
    synchronizer.preflight_commit_render(plan).unwrap();
}

#[test]
fn frontier_reanchor_failure_is_transactional() {
    let rate = SampleRate::new(1_000).unwrap();
    let mapping = ClockMapping::new(
        ClockSnapshot::new(
            domain(SOURCE_DOMAIN_VALUE),
            ClockTime::from_nanos(NANOS_PER_SECOND),
        ),
        ClockSnapshot::new(domain(MASTER_DOMAIN_VALUE), ClockTime::from_nanos(0)),
        0,
    )
    .unwrap();
    let mut synchronizer = ClockMappedAudioSynchronizer::new(
        rate,
        rate,
        ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        mapping,
        AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
        AudioCadenceOrigin::new(
            NormalizedTimestamp::from_nanos(-i64::try_from(NANOS_PER_SECOND).unwrap()),
            0,
        ),
        AudioSynchronizerLimits::new(4, 32, 32 * size_of::<f32>(), 8).unwrap(),
    )
    .unwrap();
    synchronizer.configure_clock_recalibration(
        ClockRecalibrationPolicy::new(4, 2, 500, 10_000_000).unwrap(),
    );
    synchronizer.push(&ramp_block(8, rate)).unwrap();
    let mut first_output = [0.0];
    synchronizer
        .render_into(
            interval(-1_000_000_000, 1, rate.hertz()),
            &mut [&mut first_output],
        )
        .unwrap();
    assert_eq!(first_output, [0.0]);

    let first = observation(2 * NANOS_PER_SECOND, NANOS_PER_SECOND);
    synchronizer.observe_clock_pair(first.0, first.1).unwrap();
    let plan = synchronizer
        .plan_render(interval(-999_000_000, 1, rate.hertz()), 1)
        .unwrap();
    let mapping_before = synchronizer.mapping();
    let occupancy_before = synchronizer.telemetry();
    let recalibration_before = synchronizer.clock_recalibration_telemetry();

    let second = observation(3 * NANOS_PER_SECOND, 2_000_100_000);
    assert_eq!(
        synchronizer.observe_clock_pair(second.0, second.1),
        Err(ClockRecalibrationError::Mapping(
            MappingError::ArithmeticOverflow
        ))
    );
    assert_eq!(synchronizer.mapping(), mapping_before);
    assert_eq!(synchronizer.telemetry(), occupancy_before);
    let recalibration_after = synchronizer.clock_recalibration_telemetry();
    assert_eq!(
        recalibration_after.observation_count(),
        recalibration_before.observation_count()
    );
    assert_eq!(
        recalibration_after.accepted_recalibrations(),
        recalibration_before.accepted_recalibrations()
    );
    assert_eq!(
        recalibration_after.rejected_recalibrations(),
        recalibration_before.rejected_recalibrations() + 1
    );
    synchronizer.preflight_commit_render(plan).unwrap();
}
