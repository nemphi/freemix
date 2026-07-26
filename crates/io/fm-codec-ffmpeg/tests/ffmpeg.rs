use std::num::{NonZeroU32, NonZeroU128};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fm_codec_ffmpeg::{
    Adapter, Config, DecodeRequest, Error, Executable, LimitKind, SequenceRequest, StreamSelector,
    ToolAvailability,
};
use fm_frame::{AudioBlock, Channel, ClockDomainId, CpuVideoFrame};
use tempfile::tempdir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
static FFMPEG_TEST_LOCK: Mutex<()> = Mutex::new(());

fn ffmpeg_test_guard() -> MutexGuard<'static, ()> {
    FFMPEG_TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn require_ffmpeg() -> Option<Adapter> {
    let directory = tempdir().expect("temporary discovery root");
    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .expect("valid adapter configuration");
    let capabilities = adapter.capabilities();
    let available = matches!(capabilities.ffmpeg, ToolAvailability::Available { .. })
        && matches!(capabilities.ffprobe, ToolAvailability::Available { .. });
    if available {
        Some(Adapter::new(Config::default()).expect("valid adapter configuration"))
    } else if std::env::var("FM_REQUIRE_FFMPEG").as_deref() == Ok("1") {
        panic!("FM_REQUIRE_FFMPEG=1 but FFmpeg tools are unavailable: {capabilities:?}");
    } else {
        eprintln!("skipping FFmpeg integration: tools unavailable: {capabilities:?}");
        None
    }
}

