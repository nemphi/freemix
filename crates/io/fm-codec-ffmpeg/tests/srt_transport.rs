//! Live SRT sink integration coverage.
//!
//! # Receiver path
//!
//! This mirrors the RTMP receiver pattern in `stream.rs`: a real `ffmpeg`
//! listening on the destination and capturing what the sink publishes, then
//! `ffprobe` over the captured file. The differences are the transport and the
//! container: SRT rides on UDP, so "the receiver is ready" is observed as a
//! bound socket in `/proc/net/udp` (UDP has no listen state), the receiver is
//! started as `ffmpeg -listen 1 -i srt://127.0.0.1:PORT?mode=caller` so it
//! accepts one caller connection, and the sink muxes `-f mpegts`, so the
//! capture is an MPEG-TS file whose fixed-size packets begin with the 0x47
//! sync byte rather than an FLV header.
//!
//! The sink's own child connects out in default caller mode with the stream id
//! carried in its `?streamid=` parameter; the receiver ignores admission ids
//! by default and accepts the single caller that arrives.
//!
//! Like every ffmpeg-dependent test in this crate, these cases skip silently
//! when `ffmpeg` or `ffprobe` is unavailable, unless `FM_REQUIRE_FFMPEG=1`
//! turns the absence into a hard failure.

use std::net::{Ipv4Addr, TcpListener};
use std::num::NonZeroU128;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use fm_codec_ffmpeg::stream::{
    CleanupStatus, DestinationKind, PairedFrame, RecordFormat, StopOutcome, StreamConfig,
    StreamDestination, StreamLimits, Streamer,
};
use fm_frame::{
    AudioBlock, ChannelLayout, ClockDomainId, MediaTimestamp, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SampleRate, SequenceNumber, TimeBase,
};
use fm_types::FrameRate;
use tempfile::tempdir;

const FPS: u64 = 30;
const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / FPS);
const SAMPLES_PER_FRAME: u64 = 1_600;
const FIRST_SEQUENCE: u64 = 100;
/// MPEG-TS packets are fixed size; a real capture starts inside one.
const TS_PACKET_BYTES: usize = 188;

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
    // A per-frame sawtooth and moving picture detail, exactly as in the RTMP
    // end-to-end tests: silence and a static picture would both let the
    // encoder emit near-empty frames and hide a stream carrying nothing.
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
    format!("fmsrt_{label}_{}_{nanos}", std::process::id())
}

fn free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn srt_destination(port: u16, key: &str) -> StreamDestination {
    StreamDestination::parse(&format!("srt://127.0.0.1:{port}?streamid={key}")).unwrap()
}

/// Waits until something has bound the UDP port the receiver will listen on.
/// SRT is UDP, so `/proc/net/tcp` does not apply and there is no listen state
/// to look for: a bound entry of any state proves the receiver created its
/// socket. Without `/proc` this falls back to a fixed settle window.
fn wait_for_udp_bind(port: u16, deadline: Instant) -> bool {
    let suffix = format!(":{port:04X}");
    loop {
        let Some(table) = std::fs::read_to_string("/proc/net/udp").ok() else {
            thread::sleep(Duration::from_millis(750));
            return true;
        };
        let bound = table.lines().skip(1).any(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            columns.len() > 1 && columns[1].ends_with(&suffix)
        });
        if bound {
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
/// which shares the stream id. A reparented orphan is still found.
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

fn spawn_srt_receiver(port: u16, output: &Path) -> Child {
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
        .arg(format!("srt://127.0.0.1:{port}?mode=caller"))
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

/// Runs a wall-clock-paced producer, the way a render thread actually behaves:
/// the sequence number is derived from elapsed time, so falling behind skips
/// ahead rather than stretching the timeline.
fn produce(streamer: &mut Streamer, format: &RecordFormat, frames: u64) {
    let started = Instant::now();
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
        // A live render thread never retries: a refused frame is simply gone.
        let _ = streamer.enqueue(frame(format, offset));
    }
}

#[test]
fn live_srt_stream_reaches_a_real_listener_as_nonempty_mpegts() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = tempdir().unwrap();
    let captured_path = directory.path().join("received.ts");
    let key = unique_key("e2e");
    let port = free_port();
    let mut receiver = spawn_srt_receiver(port, &captured_path);
    assert!(
        wait_for_udp_bind(port, Instant::now() + Duration::from_secs(10)),
        "SRT receiver never bound its UDP port"
    );

    let format = format(1280, 720);
    let config = StreamConfig::new(format.clone(), srt_destination(port, &key));
    assert_eq!(config.destination.kind(), DestinationKind::Srt);
    let mut streamer = match Streamer::start(config) {
        Ok(streamer) => streamer,
        Err(error) => {
            let _ = receiver.kill();
            let _ = receiver.wait();
            panic!("sink never started: {error:?}");
        }
    };
    assert_eq!(
        streamer.destination(),
        format!("srt://127.0.0.1:{port}?streamid=****")
    );
    assert!(publisher_alive(&key), "no publisher child is running");

    produce(&mut streamer, &format, 120);
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
    assert!(report.telemetry.connected, "{report:?}");
    assert!(report.telemetry.muxed_bytes > 0, "{report:?}");
    assert!(
        report.telemetry.peak_outstanding_pairs <= StreamLimits::default().max_outstanding_pairs,
        "{report:?}"
    );

    reap(&mut receiver, Instant::now() + Duration::from_secs(20));
    let bytes = std::fs::read(&captured_path).unwrap();
    assert!(
        bytes.len() > 64 * 1024,
        "receiver captured only {} bytes",
        bytes.len()
    );
    // MPEG-TS, not FLV: the capture begins with packet-aligned sync bytes.
    assert_eq!(
        bytes[0], 0x47,
        "capture does not begin with an MPEG-TS sync byte"
    );
    assert_eq!(
        bytes[TS_PACKET_BYTES], 0x47,
        "capture is not packet-aligned MPEG-TS"
    );

    let probed = probe(&captured_path);
    let video = stream_of(&probed, "video");
    let audio = stream_of(&probed, "audio");
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 1280);
    assert_eq!(video["height"], 720);
    assert_eq!(video["pix_fmt"], "yuv420p");
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "48000");
    assert_eq!(audio["channels"], 2);
    assert!(
        probed["format"]["format_name"]
            .as_str()
            .is_some_and(|name| name.contains("mpegts")),
        "captured container is not MPEG-TS: {:?}",
        probed["format"]["format_name"]
    );
    // Non-empty media: real decodable frames and packets reached the receiver,
    // not merely connection teardown with empty containers.
    assert!(
        counted(video, "nb_read_frames") > 0.0,
        "no video frames were captured: {probed}"
    );
    assert!(
        counted(audio, "nb_read_packets") > 0.0,
        "no audio packets were captured: {probed}"
    );
    assert!(!publisher_alive(&key), "an ffmpeg publisher survived stop");
}
