//! Bounded RTMP/RTMPS live streaming through one direct `FFmpeg` child.
//!
//! The streamer feeds one `FFmpeg` child tightly packed RGBA8 video frames and
//! sample-major interleaved `f32le` audio over two authenticated loopback HTTP
//! inputs, and muxes `-f flv` to one `rtmp://` or `rtmps://` destination. It
//! reuses [`RecordFormat`] and [`PairedFrame`] from [`crate::record`], which
//! own cadence, layout, and per-pair timing validation. The recorder's private
//! process plumbing (loopback HTTP handshake, child reaping, fallback cleanup)
//! is not reachable across module boundaries, so the equivalent bounded
//! plumbing is restated here.
//!
//! # Secret handling
//!
//! The destination carries a stream key. [`StreamDestination`] keeps the key
//! private: its `Debug` and `Display` render the redacted form only, every
//! typed error is path- and key-free, and child stderr is redacted *before* it
//! enters the bounded retention ring, so neither truncation nor a read split
//! across chunk boundaries can expose a partial key. The key still appears in
//! the child's argv, and on Linux `/proc/<pid>/cmdline` is mode 0444: unless
//! the operating system is configured to hide it, **any** local user can read
//! the key while the child runs, not merely a process running as the same user.
//! This crate cannot close that hole. Containing it is an OS-level measure
//! (`hidepid=2`, a PID namespace, or a dedicated unprivileged user) and lies
//! outside this crate's isolation boundary, exactly as for the recorder's
//! loopback input tokens.
//!
//! # Loss is explicit, and the media clock is not allowed to drift
//!
//! Admission is bounded by pair count and retained bytes. Every refusal or
//! discard is classified and counted in [`StreamTelemetry`]; nothing is lost
//! silently. Dropping or skipping a pair removes exactly one video frame and
//! its matching audio span, so audio and video stay mutually aligned. Because
//! the child derives presentation timestamps from the *count* of raw frames and
//! samples it receives, a pair that is simply omitted would also shorten the
//! muxed timeline by one frame period and leave the stream permanently behind
//! wall clock. The dispatcher therefore pads every gap in the dispatched
//! sequence: it repeats the last delivered video frame and emits the matching
//! span of silence, counted as `padded_pairs`. Residual divergence between the
//! child's muxed media clock and wall clock is reported as
//! [`StreamTelemetry::media_drift`] and, once it exceeds `no_progress_timeout`,
//! becomes a terminal [`StreamFailure::NoProgress`].
//!
//! # Explicitly out of scope for this slice
//!
//! Automatic reconnection is **not** implemented and will not be: the first
//! fatal cause is sticky, every later call reports it, and retry policy belongs
//! to the caller, which owns backoff, destination rotation, and whether a new
//! [`Streamer`] should be started at all. Also out of scope: per-output wiring
//! into the engine, hardware encoders, adaptive bitrate, and TLS certificate
//! policy (`rtmps://` uses the child's default trust configuration).

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fm_frame::SequenceNumber;
use fm_types::{Channel, ChannelLayout};

use crate::record::PROBE_SIZE;
pub use crate::record::{CleanupStatus, MediaInput, PairedFrame, RecordFormat};
use crate::{Executable, UnavailableReason};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const WRITE_POLL: Duration = Duration::from_millis(100);
/// Bounded window a failing input writer allows the reaper so the sticky cause
/// can name the child's exit rather than the broken pipe it caused.
const WRITE_ATTRIBUTION_WINDOW: Duration = Duration::from_millis(50);
const MAX_HTTP_HEADER_BYTES: usize = 8 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const STDERR_PENDING_BYTES: usize = 8 * 1024;
const PROGRESS_PENDING_BYTES: usize = 4 * 1024;
const TOKEN_BYTES: usize = 32;
const MAX_DESTINATION_BYTES: usize = 2 * 1024;
const MIN_STREAM_KEY_BYTES: usize = 4;
const REDACTED_KEY: &str = "****";
const REDACTED_TOKEN: &[u8] = b"<input-token>";
const STATS_PERIOD: &str = "0.25";
/// Backlog each input writer may hold beyond the payload it is actively
/// writing.
///
/// Zero (a rendezvous) is wrong: the dispatcher must be able to hand the child
/// the next pair's audio while the previous pair's video is still draining into
/// the socket, or the two inputs can only advance in lockstep with whichever
/// one the child happens to be reading. Anything large is also wrong: a pair
/// sitting in a writer channel is still accounted in `outstanding`, but
/// [`OverflowPolicy`] can no longer recall it, so a deep writer channel would
/// quietly move the sink's loss policy out of the caller's reach.
const WRITER_BACKLOG: usize = 1;

/// Why a destination URL was refused before any process was spawned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationError {
    Empty,
    TooLong,
    /// The URL contains whitespace, control, or non-ASCII bytes.
    InvalidCharacter,
    /// Only `rtmp://` and `rtmps://` are accepted.
    UnsupportedScheme,
    MissingHost,
    /// `user:password@host` credentials are refused; they cannot be redacted
    /// as reliably as a trailing stream key.
    EmbeddedCredentials,
    /// The URL has no `/app/key` shape, so no segment can be treated as secret.
    MissingStreamKey,
    /// The stream key is too short to be substituted out of captured text
    /// without mangling unrelated output.
    StreamKeyTooShort,
}

impl fmt::Display for DestinationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid streaming destination: {self:?}")
    }
}

impl std::error::Error for DestinationError {}

/// One validated `rtmp://` or `rtmps://` destination with a redacted view.
///
/// The secret tail is never rendered. `Debug` and `Display` both produce the
/// redacted form, so embedding this value in a derived `Debug` type is safe.
#[derive(Clone, Eq, PartialEq)]
pub struct StreamDestination {
    url: String,
    redacted: String,
    key_offset: usize,
}

impl StreamDestination {
    /// Validates scheme, authority, and the presence of a redactable key.
    ///
    /// The secret is everything after the final `/`, including any query
    /// string, which is where RTMP services carry stream keys and tokens.
    ///
    /// # Errors
    ///
    /// Returns a typed [`DestinationError`]. No variant carries URL text.
    pub fn parse(url: &str) -> Result<Self, DestinationError> {
        if url.is_empty() {
            return Err(DestinationError::Empty);
        }
        if url.len() > MAX_DESTINATION_BYTES {
            return Err(DestinationError::TooLong);
        }
        if !url.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(DestinationError::InvalidCharacter);
        }
        let rest = url
            .strip_prefix("rtmp://")
            .or_else(|| url.strip_prefix("rtmps://"))
            .ok_or(DestinationError::UnsupportedScheme)?;
        let (authority, path) = rest
            .split_once('/')
            .ok_or(DestinationError::MissingStreamKey)?;
        if authority.is_empty() {
            return Err(DestinationError::MissingHost);
        }
        if authority.contains('@') {
            return Err(DestinationError::EmbeddedCredentials);
        }
        let key_start = path
            .rfind('/')
            .map(|index| index + 1)
            .ok_or(DestinationError::MissingStreamKey)?;
        let key = &path[key_start..];
        if key.len() < MIN_STREAM_KEY_BYTES {
            return Err(DestinationError::StreamKeyTooShort);
        }
        let key_offset = url.len() - key.len();
        Ok(Self {
            redacted: format!("{}{REDACTED_KEY}", &url[..key_offset]),
            url: url.to_owned(),
            key_offset,
        })
    }

    /// The destination with its stream key replaced by `****`.
    #[must_use]
    pub fn redacted(&self) -> &str {
        &self.redacted
    }

    fn key(&self) -> &str {
        &self.url[self.key_offset..]
    }
}

impl fmt::Debug for StreamDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StreamDestination")
            .field(&self.redacted)
            .finish()
    }
}

impl fmt::Display for StreamDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted)
    }
}

/// What the sink does when an accepted pair does not fit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverflowPolicy {
    /// Return the new pair to the caller, counted as a rejection.
    Reject,
    /// Discard the oldest still-queued pair and admit the new one, counted as
    /// a drop. Exactly one pair is discarded per admission.
    ///
    /// Pairs already handed to the input writers are committed to the child and
    /// cannot be recalled, so `max_outstanding_pairs` must leave room for at
    /// least one pair to remain in the queue; a shallower queue is refused at
    /// startup rather than silently behaving as [`OverflowPolicy::Reject`].
    DropOldest,
}

/// Bounded resource and deadline policy for one streaming session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamLimits {
    pub max_outstanding_pairs: usize,
    pub max_retained_bytes: usize,
    /// Upper bound on how long [`Streamer::enqueue`] may wait for space.
    ///
    /// Defaults to zero, which makes enqueue fully nonblocking, and may not
    /// exceed one frame period: a live render thread must never wait on a
    /// network sink for longer than the frame it is producing, because every
    /// millisecond spent here is taken from the next frame's budget rather than
    /// from the sink's.
    pub enqueue_timeout: Duration,
    /// Budget from the first dispatched pair to the first observed muxer
    /// progress, which is the child's proof that the destination opened. It is
    /// armed by the first dispatch, not by startup, so an idle sink is never
    /// failed for a destination it has not yet had reason to open; an
    /// unresponsive destination on a sink that never sent a pair is bounded by
    /// `stop_timeout` instead.
    pub connect_timeout: Duration,
    /// How far the child's muxed media clock may fall behind wall clock, and
    /// how long the child may report nothing at all, before the sink is
    /// declared dead.
    ///
    /// This is deliberately measured against the media clock rather than
    /// against byte movement: a child that keeps encoding into its own buffers,
    /// or that muxes slower than real time, is producing bytes while the stream
    /// its viewers see is already broken.
    pub no_progress_timeout: Duration,
    pub stop_timeout: Duration,
    pub kill_timeout: Duration,
    pub max_stderr_bytes: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_outstanding_pairs: 4,
            max_retained_bytes: 256 * 1024 * 1024,
            enqueue_timeout: Duration::ZERO,
            connect_timeout: Duration::from_secs(10),
            no_progress_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(15),
            kill_timeout: Duration::from_secs(2),
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }
}

/// Encoder settings applied to the FLV output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderSettings {
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub keyframe_interval_seconds: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            video_bitrate_kbps: 4_500,
            audio_bitrate_kbps: 128,
            keyframe_interval_seconds: 2,
        }
    }
}

/// Streaming startup configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamConfig {
    pub ffmpeg: Executable,
    pub format: RecordFormat,
    pub destination: StreamDestination,
    pub limits: StreamLimits,
    pub overflow: OverflowPolicy,
    pub encoder: EncoderSettings,
}

impl StreamConfig {
    #[must_use]
    pub fn new(format: RecordFormat, destination: StreamDestination) -> Self {
        Self {
            ffmpeg: Executable::SearchPath,
            format,
            destination,
            limits: StreamLimits::default(),
            overflow: OverflowPolicy::DropOldest,
            encoder: EncoderSettings::default(),
        }
    }
}