fn generate_asset(path: &std::path::Path) {
    let args = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=64x48:rate=5:duration=1",
        "-f",
        "lavfi",
        "-i",
        "aevalsrc=0.30*sin(2*PI*440*t)|0.08*sin(2*PI*660*t):s=48000:d=1",
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-c:v",
        "ffv1",
        "-pix_fmt",
        "yuv420p",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-colorspace",
        "bt709",
        "-c:a",
        "pcm_s16le",
        "-channel_layout",
        "stereo",
        "-f",
        "nut",
        "-y",
    ];
    let mut child = Command::new("ffmpeg")
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("LC_ALL", "C")
        .spawn()
        .expect("spawn FFmpeg asset generator");
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll FFmpeg generator") {
            assert!(status.success(), "FFmpeg asset generation failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let kill_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < kill_deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("FFmpeg asset generation timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn sequence_request(clock_domain: ClockDomainId) -> DecodeRequest {
    let sequence = SequenceRequest {
        selector: StreamSelector::Best,
        count: NonZeroU32::new(3).unwrap(),
    };
    DecodeRequest {
        clock_domain,
        video: Some(sequence),
        audio: Some(sequence),
    }
}

fn assert_audio(blocks: &[AudioBlock], clock_domain: ClockDomainId) {
    let expected_starts = [0, 21_333_333, 42_666_666];
    let expected_durations = [21_333_333, 21_333_333, 21_333_334];
    for (sequence, block) in blocks.iter().enumerate() {
        assert_eq!(block.sample_rate().hertz(), 48_000);
        assert_eq!(
            block.channel_layout().channels(),
            &[Channel::Left, Channel::Right]
        );
        assert_eq!(block.sample_count(), 1024);
        assert_eq!(block.planes().len(), 2);
        assert_eq!(block.plane(0).unwrap().len(), 1024);
        assert_eq!(block.plane(1).unwrap().len(), 1024);
        assert_eq!(block.timing().clock_domain(), clock_domain);
        assert_eq!(block.timing().sequence().get(), sequence as u64);
        assert_eq!(
            block.timing().original_timestamp().timestamp().ticks(),
            i64::try_from(sequence).unwrap() * 1024
        );
        assert_eq!(
            block.timing().presentation_timestamp().as_nanos(),
            expected_starts[sequence]
        );
        assert_eq!(
            block.timing().duration().as_nanos(),
            expected_durations[sequence]
        );
    }
    for pair in blocks.windows(2) {
        assert_eq!(
            pair[0].timing().presentation_timestamp().as_nanos()
                + i64::try_from(pair[0].timing().duration().as_nanos()).unwrap(),
            pair[1].timing().presentation_timestamp().as_nanos()
        );
    }
    assert_eq!(
        blocks[2].timing().presentation_timestamp().as_nanos()
            + i64::try_from(blocks[2].timing().duration().as_nanos()).unwrap(),
        64_000_000
    );
    assert!(
        blocks[0]
            .plane(0)
            .unwrap()
            .iter()
            .any(|sample| sample.abs() > 0.20)
    );
    assert!(
        blocks[0]
            .plane(1)
            .unwrap()
            .iter()
            .all(|sample| sample.abs() < 0.10)
    );
    assert_ne!(blocks[0].plane(0), blocks[0].plane(1));
}

fn video_request(clock_domain: ClockDomainId, count: u32) -> DecodeRequest {
    DecodeRequest {
        clock_domain,
        video: Some(SequenceRequest {
            selector: StreamSelector::Best,
            count: NonZeroU32::new(count).unwrap(),
        }),
        audio: None,
    }
}

fn audio_request(clock_domain: ClockDomainId, count: u32) -> DecodeRequest {
    DecodeRequest {
        clock_domain,
        video: None,
        audio: Some(SequenceRequest {
            selector: StreamSelector::Best,
            count: NonZeroU32::new(count).unwrap(),
        }),
    }
}

fn assert_same_video(left: &[CpuVideoFrame], right: &[CpuVideoFrame]) {
    assert_eq!(left.len(), right.len());
    for (left, right) in left.iter().zip(right) {
        assert_eq!(left, right);
    }
}

fn assert_same_audio(left: &[AudioBlock], right: &[AudioBlock]) {
    assert_eq!(left.len(), right.len());
    for (left, right) in left.iter().zip(right) {
        assert_eq!(left, right);
    }
}

fn assert_audio_cursor_limit_is_per_page(
    path: &std::path::Path,
    root: &std::path::Path,
    clock: ClockDomainId,
    limits: fm_codec_ffmpeg::Limits,
    expected_kind: LimitKind,
    expected_actual: u64,
    expected_maximum: u64,
) {
    let adapter = Adapter::new(Config {
        allowed_root: Some(root.to_owned()),
        limits,
        ..Config::default()
    })
    .unwrap();
    let mut cursor = adapter
        .open_local_audio(path, clock, StreamSelector::Best)
        .unwrap();
    let first = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    let second = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    assert_eq!(first.blocks[0].timing().sequence().get(), 0);
    assert_eq!(second.blocks[0].timing().sequence().get(), 2);

    let cumulative_blocks = first.blocks.len() + second.blocks.len();
    let cumulative_samples = first
        .blocks
        .iter()
        .chain(&second.blocks)
        .map(AudioBlock::sample_count)
        .sum::<usize>();
    let cumulative_bytes = cumulative_samples * 2 * size_of::<f32>();
    match expected_kind {
        LimitKind::AudioBlocks => {
            assert!(u64::try_from(cumulative_blocks).unwrap() > expected_maximum);
        }
        LimitKind::AudioSamples => {
            assert!(u64::try_from(cumulative_samples).unwrap() > expected_maximum);
        }
        LimitKind::DecodedBytes => {
            assert!(u64::try_from(cumulative_bytes).unwrap() > expected_maximum);
        }
        _ => panic!("unexpected audio page limit kind"),
    }

    assert_eq!(
        cursor.decode_up_to_bounded(NonZeroU32::new(3).unwrap(), usize::MAX, usize::MAX),
        Err(Error::LimitExceeded {
            kind: expected_kind,
            actual: expected_actual,
            maximum: expected_maximum,
        })
    );
    let recovered = cursor.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    assert_eq!(recovered.blocks[0].timing().sequence().get(), 4);
}

#[test]
fn probes_and_decodes_three_video_frames_and_audio_blocks() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("safe; literal asset.nut");
    generate_asset(&path);
    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let probe = adapter.probe_local(&path).unwrap();
    assert_eq!(probe.format.name, "nut");
    let video_stream = probe.select_video(StreamSelector::Best).unwrap();
    assert_eq!(video_stream.index, 0);
    assert_eq!(video_stream.codec_name.as_deref(), Some("ffv1"));
    assert_eq!(
        (video_stream.width, video_stream.height),
        (Some(64), Some(48))
    );
    let audio_stream = probe.select_audio(StreamSelector::Best).unwrap();
    assert_eq!(audio_stream.index, 1);
    assert_eq!(audio_stream.codec_name.as_deref(), Some("pcm_s16le"));
    assert_eq!(
        (audio_stream.sample_rate, audio_stream.channels),
        (Some(48_000), Some(2))
    );

    let clock_domain = ClockDomainId::new(NonZeroU128::new(44).unwrap());
    let decoded = adapter
        .decode_local(&path, sequence_request(clock_domain))
        .unwrap();
    assert_eq!(decoded.video.len(), 3);
    assert_eq!(decoded.audio.len(), 3);

    for (sequence, frame) in decoded.video.iter().enumerate() {
        assert_eq!(frame.payload().dimensions().width(), 64);
        assert_eq!(frame.payload().dimensions().height(), 48);
        assert_eq!(frame.payload().plane(0).unwrap().stride(), 64 * 4);
        assert_eq!(frame.payload().plane(0).unwrap().bytes().len(), 64 * 48 * 4);
        assert_eq!(frame.timing().clock_domain(), clock_domain);
        assert_eq!(frame.timing().sequence().get(), sequence as u64);
        assert_eq!(
            frame.timing().presentation_timestamp().as_nanos(),
            i64::try_from(sequence).unwrap() * 200_000_000
        );
        assert_eq!(frame.timing().duration().as_nanos(), 200_000_000);
        // FFV1-in-NUT does not retain these source tags in FFmpeg 8.1.2.
        assert_eq!(frame.metadata(), None);
    }
    assert_ne!(
        decoded.video[0].payload().plane(0).unwrap().bytes(),
        decoded.video[1].payload().plane(0).unwrap().bytes()
    );

    assert_audio(&decoded.audio, clock_domain);
}

