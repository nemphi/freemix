//! Live RTMP sink integration coverage.
//!
//! # Receiver path
//!
//! This box's `ffmpeg` (6.1.1, Ubuntu) supports RTMP listen mode, verified by
//! hand before these tests were written: `ffmpeg -listen 1 -i rtmp://127.0.0.1:PORT/live/key
//! -c copy -y out.flv` accepts a publish from a second `ffmpeg` and writes a
//! playable FLV. The end-to-end tests therefore use a real `ffmpeg` RTMP
//! receiver and reprobe the received file with `ffprobe`, rather than the
//! `tcp://` + `TcpListener` fallback the task allowed for builds without
//! listen mode.
//!
//! Backpressure, connect-deadline, and wedged-writer coverage instead points the
//! sink at a plain `TcpListener` that accepts the connection and never answers
//! the RTMP handshake. Measured behaviour of this `ffmpeg` build in that
//! situation is to block forever with no muxer progress and to stop draining its
//! own inputs, which is precisely the stall the sink's deadlines exist to bound.
//!
//! # What these tests are really asserting
//!
//! A live sink is not proven by "some bytes arrived". It is proven by the
//! receiver ending up with as much *media* as wall-clock time elapsed, with
//! audio and video agreeing, at a resolution anyone would actually broadcast.
//! Every end-to-end case here runs a wall-clock-paced producer and reprobes the
//! captured file for duration, because the two worst failures this sink can
//! have — never starting at a real frame size, and quietly falling behind real
//! time — are both invisible to a test that only counts bytes at 64x48.

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
use fm_types::{Channel, FrameRate};
use tempfile::tempdir;

const FPS: u64 = 30;
const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / FPS);
const SAMPLES_PER_FRAME: u64 = 1_600;
/// One AAC-LC frame, the granularity of the captured audio timeline.
const AAC_SAMPLES_PER_PACKET: f64 = 1_024.0;
const SAMPLE_RATE: f64 = 48_000.0;
const FIRST_SEQUENCE: u64 = 100;
/// Deep enough that `DropOldest` always has at least one pair it can still
/// recall; anything shallower is refused at startup.
const MAX_PAIRS: usize = 4;
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Audio and video are decoded from independent packet counts, so they may
/// legitimately differ by about one AAC packet plus one video frame.
const AV_AGREEMENT_TOLERANCE: f64 = 0.25;
/// How far captured media duration may sit from the wall-clock span of the
/// producer that generated it. One keyframe interval of slack.
const WALL_CLOCK_TOLERANCE: f64 = 0.75;

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
        FrameRate::new(u32::try_from(FPS).unwrap(), 1).unwrap(),
        SampleRate::new(48_000).unwrap(),
        ChannelLayout::stereo(),
        SequenceNumber::new(FIRST_SEQUENCE),
    )
    .unwrap()
}

fn frame(format: &RecordFormat, offset: u64) -> PairedFrame {
    let sequence = SequenceNumber::new(format.first_sequence().get() + offset);
    let start_sample = sequence.get() * SAMPLES_PER_FRAME;
    let start_nanos = sequence.get() * 1_000_000_000 / FPS;
    let end_nanos = (sequence.get() + 1) * 1_000_000_000 / FPS;
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
    // Moving detail in every frame: a static picture would let x264 emit
    // near-empty frames and hide any real throughput problem.
    let width = usize::try_from(format.dimensions().width()).unwrap();
    let mut rgba = vec![0_u8; format.rgba_bytes_per_frame()];
    for (row, line) in rgba.chunks_exact_mut(width * 4).enumerate() {
        let base = u8::try_from((row + usize::try_from(offset).unwrap() * 11) % 255).unwrap();
        for (column, pixel) in line.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[
                base,
                u8::try_from(column % 251).unwrap(),
                u8::try_from((offset * 3) % 255).unwrap(),
                255,
            ]);
        }
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
/// Padding is deliberately outside this identity: it is synthesized by the
/// sink, never accepted from the caller.
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
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stream_of<'a>(probe: &'a serde_json::Value, kind: &str) -> &'a serde_json::Value {
    probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["codec_type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} stream in {probe}"))
}

fn counted(stream: &serde_json::Value, field: &str) -> f64 {
    stream[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} missing: {stream}"))
        .parse::<f64>()
        .unwrap()
}

