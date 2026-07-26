use std::num::NonZeroU128;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fm_codec_ffmpeg::record::{
    CleanupStatus, EnqueueRejection, OutputFinalization, PairedFrame, RecordConfig, RecordFormat,
    Recorder, StopOutcome,
};
use fm_frame::{
    AudioBlock, ChannelLayout, ClockDomainId, MediaTimestamp, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SampleRate, SequenceNumber, TimeBase,
};
use fm_types::FrameRate;
use tempfile::tempdir;

const FRAME_COUNT: u64 = 90;

fn tools_available() -> bool {
    let available = ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    assert!(
        available || std::env::var("FM_REQUIRE_FFMPEG").as_deref() != Ok("1"),
        "FM_REQUIRE_FFMPEG=1 but ffmpeg or ffprobe is unavailable"
    );
    available
}

fn format() -> RecordFormat {
    RecordFormat::new(
        64,
        48,
        FrameRate::new(30, 1).unwrap(),
        SampleRate::new(48_000).unwrap(),
        ChannelLayout::stereo(),
        SequenceNumber::new(100),
    )
    .unwrap()
}

fn frame(format: &RecordFormat, offset: u64) -> PairedFrame {
    let sequence = SequenceNumber::new(format.first_sequence().get() + offset);
    let absolute_start_sample = sequence.get() * 1_600;
    let absolute_start_nanos = sequence.get() * 1_000_000_000 / 30;
    let absolute_end_nanos = (sequence.get() + 1) * 1_000_000_000 / 30;
    let timing = MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(absolute_start_sample).unwrap()),
            TimeBase::new(1, 48_000).unwrap(),
        ),
        NormalizedTimestamp::from_nanos(i64::try_from(absolute_start_nanos).unwrap()),
        NormalizedDuration::from_nanos(absolute_end_nanos - absolute_start_nanos).unwrap(),
        ClockDomainId::new(NonZeroU128::new(7).unwrap()),
        sequence,
    )
    .unwrap();
    let sample_count = 1_600;
    let left = vec![0.1; sample_count];
    let right = vec![-0.1; sample_count];
    let audio = AudioBlock::new(
        timing,
        format.sample_rate(),
        format.channel_layout().clone(),
        vec![left, right],
    )
    .unwrap();
    let mut rgba = vec![0_u8; format.rgba_bytes_per_frame()];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[
            u8::try_from(offset % 255).unwrap(),
            80,
            u8::try_from((offset * 3) % 255).unwrap(),
            255,
        ]);
    }
    PairedFrame::new(format, sequence, rgba, audio).unwrap()
}

fn enqueue_all(recorder: &mut Recorder, format: &RecordFormat) {
    for offset in 0..FRAME_COUNT {
        let mut pending = frame(format, offset);
        loop {
            match recorder.enqueue(pending) {
                Ok(()) => break,
                Err(error) if error.reason == EnqueueRejection::QueueFull => {
                    pending = error.into_frame();
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!(
                    "unexpected enqueue rejection: {:?}; telemetry: {:?}",
                    error.reason,
                    recorder.telemetry()
                ),
            }
        }
    }
}

fn probe(path: &std::path::Path) -> serde_json::Value {
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-count_packets",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    serde_json::from_slice(&probe.stdout).unwrap()
}

fn numeric_string(value: &serde_json::Value, field: &str) -> f64 {
    value[field].as_str().unwrap().parse().unwrap()
}

fn count(value: &serde_json::Value, field: &str) -> u64 {
    value[field].as_str().unwrap().parse().unwrap()
}

fn assert_probe_and_decode(path: &std::path::Path, minimum_video_frames: u64) {
    let probe = probe(path);
    let minimum_seconds = f64::from(u32::try_from(minimum_video_frames).unwrap()) / 30.0;
    let streams = probe["streams"].as_array().unwrap();
    let video = streams
        .iter()
        .find(|stream| stream["codec_type"] == "video")
        .unwrap();
    let audio = streams
        .iter()
        .find(|stream| stream["codec_type"] == "audio")
        .unwrap();
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 64);
    assert_eq!(video["height"], 48);
    assert_eq!(video["pix_fmt"], "yuv420p");
    assert_eq!(video["r_frame_rate"], "30/1");
    assert_eq!(video["avg_frame_rate"], "30/1");
    assert!((0.0..=0.1).contains(&numeric_string(video, "start_time")));
    assert!(numeric_string(video, "duration") >= minimum_seconds);
    assert!(count(video, "nb_read_frames") >= minimum_video_frames);
    assert!(count(video, "nb_read_packets") >= minimum_video_frames);

    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");
    assert_eq!(audio["channels"], 2);
    assert_eq!(audio["channel_layout"], "stereo");
    assert!(numeric_string(audio, "start_time").abs() < 0.05);
    assert!(numeric_string(audio, "duration") >= minimum_seconds);
    assert!(count(audio, "nb_read_frames") > 0);
    assert!(count(audio, "nb_read_packets") > 0);
    assert!(numeric_string(&probe["format"], "duration") >= minimum_seconds);

    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .unwrap();
    assert!(
        decode.status.success(),
        "decode failed: {}",
        String::from_utf8_lossy(&decode.stderr)
    );

    let decoded_audio = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        decoded_audio.status.success(),
        "audio decode failed: {}",
        String::from_utf8_lossy(&decoded_audio.stderr)
    );
    assert_eq!(decoded_audio.stdout.len() % (2 * size_of::<f32>()), 0);
    let decoded_sample_frames = decoded_audio.stdout.len() / (2 * size_of::<f32>());
    assert!(
        decoded_sample_frames >= usize::try_from(minimum_video_frames).unwrap() * 1_500,
        "only {decoded_sample_frames} decoded AAC sample frames"
    );
}