/// Limits or encoder validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitsError {
    ZeroOutstandingPairs,
    /// [`OverflowPolicy::DropOldest`] can only discard pairs still sitting in
    /// the accounted queue. A queue this shallow would put every admitted pair
    /// straight into a writer, where nothing can be recalled, so the policy
    /// would silently behave as [`OverflowPolicy::Reject`].
    OutstandingPairsTooFewToDropOldest {
        required: usize,
    },
    ZeroRetainedBytes,
    ZeroTimeout,
    EnqueueTimeoutTooLong,
    ZeroStderrBytes,
    StderrTooLarge,
    TimeoutOverflow,
    ByteCountOverflow,
    RetainedBytesTooSmall {
        required: usize,
        maximum: usize,
    },
    VideoBitrate,
    AudioBitrate,
    KeyframeInterval,
}

impl fmt::Display for LimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid streaming limits: {self:?}")
    }
}

impl std::error::Error for LimitsError {}

/// Path-, URL-, and key-free startup failure category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartErrorKind {
    InvalidLimits(LimitsError),
    InvalidExecutable,
    ToolUnavailable(UnavailableReason),
    /// FLV carries mono or stereo audio only.
    UnsupportedChannelLayout,
    Randomness,
    ThreadSpawn(io::ErrorKind),
    Bind {
        input: MediaInput,
        kind: io::ErrorKind,
    },
    Spawn(io::ErrorKind),
    MissingPipe,
    ConnectTimeout {
        input: MediaInput,
    },
    Connect {
        input: MediaInput,
        kind: io::ErrorKind,
    },
    EarlyExit {
        status: Option<i32>,
        /// Redacted child stderr tail.
        stderr: String,
    },
}

/// Startup failure plus an explicit bounded-cleanup result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartError {
    pub kind: StartErrorKind,
    pub cleanup: CleanupStatus,
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "FFmpeg streamer startup failed: {:?} ({:?} cleanup)",
            self.kind, self.cleanup
        )
    }
}

impl std::error::Error for StartError {}

impl StartError {
    const fn complete(kind: StartErrorKind) -> Self {
        Self {
            kind,
            cleanup: CleanupStatus::Complete,
        }
    }
}

/// First terminal runtime failure. This value is sticky: it is recorded once
/// and reported by every later call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamFailure {
    Cancelled,
    ChildExited {
        status: Option<i32>,
    },
    /// The child never opened the destination within `connect_timeout` of the
    /// first dispatched pair. The child was alive and had media to send, so the
    /// destination itself is the suspect.
    DestinationTimeout,
    /// The child never completed its HTTP request for this loopback input
    /// within `connect_timeout`, so this input could never be written. Nothing
    /// was attempted against the destination; the child is the suspect.
    InputTimeout {
        input: MediaInput,
    },
    /// The child's muxed media clock fell more than `no_progress_timeout`
    /// behind wall clock, or the child stopped reporting progress entirely.
    NoProgress,
    Connect {
        input: MediaInput,
        kind: io::ErrorKind,
    },
    Write {
        input: MediaInput,
        kind: io::ErrorKind,
    },
    ProgressRead(io::ErrorKind),
    StderrRead(io::ErrorKind),
    DispatcherClosed(MediaInput),
    WorkerPanicked,
    StopTimedOut,
    KillTimedOut,
    CleanupUnconfirmed,
}

/// Streamer lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamState {
    Starting,
    Streaming,
    Stopping,
    Stopped,
    Failed,
}

/// Observed direct-child state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildState {
    Running,
    Exited { code: Option<i32>, success: bool },
}

/// Per-reason counts of pairs the sink refused to admit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RejectionCounts {
    pub queue_full: u64,
    pub retained_byte_limit: u64,
    pub not_streaming: u64,
    pub failed: u64,
    pub format_mismatch: u64,
    pub sequence: u64,
}

impl RejectionCounts {
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.queue_full
            .saturating_add(self.retained_byte_limit)
            .saturating_add(self.not_streaming)
            .saturating_add(self.failed)
            .saturating_add(self.format_mismatch)
            .saturating_add(self.sequence)
    }
}

/// Stable point-in-time telemetry.
///
/// Accepted pairs satisfy
/// `accepted = delivered + write_failed + dropped_oldest + discarded + outstanding`.
/// `padded_pairs` is outside that identity: padding is synthesized by the sink,
/// never accepted from the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamTelemetry {
    pub state: StreamState,
    pub child: ChildState,
    pub failure: Option<StreamFailure>,
    /// Redacted destination, safe to log.
    pub destination: String,
    pub accepted_pairs: u64,
    pub delivered_pairs: u64,
    pub write_failed_pairs: u64,
    /// Admitted pairs discarded by [`OverflowPolicy::DropOldest`].
    pub dropped_oldest_pairs: u64,
    /// Admitted pairs discarded because shutdown or cancellation intervened.
    pub discarded_pairs: u64,
    /// Sequence numbers the producer skipped between accepted pairs.
    pub skipped_pairs: u64,
    /// Pairs the sink synthesized to cover a gap in the dispatched sequence:
    /// the previous video frame repeated plus the matching span of silence.
    /// These keep the child's media clock on wall clock; they are not caller
    /// media and are not counted as accepted or delivered.
    pub padded_pairs: u64,
    pub rejected: RejectionCounts,
    pub outstanding_pairs: usize,
    pub peak_outstanding_pairs: usize,
    pub retained_bytes: usize,
    pub peak_retained_bytes: usize,
    /// Bytes the child reports having handed to its FLV muxer (`total_size`).
    ///
    /// This is **not** a delivery receipt. The bytes counted here have been
    /// written into the child's output pipeline; how many of them have reached
    /// the destination depends on the child's muxer interleaving, its socket
    /// send buffer, and the receiver's window, none of which are observable
    /// from here. Use [`StreamTelemetry::media_drift`] to judge liveness.
    pub muxed_bytes: u64,
    pub encoded_frames: u64,
    /// The child's muxed media timestamp (`out_time_us`): how much media it has
    /// written to the output so far.
    pub muxed_media_time: Duration,
    /// How far [`StreamTelemetry::muxed_media_time`] has fallen behind wall
    /// clock since the destination opened. A healthy real-time sink holds this
    /// near zero; sustained growth means viewers are rebuffering.
    pub media_drift: Duration,
    /// Whether the destination was ever observed open.
    pub connected: bool,
    pub stderr_tail: String,
    pub stderr_truncated: bool,
}

/// Why a bounded enqueue did not admit a pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnqueueRejection {
    QueueFull,
    RetainedByteLimit,
    FormatMismatch,
    /// Sequence numbers must strictly increase; gaps are allowed and counted.
    Sequence {
        minimum: SequenceNumber,
        actual: SequenceNumber,
    },
    SequenceExhausted,
    Stopping,
    Stopped,
    Failed(StreamFailure),
}

/// Bounded rejection that returns ownership of the pair to the caller.
pub struct EnqueueError {
    pub reason: EnqueueRejection,
    pub frame: Box<PairedFrame>,
}

impl EnqueueError {
    #[must_use]
    pub fn into_frame(self) -> PairedFrame {
        *self.frame
    }
}

impl fmt::Debug for EnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnqueueError")
            .field("reason", &self.reason)
            .field("sequence", &self.frame.sequence())
            .finish()
    }
}

impl fmt::Display for EnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "streaming enqueue rejected: {:?}", self.reason)
    }
}

impl std::error::Error for EnqueueError {}

/// How stop completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopOutcome {
    Clean,
    Failed,
    Killed,
    KillTimedOut,
}

/// Frozen, idempotent stop result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopReport {
    pub outcome: StopOutcome,
    pub exit_status: Option<i32>,
    pub cleanup: CleanupStatus,
    pub telemetry: StreamTelemetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownMode {
    Drain,
    Cancel,
}

#[derive(Clone, Copy)]
struct Shutdown {
    mode: ShutdownMode,
    deadline: Instant,
}

#[derive(Clone, Copy)]
struct ChildResult {
    success: bool,
    code: Option<i32>,
}

/// Ordered secret substitutions applied to every captured child byte.
#[derive(Clone, Debug)]
struct Redactor {
    rules: Vec<(Vec<u8>, Vec<u8>)>,
    longest: usize,
}

impl Redactor {
    fn new(destination: &StreamDestination, tokens: [&str; 2]) -> Self {
        let mut rules = vec![
            (
                destination.url.clone().into_bytes(),
                destination.redacted.clone().into_bytes(),
            ),
            (
                destination.key().as_bytes().to_vec(),
                REDACTED_KEY.as_bytes().to_vec(),
            ),
        ];
        for token in tokens {
            rules.push((token.as_bytes().to_vec(), REDACTED_TOKEN.to_vec()));
        }
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.0.len()));
        let longest = rules.first().map_or(0, |rule| rule.0.len());
        Self { rules, longest }
    }

    fn scrub(&self, buffer: &mut Vec<u8>) {
        for (needle, replacement) in &self.rules {
            replace_all(buffer, needle, replacement);
        }
    }

    fn scrub_text(&self, text: &str) -> String {
        let mut bytes = text.as_bytes().to_vec();
        self.scrub(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Bytes that must be withheld from a forced flush so that a secret split
    /// across two child writes is still reassembled before it is retained.
    const fn carry(&self) -> usize {
        self.longest.saturating_sub(1)
    }
}

fn replace_all(buffer: &mut Vec<u8>, needle: &[u8], replacement: &[u8]) {
    if needle.is_empty() || buffer.len() < needle.len() {
        return;
    }
    if !buffer.windows(needle.len()).any(|window| window == needle) {
        return;
    }
    let mut output = Vec::with_capacity(buffer.len());
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index..].starts_with(needle) {
            output.extend_from_slice(replacement);
            index += needle.len();
        } else {
            output.push(buffer[index]);
            index += 1;
        }
    }
    *buffer = output;
}

struct Queued {
    frame: PairedFrame,
    bytes: usize,
}

struct Shared {
    data: Mutex<SharedData>,
    signal: Condvar,
    kill_requested: AtomicBool,
    aborting: AtomicBool,
    redactor: Redactor,
    format: RecordFormat,
    limits: StreamLimits,
    overflow: OverflowPolicy,
    destination: String,
}

struct SharedData {
    state: StreamState,
    failure: Option<StreamFailure>,
    frozen: bool,
    /// Set once a drain or cancel has been requested.
    shutdown: Option<Shutdown>,
    minimum_sequence: SequenceNumber,
    queue: VecDeque<Queued>,
    accepted: u64,
    delivered: u64,
    write_failed: u64,
    dropped_oldest: u64,
    discarded: u64,
    skipped: u64,
    padded: u64,
    rejected: RejectionCounts,
    outstanding: usize,
    peak_outstanding: usize,
    retained_bytes: usize,
    peak_retained_bytes: usize,
    muxed_bytes: u64,
    encoded_frames: u64,
    /// Latest `out_time_us` the child reported, once it has reported a usable
    /// one at all.
    out_time: Option<Duration>,
    /// Wall instant and media timestamp of the first usable `out_time_us`.
    /// Drift is measured from here, so a slow startup is not charged to the
    /// running stream.
    media_baseline: Option<(Instant, Duration)>,
    /// Smallest drift ever observed. A child that runs a constant pipeline
    /// latency behind wall clock is healthy; only growth beyond its own best
    /// alignment is a failure.
    drift_floor: Option<Duration>,
    connected: bool,
    first_dispatch: Option<Instant>,
    last_progress: Option<Instant>,
    stderr: VecDeque<u8>,
    stderr_truncated: bool,
    child: Option<ChildResult>,
}

impl SharedData {
    fn cancelling(&self) -> bool {
        self.shutdown
            .is_some_and(|shutdown| shutdown.mode == ShutdownMode::Cancel)
    }