/// Captured media duration, derived from decoded packet counts rather than the
/// container header: an FLV written by a live publisher often carries no usable
/// stream duration, and the counts are what actually reached the receiver.
struct Captured {
    video_seconds: f64,
    audio_seconds: f64,
    video_frames: u64,
}

fn captured(path: &Path) -> (Captured, serde_json::Value) {
    let probe = probe(path);
    let video = stream_of(&probe, "video");
    let audio = stream_of(&probe, "audio");
    let frames = counted(video, "nb_read_frames");
    let packets = counted(audio, "nb_read_packets");
    (
        Captured {
            #[allow(clippy::cast_precision_loss)]
            video_seconds: frames / FPS as f64,
            audio_seconds: packets * AAC_SAMPLES_PER_PACKET / SAMPLE_RATE,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            video_frames: frames as u64,
        },
        probe,
    )
}

/// Runs a wall-clock-paced producer, the way a render thread actually behaves:
/// the sequence number is derived from elapsed time, so falling behind skips
/// ahead rather than stretching the timeline. Returns the wall-clock span and
/// the number of pairs offered.
fn produce(
    streamer: &mut Streamer,
    format: &RecordFormat,
    frames: u64,
    skip_every: Option<u64>,
) -> (Duration, u64) {
    let started = Instant::now();
    let mut offered = 0;
    let mut next = 0_u64;
    while next < frames {
        let target = started + FRAME_PERIOD * u32::try_from(next).unwrap();
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
        let elapsed = u64::try_from(started.elapsed().as_nanos() / FRAME_PERIOD.as_nanos())
            .unwrap_or(u64::MAX);
        let offset = elapsed.max(next);
        next = offset + 1;
        if skip_every.is_some_and(|every| offset % every == every - 1) {
            continue;
        }
        // A live render thread never retries: a refused frame is simply gone,
        // and the sink is responsible for keeping the timeline whole anyway.
        let _ = streamer.enqueue(frame(format, offset));
        offered += 1;
    }
    // The timeline the sink was asked to cover is the last offset it saw, not
    // the number of pairs it was handed.
    (FRAME_PERIOD * u32::try_from(next).unwrap(), offered)
}

#[test]
fn live_stream_reaches_a_real_rtmp_receiver_at_broadcast_resolution() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let captured_path = directory.path().join("received.flv");
    let key = unique_key("e2e");
    let port = free_port();
    let mut receiver = spawn_rtmp_receiver(port, &key, &captured_path);
    assert!(
        wait_for_listen(port, Instant::now() + Duration::from_secs(10)),
        "RTMP receiver never started listening"
    );

    let format = format(1280, 720);
    let mut config = StreamConfig::new(format.clone(), destination(port, &key));
    config.overflow = OverflowPolicy::DropOldest;
    config.limits = StreamLimits {
        max_outstanding_pairs: 4,
        ..StreamLimits::default()
    };
    config.encoder.keyframe_interval_seconds = 1;
    let mut streamer = Streamer::start(config).unwrap();
    assert_eq!(
        streamer.destination(),
        format!("rtmp://127.0.0.1:{port}/live/****")
    );

    reject_byte_compatible_impostors(&mut streamer, &format);

    let (wall, offered) = produce(&mut streamer, &format, 150, None);
    let live = streamer.telemetry();
    assert!(live.connected, "{live:?}");
    assert!(
        live.media_drift < Duration::from_secs(1),
        "the sink fell behind wall clock while streaming: {live:?}"
    );

    let report = streamer.stop();
    assert_eq!(report.outcome, StopOutcome::Clean, "{report:?}");
    assert_eq!(report.exit_status, Some(0), "{report:?}");
    assert_eq!(report.cleanup, CleanupStatus::Complete, "{report:?}");
    assert_eq!(report.telemetry.failure, None, "{report:?}");
    assert_eq!(report.telemetry.rejected.format_mismatch, 2, "{report:?}");
    assert!(report.telemetry.connected, "{report:?}");
    assert!(report.telemetry.muxed_bytes > 0, "{report:?}");
    assert!(report.telemetry.peak_outstanding_pairs <= 4, "{report:?}");
    assert_accounted(&report.telemetry);
    assert_eq!(streamer.stop(), report, "stop must be idempotent");

    reap(&mut receiver, Instant::now() + Duration::from_secs(20));
    let bytes = std::fs::read(&captured_path).unwrap();
    assert!(
        bytes.len() > 64 * 1024,
        "receiver captured only {} bytes",
        bytes.len()
    );
    assert_eq!(&bytes[..3], b"FLV", "received file is not FLV");

    let (captured, probe) = captured(&captured_path);
    let video = stream_of(&probe, "video");
    let audio = stream_of(&probe, "audio");
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 1280);
    assert_eq!(video["height"], 720);
    assert_eq!(video["pix_fmt"], "yuv420p");
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");
    assert_eq!(audio["channels"], 2);

    let wall_seconds = wall.as_secs_f64();
    assert!(
        (captured.video_seconds - wall_seconds).abs() <= WALL_CLOCK_TOLERANCE,
        "captured {:.3}s of video for {wall_seconds:.3}s of wall clock \
         (offered {offered} pairs, {:?})",
        captured.video_seconds,
        report.telemetry
    );
    assert!(
        (captured.video_seconds - captured.audio_seconds).abs() <= AV_AGREEMENT_TOLERANCE,
        "audio and video disagree: video {:.3}s, audio {:.3}s",
        captured.video_seconds,
        captured.audio_seconds
    );
    assert!(!publisher_alive(&key), "an ffmpeg publisher survived stop");
}

