//! What the RTMP transport adds to the output state machine.
//!
//! The state machine's own retry, backoff, backup-endpoint and recovery
//! behaviour is already covered by the unit tests against a fake sink. These
//! three cases cover only what a real transport can be wrong about:
//!
//! 1. a destination that dies mid-show must come back on the backup endpoint,
//!    against a real `ffmpeg -listen 1` receiver that is really killed;
//! 2. a terminal cause must stop the destination and a transient one must not;
//! 3. no `FFmpeg` child may outlive the attempt that started it.
//!
//! The receiver is the same real RTMP receiver the codec crate's streaming
//! tests use, and the surviving-child check scans `/proc` for a publisher
//! command line carrying this test's unique stream key, so it also finds a
//! reparented orphan. `publisher_alive` is asserted *true* while a stream is
//! live before it is asserted false after a stop, because a detector that never
//! matches would make every leak assertion vacuous.
//!
//! Frames are 320x180: this file is about connection lifecycle, and broadcast
//! resolution is already proven end to end in `fm-codec-ffmpeg`.

use std::num::NonZeroU32;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use fm_codec_ffmpeg::stream::{RecordFormat, StopOutcome};
use fm_frame::{ChannelLayout, SampleRate, SequenceNumber};
use fm_io_network::rtmp::{FfmpegRtmpSink, RtmpSinkConfig, StreamKey};
use fm_io_network::{
    ConnectionTarget, DestinationConfig, DestinationId, DestinationState, Endpoint, FailureStage,
    OutputProtocol, OutputSet, PollEvent, QueueCapacity, ReconnectPolicy, RenditionId,
};
use fm_types::FrameRate;

const FPS: u64 = 30;
const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / FPS);
const SAMPLES_PER_FRAME: usize = 1_600;
const CHANNELS: usize = 2;
const FIRST_SEQUENCE: u64 = 100;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
/// Polls per iteration are cheap; a full frame period between them is not.
const STEP: Duration = Duration::from_millis(2);

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

fn format() -> RecordFormat {
    RecordFormat::new(
        WIDTH,
        HEIGHT,
        FrameRate::new(u32::try_from(FPS).unwrap(), 1).unwrap(),
        SampleRate::new(48_000).unwrap(),
        ChannelLayout::stereo(),
        SequenceNumber::new(FIRST_SEQUENCE),
    )
    .unwrap()
}

fn rendition() -> RenditionId {
    RenditionId::new(NonZeroU32::new(1).unwrap())
}

/// Moving detail in every frame and a per-frame sawtooth: a static picture and
/// silence would both let the encoder emit near-empty frames and hide a stream
/// that is not really carrying anything.
fn pair_bytes(offset: u64) -> (Vec<u8>, Vec<u8>) {
    let width = usize::try_from(WIDTH).unwrap();
    let mut rgba = vec![0_u8; width * usize::try_from(HEIGHT).unwrap() * 4];
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
    let mut audio = Vec::with_capacity(SAMPLES_PER_FRAME * CHANNELS * size_of::<f32>());
    for index in 0..SAMPLES_PER_FRAME {
        let step = (u64::try_from(index).unwrap() + offset * 37) % 400;
        let sample = f32::from(u16::try_from(step).unwrap()) / 200.0 - 1.0;
        for _ in 0..CHANNELS {
            audio.extend_from_slice(&sample.to_le_bytes());
        }
    }
    (rgba, audio)
}