    /// How far the child's muxed media clock has fallen behind wall clock as of
    /// `now`, using the latest sample rather than the latest report, so a child
    /// that has stopped reporting shows its drift growing in real time.
    fn media_drift(&self, now: Instant) -> Option<Duration> {
        let (wall_origin, media_origin) = self.media_baseline?;
        let advanced = self.out_time?.saturating_sub(media_origin);
        Some(
            now.saturating_duration_since(wall_origin)
                .saturating_sub(advanced),
        )
    }
}

impl Shared {
    fn new(
        format: &RecordFormat,
        limits: StreamLimits,
        overflow: OverflowPolicy,
        destination: &StreamDestination,
        redactor: Redactor,
    ) -> Self {
        Self {
            data: Mutex::new(SharedData {
                state: StreamState::Starting,
                failure: None,
                frozen: false,
                shutdown: None,
                minimum_sequence: format.first_sequence(),
                queue: VecDeque::new(),
                accepted: 0,
                delivered: 0,
                write_failed: 0,
                dropped_oldest: 0,
                discarded: 0,
                skipped: 0,
                padded: 0,
                rejected: RejectionCounts::default(),
                outstanding: 0,
                peak_outstanding: 0,
                retained_bytes: 0,
                peak_retained_bytes: 0,
                muxed_bytes: 0,
                encoded_frames: 0,
                out_time: None,
                media_baseline: None,
                drift_floor: None,
                connected: false,
                first_dispatch: None,
                last_progress: None,
                stderr: VecDeque::new(),
                stderr_truncated: false,
                child: None,
            }),
            signal: Condvar::new(),
            kill_requested: AtomicBool::new(false),
            aborting: AtomicBool::new(false),
            redactor,
            format: format.clone(),
            limits,
            overflow,
            destination: destination.redacted.clone(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SharedData> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fail(&self, failure: StreamFailure) {
        let mut data = self.lock();
        self.fail_locked(&mut data, failure);
    }

    fn fail_locked(&self, data: &mut SharedData, failure: StreamFailure) {
        if !data.frozen && data.failure.is_none() {
            data.failure = Some(failure);
            data.state = StreamState::Failed;
            self.aborting.store(true, Ordering::Release);
            self.signal.notify_all();
        }
    }

    /// Classifies a failed input write. A broken loopback input is nearly
    /// always the consequence of the child exiting, so wait briefly for the
    /// reaper before deciding which cause becomes sticky.
    fn fail_write(&self, input: MediaInput, kind: io::ErrorKind) {
        let deadline = Instant::now() + WRITE_ATTRIBUTION_WINDOW;
        let mut data = self.lock();
        while data.child.is_none() && data.failure.is_none() && Instant::now() < deadline {
            let (guard, _) = self
                .signal
                .wait_timeout(data, POLL_INTERVAL)
                .unwrap_or_else(PoisonError::into_inner);
            data = guard;
        }
        let failure = data
            .child
            .map_or(StreamFailure::Write { input, kind }, |child| {
                StreamFailure::ChildExited { status: child.code }
            });
        self.fail_locked(&mut data, failure);
    }

    fn is_aborting(&self) -> bool {
        self.aborting.load(Ordering::Acquire) || self.kill_requested.load(Ordering::Acquire)
    }

    fn append_stderr(&self, bytes: &[u8]) {
        let mut data = self.lock();
        if data.frozen {
            return;
        }
        for &byte in bytes {
            if data.stderr.len() == self.limits.max_stderr_bytes {
                data.stderr.pop_front();
                data.stderr_truncated = true;
            }
            data.stderr.push_back(byte);
        }
    }

    fn observe_progress(&self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        let Some((key, value)) = text.trim().split_once('=') else {
            return;
        };
        let value = value.trim();
        let mut data = self.lock();
        if data.frozen {
            return;
        }
        match key.trim() {
            "total_size" => {
                if let Ok(bytes) = value.parse::<u64>() {
                    data.muxed_bytes = data.muxed_bytes.max(bytes);
                }
            }
            "frame" => {
                if let Ok(frames) = value.parse::<u64>() {
                    data.encoded_frames = data.encoded_frames.max(frames);
                }
            }
            // Reported in microseconds, and `N/A` or negative until the child
            // has actually muxed something.
            "out_time_us" => {
                if let Ok(micros) = value.parse::<i64>()
                    && let Ok(micros) = u64::try_from(micros)
                {
                    let media = Duration::from_micros(micros);
                    data.out_time = Some(data.out_time.map_or(media, |seen| seen.max(media)));
                }
            }
            // A progress block is only emitted once the child has opened every
            // output, so the first one proves the destination accepted us.
            "progress" => {
                let now = Instant::now();
                data.connected = true;
                data.last_progress = Some(now);
                if let Some(media) = data.out_time {
                    data.media_baseline.get_or_insert((now, media));
                    if let Some(drift) = data.media_drift(now) {
                        data.drift_floor =
                            Some(data.drift_floor.map_or(drift, |floor| floor.min(drift)));
                    }
                }
            }
            _ => {}
        }
    }

    /// Returns true when a deadline just became the sticky failure and the
    /// child must be torn down.
    ///
    /// The no-progress deadline runs whenever the sink is connected and
    /// streaming. It is deliberately *not* gated on there being outstanding
    /// pairs: a producer that pauses is routine, and a dead destination that
    /// happens to coincide with a paused producer is exactly the case a live
    /// operator must be told about rather than shielded from.
    fn check_deadlines(&self) -> bool {
        let mut data = self.lock();
        if data.frozen || data.failure.is_some() || data.cancelling() {
            return false;
        }
        // A drain is still streaming while pairs remain to flush. Once nothing
        // is outstanding the drain is only waiting for the child to finalize
        // and exit, which `stop_timeout` bounds.
        if data.shutdown.is_some() && data.outstanding == 0 {
            return false;
        }
        let now = Instant::now();
        let failure = if data.connected {
            let silent = data.last_progress.is_some_and(|last| {
                now.saturating_duration_since(last) > self.limits.no_progress_timeout
            });
            let diverged = match (data.media_drift(now), data.drift_floor) {
                (Some(drift), Some(floor)) => {
                    drift.saturating_sub(floor) > self.limits.no_progress_timeout
                }
                _ => false,
            };
            (silent || diverged).then_some(StreamFailure::NoProgress)
        } else {
            data.first_dispatch
                .filter(|armed| now.saturating_duration_since(*armed) > self.limits.connect_timeout)
                .map(|_| StreamFailure::DestinationTimeout)
        };
        let Some(failure) = failure else {
            return false;
        };
        self.fail_locked(&mut data, failure);
        true
    }

    fn record_child(&self, status: ExitStatus) {
        let mut data = self.lock();
        if data.child.is_some() {
            return;
        }
        let result = ChildResult {
            success: status.success(),
            code: status.code(),
        };
        data.child = Some(result);
        if self.kill_requested.load(Ordering::Acquire) {
            self.fail_locked(&mut data, StreamFailure::Cancelled);
        }
        if data.shutdown.is_none() && (!result.success || data.state == StreamState::Streaming) {
            self.fail_locked(
                &mut data,
                StreamFailure::ChildExited {
                    status: result.code,
                },
            );
        }
        self.signal.notify_all();
    }

    fn discard_queue(&self, data: &mut SharedData) {
        while let Some(queued) = data.queue.pop_front() {
            data.outstanding = data.outstanding.saturating_sub(1);
            data.retained_bytes = data.retained_bytes.saturating_sub(queued.bytes);
            data.discarded = data.discarded.saturating_add(1);
        }
        self.signal.notify_all();
    }

    fn snapshot(&self) -> StreamTelemetry {
        self.telemetry_locked(&self.lock())
    }

    fn telemetry_locked(&self, data: &SharedData) -> StreamTelemetry {
        let stderr = data.stderr.iter().copied().collect::<Vec<_>>();
        // Once the child is gone the media clock cannot advance again, so drift
        // is frozen at its last report rather than growing with wall clock.
        let clock = match data.child {
            Some(_) => data.last_progress.unwrap_or_else(Instant::now),
            None => Instant::now(),
        };
        StreamTelemetry {
            state: data.state,
            child: match data.child {
                None => ChildState::Running,
                Some(child) => ChildState::Exited {
                    code: child.code,
                    success: child.success,
                },
            },
            failure: data.failure.clone(),
            destination: self.destination.clone(),
            accepted_pairs: data.accepted,
            delivered_pairs: data.delivered,
            write_failed_pairs: data.write_failed,
            dropped_oldest_pairs: data.dropped_oldest,
            discarded_pairs: data.discarded,
            skipped_pairs: data.skipped,
            padded_pairs: data.padded,
            rejected: data.rejected,
            outstanding_pairs: data.outstanding,
            peak_outstanding_pairs: data.peak_outstanding,
            retained_bytes: data.retained_bytes,
            peak_retained_bytes: data.peak_retained_bytes,
            muxed_bytes: data.muxed_bytes,
            encoded_frames: data.encoded_frames,
            muxed_media_time: data.out_time.unwrap_or_default(),
            media_drift: data.media_drift(clock).unwrap_or_default(),
            connected: data.connected,
            // Retained stderr was redacted before storage; this is a second,
            // cheap pass in case a rule was added after a byte was retained.
            stderr_tail: self.redactor.scrub_text(&clean_tail(&stderr)),
            stderr_truncated: data.stderr_truncated,
        }
    }
}

fn clean_tail(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    text.retain(|character| character == '\n' || character == '\t' || !character.is_control());
    text.trim().to_owned()
}

/// Shared completion state for the two halves of one dispatched pair.
struct Completion {
    shared: Arc<Shared>,
    bytes: usize,
    remaining: AtomicUsize,
    successful: AtomicBool,
}

impl Completion {
    fn finish_part(&self, successful: bool) {
        if !successful {
            self.successful.store(false, Ordering::Release);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut data = self.shared.lock();
            if !data.frozen {
                data.outstanding = data.outstanding.saturating_sub(1);
                data.retained_bytes = data.retained_bytes.saturating_sub(self.bytes);
                if self.successful.load(Ordering::Acquire) {
                    data.delivered = data.delivered.saturating_add(1);
                } else {
                    data.write_failed = data.write_failed.saturating_add(1);
                }
            }
            drop(data);
            self.shared.signal.notify_all();
        }
    }
}

/// What one write consumes: a real pair's payload, or synthesized padding.
enum Payload {
    /// A pair's own video or audio bytes. Also used for padding video, where
    /// the previously delivered pair is simply repeated.
    Pair(Arc<PairedFrame>),
    /// Padding audio: `length` bytes taken from a shared zero buffer, sized for
    /// the exact sequence being padded.
    Silence { zeros: Arc<Vec<u8>>, length: usize },
}

struct WritePart {
    payload: Payload,
    input: MediaInput,
    /// `None` for padding: synthesized pairs were never admitted, so they own
    /// no share of `outstanding` or `retained_bytes`.
    completion: Option<Arc<Completion>>,
}

impl WritePart {
    fn bytes(&self) -> &[u8] {
        match (&self.payload, self.input) {
            (Payload::Pair(frame), MediaInput::Video) => frame.rgba(),
            (Payload::Pair(frame), MediaInput::Audio) => frame.audio_f32le(),
            (Payload::Silence { zeros, length }, _) => &zeros[..*length],
        }
    }

    fn complete(mut self, successful: bool) {
        if let Some(completion) = self.completion.take() {
            completion.finish_part(successful);
        }
    }
}

impl Drop for WritePart {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            completion.finish_part(false);
        }
    }
}

struct Endpoint {
    listener: TcpListener,
    path: String,
    input: MediaInput,
}

enum CleanupWorker {
    Unit(JoinHandle<()>),
}

/// Long-lived bounded RTMP/RTMPS sink over one direct `FFmpeg` child.
pub struct Streamer {
    format: RecordFormat,
    limits: StreamLimits,
    shared: Arc<Shared>,
    child: Arc<Mutex<Child>>,
    workers: Vec<JoinHandle<()>>,
    monitor: Option<JoinHandle<()>>,
    report: Option<StopReport>,
}

impl Streamer {
    /// Validates the configuration, binds two authenticated loopback HTTP
    /// inputs, starts `FFmpeg`, and confirms the child's first input request.
    ///
    /// The destination scheme is checked before anything is spawned. No error
    /// returned by this function carries the stream key.
    ///
    /// # Errors
    ///
    /// Returns a typed, redacted [`StartError`]. [`StartError::cleanup`] says
    /// explicitly whether child reaping may still be running.
    #[allow(clippy::too_many_lines)]
    pub fn start(config: StreamConfig) -> Result<Self, StartError> {
        validate_limits(
            &config.format,
            config.limits,
            config.overflow,
            config.encoder,
        )
        .map_err(|error| StartError::complete(StartErrorKind::InvalidLimits(error)))?;
        let layout = flv_channel_layout(config.format.channel_layout())
            .ok_or_else(|| StartError::complete(StartErrorKind::UnsupportedChannelLayout))?;
        let executable = streamer_executable(config.ffmpeg.clone())?;
        let video_token = random_token()?;
        let audio_token = random_token()?;
        let video_listener = bind_listener(MediaInput::Video)?;
        let audio_listener = bind_listener(MediaInput::Audio)?;
        let video_address = listener_address(&video_listener, MediaInput::Video)?;
        let audio_address = listener_address(&audio_listener, MediaInput::Audio)?;
        let args = command_args(
            &config,
            layout,
            &Inputs {
                video_address,
                audio_address,
                video_token: &video_token,
                audio_token: &audio_token,
            },
        );
        let redactor = Redactor::new(&config.destination, [&video_token, &audio_token]);
        let shared = Arc::new(Shared::new(
            &config.format,
            config.limits,
            config.overflow,
            &config.destination,
            redactor,
        ));
        let (monitor_sender, monitor) = spawn_monitor_waiter()?;
        let child = match command(&executable, &args).spawn() {
            Ok(child) => Arc::new(Mutex::new(child)),
            Err(error) => {
                drop(monitor_sender);
                let _ = monitor.join();
                return Err(StartError::complete(spawn_error(&error)));
            }
        };
        if monitor_sender
            .send((Arc::clone(&child), Arc::clone(&shared)))
            .is_err()
        {
            let cleanup = terminate_without_monitor(&child, config.limits.kill_timeout);
            return Err(StartError {
                kind: StartErrorKind::ThreadSpawn(io::ErrorKind::Other),
                cleanup,
            });
        }
        drop(monitor_sender);
        let (stdout, stderr) = {
            let mut guard = child.lock().unwrap_or_else(PoisonError::into_inner);
            (guard.stdout.take(), guard.stderr.take())
        };
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            return Err(cleanup_startup(
                StartErrorKind::MissingPipe,
                &child,
                &shared,
                Vec::new(),
                monitor,
                config.limits.kill_timeout,
            ));
        };
        let mut workers = Vec::new();
        for worker in [
            spawn_progress(stdout, Arc::clone(&shared)),
            spawn_stderr(stderr, Arc::clone(&shared)),
        ] {
            match worker {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    return Err(cleanup_startup(
                        StartErrorKind::ThreadSpawn(error.kind()),
                        &child,
                        &shared,
                        workers,
                        monitor,
                        config.limits.kill_timeout,
                    ));
                }
            }
        }