/// A sink that checks payload byte counts instead of the format accepts media
/// it will mux as garbage. Both impostors below have byte-for-byte the same
/// video and audio payload sizes as `format`, and describe completely different
/// media: a transposed picture, and mono at twice the sample rate.
fn reject_byte_compatible_impostors(streamer: &mut Streamer, format: &RecordFormat) {
    let transposed = crate::format(format.dimensions().height(), format.dimensions().width());
    assert_eq!(
        transposed.rgba_bytes_per_frame(),
        format.rgba_bytes_per_frame()
    );
    let relaid = RecordFormat::new(
        format.dimensions().width(),
        format.dimensions().height(),
        FrameRate::new(u32::try_from(FPS).unwrap(), 1).unwrap(),
        SampleRate::new(96_000).unwrap(),
        ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        SequenceNumber::new(FIRST_SEQUENCE),
    )
    .unwrap();
    let reference = frame(format, 0);
    for wrong in [
        frame(&transposed, 0),
        PairedFrame::new(
            &relaid,
            SequenceNumber::new(FIRST_SEQUENCE),
            vec![0_u8; relaid.rgba_bytes_per_frame()],
            mono_block(&relaid, SequenceNumber::new(FIRST_SEQUENCE)),
        )
        .unwrap(),
    ] {
        assert_eq!(
            (wrong.rgba().len(), wrong.audio_f32le().len()),
            (reference.rgba().len(), reference.audio_f32le().len()),
            "an impostor that does not match the byte counts proves nothing"
        );
        let error = streamer.enqueue(wrong).unwrap_err();
        assert_eq!(error.reason, EnqueueRejection::FormatMismatch, "{error:?}");
    }
}

fn mono_block(format: &RecordFormat, sequence: SequenceNumber) -> AudioBlock {
    let samples = usize::try_from(u64::from(format.sample_rate().hertz()) / FPS).unwrap();
    let start_sample = sequence.get() * u64::from(format.sample_rate().hertz()) / FPS;
    let start_nanos = sequence.get() * 1_000_000_000 / FPS;
    let end_nanos = (sequence.get() + 1) * 1_000_000_000 / FPS;
    let timing = MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(start_sample).unwrap()),
            TimeBase::new(1, format.sample_rate().hertz()).unwrap(),
        ),
        NormalizedTimestamp::from_nanos(i64::try_from(start_nanos).unwrap()),
        NormalizedDuration::from_nanos(end_nanos - start_nanos).unwrap(),
        ClockDomainId::new(NonZeroU128::new(7).unwrap()),
        sequence,
    )
    .unwrap();
    AudioBlock::new(
        timing,
        format.sample_rate(),
        format.channel_layout().clone(),
        vec![vec![0.25; samples]],
    )
    .unwrap()
}

