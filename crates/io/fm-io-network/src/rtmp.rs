//! A real RTMP/RTMPS transport for [`OutputSet`](crate::OutputSet), one bounded
//! `FFmpeg` child per connection attempt.
//!
//! [`OutputSet`](crate::OutputSet) owns retry budgets, backoff, backup-endpoint rotation, the
//! recovery queue, and failure isolation. [`Streamer`] owns one live RTMP
//! session: an `FFmpeg` child fed raw video and audio over authenticated
//! loopback inputs, muxing `-f flv` to one destination, with automatic
//! reconnection explicitly left to its caller. This module is that caller.
//!
//! # The mapping, including where it is imperfect
//!
//! | [`TransportSink`] | [`Streamer`] |
//! | --- | --- |
//! | [`connect`](TransportSink::connect) | [`Streamer::start`] for the selected endpoint |
//! | [`write`](TransportSink::write) | [`Streamer::enqueue`] of one raw pair |
//! | [`disconnect`](TransportSink::disconnect) | [`Streamer::stop`], or [`Streamer::cancel`] once the session is already dead |
//!
//! Six things do not line up, and are resolved here rather than papered over.
//!
//! **This adapter is narrower than the trait.** [`OutputPacket`] describes an
//! *encoded* packet: it carries encoder latency and a random-access flag, and
//! the state machine's recovery queue is written for encoded GOPs. The streamer
//! accepts only *raw* paired frames, because the child does the encoding. There
//! is no honest way to feed an encoded elementary stream to this transport, so
//! the packet payload is defined here instead: exactly one raw pair, the
//! tightly packed RGBA frame followed by the sample-major `f32le` audio span
//! for the same sequence, built by [`raw_pair_packet`]. A payload this sink did
//! not define is refused as a terminal protocol error rather than muxed as
//! garbage. Anything that wants an encoded transport needs a different sink.
//!
//! **Connect cannot prove the destination is open.** The child opens its RTMP
//! output only after it has media to send, so at [`Streamer::start`] the
//! destination has not been contacted at all. `connect` therefore reports
//! success once the child is running and has requested its first input, and the
//! *first writes* are what prove or refute the destination. That proof is not
//! open-ended: the streamer arms its own `connect_timeout` on the first
//! dispatched pair and reports [`StreamFailure::DestinationTimeout`], which
//! this module classifies as a retryable connect failure, so a destination that
//! accepts TCP and never speaks RTMP still ends in backoff and failover.
//!
//! **Write lends, enqueue owns.** The state machine keeps a packet queued until
//! a write succeeds, so the pair handed to the streamer is copied out of the
//! borrowed payload on every attempt. That copy is what makes a retry after a
//! reconnect possible at all, and it is the price of the two contracts meeting.
//!
//! **Congestion may only be reported for a pair that was refused.** The state
//! machine re-writes the packet at the front of its queue after
//! [`SinkWrite::Congested`]. Reporting congestion for a pair the streamer
//! already admitted would offer the same sequence twice, which the streamer
//! rejects as a terminal protocol error. Congestion here therefore means
//! exactly one thing: the streamer's bounded admission window is full.
//! [`OverflowPolicy::Reject`] is forced for the same reason — the state
//! machine's recovery queue is the loss policy, and a sink that silently
//! dropped the oldest pair would bypass its accounting.
//!
//! **Every raw pair is a random-access point.** Raw frames are independently
//! decodable, and each connection gets a fresh child whose encoder opens its
//! own GOP with an IDR, so the state machine's keyframe recovery degenerates to
//! "resume at the next pair". [`raw_pair_packet`] marks packets accordingly and
//! this sink refuses a packet that is not marked, because a recovery queue full
//! of non-random-access raw pairs would discard every one of them.
//!
//! **Sequence numbers pass through unrebased.** A pair's audio span and
//! presentation time are derived from its absolute sequence, so rebasing per
//! connection would change the span under non-integer cadences. The child
//! derives its own media clock from the count of frames it receives, so a
//! reconnected session starts at zero regardless; what a reconnect does show is
//! [`StreamTelemetry::skipped_pairs`], counting everything this connection
//! missed since the format's origin.
//!
//! Round-trip time, packet loss, and retransmissions are not observable through
//! an `FFmpeg` child, so they are reported as unknown rather than invented.
//! Real transport evidence — muxed bytes, media drift, whether the destination
//! was ever open — is in [`FfmpegRtmpSink::stream_telemetry`].
//!
//! # Retryable or terminal
//!
//! The whole state machine hinges on this split, so it is deliberately
//! conservative in one direction: only an explicitly recognized cause is
//! terminal, because giving up on a live show is worse than one more bounded
//! attempt, and the retry budget in [`ReconnectPolicy`](crate::ReconnectPolicy)
//! bounds every unrecognized cause anyway.
//!
//! Terminal, never retried: a protocol this transport does not carry, a
//! destination URL that cannot be formed or has no stream key, invalid limits
//! or an unsupported channel layout, a missing or unusable `ffmpeg`, a payload
//! that is not a raw pair for the configured format, a sequence the streamer
//! refuses, and a child whose captured output names authentication rejection,
//! access denial, certificate rejection, or an unsupported output format.
//!
//! Retryable: connection refused, reset, or timed out, a destination that never
//! opened, a broken pipe, name resolution failure, a stalled media clock, local
//! spawn or bind pressure, and any child exit whose cause is not recognized.
//!
//! # Bounds
//!
//! At most one child exists at a time: `connect` stops any previous session
//! before starting one, `disconnect` stops the current one, and dropping the
//! sink stops it too. Every wait is deadline-bounded by [`StreamLimits`]:
//! startup by `connect_timeout`, admission by `enqueue_timeout` (zero, so the
//! caller's poll never blocks on the network), shutdown by `stop_timeout` and
//! `kill_timeout`. This module adds no queue of its own: the state machine's
//! bounded queue and the streamer's bounded admission window are the only two
//! places a packet can wait.
//!
//! # Not covered
//!
//! SRT, HLS, and LAN outputs; TLS certificate policy (`rtmps://` uses the
//! child's default trust configuration); credential resolution (this sink is
//! handed an already-resolved stream key); adaptive bitrate; hardware encoders;
//! and any wiring into the daemon or a per-output audio bus.