        let mut video = Endpoint {
            listener: video_listener,
            path: format!("/{video_token}"),
            input: MediaInput::Video,
        };
        let mut audio = Endpoint {
            listener: audio_listener,
            path: format!("/{audio_token}"),
            input: MediaInput::Audio,
        };
        let deadline = Instant::now() + config.limits.connect_timeout;
        let first = wait_for_first_http(
            &mut video,
            &mut audio,
            deadline,
            config.limits.no_progress_timeout,
            &shared,
        );
        let (first_input, first_stream) = match first {
            Ok(value) => value,
            Err(kind) => {
                return Err(cleanup_startup(
                    kind,
                    &child,
                    &shared,
                    workers,
                    monitor,
                    config.limits.kill_timeout,
                ));
            }
        };
        {
            let mut data = shared.lock();
            if data.failure.is_some() || data.child.is_some() {
                let kind = StartErrorKind::EarlyExit {
                    status: data.child.and_then(|child| child.code),
                    stderr: shared.telemetry_locked(&data).stderr_tail,
                };
                drop(data);
                return Err(cleanup_startup(
                    kind,
                    &child,
                    &shared,
                    workers,
                    monitor,
                    config.limits.kill_timeout,
                ));
            }
            data.state = StreamState::Streaming;
        }

        // A one-deep buffer, not a rendezvous: see `WRITER_BACKLOG`. Everything
        // handed to a writer is still counted in `outstanding`, so the admission
        // bound stays honest either way.
        let (video_sender, video_receiver) = mpsc::sync_channel(WRITER_BACKLOG);
        let (audio_sender, audio_receiver) = mpsc::sync_channel(WRITER_BACKLOG);
        let (connected, pending, pending_receiver) = match first_input {
            MediaInput::Video => ((video, first_stream, video_receiver), audio, audio_receiver),
            MediaInput::Audio => ((audio, first_stream, audio_receiver), video, video_receiver),
        };
        for worker in [
            spawn_writer(connected.0, connected.1, connected.2, Arc::clone(&shared)),
            spawn_pending_writer(pending, pending_receiver, Arc::clone(&shared)),
        ] {
            match worker {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    return Err(cleanup_startup(
                        StartErrorKind::ThreadSpawn(error.kind()),
                        &child,
                        &shared,
                        workers,
                        monitor,
                        config.limits.kill_timeout,
                    ));
                }
            }
        }
        match spawn_dispatcher(Arc::clone(&shared), video_sender, audio_sender) {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                return Err(cleanup_startup(
                    StartErrorKind::ThreadSpawn(error.kind()),
                    &child,
                    &shared,
                    workers,
                    monitor,
                    config.limits.kill_timeout,
                ));
            }
        }
        Ok(Self {
            format: config.format,
            limits: config.limits,
            shared,
            child,
            workers,
            monitor: Some(monitor),
            report: None,
        })
    }

    /// The destination with its stream key replaced by `****`.
    #[must_use]
    pub fn destination(&self) -> &str {
        &self.shared.destination
    }

    /// Admits one pair, waiting at most `enqueue_timeout` for space.
    ///
    /// This is called from the render thread, so it is nonblocking by default
    /// and can never wait longer than one frame period. Sequence numbers must
    /// strictly increase; gaps are accepted, counted as `skipped_pairs`, and
    /// padded out to the child so the media clock keeps tracking wall clock.
    /// Every refusal is counted by reason and returns the pair to the caller.
    ///
    /// # Errors
    ///
    /// Returns the original pair with a classified [`EnqueueRejection`].
    pub fn enqueue(&mut self, frame: PairedFrame) -> Result<(), EnqueueError> {
        let deadline = Instant::now().checked_add(self.limits.enqueue_timeout);
        let bytes = frame.retained_bytes();
        let mut evicted = false;
        let mut data = self.shared.lock();
        loop {
            if self.shared.kill_requested.load(Ordering::Acquire) {
                self.shared.fail_locked(&mut data, StreamFailure::Cancelled);
            }
            if let Some(failure) = data.failure.clone() {
                data.rejected.failed = data.rejected.failed.saturating_add(1);
                return Err(reject(EnqueueRejection::Failed(failure), frame));
            }
            match data.state {
                StreamState::Streaming => {}
                StreamState::Stopped => {
                    data.rejected.not_streaming = data.rejected.not_streaming.saturating_add(1);
                    return Err(reject(EnqueueRejection::Stopped, frame));
                }
                _ => {
                    data.rejected.not_streaming = data.rejected.not_streaming.saturating_add(1);
                    return Err(reject(EnqueueRejection::Stopping, frame));
                }
            }
            if !self.matches_format(&frame) {
                data.rejected.format_mismatch = data.rejected.format_mismatch.saturating_add(1);
                return Err(reject(EnqueueRejection::FormatMismatch, frame));
            }
            let sequence = frame.sequence();
            if sequence < data.minimum_sequence {
                data.rejected.sequence = data.rejected.sequence.saturating_add(1);
                return Err(reject(
                    EnqueueRejection::Sequence {
                        minimum: data.minimum_sequence,
                        actual: sequence,
                    },
                    frame,
                ));
            }
            let Some(next) = sequence.checked_next() else {
                data.rejected.sequence = data.rejected.sequence.saturating_add(1);
                return Err(reject(EnqueueRejection::SequenceExhausted, frame));
            };
            let fits = data.outstanding < self.limits.max_outstanding_pairs
                && data
                    .retained_bytes
                    .checked_add(bytes)
                    .is_some_and(|total| total <= self.limits.max_retained_bytes);
            if fits {
                data.skipped = data
                    .skipped
                    .saturating_add(sequence.get() - data.minimum_sequence.get());
                data.minimum_sequence = next;
                data.outstanding += 1;
                data.peak_outstanding = data.peak_outstanding.max(data.outstanding);
                data.retained_bytes += bytes;
                data.peak_retained_bytes = data.peak_retained_bytes.max(data.retained_bytes);
                data.accepted = data.accepted.saturating_add(1);
                data.queue.push_back(Queued { frame, bytes });
                self.shared.signal.notify_all();
                return Ok(());
            }
            // Exactly one eviction per admission: `DropOldest` trades the
            // oldest pair for the newest one. A pair that still does not fit
            // after that is over the per-pair budget the limits were validated
            // against, and must be refused rather than allowed to drain the
            // whole queue behind it.
            if self.shared.overflow == OverflowPolicy::DropOldest
                && !evicted
                && let Some(queued) = data.queue.pop_front()
            {
                evicted = true;
                data.outstanding = data.outstanding.saturating_sub(1);
                data.retained_bytes = data.retained_bytes.saturating_sub(queued.bytes);
                data.dropped_oldest = data.dropped_oldest.saturating_add(1);
                drop(queued);
                continue;
            }
            let now = Instant::now();
            match deadline {
                Some(deadline) if now < deadline => {
                    let (guard, _) = self
                        .shared
                        .signal
                        .wait_timeout(data, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner);
                    data = guard;
                }
                _ => {
                    let reason = if data.outstanding >= self.limits.max_outstanding_pairs {
                        data.rejected.queue_full = data.rejected.queue_full.saturating_add(1);
                        EnqueueRejection::QueueFull
                    } else {
                        data.rejected.retained_byte_limit =
                            data.rejected.retained_byte_limit.saturating_add(1);
                        EnqueueRejection::RetainedByteLimit
                    };
                    return Err(reject(reason, frame));
                }
            }
        }
    }

    #[must_use]
    pub fn telemetry(&self) -> StreamTelemetry {
        self.report
            .as_ref()
            .map_or_else(|| self.shared.snapshot(), |report| report.telemetry.clone())
    }

    /// Stops accepting, flushes queued pairs within `stop_timeout`, closes the
    /// inputs, then waits for and if necessary kills and reaps the child.
    /// Repeated calls return the same frozen report.
    #[must_use]
    pub fn stop(&mut self) -> StopReport {
        self.stop_inner(ShutdownMode::Drain)
    }

    /// Immediately stops accepting, abandons queued pairs, and requests child
    /// termination without waiting for any worker or exit status.
    pub fn request_cancel(&mut self) {
        if self.report.is_some() {
            return;
        }
        self.shared.kill_requested.store(true, Ordering::Release);
        self.shared.aborting.store(true, Ordering::Release);
        match self.shared.data.try_lock() {
            Ok(mut data) => self.mark_cancelled(&mut data),
            Err(TryLockError::Poisoned(error)) => self.mark_cancelled(&mut error.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        self.shared.signal.notify_all();
        try_kill_child(&self.child);
    }

    /// Kills the child immediately and returns the frozen report.
    #[must_use]
    pub fn cancel(&mut self) -> StopReport {
        self.request_cancel();
        self.stop_inner(ShutdownMode::Cancel)
    }

    fn mark_cancelled(&self, data: &mut SharedData) {
        let deadline = data.shutdown.map_or_else(
            || Instant::now() + self.limits.kill_timeout,
            |shutdown| shutdown.deadline,
        );
        data.shutdown = Some(Shutdown {
            mode: ShutdownMode::Cancel,
            deadline,
        });
        self.shared.fail_locked(data, StreamFailure::Cancelled);
        self.shared.discard_queue(data);
    }

    /// Compares the whole format, not the payload byte counts.
    ///
    /// Byte counts are not a format: a 48x64 frame has exactly as many RGBA
    /// bytes as a 64x48 one and would be muxed transposed, and mono at 96 kHz
    /// has exactly as many audio bytes as stereo at 48 kHz.
    fn matches_format(&self, frame: &PairedFrame) -> bool {
        frame.format() == &self.format
    }

    #[allow(clippy::too_many_lines)]
    fn stop_inner(&mut self, mode: ShutdownMode) -> StopReport {
        if let Some(report) = &self.report {
            return report.clone();
        }
        let started = Instant::now();
        let drain_deadline = started + self.limits.stop_timeout;
        let mode = {
            let mut data = self.shared.lock();
            let mode = if mode == ShutdownMode::Cancel
                || data.cancelling()
                || self.shared.kill_requested.load(Ordering::Acquire)
            {
                ShutdownMode::Cancel
            } else {
                ShutdownMode::Drain
            };
            if data.state != StreamState::Failed {
                data.state = StreamState::Stopping;
            }
            if data.shutdown.is_none() {
                data.shutdown = Some(Shutdown {
                    mode,
                    deadline: drain_deadline,
                });
            }
            if mode == ShutdownMode::Cancel {
                self.mark_cancelled(&mut data);
            }
            drop(data);
            self.shared.signal.notify_all();
            mode
        };

        let mut killed = mode == ShutdownMode::Cancel;
        let cleanup_deadline = if mode == ShutdownMode::Cancel {
            request_kill(&self.child);
            started + self.limits.kill_timeout
        } else {
            loop {
                poll_child(&self.child, &self.shared);
                let data = self.shared.lock();
                let exited = data.child.is_some();
                let failed = data.failure.is_some();
                drop(data);
                if exited {
                    break drain_deadline;
                }
                if failed || Instant::now() >= drain_deadline {
                    if !failed {
                        self.shared.fail(StreamFailure::StopTimedOut);
                    }
                    killed = true;
                    let kill_deadline = Instant::now() + self.limits.kill_timeout;
                    {
                        let mut data = self.shared.lock();
                        data.shutdown = Some(Shutdown {
                            mode: ShutdownMode::Cancel,
                            deadline: kill_deadline,
                        });
                        self.shared.discard_queue(&mut data);
                    }
                    self.shared.kill_requested.store(true, Ordering::Release);
                    self.shared.aborting.store(true, Ordering::Release);
                    self.shared.signal.notify_all();
                    request_kill(&self.child);
                    break kill_deadline;
                }
                thread::sleep(POLL_INTERVAL);
            }
        };

        wait_until(cleanup_deadline, || {
            poll_child(&self.child, &self.shared);
            self.workers.iter().all(JoinHandle::is_finished)
                && self.monitor.as_ref().is_none_or(JoinHandle::is_finished)
        });
        poll_child(&self.child, &self.shared);
        let child_result = self.shared.lock().child;
        if child_result.is_none() {
            self.shared.fail(StreamFailure::KillTimedOut);
        } else if child_result.is_some_and(|child| !child.success) && !killed {
            self.shared.fail(StreamFailure::ChildExited {
                status: child_result.and_then(|child| child.code),
            });
        }

        let mut unfinished = Vec::new();
        for worker in self
            .workers
            .drain(..)
            .chain(self.monitor.take())
            .collect::<Vec<_>>()
        {
            if worker.is_finished() {
                if worker.join().is_err() {
                    self.shared.fail(StreamFailure::WorkerPanicked);
                }
            } else {
                unfinished.push(CleanupWorker::Unit(worker));
            }
        }
        if !unfinished.is_empty() {
            self.shared.fail(StreamFailure::CleanupUnconfirmed);
        }
        let (telemetry, final_child) = {
            let mut data = self.shared.lock();
            // Anything still queued was never handed to the child; count it as
            // discarded rather than leaving it reported as outstanding.
            self.shared.discard_queue(&mut data);
            if data.failure.is_none()
                && data.child.is_some_and(|child| child.success)
                && unfinished.is_empty()
            {
                data.state = StreamState::Stopped;
            } else {
                data.state = StreamState::Failed;
            }
            data.frozen = true;
            (self.shared.telemetry_locked(&data), data.child)
        };
        let cleanup = if unfinished.is_empty() && final_child.is_some() {
            CleanupStatus::Complete
        } else {
            spawn_fallback_cleanup(unfinished);
            CleanupStatus::Unconfirmed
        };
        let outcome = if final_child.is_none() {
            StopOutcome::KillTimedOut
        } else if killed {
            StopOutcome::Killed
        } else if telemetry.failure.is_some() {
            StopOutcome::Failed
        } else {
            StopOutcome::Clean
        };
        let report = StopReport {
            outcome,
            exit_status: final_child.and_then(|child| child.code),
            cleanup,
            telemetry,
        };
        self.report = Some(report.clone());
        report
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn reject(reason: EnqueueRejection, frame: PairedFrame) -> EnqueueError {
    EnqueueError {
        reason,
        frame: Box::new(frame),
    }
}

fn expected_audio_bytes(format: &RecordFormat, sequence: SequenceNumber) -> Option<usize> {
    let rate = format.frame_rate();
    let hertz = u128::from(format.sample_rate().hertz());
    let boundary = |frames: u64| -> Option<u128> {
        u128::from(frames)
            .checked_mul(hertz)?
            .checked_mul(u128::from(rate.denominator()))
            .map(|value| value / u128::from(rate.numerator()))
    };
    let start = boundary(sequence.get())?;
    let end = boundary(sequence.get().checked_add(1)?)?;
    usize::try_from(end - start)
        .ok()?
        .checked_mul(format.channel_layout().channels().len())?
        .checked_mul(size_of::<f32>())
}

/// One frame period, rounded up.
fn frame_period(format: &RecordFormat) -> Duration {
    let rate = format.frame_rate();
    let nanos = u128::from(rate.denominator())
        .saturating_mul(1_000_000_000)
        .div_ceil(u128::from(rate.numerator()));
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// How many pairs the dispatcher may synthesize to close one gap.
///
/// Padding restores the media clock at real time and no faster: the child
/// consumes it at the cadence it was told the input runs at. A gap wider than
/// the no-progress budget therefore cannot be closed by padding at all, because
/// the sink is already failing by the time the padding would finish, so the
/// burst is bounded there and the watchdog owns the rest.
fn maximum_padding_pairs(format: &RecordFormat, no_progress: Duration) -> u64 {
    let rate = format.frame_rate();
    let pairs = no_progress
        .as_nanos()
        .saturating_mul(u128::from(rate.numerator()))
        / u128::from(rate.denominator()).saturating_mul(1_000_000_000);
    u64::try_from(pairs).unwrap_or(u64::MAX).max(1)
}

/// Pairs that are admitted but no longer recallable: one being written, up to
/// `WRITER_BACKLOG` buffered for the writer, and one held by the dispatcher
/// while it hands the previous one over.
const COMMITTED_PAIRS: usize = WRITER_BACKLOG + 2;

fn validate_limits(
    format: &RecordFormat,
    limits: StreamLimits,
    overflow: OverflowPolicy,
    encoder: EncoderSettings,
) -> Result<(), LimitsError> {
    if limits.max_outstanding_pairs == 0 {
        return Err(LimitsError::ZeroOutstandingPairs);
    }
    if overflow == OverflowPolicy::DropOldest && limits.max_outstanding_pairs <= COMMITTED_PAIRS {
        return Err(LimitsError::OutstandingPairsTooFewToDropOldest {
            required: COMMITTED_PAIRS + 1,
        });
    }
    if limits.max_retained_bytes == 0 {
        return Err(LimitsError::ZeroRetainedBytes);
    }
    if limits.connect_timeout.is_zero()
        || limits.no_progress_timeout.is_zero()
        || limits.stop_timeout.is_zero()
        || limits.kill_timeout.is_zero()
    {
        return Err(LimitsError::ZeroTimeout);
    }
    if limits.enqueue_timeout > frame_period(format) {
        return Err(LimitsError::EnqueueTimeoutTooLong);
    }
    if limits.max_stderr_bytes == 0 {
        return Err(LimitsError::ZeroStderrBytes);
    }
    if limits.max_stderr_bytes > MAX_STDERR_BYTES {
        return Err(LimitsError::StderrTooLarge);
    }
    let now = Instant::now();
    for timeout in [
        limits.enqueue_timeout,
        limits.connect_timeout,
        limits.no_progress_timeout,
        limits.stop_timeout,
        limits.kill_timeout,
    ] {
        if now.checked_add(timeout).is_none() {
            return Err(LimitsError::TimeoutOverflow);
        }
    }
    if !(1..=100_000).contains(&encoder.video_bitrate_kbps) {
        return Err(LimitsError::VideoBitrate);
    }
    if !(8..=1_024).contains(&encoder.audio_bitrate_kbps) {
        return Err(LimitsError::AudioBitrate);
    }
    if !(1..=10).contains(&encoder.keyframe_interval_seconds) {
        return Err(LimitsError::KeyframeInterval);
    }
    // Derived from `PairedFrame`'s own accounting rather than restated here, so
    // adding a field to a pair cannot silently invalidate this budget.
    let required = format
        .maximum_pair_bytes()
        .and_then(|bytes| bytes.checked_mul(limits.max_outstanding_pairs))
        .ok_or(LimitsError::ByteCountOverflow)?;
    if required > limits.max_retained_bytes {
        return Err(LimitsError::RetainedBytesTooSmall {
            required,
            maximum: limits.max_retained_bytes,
        });
    }
    Ok(())
}

/// FLV carries one or two audio channels; wider layouts are refused rather
/// than silently downmixed.
fn flv_channel_layout(layout: &ChannelLayout) -> Option<&'static str> {
    match layout.channels() {
        [Channel::Mono] => Some("mono"),
        [Channel::Left, Channel::Right] => Some("stereo"),
        _ => None,
    }
}

fn random_token() -> Result<String, StartError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| StartError::complete(StartErrorKind::Randomness))?;
    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

fn bind_listener(input: MediaInput) -> Result<TcpListener, StartError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|error| {
        StartError::complete(StartErrorKind::Bind {
            input,
            kind: error.kind(),
        })
    })?;
    listener.set_nonblocking(true).map_err(|error| {
        StartError::complete(StartErrorKind::Bind {
            input,
            kind: error.kind(),
        })
    })?;
    Ok(listener)
}