#[test]
fn decodes_nonempty_prefixes_through_end_of_stream_without_weakening_exact_counts() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("short-prefix.nut");
    generate_asset(&path);
    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let clock_domain = ClockDomainId::new(NonZeroU128::new(46).unwrap());

    let video_request = DecodeRequest {
        clock_domain,
        video: Some(SequenceRequest {
            selector: StreamSelector::Best,
            count: NonZeroU32::new(8).unwrap(),
        }),
        audio: None,
    };
    let video = adapter
        .decode_local_up_to(&path, video_request)
        .expect("decode all available video frames");
    assert_eq!(video.video.len(), 5);
    assert!(video.audio.is_empty());
    assert_eq!(
        adapter.decode_local(&path, video_request),
        Err(Error::MissingFrames)
    );

    let bounded = adapter
        .decode_local_up_to(
            &path,
            DecodeRequest {
                video: Some(SequenceRequest {
                    selector: StreamSelector::Best,
                    count: NonZeroU32::new(3).unwrap(),
                }),
                ..video_request
            },
        )
        .expect("honor prefix maximum before end of stream");
    assert_eq!(bounded.video.len(), 3);

    let audio = adapter
        .decode_local_up_to(
            &path,
            DecodeRequest {
                clock_domain,
                video: None,
                audio: Some(SequenceRequest {
                    selector: StreamSelector::Best,
                    count: NonZeroU32::new(60).unwrap(),
                }),
            },
        )
        .expect("decode all available audio blocks");
    assert!(audio.video.is_empty());
    assert!(!audio.audio.is_empty());
    assert!(audio.audio.len() <= 60);
    assert_eq!(
        audio
            .audio
            .iter()
            .map(AudioBlock::sample_count)
            .sum::<usize>(),
        48_000
    );
    for pair in audio.audio.windows(2) {
        assert_eq!(
            pair[0].timing().presentation_timestamp().as_nanos()
                + i64::try_from(pair[0].timing().duration().as_nanos()).unwrap(),
            pair[1].timing().presentation_timestamp().as_nanos()
        );
    }
}