use core::fmt;
use std::num::NonZeroU128;
use std::time::Duration;

use fm_codec_ffmpeg::Executable;
use fm_codec_ffmpeg::stream::{
    CleanupStatus, EncoderSettings, EnqueueRejection, OverflowPolicy, PairedFrame, RecordFormat,
    StartError, StartErrorKind, StopReport, StreamConfig, StreamDestination, StreamFailure,
    StreamLimits, StreamTelemetry, Streamer,
};
use fm_frame::{
    AudioBlock, ClockDomainId, MediaTimestamp, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SequenceNumber, TimeBase,
};

use crate::{
    CongestionObservation, ConnectionObservation, DestinationConfig, Endpoint, FailureStage,
    OutputError, OutputPacket, OutputProtocol, RenditionId, SendObservation, SinkError, SinkWrite,
    TransportSink,
};

/// Raw pairs carry no clock domain of their own; the child's media clock is
/// derived from the frames it receives.
const CLOCK_DOMAIN: u128 = 1;
/// The shortest tail [`StreamDestination`] can redact out of captured output.
const MINIMUM_STREAM_KEY_BYTES: usize = 4;
const MAXIMUM_STREAM_KEY_BYTES: usize = 512;
const BYTES_PER_SAMPLE: usize = size_of::<f32>();