fn listener_address(listener: &TcpListener, input: MediaInput) -> Result<SocketAddr, StartError> {
    listener.local_addr().map_err(|error| {
        StartError::complete(StartErrorKind::Bind {
            input,
            kind: error.kind(),
        })
    })
}

fn wait_for_first_http(
    video: &mut Endpoint,
    audio: &mut Endpoint,
    deadline: Instant,
    no_progress: Duration,
    shared: &Shared,
) -> Result<(MediaInput, TcpStream), StartErrorKind> {
    while Instant::now() < deadline {
        for endpoint in [&mut *video, &mut *audio] {
            if let Some(stream) = try_accept_http(endpoint, deadline, no_progress)? {
                return Ok((endpoint.input, stream));
            }
        }
        let data = shared.lock();
        if let Some(child) = data.child {
            return Err(StartErrorKind::EarlyExit {
                status: child.code,
                stderr: shared.telemetry_locked(&data).stderr_tail,
            });
        }
        drop(data);
        thread::sleep(POLL_INTERVAL);
    }
    Err(StartErrorKind::ConnectTimeout {
        input: MediaInput::Video,
    })
}

fn try_accept_http(
    endpoint: &Endpoint,
    deadline: Instant,
    no_progress: Duration,
) -> Result<Option<TcpStream>, StartErrorKind> {
    let (mut stream, peer) = match endpoint.listener.accept() {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(error) => {
            return Err(StartErrorKind::Connect {
                input: endpoint.input,
                kind: error.kind(),
            });
        }
    };
    if !peer.ip().is_loopback() {
        return Ok(None);
    }
    configure_http_stream(&stream, deadline, no_progress).map_err(|error| {
        StartErrorKind::Connect {
            input: endpoint.input,
            kind: error.kind(),
        }
    })?;
    match read_http_request(&mut stream, deadline, no_progress) {
        Ok(request) if request_path(&request) == Some(endpoint.path.as_str()) => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                )
                .map_err(|error| StartErrorKind::Connect {
                    input: endpoint.input,
                    kind: error.kind(),
                })?;
            stream
                .set_read_timeout(None)
                .and_then(|()| stream.set_write_timeout(Some(WRITE_POLL)))
                .map_err(|error| StartErrorKind::Connect {
                    input: endpoint.input,
                    kind: error.kind(),
                })?;
            Ok(Some(stream))
        }
        Ok(_) => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            Ok(None)
        }
        Err(HttpRequestError::TooLarge) => {
            let _ = stream.write_all(
                b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            Ok(None)
        }
        Err(HttpRequestError::Timeout) => Err(StartErrorKind::ConnectTimeout {
            input: endpoint.input,
        }),
        Err(HttpRequestError::Io) if Instant::now() >= deadline => {
            Err(StartErrorKind::ConnectTimeout {
                input: endpoint.input,
            })
        }
        Err(HttpRequestError::Io) => Ok(None),
    }
}

