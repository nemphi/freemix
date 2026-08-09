use std::num::NonZeroU128;

use fm_audio::Gain;
use fm_frame::{
    AlphaMode, ChannelLayout, ChromaLocation, ClockDomainId, ColorMetadata, ColorPrimaries,
    MatrixCoefficients, MediaFlags, PixelFormat, SequenceNumber, SignalRange, TransferFunction,
    VideoFrameMetadata,
};
use fm_sim::{
    AudioPattern, CollectError, CollectOutcome, CollectingAudioSink, CollectingSink,
    CollectingVideoSink, FaultSchedule, OverflowPolicy, Rgba8, SimulatedAudioSource,
    SimulatedVideoSource, SourceEvent, SourcePattern, audio_block_hash, video_frame_hash,
};
use fm_types::FrameRate;

fn clock() -> ClockDomainId {
    ClockDomainId::new(NonZeroU128::new(1).unwrap())
}

fn ntsc_60() -> FrameRate {
    FrameRate::new(60_000, 1_001).unwrap()
}

fn video_source(pattern: SourcePattern) -> SimulatedVideoSource {
    SimulatedVideoSource::new(14, 4, ntsc_60(), clock(), pattern).unwrap()
}

fn silent_audio() -> SimulatedAudioSource {
    SimulatedAudioSource::new(
        ntsc_60(),
        ChannelLayout::stereo(),
        clock(),
        AudioPattern::Silence,
    )
    .unwrap()
}

fn expected_video_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

#[test]
fn video_frames_have_importer_compatible_rgba_contracts() {
    let SourceEvent::Frame(bars) = video_source(SourcePattern::Bars).next_event().unwrap() else {
        panic!("bars source should produce a frame");
    };
    let solid_color = Rgba8::new(1, 2, 3, 255);
    let solid = video_source(SourcePattern::Solid(solid_color))
        .next_frame()
        .unwrap()
        .unwrap();

    for frame in [&bars, &solid] {
        assert_eq!(frame.metadata(), Some(expected_video_metadata()));
        assert_eq!(frame.payload().format(), PixelFormat::Rgba8);
        assert_eq!(frame.payload().planes().len(), 1);
        assert_eq!(frame.payload().plane(0).unwrap().stride(), 14 * 4);
    }

    assert_eq!(video_frame_hash(&bars), 727_902_077_449_732_892);
    assert_eq!(video_frame_hash(&solid), 681_754_405_434_072_565);
    assert!(
        solid
            .payload()
            .plane(0)
            .unwrap()
            .bytes()
            .chunks_exact(4)
            .all(|pixel| pixel == solid_color.to_bytes())
    );
}

#[test]
fn rational_cadence_accumulates_exactly_at_60000_over_1001() {
    let mut audio = silent_audio();
    let mut total = 0;
    for sequence in 0..1_001 {
        let block = audio.next_block().unwrap().unwrap();
        assert_eq!(block.timing().sequence(), SequenceNumber::new(sequence));
        assert!(matches!(block.sample_count(), 800 | 801));
        total += block.sample_count();
    }

    assert_eq!(total, 801_600);
    assert_eq!(audio.cumulative_samples(), 801_600);
    assert_eq!(audio.sequence(), SequenceNumber::new(1_001));
}

#[test]
fn video_and_audio_timing_and_injected_flags_are_exact() {
    let faults = FaultSchedule::default()
        .discontinuity_at(0)
        .corruption_at(1);
    let mut video = video_source(SourcePattern::Solid(Rgba8::new(1, 2, 3, 255)));
    video.set_faults(faults.clone());
    let first = video.next_frame().unwrap().unwrap();
    let second = video.next_frame().unwrap().unwrap();

    assert_eq!(first.timing().presentation_timestamp().as_nanos(), 0);
    assert_eq!(first.timing().duration().as_nanos(), 16_683_333);
    assert!(first.timing().flags().contains(MediaFlags::DISCONTINUITY));
    assert_eq!(
        second.timing().presentation_timestamp().as_nanos(),
        16_683_333
    );
    assert_eq!(second.timing().duration().as_nanos(), 16_683_333);
    assert!(second.timing().flags().contains(MediaFlags::CORRUPTED));

    let mut audio = silent_audio();
    audio.set_faults(faults);
    let first = audio.next_block().unwrap().unwrap();
    let second = audio.next_block().unwrap().unwrap();
    assert_eq!(first.sample_count(), 800);
    assert_eq!(first.timing().duration().as_nanos(), 16_666_666);
    assert!(first.timing().flags().contains(MediaFlags::DISCONTINUITY));
    assert_eq!(second.sample_count(), 801);
    assert_eq!(
        second.timing().presentation_timestamp().as_nanos(),
        16_666_666
    );
    assert!(second.timing().flags().contains(MediaFlags::CORRUPTED));
}

