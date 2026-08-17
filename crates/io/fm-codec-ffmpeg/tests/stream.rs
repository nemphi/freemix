//! Live RTMP sink integration coverage.
//!
//! # Receiver path
//!
//! This box's `ffmpeg` (6.1.1, Ubuntu) supports RTMP listen mode, verified by
//! hand before these tests were written: `ffmpeg -listen 1 -i rtmp://127.0.0.1:PORT/live/key
//! -c copy -y out.flv` accepts a publish from a second `ffmpeg` and writes a
//! playable FLV. The end-to-end test therefore uses a real `ffmpeg` RTMP
//! receiver and reprobes the received file with `ffprobe`, rather than the
//! `tcp://` + `TcpListener` fallback the task allowed for builds without
//! listen mode.
//!
//! Backpressure and connect-deadline coverage instead points the sink at a
//! plain `TcpListener` that accepts the connection and never answers the RTMP
//! handshake. Measured behaviour of this `ffmpeg` build in that situation is to
//! block forever with no muxer progress, which is precisely the stall the
//! sink's own connect deadline exists to bound.

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::num::NonZeroU128;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use fm_codec_ffmpeg::stream::{
    ChildState, CleanupStatus, EnqueueRejection, OverflowPolicy, PairedFrame, RecordFormat,
    StopOutcome, StreamConfig, StreamDestination, StreamFailure, StreamLimits, StreamTelemetry,
    Streamer,
};
use fm_frame::{
    AudioBlock, ChannelLayout, ClockDomainId, MediaTimestamp, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SampleRate, SequenceNumber, TimeBase,
};
use fm_types::FrameRate;
use tempfile::tempdir;

const SAMPLES_PER_FRAME: u64 = 1_600;
const FIRST_SEQUENCE: u64 = 100;
const E2E_FRAMES: u64 = 45;
const MAX_PAIRS: usize = 3;
const MAX_BYTES: usize = 8 * 1024 * 1024;

static SERIAL: Mutex<()> = Mutex::new(());

/// `FFmpeg` children are CPU heavy on a 4-core box; run one scenario at a time.
fn serialize() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

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

fn format(width: u32, height: u32) -> RecordFormat {
    RecordFormat::new(
        width,
        height,
        FrameRate::new(30, 1).unwrap(),
        SampleRate::new(48_000).unwrap(),
        ChannelLayout::stereo(),
        SequenceNumber::new(FIRST_SEQUENCE),
    )
    .unwrap()
}

fn frame(format: &RecordFormat, offset: u64) -> PairedFrame {
    let sequence = SequenceNumber::new(format.first_sequence().get() + offset);
    let start_sample = sequence.get() * SAMPLES_PER_FRAME;
    let start_nanos = sequence.get() * 1_000_000_000 / 30;
    let end_nanos = (sequence.get() + 1) * 1_000_000_000 / 30;
    let timing = MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(start_sample).unwrap()),
            TimeBase::new(1, 48_000).unwrap(),
        ),
        NormalizedTimestamp::from_nanos(i64::try_from(start_nanos).unwrap()),
        NormalizedDuration::from_nanos(end_nanos - start_nanos).unwrap(),
        ClockDomainId::new(NonZeroU128::new(7).unwrap()),
        sequence,
    )
    .unwrap();
    // A per-frame sawtooth: real, finite, non-silent audio without lossy casts.
    let tone = (0..SAMPLES_PER_FRAME)
        .map(|index| {
            let step = (index + offset * 37) % 400;
            f32::from(u16::try_from(step).unwrap()) / 200.0 - 1.0
        })
        .collect::<Vec<_>>();
    let audio = AudioBlock::new(
        timing,
        format.sample_rate(),
        format.channel_layout().clone(),
        vec![tone.clone(), tone],
    )
    .unwrap();
    let mut rgba = vec![0_u8; format.rgba_bytes_per_frame()];
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        pixel.copy_from_slice(&[
            u8::try_from((offset * 7 + u64::try_from(index).unwrap() / 16) % 255).unwrap(),
            u8::try_from(index % 251).unwrap(),
            u8::try_from((offset * 3) % 255).unwrap(),
            255,
        ]);
    }
    PairedFrame::new(format, sequence, rgba, audio).unwrap()
}

fn unique_key(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("fmkey_{label}_{}_{nanos}", std::process::id())
}