/// Causes that must never be retried, matched against the child's redacted
/// output. Kept narrow on purpose: a false terminal ends the show.
const TERMINAL_MARKERS: [(&str, FailureStage); 10] = [
    ("authentication failed", FailureStage::Authentication),
    (
        "netconnection.connect.rejected",
        FailureStage::Authentication,
    ),
    ("netstream.publish.badname", FailureStage::Authentication),
    ("access denied", FailureStage::Authentication),
    ("unauthorized", FailureStage::Authentication),
    ("forbidden", FailureStage::Authentication),
    ("invalid stream key", FailureStage::Authentication),
    ("certificate verify failed", FailureStage::Tls),
    (
        "unable to find a suitable output format",
        FailureStage::Protocol,
    ),
    ("protocol not found", FailureStage::Protocol),
];

/// Recognized transient causes. These only sharpen the reported stage; every
/// unrecognized cause is retried as a connect failure anyway.
const RETRYABLE_MARKERS: [(&str, FailureStage); 9] = [
    ("connection refused", FailureStage::Connect),
    ("connection reset", FailureStage::Write),
    ("broken pipe", FailureStage::Write),
    ("end of file", FailureStage::Write),
    ("connection timed out", FailureStage::Connect),
    ("no route to host", FailureStage::Connect),
    ("network is unreachable", FailureStage::Connect),
    ("name or service not known", FailureStage::Dns),
    ("failed to resolve", FailureStage::Dns),
];

/// One already-resolved stream key. Never rendered, including by `Debug`.
#[derive(Clone, Eq, PartialEq)]
pub struct StreamKey(String);

impl StreamKey {
    /// Accepts one URL path segment that [`StreamDestination`] can redact.
    ///
    /// # Errors
    ///
    /// Rejects a key that is too short to substitute out of captured child
    /// output, too long, or not a single printable ASCII path segment.
    pub fn new(value: impl Into<String>) -> Result<Self, RtmpConfigError> {
        let value = value.into();
        if !(MINIMUM_STREAM_KEY_BYTES..=MAXIMUM_STREAM_KEY_BYTES).contains(&value.len())
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            || value.contains('/')
        {
            return Err(RtmpConfigError::InvalidStreamKey);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StreamKey(****)")
    }
}

/// Startup configuration rejected before any process exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtmpConfigError {
    InvalidStreamKey,
}

impl fmt::Display for RtmpConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("stream key is not one redactable printable path segment")
    }
}

impl std::error::Error for RtmpConfigError {}

/// Everything one destination's `FFmpeg` sessions are built from.
///
/// The raw media format is supplied by the caller: it belongs to the encoder
/// side of the engine, and this crate has no business inventing a cadence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtmpSinkConfig {
    /// The single rendition this destination carries. A packet from any other
    /// rendition is refused rather than interleaved into the same child.
    pub rendition: RenditionId,
    pub format: RecordFormat,
    pub ffmpeg: Executable,
    pub limits: StreamLimits,
    pub encoder: EncoderSettings,
    /// Appended as the final path segment of the destination URL. `None` means
    /// the endpoint path already carries the key.
    pub stream_key: Option<StreamKey>,
}

impl RtmpSinkConfig {
    /// Uses deadlines suited to an output under a reconnecting state machine:
    /// a dead destination must be given up on in seconds so backoff and the
    /// backup endpoint can take over, not held open for the streamer's
    /// standalone defaults.
    #[must_use]
    pub fn new(rendition: RenditionId, format: RecordFormat) -> Self {
        Self {
            rendition,
            format,
            ffmpeg: Executable::SearchPath,
            limits: StreamLimits {
                connect_timeout: Duration::from_secs(5),
                no_progress_timeout: Duration::from_secs(3),
                stop_timeout: Duration::from_secs(5),
                kill_timeout: Duration::from_secs(1),
                ..StreamLimits::default()
            },
            encoder: EncoderSettings::default(),
            stream_key: None,
        }
    }
}