fn configure_http_stream(
    stream: &TcpStream,
    deadline: Instant,
    no_progress: Duration,
) -> io::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = remaining.min(no_progress).max(Duration::from_millis(1));
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

enum HttpRequestError {
    TooLarge,
    Timeout,
    Io,
}

fn read_http_request(
    stream: &mut TcpStream,
    deadline: Instant,
    no_progress: Duration,
) -> Result<Vec<u8>, HttpRequestError> {
    let mut request = Vec::with_capacity(512);
    let mut buffer = [0_u8; 512];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(HttpRequestError::Timeout);
        }
        let timeout = deadline
            .saturating_duration_since(now)
            .min(no_progress)
            .max(Duration::from_millis(1));
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| HttpRequestError::Io)?;
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) && Instant::now() >= deadline =>
            {
                return Err(HttpRequestError::Timeout);
            }
            Err(_) => return Err(HttpRequestError::Io),
        };
        if count == 0 {
            return Err(HttpRequestError::Io);
        }
        if request.len().saturating_add(count) > MAX_HTTP_HEADER_BYTES {
            return Err(HttpRequestError::TooLarge);
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

fn request_path(request: &[u8]) -> Option<&str> {
    let line_end = request.windows(2).position(|window| window == b"\r\n")?;
    let line = std::str::from_utf8(&request[..line_end]).ok()?;
    let mut parts = line.split(' ');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("GET"), Some(path), Some("HTTP/1.1" | "HTTP/1.0"), None) => Some(path),
        _ => None,
    }
}

fn spawn_writer(
    endpoint: Endpoint,
    stream: TcpStream,
    receiver: mpsc::Receiver<WritePart>,
    shared: Arc<Shared>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("fm-ffmpeg-stream-{:?}", endpoint.input))
        .spawn(move || {
            write_parts(stream, None, &receiver, endpoint.input, &shared);
            drop(endpoint);
        })
}

/// The child opens its second input only after the first one has produced
/// data, so that endpoint is accepted lazily, on the first part to write.
fn spawn_pending_writer(
    endpoint: Endpoint,
    receiver: mpsc::Receiver<WritePart>,
    shared: Arc<Shared>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("fm-ffmpeg-stream-pending-{:?}", endpoint.input))
        .spawn(move || {
            let Ok(first) = receiver.recv() else {
                // Nothing was ever streamed: accept and close so a child that
                // is waiting on this input observes end of input promptly.
                let deadline = shared
                    .lock()
                    .shutdown
                    .map_or_else(Instant::now, |shutdown| shutdown.deadline);
                if let Ok(stream) = accept_http_until(&endpoint, deadline, &shared) {
                    drop(stream);
                }
                return;
            };
            let deadline = Instant::now() + shared.limits.connect_timeout;
            match accept_http_until(&endpoint, deadline, &shared) {
                Ok(stream) => write_parts(stream, Some(first), &receiver, endpoint.input, &shared),
                Err(failure) => {
                    shared.fail(failure);
                    first.complete(false);
                }
            }
        })
}

fn accept_http_until(
    endpoint: &Endpoint,
    deadline: Instant,
    shared: &Shared,
) -> Result<TcpStream, StreamFailure> {
    while Instant::now() < deadline {
        if shared.is_aborting() {
            return Err(StreamFailure::Cancelled);
        }
        // A child that has already exited will never open this input, so do
        // not hold shutdown open for the rest of the accept budget.
        if let Some(child) = shared.lock().child {
            return Err(StreamFailure::ChildExited { status: child.code });
        }
        match try_accept_http(endpoint, deadline, shared.limits.no_progress_timeout) {
            Ok(Some(stream)) => return Ok(stream),
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(StartErrorKind::Connect { kind, .. }) => {
                return Err(StreamFailure::Connect {
                    input: endpoint.input,
                    kind,
                });
            }
            Err(_) => {
                return Err(StreamFailure::InputTimeout {
                    input: endpoint.input,
                });
            }
        }
    }
    Err(StreamFailure::InputTimeout {
        input: endpoint.input,
    })
}

fn write_parts(
    mut stream: TcpStream,
    first: Option<WritePart>,
    receiver: &mpsc::Receiver<WritePart>,
    input: MediaInput,
    shared: &Shared,
) {
    for part in first.into_iter().chain(receiver.iter()) {
        let successful = write_part(&mut stream, &part, input, shared);
        part.complete(successful);
        if !successful {
            return;
        }
    }
}

/// Writes one payload under the configured no-progress budget.
///
/// The socket's own timeout is kept short so cancellation is observed promptly,
/// but a child that accepts nothing is a terminal failure after
/// `no_progress_timeout`, not after `stop_timeout`: a wedged input writer would
/// otherwise hold a graceful drain open for the entire stop budget.
fn write_part(
    stream: &mut TcpStream,
    part: &WritePart,
    input: MediaInput,
    shared: &Shared,
) -> bool {
    let bytes = part.bytes();
    let mut offset = 0;
    let mut moved = Instant::now();
    while offset < bytes.len() {
        if shared.is_aborting() {
            return false;
        }
        match stream.write(&bytes[offset..]) {
            Ok(0) => {
                shared.fail_write(input, io::ErrorKind::WriteZero);
                return false;
            }
            Ok(count) => {
                offset += count;
                moved = Instant::now();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now().saturating_duration_since(moved)
                    > shared.limits.no_progress_timeout
                {
                    shared.fail_write(input, io::ErrorKind::TimedOut);
                    return false;
                }
            }
            Err(error) => {
                shared.fail_write(input, error.kind());
                return false;
            }
        }
    }
    true
}

/// Hands one pair to both writers, and closes any gap in the dispatched
/// sequence before it so the child's media clock keeps tracking wall clock.
struct Dispatcher {
    shared: Arc<Shared>,
    video: mpsc::SyncSender<WritePart>,
    audio: mpsc::SyncSender<WritePart>,
    /// The last pair actually handed to the writers, repeated as padding video.
    previous: Option<Arc<PairedFrame>>,
    /// The sequence the next dispatched pair should carry if nothing is lost.
    expected: Option<SequenceNumber>,
    zeros: Arc<Vec<u8>>,
    maximum_padding: u64,
}

impl Dispatcher {
    /// Returns false once the dispatcher must stop.
    fn send_pair(&mut self, video: WritePart, audio: WritePart) -> bool {
        if let Err(error) = self.video.send(video) {
            error.0.complete(false);
            audio.complete(false);
            self.shared
                .fail(StreamFailure::DispatcherClosed(MediaInput::Video));
            return false;
        }
        if let Err(error) = self.audio.send(audio) {
            error.0.complete(false);
            self.shared
                .fail(StreamFailure::DispatcherClosed(MediaInput::Audio));
            return false;
        }
        true
    }

    fn pad_to(&mut self, sequence: SequenceNumber) -> bool {
        let (Some(previous), Some(expected)) = (self.previous.clone(), self.expected) else {
            return true;
        };
        let gap = sequence.get().saturating_sub(expected.get());
        for index in 0..gap.min(self.maximum_padding) {
            if self.shared.is_aborting() {
                return false;
            }
            let missing = SequenceNumber::new(expected.get().saturating_add(index));
            let Some(length) = expected_audio_bytes(&self.shared.format, missing) else {
                return true;
            };
            let repeat = WritePart {
                payload: Payload::Pair(Arc::clone(&previous)),
                input: MediaInput::Video,
                completion: None,
            };
            let silence = WritePart {
                payload: Payload::Silence {
                    zeros: Arc::clone(&self.zeros),
                    length: length.min(self.zeros.len()),
                },
                input: MediaInput::Audio,
                completion: None,
            };
            if !self.send_pair(repeat, silence) {
                return false;
            }
            let mut data = self.shared.lock();
            data.padded = data.padded.saturating_add(1);
        }
        true
    }