fn free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Waits for a listening socket without connecting to it: an RTMP receiver
/// started with `-listen 1` accepts exactly one connection, so a probe connect
/// would consume the slot the test needs.
fn wait_for_listen(port: u16, deadline: Instant) -> bool {
    let suffix = format!(":{port:04X}");
    loop {
        let Ok(table) = std::fs::read_to_string("/proc/net/tcp") else {
            // Not Linux: fall back to a fixed settle window.
            thread::sleep(Duration::from_millis(750));
            return true;
        };
        let listening = table.lines().skip(1).any(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            columns.len() > 3 && columns[1].ends_with(&suffix) && columns[3] == "0A"
        });
        if listening {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// True while any process is still running our publisher command line. The
/// `rawvideo` term distinguishes the sink's own child from the test receiver,
/// which shares the destination URL. A reparented orphan is still found.
fn publisher_alive(key: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        std::fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
            let text = String::from_utf8_lossy(&cmdline);
            text.contains(key) && text.contains("rawvideo")
        })
    })
}

fn spawn_rtmp_receiver(port: u16, key: &str, output: &Path) -> Child {
    Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-listen",
            "1",
            "-i",
        ])
        .arg(format!("rtmp://127.0.0.1:{port}/live/{key}"))
        .args(["-c", "copy", "-y"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn reap(child: &mut Child, deadline: Instant) {
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Accepts TCP connections and never answers the RTMP handshake.
struct StalledReceiver {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl StalledReceiver {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut held: Vec<TcpStream> = Vec::new();
            while !signal.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => held.push(stream),
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
            drop(held);
        });
        Self {
            port,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for StalledReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn destination(port: u16, key: &str) -> StreamDestination {
    StreamDestination::parse(&format!("rtmp://127.0.0.1:{port}/live/{key}")).unwrap()
}

/// `accepted` must always be fully accounted for by an explicit outcome.
fn assert_accounted(telemetry: &StreamTelemetry) {
    let accounted = telemetry.delivered_pairs
        + telemetry.write_failed_pairs
        + telemetry.dropped_oldest_pairs
        + telemetry.discarded_pairs
        + telemetry.outstanding_pairs as u64;
    assert_eq!(
        telemetry.accepted_pairs, accounted,
        "unaccounted pairs: {telemetry:?}"
    );
}

fn probe(path: &Path) -> serde_json::Value {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn live_stream_reaches_a_real_rtmp_receiver() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let captured = directory.path().join("received.flv");
    let key = unique_key("e2e");
    let port = free_port();
    let mut receiver = spawn_rtmp_receiver(port, &key, &captured);
    assert!(
        wait_for_listen(port, Instant::now() + Duration::from_secs(10)),
        "RTMP receiver never started listening"
    );

    let format = format(64, 48);
    let mut config = StreamConfig::new(format.clone(), destination(port, &key));
    config.overflow = OverflowPolicy::Reject;
    config.limits = StreamLimits {
        max_outstanding_pairs: 4,
        enqueue_timeout: Duration::from_millis(100),
        ..StreamLimits::default()
    };
    config.encoder.keyframe_interval_seconds = 1;
    let mut streamer = Streamer::start(config).unwrap();
    assert_eq!(
        streamer.destination(),
        format!("rtmp://127.0.0.1:{port}/live/****")
    );
    for offset in 0..E2E_FRAMES {
        let mut pending = frame(&format, offset);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match streamer.enqueue(pending) {
                Ok(()) => break,
                Err(error) if error.reason == EnqueueRejection::QueueFull => {
                    assert!(
                        Instant::now() < deadline,
                        "stuck feeding frame {offset}: {:?}",
                        streamer.telemetry()
                    );
                    pending = error.into_frame();
                }
                Err(error) => panic!("{error:?}; telemetry {:?}", streamer.telemetry()),
            }
        }
    }
    let report = streamer.stop();
    assert_eq!(report.outcome, StopOutcome::Clean, "{report:?}");
    assert_eq!(report.exit_status, Some(0), "{report:?}");
    assert_eq!(report.cleanup, CleanupStatus::Complete, "{report:?}");
    assert_eq!(report.telemetry.failure, None, "{report:?}");
    assert_eq!(report.telemetry.accepted_pairs, E2E_FRAMES);
    assert_eq!(report.telemetry.delivered_pairs, E2E_FRAMES);
    assert_eq!(report.telemetry.skipped_pairs, 0);
    assert!(report.telemetry.connected, "{report:?}");
    assert!(report.telemetry.sent_bytes > 0, "{report:?}");
    assert!(report.telemetry.peak_outstanding_pairs <= 4);
    assert_accounted(&report.telemetry);
    assert_eq!(streamer.stop(), report, "stop must be idempotent");

    reap(&mut receiver, Instant::now() + Duration::from_secs(20));
    let bytes = std::fs::read(&captured).unwrap();
    assert!(
        bytes.len() > 4_096,
        "receiver captured only {} bytes",
        bytes.len()
    );
    assert_eq!(&bytes[..3], b"FLV", "received file is not FLV");

    let probe = probe(&captured);
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
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");
    assert_eq!(audio["channels"], 2);
    let decoded_frames = video["nb_read_frames"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(
        decoded_frames >= E2E_FRAMES - 5,
        "receiver decoded only {decoded_frames} of {E2E_FRAMES} frames"
    );
    assert!(!publisher_alive(&key), "an ffmpeg publisher survived stop");
}

#[test]
fn a_forced_failure_never_exposes_the_stream_key() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let key = unique_key("secret");
    // Nothing is listening: the child fails opening the destination and prints
    // the full URL, with the key, to stderr twice.
    let port = free_port();
    let destination = destination(port, &key);
    assert_eq!(
        destination.redacted(),
        format!("rtmp://127.0.0.1:{port}/live/****")
    );
    let format = format(64, 48);
    let mut config = StreamConfig::new(format.clone(), destination);
    config.limits = StreamLimits {
        connect_timeout: Duration::from_secs(5),
        stop_timeout: Duration::from_secs(5),
        ..StreamLimits::default()
    };

    let mut surfaces = Vec::new();
    let mut streamer = match Streamer::start(config) {
        Ok(streamer) => streamer,
        Err(error) => {
            for text in [format!("{error:?}"), format!("{error}")] {
                assert!(!text.contains(&key), "start error leaked the key: {text}");
            }
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut failure = None;
    let mut offset = 0;
    while Instant::now() < deadline {
        match streamer.enqueue(frame(&format, offset)) {
            Ok(()) => offset += 1,
            Err(error) => {
                surfaces.push(format!("{error:?}"));
                surfaces.push(format!("{error}"));
                if let EnqueueRejection::Failed(cause) = &error.reason {
                    failure = Some(cause.clone());
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let failure = failure.expect("the unreachable destination must become terminal");
    assert!(
        matches!(failure, StreamFailure::ChildExited { .. }),
        "{failure:?}"
    );
    // The first cause is sticky: no retry, same classification afterwards.
    let repeat = streamer.enqueue(frame(&format, offset)).unwrap_err();
    assert_eq!(repeat.reason, EnqueueRejection::Failed(failure.clone()));

    let report = streamer.stop();
    let telemetry = &report.telemetry;
    assert_eq!(telemetry.failure, Some(failure.clone()));
    assert!(!telemetry.connected, "{telemetry:?}");
    assert!(
        telemetry.stderr_tail.contains("****"),
        "expected redacted child stderr, got {:?}",
        telemetry.stderr_tail
    );
    surfaces.extend([
        format!("{report:?}"),
        format!("{telemetry:?}"),
        format!("{failure:?}"),
        telemetry.stderr_tail.clone(),
        telemetry.destination.clone(),
        streamer.destination().to_owned(),
    ]);
    for text in surfaces {
        assert!(!text.contains(&key), "stream key leaked: {text}");
        assert!(
            !text.contains(&key[..8]),
            "stream key prefix leaked: {text}"
        );
    }
    assert!(!publisher_alive(&key), "an ffmpeg publisher survived stop");
}

#[test]
fn backpressure_stays_bounded_counted_and_terminal() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    for policy in [OverflowPolicy::Reject, OverflowPolicy::DropOldest] {
        let key = unique_key("stall");
        let stalled = StalledReceiver::start();
        let format = format(640, 480);
        let mut config = StreamConfig::new(format.clone(), destination(stalled.port, &key));
        config.overflow = policy;
        config.limits = StreamLimits {
            max_outstanding_pairs: MAX_PAIRS,
            max_retained_bytes: MAX_BYTES,
            enqueue_timeout: Duration::ZERO,
            connect_timeout: Duration::from_secs(3),
            stop_timeout: Duration::from_secs(5),
            ..StreamLimits::default()
        };
        let mut streamer = Streamer::start(config).unwrap();
        for offset in 0..80 {
            let _ = streamer.enqueue(frame(&format, offset));
            let telemetry = streamer.telemetry();
            assert!(
                telemetry.outstanding_pairs <= MAX_PAIRS,
                "{policy:?} exceeded the pair bound: {telemetry:?}"
            );
            assert!(
                telemetry.retained_bytes <= MAX_BYTES,
                "{policy:?} exceeded the byte bound: {telemetry:?}"
            );
            assert_accounted(&telemetry);
        }
        let telemetry = streamer.telemetry();
        assert!(
            telemetry.peak_outstanding_pairs <= MAX_PAIRS,
            "{telemetry:?}"
        );
        assert!(telemetry.peak_outstanding_pairs > 0, "{telemetry:?}");
        assert!(
            telemetry.rejected.total() + telemetry.dropped_oldest_pairs > 0,
            "an overloaded sink must record explicit loss: {telemetry:?}"
        );
        match policy {
            OverflowPolicy::Reject => {
                assert!(telemetry.rejected.queue_full > 0, "{telemetry:?}");
                assert_eq!(telemetry.dropped_oldest_pairs, 0, "{telemetry:?}");
            }
            OverflowPolicy::DropOldest => {
                assert!(telemetry.dropped_oldest_pairs > 0, "{telemetry:?}");
            }
        }

        // A destination that accepts TCP but never completes the handshake is
        // bounded by the sink's own connect deadline.
        let deadline = Instant::now() + Duration::from_secs(15);
        while streamer.telemetry().failure.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let telemetry = streamer.telemetry();
        assert_eq!(
            telemetry.failure,
            Some(StreamFailure::ConnectTimeout),
            "{telemetry:?}"
        );
        assert!(!telemetry.connected, "{telemetry:?}");
        let rejection = streamer.enqueue(frame(&format, 200)).unwrap_err();
        assert_eq!(
            rejection.reason,
            EnqueueRejection::Failed(StreamFailure::ConnectTimeout)
        );

        let report = streamer.stop();
        assert_eq!(
            report.telemetry.failure,
            Some(StreamFailure::ConnectTimeout),
            "{report:?}"
        );
        assert!(
            matches!(report.telemetry.child, ChildState::Exited { .. }),
            "{report:?}"
        );
        assert_accounted(&report.telemetry);
        assert!(
            !publisher_alive(&key),
            "{policy:?} left an ffmpeg publisher"
        );
        drop(stalled);
    }
}

#[test]
fn no_ffmpeg_child_survives_stop_or_cancel() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let format = format(64, 48);

    let directory = tempdir().unwrap();
    let clean_key = unique_key("stopclean");
    let port = free_port();
    let mut receiver = spawn_rtmp_receiver(port, &clean_key, &directory.path().join("clean.flv"));
    assert!(wait_for_listen(
        port,
        Instant::now() + Duration::from_secs(10)
    ));
    let mut streamer = Streamer::start(StreamConfig::new(
        format.clone(),
        destination(port, &clean_key),
    ))
    .unwrap();
    for offset in 0..10 {
        let _ = streamer.enqueue(frame(&format, offset));
    }
    let report = streamer.stop();
    assert_eq!(report.cleanup, CleanupStatus::Complete, "{report:?}");
    assert!(
        matches!(report.telemetry.child, ChildState::Exited { .. }),
        "child was never reaped: {report:?}"
    );
    assert!(
        !publisher_alive(&clean_key),
        "an ffmpeg publisher survived stop"
    );
    drop(streamer);
    reap(&mut receiver, Instant::now() + Duration::from_secs(20));

    let cancel_key = unique_key("cancel");
    let stalled = StalledReceiver::start();
    let mut config = StreamConfig::new(format.clone(), destination(stalled.port, &cancel_key));
    config.limits = StreamLimits {
        stop_timeout: Duration::from_secs(5),
        ..StreamLimits::default()
    };
    let mut streamer = Streamer::start(config).unwrap();
    for offset in 0..10 {
        let _ = streamer.enqueue(frame(&format, offset));
    }
    streamer.request_cancel();
    let report = streamer.stop();
    assert_eq!(report.outcome, StopOutcome::Killed, "{report:?}");
    assert_eq!(
        report.telemetry.failure,
        Some(StreamFailure::Cancelled),
        "{report:?}"
    );
    assert!(
        matches!(report.telemetry.child, ChildState::Exited { .. }),
        "child was never reaped after cancel: {report:?}"
    );
    assert_accounted(&report.telemetry);
    assert!(
        !publisher_alive(&cancel_key),
        "an ffmpeg publisher survived request_cancel"
    );

    // Dropping a streamer that was never stopped must not orphan a child.
    let drop_key = unique_key("dropped");
    let stalled_drop = StalledReceiver::start();
    let mut config = StreamConfig::new(format.clone(), destination(stalled_drop.port, &drop_key));
    // No pair is ever dispatched here, so the connect deadline never arms and
    // the drain is bounded by `stop_timeout` alone. Keep that budget short so
    // the suite does not pay for a destination that answers nothing.
    config.limits = StreamLimits {
        stop_timeout: Duration::from_secs(2),
        ..StreamLimits::default()
    };
    let dropped = Streamer::start(config).unwrap();
    drop(dropped);
    assert!(
        !publisher_alive(&drop_key),
        "dropping a Streamer orphaned its ffmpeg child"
    );
}