/// Timing every raw pair of one format and sequence must carry exactly.
#[derive(Clone, Copy)]
struct PairTiming {
    start_sample: i64,
    samples: usize,
    presentation_nanos: i64,
    duration_nanos: u64,
}

impl PairTiming {
    fn audio_bytes(self, channels: usize) -> Option<usize> {
        self.samples
            .checked_mul(channels)?
            .checked_mul(BYTES_PER_SAMPLE)
    }
}

/// Absolute engine-sequence boundaries, restated from the same arithmetic
/// [`PairedFrame`] validates against.
fn pair_timing(format: &RecordFormat, sequence: u64) -> Option<PairTiming> {
    let rate = format.frame_rate();
    let hertz = u128::from(format.sample_rate().hertz());
    let boundary = |frames: u64| -> Option<u128> {
        u128::from(frames)
            .checked_mul(hertz)?
            .checked_mul(u128::from(rate.denominator()))
            .map(|value| value / u128::from(rate.numerator()))
    };
    let nanos = |samples: u128| -> Option<u128> {
        samples
            .checked_mul(1_000_000_000)
            .map(|value| value / hertz)
    };
    let start = boundary(sequence)?;
    let end = boundary(sequence.checked_add(1)?)?;
    Some(PairTiming {
        start_sample: i64::try_from(start).ok()?,
        samples: usize::try_from(end.checked_sub(start)?).ok()?,
        presentation_nanos: i64::try_from(nanos(start)?).ok()?,
        duration_nanos: u64::try_from(nanos(end)?.checked_sub(nanos(start)?)?).ok()?,
    })
}

/// Why one raw pair could not be packed into an [`OutputPacket`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawPairError {
    /// The sequence has no representable audio span or presentation time.
    Timing,
    RgbaLength {
        expected: usize,
        actual: usize,
    },
    AudioLength {
        expected: usize,
        actual: usize,
    },
    Packet(OutputError),
}

impl fmt::Display for RawPairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timing => formatter.write_str("raw pair timing is not representable"),
            Self::RgbaLength { expected, actual } => {
                write!(
                    formatter,
                    "raw pair needs {expected} RGBA bytes, got {actual}"
                )
            }
            Self::AudioLength { expected, actual } => {
                write!(
                    formatter,
                    "raw pair needs {expected} audio bytes, got {actual}"
                )
            }
            Self::Packet(error) => write!(formatter, "raw pair packet rejected: {error}"),
        }
    }
}

impl std::error::Error for RawPairError {}