/// Losing pairs must cost picture quality, never timeline. A sink that simply
/// omits a lost pair shortens the muxed timeline by a frame period every time
/// and falls behind wall clock forever, which is a rebuffering loop for every
/// viewer rather than a glitch.
#[test]
fn media_time_tracks_wall_clock_through_producer_loss() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let captured_path = directory.path().join("lossy.flv");
    let key = unique_key("loss");
    let port = free_port();
    let mut receiver = spawn_rtmp_receiver(port, &key, &captured_path);
    assert!(wait_for_listen(
        port,
        Instant::now() + Duration::from_secs(10)
    ));

    let format = format(640, 480);
    let mut config = StreamConfig::new(format.clone(), destination(port, &key));
    config.overflow = OverflowPolicy::DropOldest;
    config.encoder.keyframe_interval_seconds = 1;
    let mut streamer = Streamer::start(config).unwrap();

    // The producer never offers one pair in four.
    let (wall, offered) = produce(&mut streamer, &format, 180, Some(4));
    let report = streamer.stop();
    assert_eq!(report.outcome, StopOutcome::Clean, "{report:?}");
    assert_eq!(report.telemetry.failure, None, "{report:?}");
    assert!(
        report.telemetry.skipped_pairs >= 40,
        "the producer's own loss must be counted: {:?}",
        report.telemetry
    );
    assert!(
        report.telemetry.padded_pairs >= 40,
        "loss must be padded, not swallowed: {:?}",
        report.telemetry
    );
    assert!(
        report.telemetry.media_drift < Duration::from_millis(750),
        "media clock drifted under loss: {:?}",
        report.telemetry
    );
    assert_accounted(&report.telemetry);

    reap(&mut receiver, Instant::now() + Duration::from_secs(20));
    let (captured, _) = captured(&captured_path);
    let wall_seconds = wall.as_secs_f64();
    assert!(
        (captured.video_seconds - wall_seconds).abs() <= WALL_CLOCK_TOLERANCE,
        "25% pair loss cost {:.3}s of timeline over {wall_seconds:.3}s \
         (offered {offered}, captured {} frames, {:?})",
        wall_seconds - captured.video_seconds,
        captured.video_frames,
        report.telemetry
    );
    assert!(
        (captured.video_seconds - captured.audio_seconds).abs() <= AV_AGREEMENT_TOLERANCE,
        "audio and video disagree under loss: video {:.3}s, audio {:.3}s",
        captured.video_seconds,
        captured.audio_seconds
    );
    assert!(!publisher_alive(&key), "an ffmpeg publisher survived stop");
}