#[test]
fn enforces_input_output_request_and_selector_bounds() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("bounds.nut");
    generate_asset(&path);

    let limits = fm_codec_ffmpeg::Limits {
        max_input_bytes: 1,
        ..fm_codec_ffmpeg::Limits::default()
    };
    let input_adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        limits,
        ..Config::default()
    })
    .unwrap();
    assert!(matches!(
        input_adapter.probe_local(&path),
        Err(Error::LimitExceeded {
            kind: LimitKind::InputBytes,
            ..
        })
    ));

    let limits = fm_codec_ffmpeg::Limits {
        max_total_decoded_bytes: 100,
        ..fm_codec_ffmpeg::Limits::default()
    };
    let output_adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        limits,
        ..Config::default()
    })
    .unwrap();
    let clock = ClockDomainId::new(NonZeroU128::new(45).unwrap());
    assert!(matches!(
        output_adapter.decode_local(&path, sequence_request(clock)),
        Err(Error::LimitExceeded {
            kind: LimitKind::DecodedBytes,
            ..
        })
    ));

    let limits = fm_codec_ffmpeg::Limits {
        max_video_frames: 2,
        ..fm_codec_ffmpeg::Limits::default()
    };
    let request_adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        limits,
        ..Config::default()
    })
    .unwrap();
    assert!(matches!(
        request_adapter.decode_local(&path, sequence_request(clock)),
        Err(Error::LimitExceeded {
            kind: LimitKind::VideoFrames,
            ..
        })
    ));

    let adapter = Adapter::new(Config {
        ffmpeg: Executable::SearchPath,
        ffprobe: Executable::SearchPath,
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let invalid = DecodeRequest {
        clock_domain: clock,
        video: Some(SequenceRequest {
            selector: StreamSelector::Index(99),
            count: NonZeroU32::new(1).unwrap(),
        }),
        audio: None,
    };
    assert_eq!(
        adapter.decode_local(&path, invalid),
        Err(Error::InvalidSelector)
    );
}

#[test]
fn sequential_video_windows_match_leading_decode_and_have_sticky_eos() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("cursor-pages.nut");
    generate_asset(&path);
    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let clock = ClockDomainId::new(NonZeroU128::new(47).unwrap());
    let leading = adapter
        .decode_local(&path, video_request(clock, 5))
        .unwrap();

    let mut cursor = adapter
        .open_local_video(&path, clock, StreamSelector::Best)
        .unwrap();
    let first = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    let second = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    let final_page = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    assert!(!first.end_of_stream);
    assert!(!second.end_of_stream);
    assert!(final_page.end_of_stream);
    assert_eq!(final_page.frames.len(), 1);

    let mut pages = first.frames;
    pages.extend(second.frames);
    pages.extend(final_page.frames);
    assert_same_video(&pages, &leading.video);
    assert_eq!(
        pages
            .iter()
            .map(|frame| frame.timing().sequence().get())
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4]
    );

    std::fs::remove_file(&path).unwrap();
    let sticky = cursor.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    assert!(sticky.frames.is_empty());
    assert!(sticky.end_of_stream);
}