    fn dispatch(&mut self, queued: Queued) -> bool {
        let sequence = queued.frame.sequence();
        if !self.pad_to(sequence) {
            return false;
        }
        let completion = Arc::new(Completion {
            shared: Arc::clone(&self.shared),
            bytes: queued.bytes,
            remaining: AtomicUsize::new(2),
            successful: AtomicBool::new(true),
        });
        let frame = Arc::new(queued.frame);
        let video = WritePart {
            payload: Payload::Pair(Arc::clone(&frame)),
            input: MediaInput::Video,
            completion: Some(Arc::clone(&completion)),
        };
        let audio = WritePart {
            payload: Payload::Pair(Arc::clone(&frame)),
            input: MediaInput::Audio,
            completion: Some(completion),
        };
        self.expected = sequence.checked_next();
        self.previous = Some(frame);
        self.send_pair(video, audio)
    }
}

fn spawn_dispatcher(
    shared: Arc<Shared>,
    video: mpsc::SyncSender<WritePart>,
    audio: mpsc::SyncSender<WritePart>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("fm-ffmpeg-stream-dispatcher".to_owned())
        .spawn(move || {
            let zeros = vec![0_u8; shared.format.maximum_audio_bytes().unwrap_or_default()];
            let maximum_padding =
                maximum_padding_pairs(&shared.format, shared.limits.no_progress_timeout);
            let mut dispatcher = Dispatcher {
                shared: Arc::clone(&shared),
                video,
                audio,
                previous: None,
                expected: None,
                zeros: Arc::new(zeros),
                maximum_padding,
            };
            loop {
                let queued = {
                    let mut data = shared.lock();
                    loop {
                        if data.cancelling() || shared.kill_requested.load(Ordering::Acquire) {
                            shared.discard_queue(&mut data);
                            return;
                        }
                        if let Some(queued) = data.queue.pop_front() {
                            if data.first_dispatch.is_none() {
                                data.first_dispatch = Some(Instant::now());
                            }
                            break queued;
                        }
                        if data.state != StreamState::Streaming {
                            return;
                        }
                        let (guard, _) = shared
                            .signal
                            .wait_timeout(data, POLL_INTERVAL)
                            .unwrap_or_else(PoisonError::into_inner);
                        data = guard;
                    }
                };
                if !dispatcher.dispatch(queued) {
                    return;
                }
            }
        })
}

fn spawn_progress(
    mut stdout: impl Read + Send + 'static,
    shared: Arc<Shared>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("fm-ffmpeg-stream-progress".to_owned())
        .spawn(move || {
            let mut pending = Vec::new();
            let mut buffer = [0_u8; 4 * 1024];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(count) => {
                        pending.extend_from_slice(&buffer[..count]);
                        while let Some(index) = pending.iter().position(|&byte| byte == b'\n') {
                            let line = pending.drain(..=index).collect::<Vec<_>>();
                            shared.observe_progress(&line);
                        }
                        if pending.len() > PROGRESS_PENDING_BYTES {
                            pending.clear();
                        }
                    }
                    Err(error) => {
                        shared.fail(StreamFailure::ProgressRead(error.kind()));
                        return;
                    }
                }
            }
        })
}

/// Redacts captured stderr before it is retained, keeping back the bytes that
/// could still be the head of a secret split across two child writes.
fn spawn_stderr(
    mut stderr: impl Read + Send + 'static,
    shared: Arc<Shared>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("fm-ffmpeg-stream-stderr".to_owned())
        .spawn(move || {
            let carry = shared.redactor.carry();
            let mut pending: Vec<u8> = Vec::new();
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        pending.extend_from_slice(&buffer[..count]);
                        shared.redactor.scrub(&mut pending);
                        let flush = match pending.iter().rposition(|&byte| byte == b'\n') {
                            Some(index) => index + 1,
                            None if pending.len() > STDERR_PENDING_BYTES + carry => {
                                pending.len() - carry
                            }
                            None => 0,
                        };
                        if flush > 0 {
                            shared.append_stderr(&pending[..flush]);
                            pending.drain(..flush);
                        }
                    }
                    Err(error) => {
                        shared.fail(StreamFailure::StderrRead(error.kind()));
                        break;
                    }
                }
            }
            if !pending.is_empty() {
                shared.redactor.scrub(&mut pending);
                shared.append_stderr(&pending);
            }
        })
}

type MonitorTarget = (Arc<Mutex<Child>>, Arc<Shared>);

/// Reaps the direct child and enforces the connect and no-progress deadlines.
fn spawn_monitor_waiter() -> Result<(mpsc::SyncSender<MonitorTarget>, JoinHandle<()>), StartError> {
    let (sender, receiver) = mpsc::sync_channel::<MonitorTarget>(1);
    let monitor = thread::Builder::new()
        .name("fm-ffmpeg-stream-monitor".to_owned())
        .spawn(move || {
            let Ok((child, shared)) = receiver.recv() else {
                return;
            };
            loop {
                if shared.kill_requested.load(Ordering::Acquire) {
                    try_kill_child(&child);
                }
                if let Some(status) = child_status(&child) {
                    shared.record_child(status);
                    return;
                }
                if shared.check_deadlines() {
                    try_kill_child(&child);
                }
                thread::sleep(POLL_INTERVAL);
            }
        })
        .map_err(|error| StartError::complete(StartErrorKind::ThreadSpawn(error.kind())))?;
    Ok((sender, monitor))
}