/// A stream whose media clock has stopped is dead, and must be reported dead,
/// even though nothing is outstanding. A paused producer is routine — an
/// operator cuts away, a caller backs off — and is exactly the condition under
/// which a silently dead destination used to be reported as healthy and then as
/// a clean stop.
#[test]
fn a_stalled_media_clock_is_terminal_with_nothing_outstanding() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let captured_path = directory.path().join("stalled.flv");
    let key = unique_key("stall2");
    let port = free_port();
    let mut receiver = spawn_rtmp_receiver(port, &key, &captured_path);
    assert!(wait_for_listen(
        port,
        Instant::now() + Duration::from_secs(10)
    ));

    let format = format(320, 240);
    let mut config = StreamConfig::new(format.clone(), destination(port, &key));
    config.limits = StreamLimits {
        no_progress_timeout: Duration::from_secs(2),
        stop_timeout: Duration::from_secs(5),
        ..StreamLimits::default()
    };
    config.encoder.keyframe_interval_seconds = 1;
    let mut streamer = Streamer::start(config).unwrap();
    produce(&mut streamer, &format, 60, None);

    // Everything offered has drained; the sink is idle and healthy.
    let settle = Instant::now() + Duration::from_secs(3);
    while Instant::now() < settle {
        let telemetry = streamer.telemetry();
        if telemetry.outstanding_pairs == 0 && telemetry.connected {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let idle = streamer.telemetry();
    assert!(
        idle.connected,
        "the sink never reached the receiver: {idle:?}"
    );
    assert_eq!(idle.outstanding_pairs, 0, "{idle:?}");
    assert_eq!(idle.failure, None, "{idle:?}");

    // The producer now stops entirely. The media clock stops with it.
    let stopped_feeding = Instant::now();
    let deadline = stopped_feeding + Duration::from_secs(20);
    let mut observed = None;
    while Instant::now() < deadline {
        let telemetry = streamer.telemetry();
        if let Some(failure) = telemetry.failure {
            observed = Some((
                stopped_feeding.elapsed(),
                failure,
                telemetry.outstanding_pairs,
            ));
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let (latency, failure, outstanding) = observed.expect(
        "a sink whose media clock stopped must become terminal, not stay Streaming forever",
    );
    assert_eq!(failure, StreamFailure::NoProgress, "{failure:?}");
    assert_eq!(outstanding, 0, "the watchdog must not need queued work");
    assert!(
        latency < Duration::from_secs(10),
        "detection took {latency:?} against a 2s no-progress budget"
    );

    let report = streamer.stop();
    assert_ne!(
        report.outcome,
        StopOutcome::Clean,
        "a dead stream must not stop cleanly: {report:?}"
    );
    assert_eq!(
        report.telemetry.failure,
        Some(StreamFailure::NoProgress),
        "{report:?}"
    );
    // The frozen report measures drift at the child's last progress report, so
    // it sits just under the budget that tripped it rather than exactly on it.
    assert!(
        report.telemetry.media_drift >= Duration::from_millis(1_500),
        "the drift that failed the stream must be observable: {:?}",
        report.telemetry
    );
    assert_accounted(&report.telemetry);
    reap(&mut receiver, Instant::now() + Duration::from_secs(10));
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
            // Long enough that the connect deadline, not the writer's own
            // no-progress budget, is what classifies this scenario.
            no_progress_timeout: Duration::from_secs(20),
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
                // One eviction per admission: `DropOldest` must never drain the
                // queue behind a single pair.
                assert!(
                    telemetry.dropped_oldest_pairs <= telemetry.accepted_pairs,
                    "{telemetry:?}"
                );
            }
        }

        // A destination that accepts TCP but never completes the handshake is
        // bounded by the sink's own connect deadline, and is named for what it
        // is: the destination, not one of the loopback inputs.
        let deadline = Instant::now() + Duration::from_secs(15);
        while streamer.telemetry().failure.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let telemetry = streamer.telemetry();
        assert_eq!(
            telemetry.failure,
            Some(StreamFailure::DestinationTimeout),
            "{telemetry:?}"
        );
        assert!(!telemetry.connected, "{telemetry:?}");
        let rejection = streamer.enqueue(frame(&format, 200)).unwrap_err();
        assert_eq!(
            rejection.reason,
            EnqueueRejection::Failed(StreamFailure::DestinationTimeout)
        );

        let report = streamer.stop();
        assert_eq!(
            report.telemetry.failure,
            Some(StreamFailure::DestinationTimeout),
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

/// A child that stops draining its own inputs wedges the writers. That is a
/// no-progress condition and must be bounded by `no_progress_timeout`, not by
/// the far larger graceful-drain budget.
#[test]
fn a_wedged_input_writer_is_bounded_by_the_no_progress_budget() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let key = unique_key("wedge");
    let stalled = StalledReceiver::start();
    // Frames far larger than a socket buffer, so the writer really does wedge
    // rather than being absorbed by the kernel.
    let format = format(1280, 720);
    let mut config = StreamConfig::new(format.clone(), destination(stalled.port, &key));
    config.limits = StreamLimits {
        max_outstanding_pairs: 4,
        enqueue_timeout: Duration::ZERO,
        // Long, so only the writer's own budget can end this.
        connect_timeout: Duration::from_secs(45),
        no_progress_timeout: Duration::from_secs(2),
        stop_timeout: Duration::from_secs(30),
        ..StreamLimits::default()
    };
    let mut streamer = Streamer::start(config).unwrap();
    for offset in 0..4 {
        let _ = streamer.enqueue(frame(&format, offset));
    }
    let started = Instant::now();
    let report = streamer.stop();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "a wedged writer held the drain for {elapsed:?} against a 2s budget: {report:?}"
    );
    let failure = report
        .telemetry
        .failure
        .clone()
        .expect("a wedged writer is a failure");
    assert!(
        matches!(
            failure,
            StreamFailure::Write { .. } | StreamFailure::NoProgress
        ),
        "{failure:?}"
    );
    assert_accounted(&report.telemetry);
    assert!(!publisher_alive(&key), "an ffmpeg publisher survived stop");
    drop(stalled);
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