#[test]
fn video_cursor_limits_apply_per_page_and_fail_transactionally() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("cursor-bounds.nut");
    generate_asset(&path);
    let clock = ClockDomainId::new(NonZeroU128::new(48).unwrap());

    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let mut full = adapter
        .open_local_video(&path, clock, StreamSelector::Best)
        .unwrap();
    let full_page = full.decode_up_to(NonZeroU32::new(5).unwrap()).unwrap();
    assert_eq!(full_page.frames.len(), 5);
    assert!(full_page.end_of_stream);

    let bounded = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        limits: fm_codec_ffmpeg::Limits {
            max_video_frames: 2,
            ..fm_codec_ffmpeg::Limits::default()
        },
        ..Config::default()
    })
    .unwrap();
    let mut cursor = bounded
        .open_local_video(&path, clock, StreamSelector::Best)
        .unwrap();
    let first = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    let second = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    assert_eq!(first.frames[0].timing().sequence().get(), 0);
    assert_eq!(second.frames[0].timing().sequence().get(), 2);
    assert!(first.frames.len() + second.frames.len() > 2);
    assert_eq!(
        cursor.decode_up_to(NonZeroU32::new(3).unwrap()),
        Err(Error::LimitExceeded {
            kind: LimitKind::VideoFrames,
            actual: 3,
            maximum: 2,
        })
    );
    let recovered = cursor.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    assert_eq!(recovered.frames[0].timing().sequence().get(), 4);

    let frame_bytes = 64 * 48 * 4;
    let byte_bounded = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        limits: fm_codec_ffmpeg::Limits {
            max_total_decoded_bytes: frame_bytes,
            ..fm_codec_ffmpeg::Limits::default()
        },
        ..Config::default()
    })
    .unwrap();
    let mut cursor = byte_bounded
        .open_local_video(&path, clock, StreamSelector::Best)
        .unwrap();
    let first = cursor.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    let second = cursor.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    let cumulative_bytes = first
        .frames
        .iter()
        .chain(&second.frames)
        .map(|frame| frame.payload().plane(0).unwrap().bytes().len())
        .sum::<usize>();
    assert!(cumulative_bytes > frame_bytes);
    assert_eq!(
        cursor.decode_up_to(NonZeroU32::new(2).unwrap()),
        Err(Error::LimitExceeded {
            kind: LimitKind::DecodedBytes,
            actual: u64::try_from(2 * frame_bytes).unwrap(),
            maximum: u64::try_from(frame_bytes).unwrap(),
        })
    );
    let recovered = cursor.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    assert_eq!(recovered.frames[0].timing().sequence().get(), 2);
}

#[test]
fn sequential_audio_windows_match_leading_decode_and_keep_global_timing() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("audio-cursor-pages.nut");
    generate_asset(&path);
    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let clock = ClockDomainId::new(NonZeroU128::new(49).unwrap());
    let leading = adapter
        .decode_local(&path, audio_request(clock, 5))
        .unwrap();

    let mut cursor = adapter
        .open_local_audio(&path, clock, StreamSelector::Best)
        .unwrap();
    let first = cursor.decode_up_to(NonZeroU32::new(2).unwrap()).unwrap();
    let second = cursor.decode_up_to(NonZeroU32::new(3).unwrap()).unwrap();
    assert!(!first.end_of_stream);
    assert!(!second.end_of_stream);

    let mut pages = first.blocks;
    pages.extend(second.blocks);
    assert_same_audio(&pages, &leading.audio);
    assert_eq!(
        pages
            .iter()
            .map(|block| block.timing().sequence().get())
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4]
    );
    for pair in pages.windows(2) {
        assert_eq!(
            pair[0].timing().presentation_timestamp().as_nanos()
                + i64::try_from(pair[0].timing().duration().as_nanos()).unwrap(),
            pair[1].timing().presentation_timestamp().as_nanos()
        );
    }
}