#[test]
fn signal_loss_advances_the_source_and_marks_recovery() {
    let faults = FaultSchedule::default().signal_loss_at(1);
    let mut video = video_source(SourcePattern::Bars);
    video.set_faults(faults.clone());

    let before = video.next_event().unwrap();
    let lost = video.next_event().unwrap();
    let recovered = video.next_event().unwrap();
    assert!(matches!(before, SourceEvent::Frame(_)));
    assert!(lost.is_signal_lost());
    assert_eq!(lost.signal_loss_timing().unwrap().sequence().get(), 1);
    let SourceEvent::Frame(recovered) = recovered else {
        panic!("sequence 2 should recover");
    };
    assert_eq!(recovered.timing().sequence().get(), 2);
    assert!(
        recovered
            .timing()
            .flags()
            .contains(MediaFlags::DISCONTINUITY)
    );

    let mut audio = silent_audio();
    audio.set_faults(faults);
    assert!(audio.next_block().unwrap().is_some());
    assert!(audio.next_block().unwrap().is_none());
    let recovered = audio.next_block().unwrap().unwrap();
    assert_eq!(audio.cumulative_samples(), 2_402);
    assert_eq!(recovered.timing().sequence().get(), 2);
    assert!(
        recovered
            .timing()
            .flags()
            .contains(MediaFlags::DISCONTINUITY)
    );
}

#[test]
fn collecting_sinks_enforce_bounds_and_report_each_policy() {
    let mut oldest = CollectingSink::new(2, OverflowPolicy::DropOldest).unwrap();
    oldest.collect(1_u8).unwrap();
    oldest.collect(2).unwrap();
    assert_eq!(oldest.collect(3), Ok(CollectOutcome::DroppedOldest(1)));
    assert_eq!(oldest.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
    assert_eq!(oldest.telemetry().received(), 3);
    assert_eq!(oldest.telemetry().accepted(), 3);
    assert_eq!(oldest.telemetry().dropped_oldest(), 1);
    assert_eq!(oldest.telemetry().high_watermark(), 2);

    let mut newest = CollectingSink::new(1, OverflowPolicy::DropNewest).unwrap();
    newest.collect(1_u8).unwrap();
    assert_eq!(newest.collect(2), Ok(CollectOutcome::DroppedNewest(2)));
    assert_eq!(newest.iter().copied().collect::<Vec<_>>(), vec![1]);
    assert_eq!(newest.telemetry().dropped_newest(), 1);

    let mut reject = CollectingSink::new(1, OverflowPolicy::Reject).unwrap();
    reject.collect(1_u8).unwrap();
    assert_eq!(reject.collect(2), Err(CollectError::Full(2)));
    assert_eq!(reject.telemetry().rejected(), 1);

    let mut video: CollectingVideoSink =
        CollectingVideoSink::new(1, OverflowPolicy::Reject).unwrap();
    video
        .collect(
            video_source(SourcePattern::Bars)
                .next_frame()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    let mut audio: CollectingAudioSink =
        CollectingAudioSink::new(1, OverflowPolicy::Reject).unwrap();
    audio
        .collect(silent_audio().next_block().unwrap().unwrap())
        .unwrap();
    assert_eq!((video.len(), audio.len()), (1, 1));
}

#[test]
fn reset_reproduces_video_hashes_and_audio_samples() {
    let mut video = video_source(SourcePattern::Bars);
    let first = video.next_frame().unwrap().unwrap();
    let first_hash = video_frame_hash(&first);
    assert_eq!(first_hash, 727_902_077_449_732_892);
    video.next_frame().unwrap();
    video.restart();
    assert_eq!(
        video_frame_hash(&video.next_frame().unwrap().unwrap()),
        first_hash
    );

    let mut audio = SimulatedAudioSource::new(
        ntsc_60(),
        ChannelLayout::stereo(),
        clock(),
        AudioPattern::Sine {
            frequency_hz: 1_000.0,
            gain: Gain::UNITY,
        },
    )
    .unwrap();
    let first = audio.next_block().unwrap().unwrap();
    assert_eq!(first.sample(0, 0), Some(0.0));
    assert_eq!(first.sample(0, 1), Some(0.130_526_2));
    let first_hash = audio_block_hash(&first);
    audio.next_block().unwrap();
    audio.reset();
    let repeated = audio.next_block().unwrap().unwrap();
    assert_eq!(repeated.planes(), first.planes());
    assert_eq!(audio_block_hash(&repeated), first_hash);
}