fn child_status(child: &Arc<Mutex<Child>>) -> Option<ExitStatus> {
    match child.try_lock() {
        Ok(mut child) => child.try_wait().ok().flatten(),
        Err(TryLockError::Poisoned(error)) => error.into_inner().try_wait().ok().flatten(),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn poll_child(child: &Arc<Mutex<Child>>, shared: &Shared) {
    if let Some(status) = child_status(child) {
        shared.record_child(status);
    }
}

fn request_kill(child: &Arc<Mutex<Child>>) {
    let mut child = child.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = child.kill();
}

fn try_kill_child(child: &Arc<Mutex<Child>>) {
    match child.try_lock() {
        Ok(mut child) => {
            let _ = child.kill();
        }
        Err(TryLockError::Poisoned(error)) => {
            let _ = error.into_inner().kill();
        }
        Err(TryLockError::WouldBlock) => {}
    }
}

fn terminate_without_monitor(child: &Arc<Mutex<Child>>, timeout: Duration) -> CleanupStatus {
    request_kill(child);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child_status(child).is_some() {
            return CleanupStatus::Complete;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let holder = Arc::new(Mutex::new(Some(Arc::clone(child))));
    let background = Arc::clone(&holder);
    if thread::Builder::new()
        .name("fm-ffmpeg-stream-emergency-reaper".to_owned())
        .spawn(move || {
            let child = background
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(child) = child {
                let mut child = child.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = child.wait();
            }
        })
        .is_err()
        && let Some(child) = holder.lock().unwrap_or_else(PoisonError::into_inner).take()
    {
        // Thread exhaustion is the sole case where not orphaning the child
        // takes priority over the bounded-return guarantee.
        let mut child = child.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = child.wait();
    }
    CleanupStatus::Unconfirmed
}

fn cleanup_startup(
    kind: StartErrorKind,
    child: &Arc<Mutex<Child>>,
    shared: &Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    monitor: JoinHandle<()>,
    timeout: Duration,
) -> StartError {
    {
        let mut data = shared.lock();
        data.shutdown = Some(Shutdown {
            mode: ShutdownMode::Cancel,
            deadline: Instant::now() + timeout,
        });
    }
    shared.kill_requested.store(true, Ordering::Release);
    shared.aborting.store(true, Ordering::Release);
    shared.signal.notify_all();
    request_kill(child);
    let deadline = Instant::now() + timeout;
    let mut all = workers;
    all.push(monitor);
    wait_until(deadline, || {
        poll_child(child, shared);
        all.iter().all(JoinHandle::is_finished)
    });
    let mut unfinished = Vec::new();
    for worker in all {
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            unfinished.push(CleanupWorker::Unit(worker));
        }
    }
    let child_done = shared.lock().child.is_some();
    let kind = match kind {
        StartErrorKind::EarlyExit { status, .. } => StartErrorKind::EarlyExit {
            status,
            stderr: shared.snapshot().stderr_tail,
        },
        kind => kind,
    };
    let cleanup = if unfinished.is_empty() && child_done {
        CleanupStatus::Complete
    } else {
        shared.lock().frozen = true;
        spawn_fallback_cleanup(unfinished);
        CleanupStatus::Unconfirmed
    };
    StartError { kind, cleanup }
}

fn wait_until(deadline: Instant, mut complete: impl FnMut() -> bool) {
    while Instant::now() < deadline {
        if complete() {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Starts unbounded joining after a bounded public return. If the operating
/// system cannot create this thread, joins synchronously as the last-resort
/// ownership path; only that case may exceed the configured deadline.
fn spawn_fallback_cleanup(workers: Vec<CleanupWorker>) {
    if workers.is_empty() {
        return;
    }
    let holder = Arc::new(Mutex::new(Some(workers)));
    let background = Arc::clone(&holder);
    if thread::Builder::new()
        .name("fm-ffmpeg-stream-fallback-cleanup".to_owned())
        .spawn(move || {
            if let Some(workers) = background
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                join_cleanup_workers(workers);
            }
        })
        .is_err()
        && let Some(workers) = holder.lock().unwrap_or_else(PoisonError::into_inner).take()
    {
        join_cleanup_workers(workers);
    }
}

fn join_cleanup_workers(workers: Vec<CleanupWorker>) {
    for CleanupWorker::Unit(worker) in workers {
        let _ = worker.join();
    }
}

fn streamer_executable(executable: Executable) -> Result<OsString, StartError> {
    match executable {
        Executable::SearchPath => Ok(OsString::from("ffmpeg")),
        Executable::Explicit(path) if path.is_absolute() => {
            let path = fs::canonicalize(path).map_err(|error| {
                StartError::complete(match error.kind() {
                    io::ErrorKind::NotFound => {
                        StartErrorKind::ToolUnavailable(UnavailableReason::Missing)
                    }
                    io::ErrorKind::PermissionDenied => {
                        StartErrorKind::ToolUnavailable(UnavailableReason::PermissionDenied)
                    }
                    _ => StartErrorKind::InvalidExecutable,
                })
            })?;
            if path.is_file() {
                Ok(path.into_os_string())
            } else {
                Err(StartError::complete(StartErrorKind::InvalidExecutable))
            }
        }
        Executable::Explicit(_) => Err(StartError::complete(StartErrorKind::InvalidExecutable)),
    }
}

fn spawn_error(error: &io::Error) -> StartErrorKind {
    match error.kind() {
        io::ErrorKind::NotFound => StartErrorKind::ToolUnavailable(UnavailableReason::Missing),
        io::ErrorKind::PermissionDenied => {
            StartErrorKind::ToolUnavailable(UnavailableReason::PermissionDenied)
        }
        kind => StartErrorKind::Spawn(kind),
    }
}

fn command(executable: &OsStr, args: &[OsString]) -> Command {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for name in retained_environment_names() {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("LC_ALL", "C");
    command
}

fn retained_environment_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["PATH", "SystemRoot", "WINDIR", "PATHEXT"]
    }
    #[cfg(not(windows))]
    {
        &["PATH"]
    }
}

struct Inputs<'a> {
    video_address: SocketAddr,
    audio_address: SocketAddr,
    video_token: &'a str,
    audio_token: &'a str,
}

fn command_args(config: &StreamConfig, layout: &str, inputs: &Inputs<'_>) -> Vec<OsString> {
    let format = &config.format;
    let dimensions = format.dimensions();
    let rate = format.frame_rate();
    let fps = u64::from(rate.numerator()).div_ceil(u64::from(rate.denominator()));
    let gop = fps.saturating_mul(u64::from(config.encoder.keyframe_interval_seconds));
    let video_bitrate = format!("{}k", config.encoder.video_bitrate_kbps);
    let values = vec![
        "-nostdin".to_owned(),
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "warning".to_owned(),
        "-nostats".to_owned(),
        "-progress".to_owned(),
        "pipe:1".to_owned(),
        "-stats_period".to_owned(),
        STATS_PERIOD.to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-pixel_format".to_owned(),
        "rgba".to_owned(),
        "-video_size".to_owned(),
        format!("{}x{}", dimensions.width(), dimensions.height()),
        "-framerate".to_owned(),
        format!("{}/{}", rate.numerator(), rate.denominator()),
        // Both raw inputs are fully described on this command line, so there is
        // nothing left to probe. Without this the child spends `analyzeduration`
        // (5s by default) collecting one input before it reads a byte of the
        // other, while the pair writers can only advance in lockstep: the sink
        // deadlocks at startup at every frame size that does not fit in a socket
        // buffer, which is every real broadcast resolution.
        "-probesize".to_owned(),
        PROBE_SIZE.to_owned(),
        "-analyzeduration".to_owned(),
        "0".to_owned(),
        "-protocol_whitelist".to_owned(),
        "http,tcp".to_owned(),
        "-i".to_owned(),
        format!("http://{}/{}", inputs.video_address, inputs.video_token),
        "-f".to_owned(),
        "f32le".to_owned(),
        "-ar".to_owned(),
        format.sample_rate().hertz().to_string(),
        "-ac".to_owned(),
        format.channel_layout().channels().len().to_string(),
        "-channel_layout".to_owned(),
        layout.to_owned(),
        "-probesize".to_owned(),
        PROBE_SIZE.to_owned(),
        "-analyzeduration".to_owned(),
        "0".to_owned(),
        "-protocol_whitelist".to_owned(),
        "http,tcp".to_owned(),
        "-i".to_owned(),
        format!("http://{}/{}", inputs.audio_address, inputs.audio_token),
        "-map".to_owned(),
        "0:v:0".to_owned(),
        "-map".to_owned(),
        "1:a:0".to_owned(),
        "-c:v".to_owned(),
        "libx264".to_owned(),
        "-preset".to_owned(),
        "veryfast".to_owned(),
        "-tune".to_owned(),
        "zerolatency".to_owned(),
        "-pix_fmt".to_owned(),
        "yuv420p".to_owned(),
        "-b:v".to_owned(),
        video_bitrate.clone(),
        "-maxrate".to_owned(),
        video_bitrate,
        "-bufsize".to_owned(),
        format!("{}k", config.encoder.video_bitrate_kbps.saturating_mul(2)),
        "-g".to_owned(),
        gop.to_string(),
        "-keyint_min".to_owned(),
        gop.to_string(),
        "-sc_threshold".to_owned(),
        "0".to_owned(),
        "-force_key_frames".to_owned(),
        format!(
            "expr:gte(t,n_forced*{})",
            config.encoder.keyframe_interval_seconds
        ),
        "-c:a".to_owned(),
        "aac".to_owned(),
        "-b:a".to_owned(),
        format!("{}k", config.encoder.audio_bitrate_kbps),
        "-flush_packets".to_owned(),
        "1".to_owned(),
        "-flvflags".to_owned(),
        "no_duration_filesize".to_owned(),
        "-f".to_owned(),
        "flv".to_owned(),
        config.destination.url.clone(),
    ];
    values.into_iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "live_998877_SuperSecretStreamKey";

    fn destination() -> StreamDestination {
        StreamDestination::parse(&format!("rtmp://ingest.example:1935/app/{KEY}")).unwrap()
    }

    #[test]
    fn destinations_are_scheme_checked_and_never_render_their_key() {
        for rejected in [
            ("", DestinationError::Empty),
            (
                "http://host/app/key12345",
                DestinationError::UnsupportedScheme,
            ),
            (
                "srt://host/app/key12345",
                DestinationError::UnsupportedScheme,
            ),
            ("file:///tmp/x", DestinationError::UnsupportedScheme),
            ("rtmp://", DestinationError::MissingStreamKey),
            ("rtmp:///app/key12345", DestinationError::MissingHost),
            ("rtmp://host/onlyapp", DestinationError::MissingStreamKey),
            ("rtmp://host/app/ab", DestinationError::StreamKeyTooShort),
            (
                "rtmp://user:pw@host/app/key12345",
                DestinationError::EmbeddedCredentials,
            ),
            (
                "rtmp://host/app/key 12345",
                DestinationError::InvalidCharacter,
            ),
        ] {
            assert_eq!(
                StreamDestination::parse(rejected.0),
                Err(rejected.1),
                "{}",
                rejected.0
            );
        }

        let destination = destination();
        assert_eq!(
            destination.redacted(),
            "rtmp://ingest.example:1935/app/****"
        );
        for rendered in [
            format!("{destination}"),
            format!("{destination:?}"),
            format!("{:?}", StreamConfig::new(format(), destination.clone())),
        ] {
            assert!(!rendered.contains(KEY), "{rendered}");
            assert!(rendered.contains("****"), "{rendered}");
        }
        assert_eq!(
            StreamDestination::parse("rtmps://host/app/key12345?token=abcdef")
                .unwrap()
                .redacted(),
            "rtmps://host/app/****"
        );
    }

    fn format() -> RecordFormat {
        RecordFormat::new(
            64,
            48,
            fm_types::FrameRate::new(30, 1).unwrap(),
            fm_types::SampleRate::new(48_000).unwrap(),
            ChannelLayout::stereo(),
            SequenceNumber::new(0),
        )
        .unwrap()
    }

    #[test]
    fn stderr_redaction_survives_chunk_splits_and_ring_truncation() {
        let destination = destination();
        let redactor = Redactor::new(&destination, ["videotoken", "audiotoken"]);
        let shared = Arc::new(Shared::new(
            &format(),
            StreamLimits {
                max_stderr_bytes: 96,
                ..StreamLimits::default()
            },
            OverflowPolicy::Reject,
            &destination,
            redactor.clone(),
        ));

        // Feed the child's real refusal message one byte at a time, which is
        // the worst case for a secret split across reads.
        let message = format!(
            "[out#0/flv @ 0x1] Error opening output rtmp://ingest.example:1935/app/{KEY}: Connection refused\nError opening output file rtmp://ingest.example:1935/app/{KEY}.\n"
        );
        let carry = redactor.carry();
        let mut pending: Vec<u8> = Vec::new();
        for byte in message.as_bytes() {
            pending.push(*byte);
            redactor.scrub(&mut pending);
            let flush = match pending.iter().rposition(|&byte| byte == b'\n') {
                Some(index) => index + 1,
                None if pending.len() > STDERR_PENDING_BYTES + carry => pending.len() - carry,
                None => 0,
            };
            if flush > 0 {
                shared.append_stderr(&pending[..flush]);
                pending.drain(..flush);
            }
            // Nothing retained may ever contain even a prefix of the key.
            let retained = shared.snapshot().stderr_tail;
            assert!(!retained.contains(&KEY[..8]), "{retained}");
        }
        assert!(pending.is_empty());

        let telemetry = shared.snapshot();
        assert!(telemetry.stderr_truncated, "{telemetry:?}");
        assert!(telemetry.stderr_tail.len() <= 96);
        assert!(!telemetry.stderr_tail.contains(&KEY[..8]));
        assert!(telemetry.stderr_tail.contains("****"));
        assert!(!telemetry.destination.contains(KEY));
    }

    #[test]
    fn limits_and_layouts_are_validated_before_any_spawn() {
        let format = format();
        let encoder = EncoderSettings::default();
        let drop_oldest = OverflowPolicy::DropOldest;
        assert_eq!(
            validate_limits(&format, StreamLimits::default(), drop_oldest, encoder),
            Ok(())
        );
        // One frame period at 30fps is 33.3ms; a render thread may not be asked
        // to wait longer than the frame it is producing.
        assert_eq!(
            validate_limits(
                &format,
                StreamLimits {
                    enqueue_timeout: Duration::from_millis(34),
                    ..StreamLimits::default()
                },
                drop_oldest,
                encoder
            ),
            Err(LimitsError::EnqueueTimeoutTooLong)
        );
        assert_eq!(
            validate_limits(
                &format,
                StreamLimits {
                    enqueue_timeout: Duration::from_millis(33),
                    ..StreamLimits::default()
                },
                drop_oldest,
                encoder
            ),
            Ok(())
        );
        // A queue with nothing recallable cannot honour `DropOldest`, but is
        // perfectly usable with `Reject`.
        assert_eq!(
            validate_limits(
                &format,
                StreamLimits {
                    max_outstanding_pairs: COMMITTED_PAIRS,
                    ..StreamLimits::default()
                },
                drop_oldest,
                encoder
            ),
            Err(LimitsError::OutstandingPairsTooFewToDropOldest {
                required: COMMITTED_PAIRS + 1
            })
        );
        assert_eq!(
            validate_limits(
                &format,
                StreamLimits {
                    max_outstanding_pairs: 1,
                    ..StreamLimits::default()
                },
                OverflowPolicy::Reject,
                encoder
            ),
            Ok(())
        );
        assert!(matches!(
            validate_limits(
                &format,
                StreamLimits {
                    max_retained_bytes: 1_024,
                    ..StreamLimits::default()
                },
                drop_oldest,
                encoder
            ),
            Err(LimitsError::RetainedBytesTooSmall { .. })
        ));
        assert_eq!(
            validate_limits(
                &format,
                StreamLimits::default(),
                drop_oldest,
                EncoderSettings {
                    video_bitrate_kbps: 0,
                    ..encoder
                }
            ),
            Err(LimitsError::VideoBitrate)
        );
        assert_eq!(flv_channel_layout(&ChannelLayout::stereo()), Some("stereo"));
        assert_eq!(
            flv_channel_layout(&ChannelLayout::new(vec![Channel::Mono]).unwrap()),
            Some("mono")
        );
        assert_eq!(
            flv_channel_layout(
                &ChannelLayout::new(vec![
                    Channel::Left,
                    Channel::Right,
                    Channel::Center,
                    Channel::LowFrequency,
                    Channel::LeftSurround,
                    Channel::RightSurround,
                ])
                .unwrap()
            ),
            None
        );
    }
}