#[test]
fn safe_stop_produces_playable_fragmented_h264_aac_mp4() {
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let path = directory.path().join("recording.mp4");
    let output = std::fs::File::create(&path).unwrap();
    let format = format();
    let mut recorder = Recorder::start(output, RecordConfig::new(format.clone())).unwrap();
    enqueue_all(&mut recorder, &format);
    let report = recorder.stop();
    assert_eq!(report.outcome, StopOutcome::Clean, "{report:?}");
    assert_eq!(report.output, OutputFinalization::Synced);
    assert_eq!(report.cleanup, CleanupStatus::Complete);
    assert_eq!(report.telemetry.completed_pairs, FRAME_COUNT);
    assert_eq!(recorder.stop(), report);
    assert_eq!(recorder.telemetry(), report.telemetry);
    let probe = probe(&path);
    let video = probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["codec_type"] == "video")
        .unwrap();
    assert_eq!(count(video, "nb_read_frames"), FRAME_COUNT);
    assert_eq!(count(video, "nb_read_packets"), FRAME_COUNT);
    assert_probe_and_decode(&path, FRAME_COUNT);
}

#[test]
fn forced_child_kill_leaves_a_playable_fragmented_prefix() {
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let path = directory.path().join("killed-prefix.mp4");
    let output = std::fs::File::create(&path).unwrap();
    let format = format();
    let mut recorder = Recorder::start(output, RecordConfig::new(format.clone())).unwrap();
    enqueue_all(&mut recorder, &format);
    let deadline = Instant::now() + Duration::from_secs(5);
    while recorder.telemetry().completed_pairs < FRAME_COUNT && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(recorder.telemetry().completed_pairs, FRAME_COUNT);
    thread::sleep(Duration::from_millis(100));
    let report = recorder.cancel();
    assert_eq!(report.outcome, StopOutcome::Killed, "{report:?}");
    assert_eq!(report.output, OutputFinalization::Synced, "{report:?}");
    assert_eq!(report.cleanup, CleanupStatus::Complete, "{report:?}");
    assert_eq!(recorder.cancel(), report);
    assert_eq!(recorder.telemetry(), report.telemetry);
    assert_probe_and_decode(&path, 30);
}

#[test]
fn idle_safe_stop_has_no_second_input_timeout() {
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let path = directory.path().join("idle.mp4");
    let output = std::fs::File::create(path).unwrap();
    let mut recorder = Recorder::start(output, RecordConfig::new(format())).unwrap();
    let report = recorder.stop();
    assert_eq!(report.outcome, StopOutcome::Clean, "{report:?}");
    assert_eq!(report.output, OutputFinalization::Synced, "{report:?}");
    assert_eq!(report.cleanup, CleanupStatus::Complete, "{report:?}");
    assert_eq!(report.telemetry.accepted_pairs, 0);
    assert_eq!(report.telemetry.completed_pairs, 0);
    assert_eq!(report.telemetry.failure, None);
}