#[test]
fn bounded_audio_window_rejects_before_decode_without_advancing() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("audio-page-bounds.nut");
    generate_asset(&path);
    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let clock = ClockDomainId::new(NonZeroU128::new(52).unwrap());
    let mut cursor = adapter
        .open_local_audio(&path, clock, StreamSelector::Best)
        .unwrap();

    assert!(matches!(
        cursor.decode_up_to_bounded(NonZeroU32::new(2).unwrap(), 1_024, 8_192),
        Err(Error::LimitExceeded {
            kind: LimitKind::AudioSamples,
            actual: 2_048,
            maximum: 1_024,
        })
    ));
    assert!(matches!(
        cursor.decode_up_to_bounded(NonZeroU32::new(1).unwrap(), 1_024, 8_191),
        Err(Error::LimitExceeded {
            kind: LimitKind::DecodedBytes,
            actual: 8_192,
            maximum: 8_191,
        })
    ));
    let recovered = cursor
        .decode_up_to_bounded(NonZeroU32::new(1).unwrap(), 1_024, 8_192)
        .unwrap();
    assert_eq!(recovered.blocks.len(), 1);
    assert_eq!(recovered.blocks[0].timing().sequence().get(), 0);
}

#[test]
fn audio_full_final_eos_is_sticky() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("audio-cursor-bounds.nut");
    generate_asset(&path);
    let clock = ClockDomainId::new(NonZeroU128::new(50).unwrap());

    let adapter = Adapter::new(Config {
        allowed_root: Some(directory.path().to_owned()),
        ..Config::default()
    })
    .unwrap();
    let all_audio = adapter
        .decode_local_up_to(&path, audio_request(clock, 60))
        .unwrap()
        .audio;
    let full_count = u32::try_from(all_audio.len()).unwrap();
    let mut full = adapter
        .open_local_audio(&path, clock, StreamSelector::Best)
        .unwrap();
    let full_page = full
        .decode_up_to(NonZeroU32::new(full_count).unwrap())
        .unwrap();
    assert!(full_page.end_of_stream);
    assert_same_audio(&full_page.blocks, &all_audio);
    assert_eq!(
        full_page
            .blocks
            .iter()
            .map(AudioBlock::sample_count)
            .sum::<usize>(),
        48_000
    );

    std::fs::remove_file(&path).unwrap();
    let sticky = full.decode_up_to(NonZeroU32::new(1).unwrap()).unwrap();
    assert!(sticky.blocks.is_empty());
    assert!(sticky.end_of_stream);
}

#[test]
fn audio_cursor_limits_apply_per_page_and_fail_transactionally() {
    let _guard = ffmpeg_test_guard();
    let Some(_) = require_ffmpeg() else {
        return;
    };
    let directory = tempdir().unwrap();
    let path = directory.path().join("audio-cursor-bounds.nut");
    generate_asset(&path);
    let clock = ClockDomainId::new(NonZeroU128::new(51).unwrap());

    assert_audio_cursor_limit_is_per_page(
        &path,
        directory.path(),
        clock,
        fm_codec_ffmpeg::Limits {
            max_audio_blocks: 2,
            ..fm_codec_ffmpeg::Limits::default()
        },
        LimitKind::AudioBlocks,
        3,
        2,
    );
    assert_audio_cursor_limit_is_per_page(
        &path,
        directory.path(),
        clock,
        fm_codec_ffmpeg::Limits {
            max_audio_samples: 2 * 1024,
            ..fm_codec_ffmpeg::Limits::default()
        },
        LimitKind::AudioSamples,
        3 * 1024,
        2 * 1024,
    );
    assert_audio_cursor_limit_is_per_page(
        &path,
        directory.path(),
        clock,
        fm_codec_ffmpeg::Limits {
            max_total_decoded_bytes: 2 * 1024 * 2 * size_of::<f32>(),
            ..fm_codec_ffmpeg::Limits::default()
        },
        LimitKind::DecodedBytes,
        u64::try_from(3 * 1024 * 2 * size_of::<f32>()).unwrap(),
        u64::try_from(2 * 1024 * 2 * size_of::<f32>()).unwrap(),
    );
}