/// Packs one raw pair into the [`OutputPacket`] this transport understands.
///
/// The payload is the tightly packed RGBA frame followed by the sample-major
/// `f32le` audio span for the same sequence — the two payloads
/// [`PairedFrame`] itself holds. Presentation time and duration come from the
/// format's cadence, and the packet is marked random-access because every raw
/// pair is one.
///
/// # Errors
///
/// Rejects payload lengths that do not match the format at this sequence, a
/// sequence with no representable timing, and a packet the output state machine
/// itself refuses.
pub fn raw_pair_packet(
    rendition: RenditionId,
    format: &RecordFormat,
    sequence: u64,
    rgba: &[u8],
    audio_f32le: &[u8],
) -> Result<OutputPacket, RawPairError> {
    let timing = pair_timing(format, sequence).ok_or(RawPairError::Timing)?;
    let expected_rgba = format.rgba_bytes_per_frame();
    if rgba.len() != expected_rgba {
        return Err(RawPairError::RgbaLength {
            expected: expected_rgba,
            actual: rgba.len(),
        });
    }
    let expected_audio = timing
        .audio_bytes(format.channel_layout().channels().len())
        .ok_or(RawPairError::Timing)?;
    if audio_f32le.len() != expected_audio {
        return Err(RawPairError::AudioLength {
            expected: expected_audio,
            actual: audio_f32le.len(),
        });
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(expected_rgba.saturating_add(expected_audio))
        .map_err(|_| RawPairError::Timing)?;
    payload.extend_from_slice(rgba);
    payload.extend_from_slice(audio_f32le);
    OutputPacket::new(
        rendition,
        sequence,
        NormalizedTimestamp::from_nanos(timing.presentation_nanos),
        NormalizedDuration::from_nanos(timing.duration_nanos).map_err(|_| RawPairError::Timing)?,
        true,
        payload,
    )
    .map_err(RawPairError::Packet)
}

/// One destination's RTMP/RTMPS transport: at most one `FFmpeg` child, started
/// by [`connect`](TransportSink::connect) and reaped by
/// [`disconnect`](TransportSink::disconnect).
///
/// Give one sink to one [`OutputSet`] destination. Sharing a sink between
/// destinations would let one destination's failure stop another's child.
pub struct FfmpegRtmpSink {
    config: RtmpSinkConfig,
    streamer: Option<Streamer>,
    connections_started: u64,
    connections_stopped: u64,
    unconfirmed_cleanups: u64,
    last_stop: Option<StopReport>,
}

impl FfmpegRtmpSink {
    #[must_use]
    pub const fn new(config: RtmpSinkConfig) -> Self {
        Self {
            config,
            streamer: None,
            connections_started: 0,
            connections_stopped: 0,
            unconfirmed_cleanups: 0,
            last_stop: None,
        }
    }

    #[must_use]
    pub const fn config(&self) -> &RtmpSinkConfig {
        &self.config
    }

    /// Children this sink has started. Equal to
    /// [`connections_stopped`](Self::connections_stopped) whenever no session
    /// is open, which is what "no child outlives its attempt" means.
    #[must_use]
    pub const fn connections_started(&self) -> u64 {
        self.connections_started
    }

    #[must_use]
    pub const fn connections_stopped(&self) -> u64 {
        self.connections_stopped
    }

    /// Stops whose child reaping could not be confirmed within the deadline.
    /// Nonzero means an `FFmpeg` child may have outlived its attempt.
    #[must_use]
    pub const fn unconfirmed_cleanups(&self) -> u64 {
        self.unconfirmed_cleanups
    }

    /// Live evidence from the open session: whether the destination was ever
    /// observed open, muxed bytes, media drift, and admission accounting.
    #[must_use]
    pub fn stream_telemetry(&self) -> Option<StreamTelemetry> {
        self.streamer.as_ref().map(Streamer::telemetry)
    }

    #[must_use]
    pub const fn last_stop_report(&self) -> Option<&StopReport> {
        self.last_stop.as_ref()
    }

    /// Packs one raw pair for this sink's rendition and format.
    ///
    /// # Errors
    ///
    /// See [`raw_pair_packet`].
    pub fn packet(
        &self,
        sequence: u64,
        rgba: &[u8],
        audio_f32le: &[u8],
    ) -> Result<OutputPacket, RawPairError> {
        raw_pair_packet(
            self.config.rendition,
            &self.config.format,
            sequence,
            rgba,
            audio_f32le,
        )
    }

    fn destination_url(
        &self,
        config: &DestinationConfig,
        endpoint: &Endpoint,
    ) -> Result<StreamDestination, SinkError> {
        let scheme = match config.protocol() {
            OutputProtocol::Rtmp => "rtmp",
            OutputProtocol::Rtmps => "rtmps",
            other => {
                return Err(SinkError::new(
                    FailureStage::Protocol,
                    None,
                    format!("{other:?} is not carried by the FFmpeg RTMP transport"),
                    false,
                ));
            }
        };
        let mut url = format!(
            "{scheme}://{}:{}{}",
            endpoint.host(),
            endpoint.port(),
            endpoint.path()
        );
        if let Some(key) = &self.config.stream_key {
            if !url.ends_with('/') {
                url.push('/');
            }
            url.push_str(&key.0);
        }
        // The typed error carries no URL text, and neither does this message.
        StreamDestination::parse(&url).map_err(|error| {
            SinkError::new(
                FailureStage::Protocol,
                None,
                format!("destination rejected: {error:?}"),
                false,
            )
        })
    }

    fn decode_pair(&self, packet: &OutputPacket) -> Result<PairedFrame, SinkError> {
        let format = &self.config.format;
        if packet.rendition() != self.config.rendition {
            return Err(terminal_packet("packet belongs to another rendition"));
        }
        if !packet.is_random_access() {
            return Err(terminal_packet("raw pairs must be random-access points"));
        }
        let sequence = packet.sequence();
        let timing = pair_timing(format, sequence)
            .ok_or_else(|| terminal_packet("packet sequence has no representable timing"))?;
        let channels = format.channel_layout().channels().len();
        let rgba_bytes = format.rgba_bytes_per_frame();
        let audio_bytes = timing
            .audio_bytes(channels)
            .ok_or_else(|| terminal_packet("packet audio span is not representable"))?;
        if packet.payload().len() != rgba_bytes.saturating_add(audio_bytes) {
            return Err(terminal_packet(
                "payload is not one raw pair for this format",
            ));
        }
        let (rgba, audio) = packet.payload().split_at(rgba_bytes);
        let mut planes = (0..channels)
            .map(|_| Vec::with_capacity(timing.samples))
            .collect::<Vec<_>>();
        for (index, sample) in audio.chunks_exact(BYTES_PER_SAMPLE).enumerate() {
            let mut bytes = [0_u8; BYTES_PER_SAMPLE];
            bytes.copy_from_slice(sample);
            planes[index % channels].push(f32::from_le_bytes(bytes));
        }
        let sequence = SequenceNumber::new(sequence);
        let clock = NonZeroU128::new(CLOCK_DOMAIN)
            .ok_or_else(|| terminal_packet("clock domain must be nonzero"))?;
        let time_base = TimeBase::new(1, format.sample_rate().hertz())
            .map_err(|error| terminal_packet(format!("audio time base rejected: {error:?}")))?;
        let media_timing = MediaTiming::new(
            OriginalTimestamp::new(MediaTimestamp::new(timing.start_sample), time_base),
            NormalizedTimestamp::from_nanos(timing.presentation_nanos),
            NormalizedDuration::from_nanos(timing.duration_nanos)
                .map_err(|error| terminal_packet(format!("audio duration rejected: {error:?}")))?,
            ClockDomainId::new(clock),
            sequence,
        )
        .map_err(|error| terminal_packet(format!("audio timing rejected: {error:?}")))?;
        let block = AudioBlock::new(
            media_timing,
            format.sample_rate(),
            format.channel_layout().clone(),
            planes,
        )
        .map_err(|error| terminal_packet(format!("audio block rejected: {error:?}")))?;
        PairedFrame::new(format, sequence, rgba.to_vec(), block)
            .map_err(|error| terminal_packet(format!("raw pair rejected: {error:?}")))
    }
}

impl TransportSink for FfmpegRtmpSink {
    fn connect(
        &mut self,
        config: &DestinationConfig,
        endpoint: &Endpoint,
    ) -> Result<ConnectionObservation, SinkError> {
        // A previous session can only still be here if the state machine
        // reconnected without a write failure; no child may outlive its attempt.
        self.disconnect();
        let destination = self.destination_url(config, endpoint)?;
        let mut stream_config = StreamConfig::new(self.config.format.clone(), destination);
        stream_config.ffmpeg = self.config.ffmpeg.clone();
        stream_config.limits = self.config.limits;
        stream_config.encoder = self.config.encoder;
        stream_config.overflow = OverflowPolicy::Reject;
        match Streamer::start(stream_config) {
            Ok(streamer) => {
                self.connections_started = self.connections_started.saturating_add(1);
                self.streamer = Some(streamer);
                // The destination itself is proven by the first writes; the
                // child's own connect deadline bounds that proof.
                Ok(ConnectionObservation::default())
            }
            Err(error) => {
                if error.cleanup != CleanupStatus::Complete {
                    self.unconfirmed_cleanups = self.unconfirmed_cleanups.saturating_add(1);
                }
                Err(classify_start(&error))
            }
        }
    }

    fn write(&mut self, packet: &OutputPacket) -> Result<SinkWrite, SinkError> {
        let pair = self.decode_pair(packet)?;
        let Some(streamer) = self.streamer.as_mut() else {
            return Err(SinkError::new(
                FailureStage::Connect,
                None,
                "no streaming session is open",
                true,
            ));
        };
        match streamer.enqueue(pair) {
            Ok(()) => {
                // A session that has already failed must not be reported as a
                // successful write just because admission happened first.
                let telemetry = streamer.telemetry();
                telemetry.failure.map_or_else(
                    || Ok(SinkWrite::Sent(SendObservation::default())),
                    |failure| Err(classify_failure(&failure, &telemetry.stderr_tail)),
                )
            }
            Err(error) => match error.reason {
                EnqueueRejection::QueueFull | EnqueueRejection::RetainedByteLimit => {
                    Ok(SinkWrite::Congested(CongestionObservation::default()))
                }
                EnqueueRejection::FormatMismatch => {
                    Err(terminal_packet("pair does not match the session format"))
                }
                EnqueueRejection::Sequence { .. } | EnqueueRejection::SequenceExhausted => {
                    Err(terminal_packet("packet sequence did not strictly increase"))
                }
                EnqueueRejection::Stopping | EnqueueRejection::Stopped => Err(SinkError::new(
                    FailureStage::Write,
                    None,
                    "streaming session stopped accepting pairs",
                    true,
                )),
                EnqueueRejection::Failed(failure) => Err(classify_failure(
                    &failure,
                    &streamer.telemetry().stderr_tail,
                )),
            },
        }
    }

    fn disconnect(&mut self) {
        let Some(mut streamer) = self.streamer.take() else {
            return;
        };
        // A dead session has nothing to drain, and a live output cannot afford
        // to spend the drain budget discovering that.
        let report = if streamer.telemetry().failure.is_some() {
            streamer.cancel()
        } else {
            streamer.stop()
        };
        self.connections_stopped = self.connections_stopped.saturating_add(1);
        if report.cleanup != CleanupStatus::Complete {
            self.unconfirmed_cleanups = self.unconfirmed_cleanups.saturating_add(1);
        }
        self.last_stop = Some(report);
    }
}

impl Drop for FfmpegRtmpSink {
    fn drop(&mut self) {
        self.disconnect();
    }
}

fn terminal_packet(message: impl Into<String>) -> SinkError {
    SinkError::new(FailureStage::Protocol, None, message, false)
}

/// Matches the child's redacted output against the recognized causes.
fn classify_text(text: &str) -> Option<(FailureStage, bool)> {
    let text = text.to_ascii_lowercase();
    for (marker, stage) in TERMINAL_MARKERS {
        if text.contains(marker) {
            return Some((stage, false));
        }
    }
    for (marker, stage) in RETRYABLE_MARKERS {
        if text.contains(marker) {
            return Some((stage, true));
        }
    }
    None
}

fn classify_start(error: &StartError) -> SinkError {
    let (stage, retryable, message) = match &error.kind {
        // Configuration and tooling: a retry changes nothing.
        StartErrorKind::InvalidLimits(limits) => (
            FailureStage::Protocol,
            false,
            format!("streaming limits rejected: {limits:?}"),
        ),
        StartErrorKind::UnsupportedChannelLayout => (
            FailureStage::Protocol,
            false,
            "FLV carries mono or stereo audio only".to_owned(),
        ),
        StartErrorKind::InvalidExecutable => (
            FailureStage::Protocol,
            false,
            "configured ffmpeg executable is unusable".to_owned(),
        ),
        StartErrorKind::ToolUnavailable(reason) => (
            FailureStage::Protocol,
            false,
            format!("ffmpeg is unavailable: {reason:?}"),
        ),
        // Local resource pressure: a fresh attempt is worth making.
        StartErrorKind::Randomness => (
            FailureStage::Connect,
            true,
            "input token randomness failed".to_owned(),
        ),
        StartErrorKind::ThreadSpawn(kind) => (
            FailureStage::Connect,
            true,
            format!("worker thread spawn failed: {kind:?}"),
        ),
        StartErrorKind::Bind { input, kind } => (
            FailureStage::Connect,
            true,
            format!("{input:?} loopback bind failed: {kind:?}"),
        ),
        StartErrorKind::Spawn(kind) => (
            FailureStage::Connect,
            true,
            format!("ffmpeg spawn failed: {kind:?}"),
        ),
        StartErrorKind::MissingPipe => (
            FailureStage::Connect,
            true,
            "ffmpeg child exposed no pipes".to_owned(),
        ),
        StartErrorKind::ConnectTimeout { input } => (
            FailureStage::Connect,
            true,
            format!("ffmpeg never requested its {input:?} input"),
        ),
        StartErrorKind::Connect { input, kind } => (
            FailureStage::Connect,
            true,
            format!("{input:?} input handshake failed: {kind:?}"),
        ),
        StartErrorKind::EarlyExit { status, stderr } => {
            let (stage, retryable) = classify_text(stderr).unwrap_or((FailureStage::Connect, true));
            return SinkError::new(
                stage,
                status.map(i64::from),
                "ffmpeg exited during startup",
                retryable,
            );
        }
    };
    SinkError::new(stage, None, message, retryable)
}

fn classify_failure(failure: &StreamFailure, stderr: &str) -> SinkError {
    // The captured cause outranks the variant: a child killed by the watchdog
    // may still have printed why the destination refused it.
    let observed = classify_text(stderr);
    if let Some((stage, false)) = observed {
        return SinkError::new(
            stage,
            exit_status(failure),
            format!("destination refused this session: {failure:?}"),
            false,
        );
    }
    let (stage, message) = match failure {
        StreamFailure::DestinationTimeout => (
            FailureStage::Connect,
            "destination never opened within the connect deadline",
        ),
        StreamFailure::NoProgress => (FailureStage::Write, "muxed media clock stalled"),
        StreamFailure::ChildExited { .. } => (FailureStage::Write, "ffmpeg exited"),
        StreamFailure::Cancelled => (FailureStage::Write, "session was cancelled"),
        StreamFailure::InputTimeout { .. } | StreamFailure::Connect { .. } => {
            (FailureStage::Connect, "ffmpeg input handshake failed")
        }
        StreamFailure::Write { .. } | StreamFailure::DispatcherClosed(_) => {
            (FailureStage::Write, "ffmpeg input write failed")
        }
        StreamFailure::ProgressRead(_) | StreamFailure::StderrRead(_) => {
            (FailureStage::Write, "ffmpeg reporting pipe failed")
        }
        StreamFailure::WorkerPanicked => (FailureStage::Write, "streaming worker panicked"),
        StreamFailure::StopTimedOut
        | StreamFailure::KillTimedOut
        | StreamFailure::CleanupUnconfirmed => {
            (FailureStage::Write, "previous session did not shut down")
        }
    };
    SinkError::new(
        observed.map_or(stage, |(observed, _)| observed),
        exit_status(failure),
        format!("{message}: {failure:?}"),
        true,
    )
}

fn exit_status(failure: &StreamFailure) -> Option<i64> {
    match failure {
        StreamFailure::ChildExited { status } => status.map(i64::from),
        _ => None,
    }
}