fn unique_key(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("fmkey_{label}_{}_{nanos}", std::process::id())
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Waits for a listening socket without connecting to it: a receiver started
/// with `-listen 1` accepts exactly one connection, so a probe connect would
/// consume the slot the transport needs.
fn wait_for_listen(port: u16, deadline: Instant) -> bool {
    let suffix = format!(":{port:04X}");
    loop {
        let Ok(table) = std::fs::read_to_string("/proc/net/tcp") else {
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

/// True while any process is still running a publisher for this key. The
/// `rawvideo` term distinguishes the transport's own child from the test
/// receiver, which shares the destination URL. A reparented orphan is found.
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

fn endpoint(port: u16) -> Endpoint {
    Endpoint::new("127.0.0.1", port, "/live").unwrap()
}

fn destination_config(
    primary: u16,
    backup: Option<u16>,
    reconnect: ReconnectPolicy,
) -> DestinationConfig {
    DestinationConfig::new(
        DestinationId::new(1).unwrap(),
        OutputProtocol::Rtmp,
        endpoint(primary),
        backup.map(endpoint),
        None,
        None,
        QueueCapacity::new(4).unwrap(),
        reconnect,
    )
    .unwrap()
}

fn sink(key: Option<&str>) -> FfmpegRtmpSink {
    let mut config = RtmpSinkConfig::new(rendition(), format());
    config.stream_key = key.map(|key| StreamKey::new(key).unwrap());
    config.encoder.keyframe_interval_seconds = 1;
    config.encoder.video_bitrate_kbps = 1_500;
    FfmpegRtmpSink::new(config)
}

/// Drives one destination the way a live engine does: a wall-clock-paced
/// producer that never retries a refused frame, and a poll loop that advances
/// the state machine by at most one operation at a time.
struct Pump {
    set: OutputSet,
    sink: FfmpegRtmpSink,
    destination: DestinationId,
    started: Instant,
    next: u64,
    saw_waiting_to_reconnect: bool,
}

impl Pump {
    fn new(config: DestinationConfig, sink: FfmpegRtmpSink) -> Self {
        let destination = config.id();
        let mut set = OutputSet::new();
        set.add_destination(config).unwrap();
        Self {
            set,
            sink,
            destination,
            started: Instant::now(),
            next: 0,
            saw_waiting_to_reconnect: false,
        }
    }

    fn start(&mut self) {
        self.set.start(self.destination).unwrap();
    }

    fn step(&mut self) {
        let due = u64::try_from(self.started.elapsed().as_nanos() / FRAME_PERIOD.as_nanos())
            .unwrap_or(u64::MAX);
        while self.next <= due {
            let (rgba, audio) = pair_bytes(self.next);
            let packet = self
                .sink
                .packet(FIRST_SEQUENCE + self.next, &rgba, &audio)
                .unwrap();
            // A live producer never retries: a refused frame is simply gone,
            // and the gap is padded out by the sink so the media clock holds.
            let _ = self.set.enqueue(self.destination, packet).unwrap();
            self.next += 1;
        }
        let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap();
        let event = self
            .set
            .poll(self.destination, now_ms, &mut self.sink)
            .unwrap();
        if matches!(
            event,
            PollEvent::WaitingToReconnect { .. } | PollEvent::ReconnectScheduled { .. }
        ) {
            self.saw_waiting_to_reconnect = true;
        }
        thread::sleep(STEP);
    }

    /// Runs the loop until `done`, or fails the test at the deadline.
    fn run_until(&mut self, label: &str, budget: Duration, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + budget;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{label} did not happen within {budget:?} (state {:?}, {:?})",
                self.state(),
                self.set.telemetry(self.destination).unwrap()
            );
            self.step();
        }
    }

    fn run_for(&mut self, span: Duration) {
        let deadline = Instant::now() + span;
        while Instant::now() < deadline {
            self.step();
        }
    }

    fn state(&self) -> DestinationState {
        self.set.state(self.destination).unwrap()
    }

    fn packets_sent(&self) -> u64 {
        self.set.telemetry(self.destination).unwrap().packets_sent()
    }

    fn connect_attempts(&self) -> u64 {
        self.set
            .telemetry(self.destination)
            .unwrap()
            .connect_attempts()
    }

    fn failure_count(&self) -> u64 {
        self.set
            .telemetry(self.destination)
            .unwrap()
            .failure_count()
    }

    fn reconnects(&self) -> u64 {
        self.set.telemetry(self.destination).unwrap().reconnects()
    }
}

fn probe_codecs(path: &Path) -> String {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The whole point of the slice: a destination that dies mid-show must be
/// retried with the state machine's backoff, must move to the configured backup
/// endpoint, and must resume streaming there — through a real receiver that is
/// really killed, with no `FFmpeg` child left over from any attempt.
#[test]
#[allow(clippy::too_many_lines)]
fn a_killed_receiver_is_retried_and_resumed_on_the_backup_endpoint() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = std::env::temp_dir().join(unique_key("dir"));
    std::fs::create_dir_all(&directory).unwrap();
    let backup_capture = directory.join("backup.flv");
    let key = unique_key("failover");
    let primary_port = free_port();
    let backup_port = free_port();

    let mut primary = spawn_rtmp_receiver(primary_port, &key, &directory.join("primary.flv"));
    assert!(
        wait_for_listen(primary_port, Instant::now() + Duration::from_secs(10)),
        "primary receiver never started listening"
    );

    let reconnect = ReconnectPolicy::new(200, 1_000, 2, Some(40)).unwrap();
    let mut pump = Pump::new(
        destination_config(primary_port, Some(backup_port), reconnect),
        sink(Some(&key)),
    );
    pump.start();
    pump.run_until(
        "the primary destination went live",
        Duration::from_secs(20),
        |pump| pump.packets_sent() >= 45 && pump.state() == DestinationState::Live,
    );
    let live = pump.sink.stream_telemetry().unwrap();
    assert!(
        live.connected,
        "the transport never opened the destination: {live:?}"
    );
    assert!(publisher_alive(&key), "no publisher child is running");
    assert_eq!(pump.connect_attempts(), 1);
    assert_eq!(pump.failure_count(), 0);
    assert_eq!(
        pump.set.connection_target(pump.destination),
        Some(ConnectionTarget::Primary)
    );

    // The receiver dies mid-show.
    primary.kill().unwrap();
    primary.wait().unwrap();
    let sent_when_lost = pump.packets_sent();
    pump.run_until(
        "the lost destination was detected",
        Duration::from_secs(20),
        |pump| pump.failure_count() >= 1,
    );
    let telemetry = pump.set.telemetry(pump.destination).unwrap();
    let failure = telemetry.latest_failure().unwrap();
    assert!(
        failure.retryable,
        "a receiver that died must be retryable: {failure:?}"
    );
    assert_eq!(
        pump.set.connection_target(pump.destination),
        Some(ConnectionTarget::Backup),
        "the first failure must move the destination to its backup endpoint"
    );

    // Nothing is listening on the backup yet, so the state machine backs off
    // against a refused endpoint until the operator's backup comes up.
    let mut backup = spawn_rtmp_receiver(backup_port, &key, &backup_capture);
    assert!(
        wait_for_listen(backup_port, Instant::now() + Duration::from_secs(10)),
        "backup receiver never started listening"
    );
    pump.run_until(
        "the destination reconnected to the backup",
        Duration::from_secs(30),
        |pump| pump.reconnects() >= 1 && pump.state() == DestinationState::Live,
    );
    let sent_when_recovered = pump.packets_sent();
    pump.run_for(Duration::from_secs(3));

    assert!(
        pump.saw_waiting_to_reconnect,
        "the destination never waited on the reconnect backoff"
    );
    assert!(
        pump.connect_attempts() >= 2,
        "recovery must have taken another connect attempt: {:?}",
        pump.set.telemetry(pump.destination).unwrap()
    );
    assert!(
        pump.packets_sent() >= sent_when_recovered + 60,
        "the stream did not resume: {} packets sent after recovery at {sent_when_recovered} \
         (lost at {sent_when_lost})",
        pump.packets_sent()
    );
    assert!(matches!(
        pump.state(),
        DestinationState::Live | DestinationState::Congested
    ));
    let recovered = pump.sink.stream_telemetry().unwrap();
    assert!(recovered.connected, "{recovered:?}");
    assert!(
        recovered.media_drift < Duration::from_secs(2),
        "the recovered stream is not keeping up with wall clock: {recovered:?}"
    );

    pump.set.stop(pump.destination, &mut pump.sink).unwrap();
    assert_eq!(pump.state(), DestinationState::Stopped);
    assert!(!publisher_alive(&key), "a publisher survived the stop");
    assert_eq!(
        pump.sink.connections_started(),
        pump.sink.connections_stopped(),
        "every attempt's child must have been reaped"
    );
    assert_eq!(pump.sink.unconfirmed_cleanups(), 0);

    reap(&mut backup, Instant::now() + Duration::from_secs(20));
    let captured = std::fs::read(&backup_capture).unwrap();
    assert!(
        captured.len() > 32 * 1024,
        "the backup receiver captured only {} bytes",
        captured.len()
    );
    assert_eq!(&captured[..3], b"FLV");
    let codecs = probe_codecs(&backup_capture);
    assert!(
        codecs.contains("h264") && codecs.contains("aac"),
        "backup capture is not a real stream: {codecs:?}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// The split the whole state machine hinges on. A destination that cannot ever
/// work must stop after one attempt; a destination that is merely refused must
/// be retried until the configured budget is spent, and only then fail.
#[test]
fn a_terminal_cause_stops_the_destination_and_a_transient_one_is_retried() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }

    // Terminal: no stream key anywhere in the destination, so no URL this
    // transport can publish to exists. Retrying cannot invent one.
    let mut pump = Pump::new(
        destination_config(
            free_port(),
            None,
            ReconnectPolicy::new(50, 200, 2, None).unwrap(),
        ),
        sink(None),
    );
    pump.start();
    pump.run_until(
        "the invalid destination failed",
        Duration::from_secs(5),
        |pump| pump.state() == DestinationState::Failed,
    );
    pump.run_for(Duration::from_millis(300));
    let telemetry = pump.set.telemetry(pump.destination).unwrap();
    assert_eq!(
        telemetry.connect_attempts(),
        1,
        "a terminal cause must not be retried: {telemetry:?}"
    );
    let failure = telemetry.latest_failure().unwrap();
    assert!(!failure.retryable, "{failure:?}");
    assert_eq!(failure.stage, FailureStage::Protocol);
    assert_eq!(
        pump.sink.connections_started(),
        0,
        "an unusable destination must not spawn a child"
    );

    // Retryable: a well-formed destination with nothing listening. The child
    // starts, the destination refuses it, and the budget bounds the retries.
    let key = unique_key("refused");
    let mut pump = Pump::new(
        destination_config(
            free_port(),
            None,
            ReconnectPolicy::new(100, 400, 2, Some(2)).unwrap(),
        ),
        sink(Some(&key)),
    );
    pump.start();
    pump.run_until(
        "the refused destination spent its retry budget",
        Duration::from_secs(40),
        |pump| pump.state() == DestinationState::Failed,
    );
    let telemetry = pump.set.telemetry(pump.destination).unwrap();
    assert_eq!(
        telemetry.connect_attempts(),
        3,
        "two retries were budgeted after the first attempt: {telemetry:?}"
    );
    assert_eq!(telemetry.reconnects(), 0);
    assert!(
        telemetry.failures().all(|failure| failure.retryable),
        "a refused connection is transient, not terminal: {telemetry:?}"
    );
    assert_eq!(
        pump.sink.connections_started(),
        pump.sink.connections_stopped()
    );
    assert!(
        !publisher_alive(&key),
        "a failed attempt leaked a publisher"
    );
}

/// A stopped or failed attempt must leave no `FFmpeg` child behind. The live
/// assertion comes first so the detector cannot be silently broken.
#[test]
fn no_ffmpeg_child_outlives_a_stop_or_a_failed_attempt() {
    let _guard = serialize();
    if !tools_available() {
        return;
    }
    let directory = std::env::temp_dir().join(unique_key("dir"));
    std::fs::create_dir_all(&directory).unwrap();
    let key = unique_key("stop");
    let port = free_port();
    let mut receiver = spawn_rtmp_receiver(port, &key, &directory.join("stopped.flv"));
    assert!(wait_for_listen(
        port,
        Instant::now() + Duration::from_secs(10)
    ));

    let mut pump = Pump::new(
        destination_config(
            port,
            None,
            ReconnectPolicy::new(200, 800, 2, Some(4)).unwrap(),
        ),
        sink(Some(&key)),
    );
    pump.start();
    pump.run_until(
        "the destination went live",
        Duration::from_secs(20),
        |pump| pump.packets_sent() >= 45,
    );
    assert!(
        publisher_alive(&key),
        "the surviving-child detector never matched a running publisher"
    );

    pump.set.stop(pump.destination, &mut pump.sink).unwrap();
    assert!(!publisher_alive(&key), "a publisher survived the stop");
    assert_eq!(pump.sink.connections_started(), 1);
    assert_eq!(pump.sink.connections_stopped(), 1);
    assert_eq!(pump.sink.unconfirmed_cleanups(), 0);
    let report = pump.sink.last_stop_report().unwrap();
    assert_eq!(report.outcome, StopOutcome::Clean, "{report:?}");
    reap(&mut receiver, Instant::now() + Duration::from_secs(20));

    // A failed attempt must be cleaned up just as completely.
    let failed_key = unique_key("failedattempt");
    let mut pump = Pump::new(
        destination_config(
            free_port(),
            None,
            ReconnectPolicy::new(2_000, 4_000, 2, Some(4)).unwrap(),
        ),
        sink(Some(&failed_key)),
    );
    pump.start();
    pump.run_until(
        "the failed attempt was recorded",
        Duration::from_secs(30),
        |pump| pump.failure_count() >= 1,
    );
    assert!(
        !publisher_alive(&failed_key),
        "the failed attempt's publisher is still running"
    );
    assert_eq!(
        pump.sink.connections_started(),
        pump.sink.connections_stopped()
    );
    assert_eq!(pump.sink.unconfirmed_cleanups(), 0);
    let _ = std::fs::remove_dir_all(&directory);
}
