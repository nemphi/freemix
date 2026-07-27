//! Opt-in local-file decode and native GPU composition root.
//!
//! [`NativeMediaRuntime::preroll_local_blocking`] launches bounded `FFmpeg` and
//! ffprobe subprocesses synchronously. Call it from a worker thread, even
//! though the function is async so that GPU normalization can remain async.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    num::NonZeroU32,
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

use fm_audio::{ChannelMap, InputState, MasterMixer, SourceGain};
use fm_clock::ClockTime;
#[cfg(test)]
use fm_codec_ffmpeg::SequenceRequest;
use fm_codec_ffmpeg::{
    Adapter, DecodeRequest, DecodedAudioWindow, DecodedVideoWindow, LocalAudioDecoder,
    LocalVideoDecoder, StreamKind, StreamSelector,
};
use fm_color::{
    NativeImportError, NativeImportNormalizer, NativeSdrOutputTransform, NativeWorkingFrame,
};
use fm_compositor::{
    NativeTransitionError, NativeTransitionRenderer, TransitionError, TransitionKind,
    TransitionPlan,
};
use fm_engine::FrameResult;
use fm_frame::{
    AudioBlock, ClockDomainId, CpuVideoFrame, MediaTimestamp, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SequenceNumber, TimingError,
};
use fm_gpu::{
    DiagnosticReadback, NativeAdapterInfo, NativeBackend, NativeContext, NativeGpuError,
    NativeTexture, NativeTextureReadback,
};
use fm_sim::{CollectingAudioSink, OverflowPolicy, SinkConfigError, SinkTelemetry};
use fm_switcher::ProgramFrame;
use fm_types::{AudioFormat, FrameRate, InputId, SampleFormat, TimeBase};

const RGBA16_FLOAT_BYTES_PER_PIXEL: u64 = 8;
const SOURCE_REFILL_LOW_WATERMARK: usize = 4;
const SOURCE_REFILL_MAX_PAGE: u32 = 4;
const AUDIO_REFILL_LOW_WATERMARK: usize = 8;

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

/// GPU-normalized video frames and decoded audio from one bounded preroll.
pub struct NativeMediaPreroll {
    video: Vec<NativeWorkingFrame>,
    audio: Vec<AudioBlock>,
}

impl NativeMediaPreroll {
    /// Returns the canonical GPU-resident video frames.
    #[must_use]
    pub fn video(&self) -> &[NativeWorkingFrame] {
        &self.video
    }

    /// Returns the decoded audio blocks without modification.
    #[must_use]
    pub fn audio(&self) -> &[AudioBlock] {
        &self.audio
    }

    /// Consumes the preroll into its GPU video frames and decoded audio blocks.
    #[must_use]
    pub fn into_parts(self) -> (Vec<NativeWorkingFrame>, Vec<AudioBlock>) {
        (self.video, self.audio)
    }
}

/// Aggregate failures from native media setup and execution.
#[derive(Debug)]
pub enum NativeMediaError {
    Ffmpeg(fm_codec_ffmpeg::Error),
    Gpu(NativeGpuError),
    Color(NativeImportError),
    Compositor(NativeTransitionError),
}

impl fmt::Display for NativeMediaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffmpeg(error) => write!(formatter, "local media decode failed: {error}"),
            Self::Gpu(error) => write!(formatter, "native GPU setup or diagnostic failed: {error}"),
            Self::Color(error) => write!(formatter, "native color normalization failed: {error}"),
            Self::Compositor(error) => write!(formatter, "native composition failed: {error}"),
        }
    }
}

impl Error for NativeMediaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ffmpeg(error) => Some(error),
            Self::Gpu(error) => Some(error),
            Self::Color(error) => Some(error),
            Self::Compositor(error) => Some(error),
        }
    }
}

impl From<fm_codec_ffmpeg::Error> for NativeMediaError {
    fn from(value: fm_codec_ffmpeg::Error) -> Self {
        Self::Ffmpeg(value)
    }
}

impl From<NativeGpuError> for NativeMediaError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

impl From<NativeImportError> for NativeMediaError {
    fn from(value: NativeImportError) -> Self {
        Self::Color(value)
    }
}

impl From<NativeTransitionError> for NativeMediaError {
    fn from(value: NativeTransitionError) -> Self {
        Self::Compositor(value)
    }
}

/// Resource bounds for GPU-resident native source rings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeSourceLimits {
    pub max_media_inputs: usize,
    pub max_video_frames_per_source: NonZeroU32,
    pub max_retained_rgba16f_bytes: u64,
}

impl NativeSourceLimits {
    pub const DEFAULT_MAX_MEDIA_INPUTS: usize = 64;
    pub const DEFAULT_MAX_VIDEO_FRAMES_PER_SOURCE: NonZeroU32 =
        NonZeroU32::new(8).expect("eight is nonzero");
    pub const DEFAULT_MAX_RETAINED_RGBA16F_BYTES: u64 = 512 * 1024 * 1024;
}

impl Default for NativeSourceLimits {
    fn default() -> Self {
        Self {
            max_media_inputs: Self::DEFAULT_MAX_MEDIA_INPUTS,
            max_video_frames_per_source: Self::DEFAULT_MAX_VIDEO_FRAMES_PER_SOURCE,
            max_retained_rgba16f_bytes: Self::DEFAULT_MAX_RETAINED_RGBA16F_BYTES,
        }
    }
}

/// A fully resolved native playback source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeResolvedSource {
    LocalVideo {
        input: InputId,
        path: PathBuf,
    },
    RetainedFrame {
        input: InputId,
        frame: CpuVideoFrame,
    },
    LiveFrame {
        input: InputId,
        frame: CpuVideoFrame,
    },
}

impl NativeResolvedSource {
    /// Returns the full-width input identity of this source.
    #[must_use]
    pub const fn input(&self) -> InputId {
        match self {
            Self::LocalVideo { input, .. }
            | Self::RetainedFrame { input, .. }
            | Self::LiveFrame { input, .. } => *input,
        }
    }
}

/// Pure source-registry validation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSourceError {
    TooManySources {
        actual: usize,
        maximum: usize,
    },
    TooManyFrames {
        input: InputId,
        actual: usize,
        maximum: u32,
    },
    DuplicateSource(InputId),
    FrameByteSizeOverflow {
        input: InputId,
        width: u32,
        height: u32,
    },
    RetainedBytesExceeded {
        required: u64,
        maximum: u64,
    },
    DimensionMismatch {
        input: InputId,
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    InvalidTimeline {
        input: InputId,
    },
}

impl fmt::Display for NativeSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManySources { actual, maximum } => {
                write!(formatter, "source count {actual} exceeds limit {maximum}")
            }
            Self::TooManyFrames {
                input,
                actual,
                maximum,
            } => write!(
                formatter,
                "source {input} frame count {actual} exceeds limit {maximum}"
            ),
            Self::DuplicateSource(input) => {
                write!(formatter, "source {input} is already registered")
            }
            Self::FrameByteSizeOverflow {
                input,
                width,
                height,
            } => write!(
                formatter,
                "source {input} dimensions {width}x{height} overflow the RGBA16F byte charge"
            ),
            Self::RetainedBytesExceeded { required, maximum } => write!(
                formatter,
                "retained RGBA16F bytes {required} exceed limit {maximum}"
            ),
            Self::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                formatter,
                "source {input} dimensions {actual_width}x{actual_height} do not match {expected_width}x{expected_height}"
            ),
            Self::InvalidTimeline { input } => {
                write!(formatter, "source {input} has an invalid video timeline")
            }
        }
    }
}

impl Error for NativeSourceError {}

/// Failures while decoding and uploading a bounded source prefix.
#[derive(Debug)]
pub enum NativeSourcePreflightError {
    Source(NativeSourceError),
    Decode {
        input: InputId,
        error: fm_codec_ffmpeg::Error,
    },
    DecodeContract {
        input: InputId,
        video_frames: usize,
        audio_blocks: usize,
    },
    Normalize {
        input: InputId,
        error: NativeImportError,
    },
    CodecAdapterRequired {
        input: InputId,
    },
    WorkerUnavailable,
}

impl fmt::Display for NativeSourcePreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Decode { input, error } => {
                write!(
                    formatter,
                    "source {input} video preflight decode failed: {error}"
                )
            }
            Self::DecodeContract {
                input,
                video_frames,
                audio_blocks,
            } => write!(
                formatter,
                "source {input} preflight returned {video_frames} video frames and {audio_blocks} audio blocks; expected a nonempty bounded video prefix and no audio"
            ),
            Self::Normalize { input, error } => {
                write!(
                    formatter,
                    "source {input} native normalization failed: {error}"
                )
            }
            Self::CodecAdapterRequired { input } => {
                write!(
                    formatter,
                    "source {input} requires a local video codec adapter"
                )
            }
            Self::WorkerUnavailable => {
                formatter.write_str("native source decode worker could not be started")
            }
        }
    }
}

impl Error for NativeSourcePreflightError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::Normalize { error, .. } => Some(error),
            Self::DecodeContract { .. }
            | Self::CodecAdapterRequired { .. }
            | Self::WorkerUnavailable => None,
        }
    }
}

impl From<NativeSourceError> for NativeSourcePreflightError {
    fn from(value: NativeSourceError) -> Self {
        Self::Source(value)
    }
}

/// Fatal failures while pumping bounded native source playback.
#[derive(Debug)]
pub enum NativeSourcePlaybackError {
    Source(NativeSourceError),
    Decode {
        input: InputId,
        error: fm_codec_ffmpeg::Error,
    },
    DecodeContract {
        input: InputId,
    },
    Normalize {
        input: InputId,
        error: NativeImportError,
    },
    WorkerDisconnected,
    WorkerPanicked,
    WorkerQueueFull,
    SourceNotLive {
        input: InputId,
    },
    Failed,
}

impl fmt::Display for NativeSourcePlaybackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Decode { input, error } => {
                write!(
                    formatter,
                    "source {input} video refill decode failed: {error}"
                )
            }
            Self::DecodeContract { input } => {
                write!(
                    formatter,
                    "source {input} video refill violated the decode contract"
                )
            }
            Self::Normalize { input, error } => {
                write!(
                    formatter,
                    "source {input} native refill normalization failed: {error}"
                )
            }
            Self::WorkerDisconnected => {
                formatter.write_str("native source decode worker disconnected")
            }
            Self::WorkerPanicked => formatter.write_str("native source decode worker panicked"),
            Self::WorkerQueueFull => formatter
                .write_str("native source decode worker request queue is unexpectedly full"),
            Self::SourceNotLive { input } => {
                write!(formatter, "source {input} is not a live video source")
            }
            Self::Failed => formatter.write_str("native source playback previously failed"),
        }
    }
}

impl Error for NativeSourcePlaybackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::Normalize { error, .. } => Some(error),
            Self::DecodeContract { .. }
            | Self::WorkerDisconnected
            | Self::WorkerPanicked
            | Self::WorkerQueueFull
            | Self::SourceNotLive { .. }
            | Self::Failed => None,
        }
    }
}

impl From<NativeSourceError> for NativeSourcePlaybackError {
    fn from(value: NativeSourceError) -> Self {
        Self::Source(value)
    }
}

/// Render failures against an authoritative source-ring registry.
#[derive(Debug)]
pub enum NativeSourceRenderError {
    MissingSource {
        input: InputId,
    },
    DimensionMismatch {
        input: InputId,
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    InvalidMix(TransitionError),
    Compositor(NativeTransitionError),
}

impl fmt::Display for NativeSourceRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource { input } => write!(formatter, "source {input} is not registered"),
            Self::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                formatter,
                "source {input} texture dimensions {actual_width}x{actual_height} do not match registry dimensions {expected_width}x{expected_height}"
            ),
            Self::InvalidMix(error) => write!(formatter, "program-frame mix is invalid: {error}"),
            Self::Compositor(error) => write!(formatter, "native composition failed: {error}"),
        }
    }
}

impl Error for NativeSourceRenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidMix(error) => Some(error),
            Self::Compositor(error) => Some(error),
            Self::MissingSource { .. } | Self::DimensionMismatch { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeVideoSourceKind {
    Decoded,
    Retained,
    Live,
}

struct NativeVideoPrefix {
    frames: Vec<NativeWorkingFrame>,
    offsets_ns: Vec<u64>,
    source_pts_origin: i64,
    last_source_pts: i64,
    last_sequence: u64,
    clock_domain: ClockDomainId,
    kind: NativeVideoSourceKind,
    end_of_stream: bool,
    in_flight: Option<NonZeroU32>,
}

/// Bounded GPU-resident video prefixes keyed by full-width input identity.
/// Textures are retained once and selected without cloning or re-uploading.
pub struct NativeSourceRegistry {
    sources: BTreeMap<InputId, NativeVideoPrefix>,
    dimensions: Option<(u32, u32)>,
    retained_rgba16f_bytes: u64,
    limits: NativeSourceLimits,
}

impl NativeSourceRegistry {
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    #[must_use]
    pub const fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }

    #[must_use]
    pub const fn retained_rgba16f_bytes(&self) -> u64 {
        self.retained_rgba16f_bytes
    }

    /// Iterates full-width source IDs in deterministic map order.
    #[must_use]
    pub fn inputs(&self) -> impl ExactSizeIterator<Item = InputId> + '_ {
        self.sources.keys().copied()
    }

    /// Returns selected source timing without exposing its texture.
    #[must_use]
    pub fn timing_at_deadline(&self, input: InputId, deadline: ClockTime) -> Option<MediaTiming> {
        self.sources
            .get(&input)
            .and_then(|prefix| prefix.frame_at_deadline(deadline))
            .map(NativeWorkingFrame::timing)
    }

    #[must_use]
    pub fn contains(&self, input: InputId) -> bool {
        self.sources.contains_key(&input)
    }
}

impl NativeVideoPrefix {
    fn frame_at_deadline(&self, deadline: ClockTime) -> Option<&NativeWorkingFrame> {
        if self.kind == NativeVideoSourceKind::Live {
            return self.frames.last();
        }
        if !source_covers_deadline(
            self.offsets_ns.last().copied(),
            self.end_of_stream,
            deadline,
        ) {
            return None;
        }
        frame_index_at_deadline(&self.offsets_ns, deadline.as_nanos())
            .and_then(|index| self.frames.get(index))
    }

    fn covers_deadline(&self, deadline: ClockTime) -> bool {
        if self.kind == NativeVideoSourceKind::Live {
            !self.frames.is_empty()
        } else {
            source_covers_deadline(
                self.offsets_ns.last().copied(),
                self.end_of_stream,
                deadline,
            )
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct NativeDecodeRequest {
    input: InputId,
    count: NonZeroU32,
}

#[derive(Debug)]
struct NativeDecodeResult {
    input: InputId,
    count: NonZeroU32,
    window: Result<DecodedVideoWindow, fm_codec_ffmpeg::Error>,
}

struct NativeDecodeWorker {
    requests: Option<SyncSender<NativeDecodeRequest>>,
    results: Receiver<NativeDecodeResult>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NativeDecodeWorker {
    fn spawn(
        mut decoders: BTreeMap<InputId, LocalVideoDecoder>,
    ) -> Result<Self, NativeSourcePreflightError> {
        let capacity = decoders.len().max(1);
        let (request_sender, request_receiver) =
            mpsc::sync_channel::<NativeDecodeRequest>(capacity);
        let (result_sender, result_receiver) = mpsc::sync_channel::<NativeDecodeResult>(capacity);
        let worker = thread::Builder::new()
            .name("freemix-native-source-decode".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    let window = decoders
                        .get_mut(&request.input)
                        .ok_or(fm_codec_ffmpeg::Error::InvalidConfig)
                        .and_then(|decoder| decoder.decode_up_to(request.count));
                    if result_sender
                        .send(NativeDecodeResult {
                            input: request.input,
                            count: request.count,
                            window,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| NativeSourcePreflightError::WorkerUnavailable)?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            thread: Some(worker),
        })
    }

    fn disconnected_error(&mut self) -> NativeSourcePlaybackError {
        self.requests.take();
        match self.thread.take().map(thread::JoinHandle::join) {
            Some(Err(_)) => NativeSourcePlaybackError::WorkerPanicked,
            Some(Ok(())) | None => NativeSourcePlaybackError::WorkerDisconnected,
        }
    }
}

impl Drop for NativeDecodeWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

/// Bounded GPU source rings paired with one CPU-only background decode worker.
pub struct NativeSourcePlayback {
    registry: NativeSourceRegistry,
    worker: NativeDecodeWorker,
    failed: bool,
}

impl NativeSourcePlayback {
    /// Returns the immutable registry used by the existing render API.
    #[must_use]
    pub const fn registry(&self) -> &NativeSourceRegistry {
        &self.registry
    }

    /// Stops the decode worker and consumes playback into its current registry.
    #[must_use]
    pub fn into_registry(self) -> NativeSourceRegistry {
        let Self {
            registry, worker, ..
        } = self;
        drop(worker);
        registry
    }
}

/// Resource bounds for the independent CPU audio runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeAudioLimits {
    pub max_blocks_per_source: NonZeroU32,
    pub max_blocks_per_page: NonZeroU32,
    pub max_samples_per_page: usize,
    pub max_retained_blocks: usize,
    pub max_retained_samples: usize,
    pub max_retained_bytes: usize,
    pub sink_blocks: usize,
}

impl Default for NativeAudioLimits {
    fn default() -> Self {
        Self {
            max_blocks_per_source: NonZeroU32::new(32).expect("32 is nonzero"),
            max_blocks_per_page: NonZeroU32::new(16).expect("16 is nonzero"),
            max_samples_per_page: 64 * 1024,
            max_retained_blocks: 1024,
            max_retained_samples: 1024 * 1024,
            max_retained_bytes: 32 * 1024 * 1024,
            sink_blocks: 8,
        }
    }
}

/// Fatal setup, decode-contract, or render failures in native CPU audio.
#[derive(Debug)]
pub enum NativeMasterError {
    Ffmpeg {
        input: InputId,
        error: fm_codec_ffmpeg::Error,
    },
    Audio(fm_audio::AudioError),
    AudioBlock(fm_frame::AudioBlockError),
    Timing(TimingError),
    SinkConfig(SinkConfigError),
    InvalidLimits,
    InvalidFormat,
    InvalidTimeline {
        input: InputId,
    },
    BoundsExceeded,
    WorkerUnavailable,
    WorkerDisconnected,
    WorkerPanicked,
    WorkerQueueFull,
    DecodeContract {
        input: InputId,
    },
    UnexpectedFrame {
        expected: u64,
        actual: u64,
    },
    FrameNotReady(u64),
    SinkRejected,
    Failed,
}

impl fmt::Display for NativeMasterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffmpeg { input, error } => {
                write!(
                    formatter,
                    "source {input} native audio decode failed: {error}"
                )
            }
            Self::Audio(error) => write!(formatter, "native Master mix failed: {error}"),
            Self::AudioBlock(error) => write!(formatter, "native audio block failed: {error}"),
            Self::Timing(error) => write!(formatter, "native audio timing failed: {error}"),
            Self::SinkConfig(error) => write!(formatter, "native fake audio sink failed: {error}"),
            Self::InvalidLimits => formatter.write_str("native audio limits are invalid"),
            Self::InvalidFormat => formatter.write_str(
                "native audio requires the exact project planar F32 sample rate and channel layout",
            ),
            Self::InvalidTimeline { input } => {
                write!(formatter, "source {input} has an invalid audio timeline")
            }
            Self::BoundsExceeded => {
                formatter.write_str("native audio retained resource bounds were exceeded")
            }
            Self::WorkerUnavailable => {
                formatter.write_str("native audio decode worker could not be started")
            }
            Self::WorkerDisconnected => {
                formatter.write_str("native audio decode worker disconnected")
            }
            Self::WorkerPanicked => formatter.write_str("native audio decode worker panicked"),
            Self::WorkerQueueFull => {
                formatter.write_str("native audio decode worker queue is unexpectedly full")
            }
            Self::DecodeContract { input } => {
                write!(
                    formatter,
                    "source {input} violated the native audio decode contract"
                )
            }
            Self::UnexpectedFrame { expected, actual } => write!(
                formatter,
                "native audio expected frame {expected}, received {actual}"
            ),
            Self::FrameNotReady(frame) => {
                write!(
                    formatter,
                    "native audio frame {frame} was not serviced before render"
                )
            }
            Self::SinkRejected => formatter.write_str("native fake audio sink rejected a block"),
            Self::Failed => formatter.write_str("native audio runtime previously failed"),
        }
    }
}

impl Error for NativeMasterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ffmpeg { error, .. } => Some(error),
            Self::Audio(error) => Some(error),
            Self::AudioBlock(error) => Some(error),
            Self::Timing(error) => Some(error),
            Self::SinkConfig(error) => Some(error),
            _ => None,
        }
    }
}

impl From<fm_audio::AudioError> for NativeMasterError {
    fn from(value: fm_audio::AudioError) -> Self {
        Self::Audio(value)
    }
}

impl From<fm_frame::AudioBlockError> for NativeMasterError {
    fn from(value: fm_frame::AudioBlockError) -> Self {
        Self::AudioBlock(value)
    }
}

impl From<TimingError> for NativeMasterError {
    fn from(value: TimingError) -> Self {
        Self::Timing(value)
    }
}

impl From<SinkConfigError> for NativeMasterError {
    fn from(value: SinkConfigError) -> Self {
        Self::SinkConfig(value)
    }
}

#[derive(Clone, Debug)]
struct NativeAudioChunk {
    start_sample: u64,
    end_sample: u64,
    planes: Vec<Vec<f32>>,
}

#[derive(Debug)]
struct NativeAudioSource {
    explicit_silence: bool,
    chunks: std::collections::VecDeque<NativeAudioChunk>,
    source_origin_sample: Option<i128>,
    next_sample: u64,
    next_sequence: u64,
    end_of_stream: bool,
    in_flight: Option<NonZeroU32>,
}

impl NativeAudioSource {
    fn silence() -> Self {
        Self {
            explicit_silence: true,
            chunks: std::collections::VecDeque::new(),
            source_origin_sample: None,
            next_sample: 0,
            next_sequence: 0,
            end_of_stream: true,
            in_flight: None,
        }
    }

    fn decoded() -> Self {
        Self {
            explicit_silence: false,
            chunks: std::collections::VecDeque::new(),
            source_origin_sample: None,
            next_sample: 0,
            next_sequence: 0,
            end_of_stream: false,
            in_flight: None,
        }
    }

    fn covers(&self, start_sample: u64, end_sample: u64) -> bool {
        if self.explicit_silence {
            return true;
        }
        if self.end_of_stream && start_sample >= self.next_sample {
            return true;
        }
        let Some(first) = self.chunks.front() else {
            return false;
        };
        if first.start_sample > start_sample {
            return false;
        }
        let available_end = self
            .chunks
            .back()
            .map_or(first.end_sample, |chunk| chunk.end_sample);
        available_end >= end_sample || (self.end_of_stream && available_end == self.next_sample)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NativeAudioCharge {
    blocks: usize,
    samples: usize,
    bytes: usize,
}

impl NativeAudioCharge {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            blocks: self.blocks.checked_add(other.blocks)?,
            samples: self.samples.checked_add(other.samples)?,
            bytes: self.bytes.checked_add(other.bytes)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            blocks: self.blocks.checked_sub(other.blocks)?,
            samples: self.samples.checked_sub(other.samples)?,
            bytes: self.bytes.checked_sub(other.bytes)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct NativeAudioDecodeRequest {
    input: InputId,
    count: NonZeroU32,
    max_samples: usize,
    max_bytes: usize,
}

#[derive(Debug)]
struct NativeAudioDecodeResult {
    input: InputId,
    count: NonZeroU32,
    window: Result<DecodedAudioWindow, fm_codec_ffmpeg::Error>,
}

struct NativeAudioDecodeWorker {
    requests: Option<SyncSender<NativeAudioDecodeRequest>>,
    results: Receiver<NativeAudioDecodeResult>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NativeAudioDecodeWorker {
    fn spawn(
        mut decoders: BTreeMap<InputId, LocalAudioDecoder>,
    ) -> Result<Self, NativeMasterError> {
        let capacity = decoders.len().max(1);
        let (request_sender, request_receiver) =
            mpsc::sync_channel::<NativeAudioDecodeRequest>(capacity);
        let (result_sender, result_receiver) =
            mpsc::sync_channel::<NativeAudioDecodeResult>(capacity);
        let worker = thread::Builder::new()
            .name("freemix-native-audio-decode".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    let window = decoders
                        .get_mut(&request.input)
                        .ok_or(fm_codec_ffmpeg::Error::InvalidConfig)
                        .and_then(|decoder| {
                            decoder.decode_up_to_bounded(
                                request.count,
                                request.max_samples,
                                request.max_bytes,
                            )
                        });
                    if result_sender
                        .send(NativeAudioDecodeResult {
                            input: request.input,
                            count: request.count,
                            window,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| NativeMasterError::WorkerUnavailable)?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            thread: Some(worker),
        })
    }

    fn disconnected_error(&mut self) -> NativeMasterError {
        self.requests.take();
        match self.thread.take().map(thread::JoinHandle::join) {
            Some(Err(_)) => NativeMasterError::WorkerPanicked,
            Some(Ok(())) | None => NativeMasterError::WorkerDisconnected,
        }
    }
}

impl Drop for NativeAudioDecodeWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

struct ValidatedAudioPage {
    chunks: Vec<NativeAudioChunk>,
    source_origin_sample: Option<i128>,
    next_sample: u64,
    next_sequence: u64,
    charge: NativeAudioCharge,
}

/// Independent bounded CPU audio playback and Master mixing runtime.
///
/// It is intentionally separate from [`NativeSourceRegistry`]. Decoder work is
/// confined to its worker; [`Self::render_frame`] only coalesces retained data,
/// mixes one authoritative primary, and writes the bounded fake sink.
pub struct NativeMasterRuntime {
    format: AudioFormat,
    frame_rate: FrameRate,
    clock_domain: ClockDomainId,
    expected_next_frame: u64,
    ready_frame: Option<(u64, u64, u64)>,
    mixer: MasterMixer,
    sink: CollectingAudioSink,
    sources: BTreeMap<InputId, NativeAudioSource>,
    worker: NativeAudioDecodeWorker,
    retained: NativeAudioCharge,
    limits: NativeAudioLimits,
    failed: bool,
}

impl NativeMasterRuntime {
    /// Probes local videos, decodes one bounded initial audio page where an
    /// audio stream exists, and configures all other sources as silence.
    ///
    /// # Errors
    ///
    /// Returns a path-free format, bound, probe, decode, timeline, mixer, sink,
    /// or worker setup failure.
    #[allow(clippy::too_many_lines)]
    pub fn preflight_local_blocking(
        adapter: Option<&Adapter>,
        resolved: &[NativeResolvedSource],
        format: AudioFormat,
        frame_rate: FrameRate,
        clock_domain: ClockDomainId,
        expected_next_frame: u64,
        limits: NativeAudioLimits,
    ) -> Result<Self, NativeMasterError> {
        validate_audio_limits(limits, format.channels.channels().len())?;
        if format.sample_format != SampleFormat::F32 {
            return Err(NativeMasterError::InvalidFormat);
        }
        fm_audio::FrameSampleAllocator::new(format.sample_rate, frame_rate)?;
        let mut mixer = MasterMixer::new(format.clone())?;
        let map = ChannelMap::identity(format.channels.channels().len())?;
        let mut sources = BTreeMap::new();
        let mut decoders = BTreeMap::new();
        let mut retained = NativeAudioCharge::default();

        for source in resolved {
            let input = source.input();
            mixer.add_input(
                input,
                format.clone(),
                map.clone(),
                InputState {
                    follow_video: true,
                    ..InputState::default()
                },
            )?;
            let NativeResolvedSource::LocalVideo { path, .. } = source else {
                sources.insert(input, NativeAudioSource::silence());
                continue;
            };
            let adapter = adapter.ok_or(NativeMasterError::InvalidFormat)?;
            let probe = adapter
                .probe_local(path)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            if !probe
                .streams
                .iter()
                .any(|stream| matches!(stream.kind, StreamKind::Audio))
            {
                sources.insert(input, NativeAudioSource::silence());
                continue;
            }
            let selected = probe
                .select_audio(StreamSelector::Best)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            if selected.sample_rate != Some(format.sample_rate.hertz())
                || selected.channels != u32::try_from(format.channels.channels().len()).ok()
            {
                return Err(NativeMasterError::InvalidFormat);
            }
            let mut decoder = adapter
                .open_local_audio(path, clock_domain, StreamSelector::Best)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            let page_bytes = audio_sample_bytes(
                limits.max_samples_per_page,
                format.channels.channels().len(),
            )?;
            let window = decoder
                .decode_up_to_bounded(
                    limits.max_blocks_per_page,
                    limits.max_samples_per_page,
                    page_bytes,
                )
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            validate_audio_window_contract(input, &window, limits.max_blocks_per_page)?;
            let first_audio_pts = window
                .blocks
                .first()
                .map(|block| block.timing().presentation_timestamp().as_nanos());
            if let Some(first_audio_pts) = first_audio_pts {
                let mut video_decoder = adapter
                    .open_local_video(path, clock_domain, StreamSelector::Best)
                    .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
                let first_video = video_decoder
                    .decode_up_to(NonZeroU32::MIN)
                    .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
                let first_video_pts = first_video
                    .frames
                    .first()
                    .map(|frame| frame.timing().presentation_timestamp().as_nanos())
                    .ok_or(NativeMasterError::InvalidTimeline { input })?;
                if first_audio_pts != first_video_pts {
                    return Err(NativeMasterError::InvalidTimeline { input });
                }
            }
            let mut state = NativeAudioSource::decoded();
            let page = validate_audio_page(input, &state, &window.blocks, &format, clock_domain)?;
            validate_page_bounds(&page, limits)?;
            let next_retained = retained
                .checked_add(page.charge)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            validate_retained_bounds(next_retained, limits)?;
            commit_audio_page(&mut state, page);
            state.end_of_stream = window.end_of_stream;
            retained = next_retained;
            sources.insert(input, state);
            decoders.insert(input, decoder);
        }

        let worker = NativeAudioDecodeWorker::spawn(decoders)?;
        let sink = CollectingAudioSink::new(limits.sink_blocks, OverflowPolicy::DropOldest)?;
        Ok(Self {
            format,
            frame_rate,
            clock_domain,
            expected_next_frame,
            ready_frame: None,
            mixer,
            sink,
            sources,
            worker,
            retained,
            limits,
            failed: false,
        })
    }

    #[must_use]
    pub const fn expected_next_frame(&self) -> u64 {
        self.expected_next_frame
    }

    #[must_use]
    pub const fn retained_blocks(&self) -> usize {
        self.retained.blocks
    }

    #[must_use]
    pub const fn retained_samples(&self) -> usize {
        self.retained.samples
    }

    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained.bytes
    }

    #[must_use]
    pub fn sink_len(&self) -> usize {
        self.sink.len()
    }

    #[must_use]
    pub const fn sink_telemetry(&self) -> SinkTelemetry {
        self.sink.telemetry()
    }

    #[must_use]
    pub fn collected_audio(&self) -> impl ExactSizeIterator<Item = &AudioBlock> {
        self.sink.iter()
    }

    /// Drains completed pages, evicts every source to the next absolute frame
    /// interval, and schedules at most one bounded page per source.
    ///
    /// `false` means a non-EOS source does not yet cover the interval and the
    /// native tick must stall. This function never waits for decoder work.
    ///
    /// # Errors
    ///
    /// Returns a sticky fatal worker, decode, timeline, or resource-bound
    /// failure.
    pub fn service_next_frame(&mut self) -> Result<bool, NativeMasterError> {
        if self.failed {
            return Err(NativeMasterError::Failed);
        }
        let result = self.service_next_frame_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn service_next_frame_inner(&mut self) -> Result<bool, NativeMasterError> {
        let (start_sample, end_sample) = absolute_frame_sample_span(
            self.expected_next_frame,
            self.format.sample_rate.hertz(),
            self.frame_rate,
        )?;
        self.commit_completed_pages()?;
        self.evict_before(start_sample)?;
        self.schedule_refills(start_sample, end_sample)?;
        let covered = self
            .sources
            .values()
            .all(|source| source.covers(start_sample, end_sample));
        self.ready_frame = covered.then_some((self.expected_next_frame, start_sample, end_sample));
        Ok(covered)
    }

    fn commit_completed_pages(&mut self) -> Result<(), NativeMasterError> {
        let mut completed = Vec::with_capacity(self.sources.len());
        loop {
            match self.worker.results.try_recv() {
                Ok(result) => completed.push(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(self.worker.disconnected_error());
                }
            }
        }
        for completed in completed {
            let input = completed.input;
            let window = completed
                .window
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            validate_audio_window_contract(input, &window, completed.count)?;
            let source = self
                .sources
                .get(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            if source.in_flight != Some(completed.count) || source.end_of_stream {
                return Err(NativeMasterError::DecodeContract { input });
            }
            let page = validate_audio_page(
                input,
                source,
                &window.blocks,
                &self.format,
                self.clock_domain,
            )?;
            validate_page_bounds(&page, self.limits)?;
            let retained = self
                .retained
                .checked_add(page.charge)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            validate_retained_bounds(retained, self.limits)?;

            let source = self
                .sources
                .get_mut(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            commit_audio_page(source, page);
            source.in_flight = None;
            source.end_of_stream = window.end_of_stream;
            self.retained = retained;
        }
        Ok(())
    }

    fn evict_before(&mut self, requested_start: u64) -> Result<(), NativeMasterError> {
        for source in self.sources.values_mut() {
            while source
                .chunks
                .front()
                .is_some_and(|front| front.end_sample <= requested_start)
            {
                let removed = source.chunks.pop_front().expect("front was present");
                self.retained = self
                    .retained
                    .checked_sub(chunk_charge(&removed)?)
                    .ok_or(NativeMasterError::BoundsExceeded)?;
            }
            let Some(front) = source.chunks.front_mut() else {
                continue;
            };
            if front.start_sample >= requested_start {
                continue;
            }
            let before = chunk_charge(front)?;
            let removed_samples = usize::try_from(requested_start - front.start_sample)
                .map_err(|_| NativeMasterError::BoundsExceeded)?;
            if removed_samples >= front.planes.first().map_or(0, Vec::len) {
                return Err(NativeMasterError::BoundsExceeded);
            }
            for plane in &mut front.planes {
                *plane = plane.split_off(removed_samples);
            }
            front.start_sample = requested_start;
            let removed = before
                .checked_sub(chunk_charge(front)?)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            self.retained = self
                .retained
                .checked_sub(removed)
                .ok_or(NativeMasterError::BoundsExceeded)?;
        }
        Ok(())
    }

    fn schedule_refills(
        &mut self,
        requested_start: u64,
        requested_end: u64,
    ) -> Result<(), NativeMasterError> {
        let channels = self.format.channels.channels().len();
        let reservation = NativeAudioCharge {
            blocks: usize::try_from(self.limits.max_blocks_per_page.get())
                .map_err(|_| NativeMasterError::BoundsExceeded)?,
            samples: self.limits.max_samples_per_page,
            bytes: audio_sample_bytes(self.limits.max_samples_per_page, channels)?,
        };
        let mut reserved = self
            .sources
            .values()
            .filter(|source| source.in_flight.is_some())
            .try_fold(NativeAudioCharge::default(), |total, _| {
                total.checked_add(reservation)
            })
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let inputs = self.sources.keys().copied().collect::<Vec<_>>();
        for input in inputs {
            let source = self
                .sources
                .get(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            let needs_coverage = !source.covers(requested_start, requested_end);
            if source.explicit_silence
                || source.end_of_stream
                || source.in_flight.is_some()
                || (!needs_coverage && source.chunks.len() > AUDIO_REFILL_LOW_WATERMARK)
            {
                continue;
            }
            let available_blocks = usize::try_from(self.limits.max_blocks_per_source.get())
                .unwrap_or(usize::MAX)
                .saturating_sub(source.chunks.len());
            let count = available_blocks
                .min(usize::try_from(self.limits.max_blocks_per_page.get()).unwrap_or(usize::MAX));
            let Some(count) = u32::try_from(count).ok().and_then(NonZeroU32::new) else {
                if needs_coverage {
                    return Err(NativeMasterError::BoundsExceeded);
                }
                continue;
            };
            let allocated = self
                .retained
                .checked_add(reserved)
                .and_then(|charge| charge.checked_add(reservation));
            if allocated.is_none_or(|charge| validate_retained_bounds(charge, self.limits).is_err())
            {
                if needs_coverage {
                    return Err(NativeMasterError::BoundsExceeded);
                }
                continue;
            }
            let request = NativeAudioDecodeRequest {
                input,
                count,
                max_samples: self.limits.max_samples_per_page,
                max_bytes: reservation.bytes,
            };
            let Some(sender) = self.worker.requests.as_ref() else {
                return Err(self.worker.disconnected_error());
            };
            match sender.try_send(request) {
                Ok(()) => {}
                Err(TrySendError::Disconnected(_)) => {
                    return Err(self.worker.disconnected_error());
                }
                Err(TrySendError::Full(_)) => return Err(NativeMasterError::WorkerQueueFull),
            }
            self.sources
                .get_mut(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?
                .in_flight = Some(count);
            reserved = reserved
                .checked_add(reservation)
                .ok_or(NativeMasterError::BoundsExceeded)?;
        }
        Ok(())
    }

    /// Mixes the serviced Program interval, retains a copy in the bounded fake
    /// sink, and discards the authoritative owned block. Fade linearly weights
    /// both sources across the exact interval; Cut keeps one source at unity.
    ///
    /// This method performs no probe, decode, channel mapping, or blocking wait.
    ///
    /// # Errors
    ///
    /// Returns a sticky fatal frame-order, readiness, coalescing, mix, timing,
    /// sink, or resource-bound failure.
    pub fn render_frame(&mut self, frame: &FrameResult) -> Result<(), NativeMasterError> {
        self.render_frame_audio(frame).map(drop)
    }

    /// Mixes one serviced authoritative Program interval and returns its exact
    /// owned audio block. A clone is retained in the bounded fake sink so
    /// existing diagnostics remain identical to [`Self::render_frame`].
    ///
    /// Fade linearly weights both sources across the exact interval; Cut keeps
    /// one source at unity. This method performs no probe, decode, channel
    /// mapping, or blocking wait.
    ///
    /// # Errors
    ///
    /// Returns a sticky fatal frame-order, readiness, coalescing, mix, timing,
    /// sink, or resource-bound failure. A failed frame does not advance the
    /// frame cursor or clear its serviced readiness state.
    pub fn render_frame_audio(
        &mut self,
        frame: &FrameResult,
    ) -> Result<AudioBlock, NativeMasterError> {
        if self.failed {
            return Err(NativeMasterError::Failed);
        }
        let result = self.render_frame_audio_inner(frame);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn render_frame_audio_inner(
        &mut self,
        frame: &FrameResult,
    ) -> Result<AudioBlock, NativeMasterError> {
        let actual = frame.frame.get();
        if actual != self.expected_next_frame {
            return Err(NativeMasterError::UnexpectedFrame {
                expected: self.expected_next_frame,
                actual,
            });
        }
        let Some((ready_frame, start_sample, end_sample)) = self.ready_frame else {
            return Err(NativeMasterError::FrameNotReady(actual));
        };
        if ready_frame != actual {
            return Err(NativeMasterError::FrameNotReady(actual));
        }
        let timing = output_audio_timing(
            actual,
            start_sample,
            end_sample,
            self.format.sample_rate.hertz(),
            self.clock_domain,
        )?;
        let samples = usize::try_from(end_sample - start_sample)
            .map_err(|_| NativeMasterError::BoundsExceeded)?;
        let plan = native_audio_mix_plan(frame.program)?;
        let primary_block = coalesce_source(
            self.sources
                .get(&plan.primary)
                .ok_or(NativeMasterError::DecodeContract {
                    input: plan.primary,
                })?,
            timing,
            start_sample,
            end_sample,
            &self.format,
        )?;
        let mut next_mixer = self.mixer.clone();
        let output = if let Some((secondary, secondary_gain)) = plan.secondary {
            let secondary_block = coalesce_source(
                self.sources
                    .get(&secondary)
                    .ok_or(NativeMasterError::DecodeContract { input: secondary })?,
                timing,
                start_sample,
                end_sample,
                &self.format,
            )?;
            next_mixer.mix_timed_with_source_gains(
                timing,
                samples,
                &[
                    (plan.primary, &primary_block, plan.primary_gain),
                    (secondary, &secondary_block, secondary_gain),
                ],
                &[plan.primary, secondary],
            )?
        } else {
            next_mixer.mix_timed(
                timing,
                samples,
                &[(plan.primary, &primary_block)],
                Some(plan.primary),
            )?
        };
        let block = output.block;
        self.sink
            .collect(block.clone())
            .map_err(|_| NativeMasterError::SinkRejected)?;
        self.mixer = next_mixer;
        self.expected_next_frame = self
            .expected_next_frame
            .checked_add(1)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        self.ready_frame = None;
        Ok(block)
    }
}

fn validate_audio_limits(
    limits: NativeAudioLimits,
    channels: usize,
) -> Result<(), NativeMasterError> {
    let page_blocks = usize::try_from(limits.max_blocks_per_page.get()).unwrap_or(usize::MAX);
    let source_blocks = usize::try_from(limits.max_blocks_per_source.get()).unwrap_or(usize::MAX);
    if page_blocks > source_blocks
        || limits.max_samples_per_page == 0
        || limits.max_retained_blocks == 0
        || limits.max_retained_samples < limits.max_samples_per_page
        || limits.max_retained_bytes < audio_sample_bytes(limits.max_samples_per_page, channels)?
        || limits.sink_blocks == 0
    {
        return Err(NativeMasterError::InvalidLimits);
    }
    Ok(())
}

fn validate_audio_window_contract(
    input: InputId,
    window: &DecodedAudioWindow,
    requested: NonZeroU32,
) -> Result<(), NativeMasterError> {
    let requested = usize::try_from(requested.get()).unwrap_or(usize::MAX);
    if window.blocks.len() > requested
        || (window.blocks.is_empty() && !window.end_of_stream)
        || (!window.end_of_stream && window.blocks.len() != requested)
    {
        return Err(NativeMasterError::DecodeContract { input });
    }
    Ok(())
}

fn validate_audio_page(
    input: InputId,
    source: &NativeAudioSource,
    blocks: &[AudioBlock],
    format: &AudioFormat,
    clock_domain: ClockDomainId,
) -> Result<ValidatedAudioPage, NativeMasterError> {
    let mut origin = source.source_origin_sample;
    let mut next_sample = source.next_sample;
    let mut next_sequence = source.next_sequence;
    let mut chunks = Vec::with_capacity(blocks.len());
    let mut charge = NativeAudioCharge::default();
    for block in blocks {
        if block.sample_rate() != format.sample_rate
            || block.channel_layout() != &format.channels
            || block.timing().clock_domain() != clock_domain
            || block.timing().sequence().get() != next_sequence
        {
            return Err(NativeMasterError::InvalidTimeline { input });
        }
        let raw_start = original_timestamp_samples(block.timing(), format.sample_rate.hertz())
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
        let source_origin = *origin.get_or_insert(raw_start);
        let rebased = raw_start
            .checked_sub(source_origin)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
        if rebased != next_sample {
            return Err(NativeMasterError::InvalidTimeline { input });
        }
        let sample_count = block.sample_count();
        let sample_count_u64 = u64::try_from(sample_count)
            .map_err(|_| NativeMasterError::InvalidTimeline { input })?;
        let end_sample = next_sample
            .checked_add(sample_count_u64)
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
        validate_block_normalized_timing(block, raw_start, format.sample_rate.hertz(), input)?;
        let chunk = NativeAudioChunk {
            start_sample: next_sample,
            end_sample,
            planes: block.planes().to_vec(),
        };
        charge = charge
            .checked_add(chunk_charge(&chunk)?)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        chunks.push(chunk);
        next_sample = end_sample;
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
    }
    Ok(ValidatedAudioPage {
        chunks,
        source_origin_sample: origin,
        next_sample,
        next_sequence,
        charge,
    })
}

fn validate_block_normalized_timing(
    block: &AudioBlock,
    raw_start_sample: i128,
    sample_rate: u32,
    input: InputId,
) -> Result<(), NativeMasterError> {
    let raw_end_sample = raw_start_sample
        .checked_add(
            i128::try_from(block.sample_count())
                .map_err(|_| NativeMasterError::InvalidTimeline { input })?,
        )
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    let normalized_start = normalized_sample_endpoint(raw_start_sample, sample_rate)
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    let normalized_end = normalized_sample_endpoint(raw_end_sample, sample_rate)
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    let duration = normalized_end
        .checked_sub(normalized_start)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    if block.timing().presentation_timestamp().as_nanos() != normalized_start
        || block.timing().duration().as_nanos() != duration
    {
        return Err(NativeMasterError::InvalidTimeline { input });
    }
    Ok(())
}

fn original_timestamp_samples(timing: MediaTiming, sample_rate: u32) -> Option<i128> {
    let original = timing.original_timestamp();
    let time_base = original.time_base();
    let numerator = i128::from(original.timestamp().ticks())
        .checked_mul(i128::from(time_base.numerator()))?
        .checked_mul(i128::from(sample_rate))?;
    let denominator = i128::from(time_base.denominator());
    (numerator % denominator == 0).then_some(numerator / denominator)
}

fn normalized_sample_endpoint(sample: i128, sample_rate: u32) -> Option<i64> {
    sample
        .checked_mul(1_000_000_000)?
        .checked_div(i128::from(sample_rate))?
        .try_into()
        .ok()
}

fn commit_audio_page(source: &mut NativeAudioSource, page: ValidatedAudioPage) {
    source.chunks.extend(page.chunks);
    source.source_origin_sample = page.source_origin_sample;
    source.next_sample = page.next_sample;
    source.next_sequence = page.next_sequence;
}

fn validate_page_bounds(
    page: &ValidatedAudioPage,
    limits: NativeAudioLimits,
) -> Result<(), NativeMasterError> {
    if page.charge.blocks > usize::try_from(limits.max_blocks_per_page.get()).unwrap_or(usize::MAX)
        || page.charge.samples > limits.max_samples_per_page
        || page.charge.bytes > limits.max_retained_bytes
    {
        return Err(NativeMasterError::BoundsExceeded);
    }
    Ok(())
}

fn validate_retained_bounds(
    charge: NativeAudioCharge,
    limits: NativeAudioLimits,
) -> Result<(), NativeMasterError> {
    if charge.blocks > limits.max_retained_blocks
        || charge.samples > limits.max_retained_samples
        || charge.bytes > limits.max_retained_bytes
    {
        return Err(NativeMasterError::BoundsExceeded);
    }
    Ok(())
}

fn chunk_charge(chunk: &NativeAudioChunk) -> Result<NativeAudioCharge, NativeMasterError> {
    let samples = chunk.planes.first().map_or(0, Vec::len);
    let bytes = chunk.planes.iter().try_fold(0_usize, |total, plane| {
        plane
            .capacity()
            .checked_mul(size_of::<f32>())
            .and_then(|plane_bytes| total.checked_add(plane_bytes))
    });
    Ok(NativeAudioCharge {
        blocks: 1,
        samples,
        bytes: bytes.ok_or(NativeMasterError::BoundsExceeded)?,
    })
}

fn audio_sample_bytes(samples: usize, channels: usize) -> Result<usize, NativeMasterError> {
    samples
        .checked_mul(channels)
        .and_then(|value| value.checked_mul(size_of::<f32>()))
        .ok_or(NativeMasterError::BoundsExceeded)
}

fn absolute_frame_sample_span(
    frame: u64,
    sample_rate: u32,
    frame_rate: FrameRate,
) -> Result<(u64, u64), NativeMasterError> {
    let samples_per_frame_numerator =
        u128::from(sample_rate) * u128::from(frame_rate.denominator());
    let denominator = u128::from(frame_rate.numerator());
    let start = u128::from(frame)
        .checked_mul(samples_per_frame_numerator)
        .ok_or(NativeMasterError::BoundsExceeded)?
        / denominator;
    let end = u128::from(frame)
        .checked_add(1)
        .and_then(|value| value.checked_mul(samples_per_frame_numerator))
        .ok_or(NativeMasterError::BoundsExceeded)?
        / denominator;
    Ok((
        u64::try_from(start).map_err(|_| NativeMasterError::BoundsExceeded)?,
        u64::try_from(end).map_err(|_| NativeMasterError::BoundsExceeded)?,
    ))
}

fn output_audio_timing(
    frame: u64,
    start_sample: u64,
    end_sample: u64,
    sample_rate: u32,
    clock_domain: ClockDomainId,
) -> Result<MediaTiming, NativeMasterError> {
    let start_tick = i64::try_from(start_sample).map_err(|_| NativeMasterError::BoundsExceeded)?;
    let start_ns = normalized_sample_endpoint(i128::from(start_sample), sample_rate)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let end_ns = normalized_sample_endpoint(i128::from(end_sample), sample_rate)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let duration_ns = end_ns
        .checked_sub(start_ns)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let time_base = TimeBase::new(1, sample_rate).map_err(|_| NativeMasterError::InvalidFormat)?;
    Ok(MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(start_tick), time_base),
        NormalizedTimestamp::from_nanos(start_ns),
        NormalizedDuration::from_nanos(duration_ns)?,
        clock_domain,
        SequenceNumber::new(frame),
    )?)
}

fn coalesce_source(
    source: &NativeAudioSource,
    timing: MediaTiming,
    start_sample: u64,
    end_sample: u64,
    format: &AudioFormat,
) -> Result<AudioBlock, NativeMasterError> {
    if !source.covers(start_sample, end_sample) {
        return Err(NativeMasterError::FrameNotReady(timing.sequence().get()));
    }
    let samples = usize::try_from(end_sample - start_sample)
        .map_err(|_| NativeMasterError::BoundsExceeded)?;
    let mut planes = vec![vec![0.0; samples]; format.channels.channels().len()];
    for chunk in &source.chunks {
        let copy_start = chunk.start_sample.max(start_sample);
        let copy_end = chunk.end_sample.min(end_sample);
        if copy_start >= copy_end {
            continue;
        }
        let source_start = usize::try_from(copy_start - chunk.start_sample)
            .map_err(|_| NativeMasterError::BoundsExceeded)?;
        let destination_start = usize::try_from(copy_start - start_sample)
            .map_err(|_| NativeMasterError::BoundsExceeded)?;
        let count = usize::try_from(copy_end - copy_start)
            .map_err(|_| NativeMasterError::BoundsExceeded)?;
        for (destination, source_plane) in planes.iter_mut().zip(&chunk.planes) {
            destination[destination_start..destination_start + count]
                .copy_from_slice(&source_plane[source_start..source_start + count]);
        }
    }
    Ok(AudioBlock::new(
        timing,
        format.sample_rate,
        format.channels.clone(),
        planes,
    )?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeAudioMixPlan {
    primary: InputId,
    primary_gain: SourceGain,
    secondary: Option<(InputId, SourceGain)>,
}

fn native_audio_mix_plan(
    program: ProgramFrame,
) -> Result<NativeAudioMixPlan, fm_audio::AudioError> {
    let Some(secondary) = program.secondary else {
        return Ok(NativeAudioMixPlan {
            primary: program.primary,
            primary_gain: SourceGain::UNITY,
            secondary: None,
        });
    };
    if secondary == program.primary {
        return Ok(NativeAudioMixPlan {
            primary: program.primary,
            primary_gain: SourceGain::UNITY,
            secondary: None,
        });
    }
    let secondary_gain = SourceGain::new(
        program.mix_start_numerator,
        program.mix_end_numerator,
        program.mix_denominator,
    )?;
    let primary_gain = SourceGain::new(
        program.mix_denominator - program.mix_start_numerator,
        program.mix_denominator - program.mix_end_numerator,
        program.mix_denominator,
    )?;
    Ok(NativeAudioMixPlan {
        primary: program.primary,
        primary_gain,
        secondary: Some((secondary, secondary_gain)),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeMixPlan {
    primary: InputId,
    secondary: InputId,
    transition: TransitionPlan,
}

fn native_mix_plan(program: ProgramFrame) -> Result<NativeMixPlan, TransitionError> {
    let (secondary, transition) = match program.secondary {
        Some(secondary) if secondary != program.primary => (
            secondary,
            TransitionPlan::compile(
                TransitionKind::Fade,
                program.mix_numerator,
                program.mix_denominator,
            )?,
        ),
        Some(_) | None => (
            program.primary,
            TransitionPlan::compile(TransitionKind::Cut, 0, 1)?,
        ),
    };
    Ok(NativeMixPlan {
        primary: program.primary,
        secondary,
        transition,
    })
}

#[cfg(test)]
fn prefix_decode_request(
    clock_domain: ClockDomainId,
    selector: StreamSelector,
    count: NonZeroU32,
) -> DecodeRequest {
    DecodeRequest {
        clock_domain,
        video: Some(SequenceRequest { selector, count }),
        audio: None,
    }
}

fn validate_source_ids<T>(
    sources: &[(InputId, T)],
    maximum: usize,
) -> Result<(), NativeSourceError> {
    if sources.len() > maximum {
        return Err(NativeSourceError::TooManySources {
            actual: sources.len(),
            maximum,
        });
    }
    let mut ids = BTreeSet::new();
    for (input, _) in sources {
        if !ids.insert(*input) {
            return Err(NativeSourceError::DuplicateSource(*input));
        }
    }
    Ok(())
}

fn validate_resolved_sources(
    sources: &[NativeResolvedSource],
    adapter: Option<&Adapter>,
    maximum: usize,
) -> Result<(), NativeSourcePreflightError> {
    let ids = sources
        .iter()
        .map(|source| (source.input(), ()))
        .collect::<Vec<_>>();
    validate_source_ids(&ids, maximum)?;
    if adapter.is_none()
        && let Some(input) = sources.iter().find_map(|source| match source {
            NativeResolvedSource::LocalVideo { input, .. } => Some(*input),
            NativeResolvedSource::RetainedFrame { .. } | NativeResolvedSource::LiveFrame { .. } => {
                None
            }
        })
    {
        return Err(NativeSourcePreflightError::CodecAdapterRequired { input });
    }
    Ok(())
}

fn validate_source_layouts(
    sources: &[(InputId, u32, u32, usize)],
    limits: NativeSourceLimits,
) -> Result<(Option<(u32, u32)>, u64), NativeSourceError> {
    validate_source_ids(
        &sources
            .iter()
            .map(|(input, _, _, _)| (*input, ()))
            .collect::<Vec<_>>(),
        limits.max_media_inputs,
    )?;

    let mut dimensions = None;
    let mut retained = 0_u64;
    for &(input, width, height, frame_count) in sources {
        let frame_count_is_bounded = u32::try_from(frame_count)
            .is_ok_and(|count| count <= limits.max_video_frames_per_source.get());
        if frame_count == 0 {
            return Err(NativeSourceError::InvalidTimeline { input });
        }
        if !frame_count_is_bounded {
            return Err(NativeSourceError::TooManyFrames {
                input,
                actual: frame_count,
                maximum: limits.max_video_frames_per_source.get(),
            });
        }
        if let Some((expected_width, expected_height)) = dimensions {
            if (width, height) != (expected_width, expected_height) {
                return Err(NativeSourceError::DimensionMismatch {
                    input,
                    expected_width,
                    expected_height,
                    actual_width: width,
                    actual_height: height,
                });
            }
        } else {
            dimensions = Some((width, height));
        }
        let frame_bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(RGBA16_FLOAT_BYTES_PER_PIXEL))
            .ok_or(NativeSourceError::FrameByteSizeOverflow {
                input,
                width,
                height,
            })?;
        let source_bytes = frame_bytes
            .checked_mul(u64::try_from(frame_count).unwrap_or(u64::MAX))
            .ok_or(NativeSourceError::RetainedBytesExceeded {
                required: u64::MAX,
                maximum: limits.max_retained_rgba16f_bytes,
            })?;
        retained =
            retained
                .checked_add(source_bytes)
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: limits.max_retained_rgba16f_bytes,
                })?;
        if retained > limits.max_retained_rgba16f_bytes {
            return Err(NativeSourceError::RetainedBytesExceeded {
                required: retained,
                maximum: limits.max_retained_rgba16f_bytes,
            });
        }
    }
    Ok((dimensions, retained))
}

fn validate_page_timing(
    input: InputId,
    frames: &[CpuVideoFrame],
    source_pts_origin: i64,
    previous_pts: Option<i64>,
    expected_sequence: u64,
    clock_domain: ClockDomainId,
) -> Result<(Vec<u64>, i64, u64), NativeSourceError> {
    let timestamps = frames
        .iter()
        .map(|frame| frame.timing().presentation_timestamp().as_nanos())
        .collect::<Vec<_>>();
    let sequences = frames
        .iter()
        .map(|frame| frame.timing().sequence().get())
        .collect::<Vec<_>>();
    if frames
        .iter()
        .any(|frame| frame.timing().clock_domain() != clock_domain)
    {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    validate_timing_values(
        input,
        &timestamps,
        &sequences,
        source_pts_origin,
        previous_pts,
        expected_sequence,
    )
}

fn validate_retained_frame_timing(
    input: InputId,
    frame: &CpuVideoFrame,
    clock_domain: ClockDomainId,
) -> Result<(Vec<u64>, i64, u64), NativeSourceError> {
    let timing = frame.timing();
    if timing.duration().as_nanos() == 0 {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    let source_pts_origin = timing.presentation_timestamp().as_nanos();
    validate_page_timing(
        input,
        std::slice::from_ref(frame),
        source_pts_origin,
        None,
        0,
        clock_domain,
    )
}

fn validate_live_seed_timing(
    input: InputId,
    frame: &CpuVideoFrame,
) -> Result<(Vec<u64>, i64, u64, ClockDomainId), NativeSourceError> {
    let timing = frame.timing();
    if timing.duration().as_nanos() == 0 {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    Ok((
        vec![0],
        timing.presentation_timestamp().as_nanos(),
        timing.sequence().get(),
        timing.clock_domain(),
    ))
}

fn validate_live_update_timing(
    input: InputId,
    frame: &CpuVideoFrame,
    previous_pts: i64,
    previous_sequence: u64,
    clock_domain: ClockDomainId,
) -> Result<(), NativeSourceError> {
    let timing = frame.timing();
    if timing.duration().as_nanos() == 0
        || timing.clock_domain() != clock_domain
        || timing.presentation_timestamp().as_nanos() <= previous_pts
        || timing.sequence().get() <= previous_sequence
    {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    Ok(())
}

fn validate_timing_values(
    input: InputId,
    timestamps: &[i64],
    sequences: &[u64],
    source_pts_origin: i64,
    mut previous_pts: Option<i64>,
    mut expected_sequence: u64,
) -> Result<(Vec<u64>, i64, u64), NativeSourceError> {
    if timestamps.is_empty() || timestamps.len() != sequences.len() {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    let mut offsets = Vec::with_capacity(timestamps.len());
    for (index, (&pts, &sequence)) in timestamps.iter().zip(sequences).enumerate() {
        if previous_pts.is_some_and(|previous| pts <= previous) || sequence != expected_sequence {
            return Err(NativeSourceError::InvalidTimeline { input });
        }
        offsets.push(
            u64::try_from(i128::from(pts) - i128::from(source_pts_origin))
                .map_err(|_| NativeSourceError::InvalidTimeline { input })?,
        );
        previous_pts = Some(pts);
        if index + 1 < sequences.len() {
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or(NativeSourceError::InvalidTimeline { input })?;
        }
    }
    Ok((
        offsets,
        previous_pts.expect("nonempty timestamps"),
        *sequences.last().expect("nonempty sequences"),
    ))
}

#[cfg(test)]
fn rebased_offsets(input: InputId, timestamps: &[i64]) -> Result<Vec<u64>, NativeSourceError> {
    let first = *timestamps
        .first()
        .ok_or(NativeSourceError::InvalidTimeline { input })?;
    let mut previous = None;
    timestamps
        .iter()
        .map(|&pts| {
            if previous.is_some_and(|previous| pts <= previous) {
                return Err(NativeSourceError::InvalidTimeline { input });
            }
            previous = Some(pts);
            u64::try_from(i128::from(pts) - i128::from(first))
                .map_err(|_| NativeSourceError::InvalidTimeline { input })
        })
        .collect()
}

fn frame_index_at_deadline(offsets_ns: &[u64], deadline_ns: u64) -> Option<usize> {
    offsets_ns
        .partition_point(|offset| *offset <= deadline_ns)
        .checked_sub(1)
}

fn floor_anchor_eviction_count(offsets_ns: &[u64], deadline: ClockTime) -> usize {
    frame_index_at_deadline(offsets_ns, deadline.as_nanos()).unwrap_or_default()
}

fn source_covers_deadline(
    latest_offset_ns: Option<u64>,
    end_of_stream: bool,
    deadline: ClockTime,
) -> bool {
    end_of_stream || latest_offset_ns.is_some_and(|latest| latest >= deadline.as_nanos())
}

fn refill_page_size(
    retained_frames: usize,
    in_flight: bool,
    end_of_stream: bool,
    maximum_frames: u32,
    budget_frames: u64,
) -> Option<NonZeroU32> {
    if in_flight
        || end_of_stream
        || retained_frames > SOURCE_REFILL_LOW_WATERMARK
        || retained_frames >= usize::try_from(maximum_frames).unwrap_or(usize::MAX)
    {
        return None;
    }
    let available_ring = u64::from(maximum_frames)
        .saturating_sub(u64::try_from(retained_frames).unwrap_or(u64::MAX));
    let count = available_ring
        .min(u64::from(SOURCE_REFILL_MAX_PAGE))
        .min(budget_frames);
    u32::try_from(count).ok().and_then(NonZeroU32::new)
}

fn rgba16f_frame_bytes(input: InputId, width: u32, height: u32) -> Result<u64, NativeSourceError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(RGBA16_FLOAT_BYTES_PER_PIXEL))
        .ok_or(NativeSourceError::FrameByteSizeOverflow {
            input,
            width,
            height,
        })
}

fn registry_frame(
    registry: &NativeSourceRegistry,
    input: InputId,
    deadline: ClockTime,
) -> Result<&NativeWorkingFrame, NativeSourceRenderError> {
    let prefix = registered_source(&registry.sources, input)?;
    let frame = prefix
        .frame_at_deadline(deadline)
        .ok_or(NativeSourceRenderError::MissingSource { input })?;
    if let Some((expected_width, expected_height)) = registry.dimensions {
        let actual_width = frame.texture().width();
        let actual_height = frame.texture().height();
        if (actual_width, actual_height) != (expected_width, expected_height) {
            return Err(NativeSourceRenderError::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            });
        }
    }
    Ok(frame)
}

fn registered_source<T>(
    sources: &BTreeMap<InputId, T>,
    input: InputId,
) -> Result<&T, NativeSourceRenderError> {
    sources
        .get(&input)
        .ok_or(NativeSourceRenderError::MissingSource { input })
}

/// One native context shared by the import and Cut/Fade executors.
pub struct NativeMediaRuntime {
    context: NativeContext,
    normalizer: NativeImportNormalizer,
    renderer: NativeTransitionRenderer,
}

/// Reusable private GPU target for blocking SDR Program capture.
///
/// The target is fixed-size `Rgba8Unorm`, and the transform explicitly writes
/// sRGB-encoded Rec.709 pixels from canonical `Rgba16Float` Program light. No
/// native texture or backend handle is exposed.
pub struct NativeProgramReadback {
    target: NativeTexture,
    transform: NativeSdrOutputTransform,
}

impl NativeProgramReadback {
    /// Returns the fixed output width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.target.width()
    }

    /// Returns the fixed output height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.target.height()
    }
}

impl NativeMediaRuntime {
    /// Synchronously creates a native runtime without requiring an async
    /// executor. The calling thread remains the runtime owner.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU, color-pipeline, or compositor-pipeline failure.
    pub fn new_blocking(
        backends: impl IntoIterator<Item = NativeBackend>,
    ) -> Result<Self, NativeMediaError> {
        block_on(Self::new(backends))
    }

    /// Selects an adapter from `backends` and compiles both native pipelines on
    /// the resulting single context.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU, color-pipeline, or compositor-pipeline failure.
    pub async fn new(
        backends: impl IntoIterator<Item = NativeBackend>,
    ) -> Result<Self, NativeMediaError> {
        let context = NativeContext::new(backends).await?;
        Self::from_context(context).await
    }

    /// Synchronously compiles the native media pipelines on an existing context.
    ///
    /// # Errors
    ///
    /// Returns a typed color-pipeline or compositor-pipeline failure.
    pub fn from_context_blocking(context: NativeContext) -> Result<Self, NativeMediaError> {
        block_on(Self::from_context(context))
    }

    /// Compiles both native media pipelines on an existing context.
    ///
    /// # Errors
    ///
    /// Returns a typed color-pipeline or compositor-pipeline failure.
    pub async fn from_context(context: NativeContext) -> Result<Self, NativeMediaError> {
        let normalizer = NativeImportNormalizer::new(&context).await?;
        let renderer = NativeTransitionRenderer::new(&context).await?;
        Ok(Self {
            context,
            normalizer,
            renderer,
        })
    }

    /// Returns the context shared by import and compositor resources.
    #[must_use]
    pub const fn context(&self) -> &NativeContext {
        &self.context
    }

    /// Synchronously decodes a bounded local file, then asynchronously uploads
    /// and normalizes every decoded video frame on this runtime's GPU context.
    /// Decoded audio blocks are preserved unchanged.
    ///
    /// The adapter's subprocess decode is blocking and this method belongs on
    /// a worker thread. Only the subsequent GPU normalization is asynchronous.
    ///
    /// # Errors
    ///
    /// Returns a typed `FFmpeg` or native color/GPU failure. Error messages do
    /// not include the input path.
    pub async fn preroll_local_blocking(
        &self,
        adapter: &Adapter,
        path: impl AsRef<Path>,
        request: DecodeRequest,
    ) -> Result<NativeMediaPreroll, NativeMediaError> {
        let decoded = adapter.decode_local(path, request)?;
        let mut video = Vec::with_capacity(decoded.video.len());
        for frame in &decoded.video {
            video.push(self.normalizer.normalize(&self.context, frame).await?);
        }
        Ok(NativeMediaPreroll {
            video,
            audio: decoded.audio,
        })
    }

    /// Preflights resolved local media into atomic bounded GPU prefixes.
    ///
    /// Each `(InputId, PathBuf)` must already have been resolved by the project
    /// store (or another policy-owning resolver). The same `FFmpeg` adapter is
    /// reused to decode up to the configured number of leading selected video
    /// frames and no audio from each source. All ID, timeline, dimension, and
    /// retained RGBA16F-byte checks finish before any upload; every accepted
    /// frame is then normalized/uploaded once.
    /// Rendering the returned registry performs no decode or source upload.
    ///
    /// `FFmpeg` and ffprobe subprocess work is blocking. Call this method from a
    /// blocking worker even though GPU normalization makes the method async.
    /// The registry is returned only after every source succeeds; failed work
    /// and any temporary GPU textures are dropped without exposing a partial
    /// registry.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound, decode-contract, decode, or
    /// normalization failure.
    pub async fn preflight_resolved_sources_local_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourceRegistry, NativeSourcePreflightError> {
        self.preflight_resolved_source_playback_local_blocking(
            adapter,
            sources,
            clock_domain,
            selector,
            limits,
        )
        .await
        .map(NativeSourcePlayback::into_registry)
    }

    /// Preflights bounded source rings and retains their sequential cursors in
    /// one background CPU decode worker.
    ///
    /// All initial decode contracts, timelines, dimensions, and retained-byte
    /// charges are validated before any GPU upload. GPU normalization remains
    /// on this runtime and is committed only after a complete source batch has
    /// normalized. Retained-byte accounting covers committed RGBA16F ring
    /// textures; temporary CPU pages and normalization staging are not retained
    /// and are dropped if the batch cannot be committed.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    pub async fn preflight_resolved_source_playback_local_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        self.preflight_resolved_source_playback_mixed_local_blocking(
            Some(adapter),
            sources
                .into_iter()
                .map(|(input, path)| NativeResolvedSource::LocalVideo { input, path }),
            clock_domain,
            selector,
            limits,
        )
        .await
    }

    /// Preflights a mix of resolved local videos and retained CPU frames into
    /// bounded source rings.
    ///
    /// Local videos preserve their sequential decode cursors for worker refill.
    /// Each retained frame is rebased to offset zero and retained as an EOS
    /// source without a decoder cursor. All source IDs, initial timelines,
    /// dimensions, and retained RGBA16F charges are validated before any GPU
    /// upload. An adapter is required only when at least one local video exists.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    #[allow(clippy::too_many_lines)]
    pub async fn preflight_resolved_source_playback_mixed_local_blocking(
        &self,
        adapter: Option<&Adapter>,
        sources: impl IntoIterator<Item = NativeResolvedSource>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        let sources = sources.into_iter().collect::<Vec<_>>();
        validate_resolved_sources(&sources, adapter, limits.max_media_inputs)?;

        let mut decoded_sources = Vec::with_capacity(sources.len());
        let mut decoders = BTreeMap::new();
        for source in sources {
            match source {
                NativeResolvedSource::LocalVideo { input, path } => {
                    let adapter = adapter
                        .ok_or(NativeSourcePreflightError::CodecAdapterRequired { input })?;
                    let mut decoder = adapter
                        .open_local_video(path, clock_domain, selector)
                        .map_err(|error| NativeSourcePreflightError::Decode { input, error })?;
                    let initial_window =
                        decoder
                            .decode_up_to(limits.max_video_frames_per_source)
                            .map_err(|error| NativeSourcePreflightError::Decode { input, error })?;
                    let video_frames = initial_window.frames.len();
                    let frame_count_is_bounded = u32::try_from(video_frames)
                        .is_ok_and(|count| count <= limits.max_video_frames_per_source.get());
                    if video_frames == 0
                        || !frame_count_is_bounded
                        || (!initial_window.end_of_stream
                            && video_frames
                                != usize::try_from(limits.max_video_frames_per_source.get())
                                    .unwrap_or(usize::MAX))
                    {
                        return Err(NativeSourcePreflightError::DecodeContract {
                            input,
                            video_frames,
                            audio_blocks: 0,
                        });
                    }
                    let source_pts_origin = initial_window.frames[0]
                        .timing()
                        .presentation_timestamp()
                        .as_nanos();
                    let (offsets_ns, last_source_pts, last_sequence) = validate_page_timing(
                        input,
                        &initial_window.frames,
                        source_pts_origin,
                        None,
                        0,
                        clock_domain,
                    )?;
                    decoded_sources.push((
                        input,
                        initial_window.frames,
                        offsets_ns,
                        source_pts_origin,
                        last_source_pts,
                        last_sequence,
                        initial_window.end_of_stream,
                        clock_domain,
                        NativeVideoSourceKind::Decoded,
                    ));
                    decoders.insert(input, decoder);
                }
                NativeResolvedSource::RetainedFrame { input, frame } => {
                    let source_pts_origin = frame.timing().presentation_timestamp().as_nanos();
                    let (offsets_ns, last_source_pts, last_sequence) =
                        validate_retained_frame_timing(input, &frame, clock_domain)?;
                    decoded_sources.push((
                        input,
                        vec![frame],
                        offsets_ns,
                        source_pts_origin,
                        last_source_pts,
                        last_sequence,
                        true,
                        clock_domain,
                        NativeVideoSourceKind::Retained,
                    ));
                }
                NativeResolvedSource::LiveFrame { input, frame } => {
                    let (offsets_ns, last_source_pts, last_sequence, source_clock_domain) =
                        validate_live_seed_timing(input, &frame)?;
                    decoded_sources.push((
                        input,
                        vec![frame],
                        offsets_ns,
                        last_source_pts,
                        last_source_pts,
                        last_sequence,
                        false,
                        source_clock_domain,
                        NativeVideoSourceKind::Live,
                    ));
                }
            }
        }

        let mut layouts = Vec::with_capacity(decoded_sources.len());
        for (input, frames, ..) in &decoded_sources {
            let dimensions = frames
                .first()
                .ok_or(NativeSourceError::InvalidTimeline { input: *input })?
                .payload()
                .dimensions();
            for frame in &frames[1..] {
                let actual = frame.payload().dimensions();
                if actual != dimensions {
                    return Err(NativeSourceError::DimensionMismatch {
                        input: *input,
                        expected_width: dimensions.width(),
                        expected_height: dimensions.height(),
                        actual_width: actual.width(),
                        actual_height: actual.height(),
                    }
                    .into());
                }
            }
            layouts.push((
                *input,
                dimensions.width(),
                dimensions.height(),
                frames.len(),
            ));
        }
        let (dimensions, retained_rgba16f_bytes) = validate_source_layouts(&layouts, limits)?;

        let mut registry = BTreeMap::new();
        for (
            input,
            frames,
            offsets_ns,
            source_pts_origin,
            last_source_pts,
            last_sequence,
            end_of_stream,
            source_clock_domain,
            kind,
        ) in decoded_sources
        {
            let mut normalized_frames = Vec::with_capacity(frames.len());
            for frame in &frames {
                normalized_frames.push(
                    self.normalizer
                        .normalize(&self.context, frame)
                        .await
                        .map_err(|error| NativeSourcePreflightError::Normalize { input, error })?,
                );
            }
            registry.insert(
                input,
                NativeVideoPrefix {
                    frames: normalized_frames,
                    offsets_ns,
                    source_pts_origin,
                    last_source_pts,
                    last_sequence,
                    clock_domain: source_clock_domain,
                    kind,
                    end_of_stream,
                    in_flight: None,
                },
            );
        }
        let worker = NativeDecodeWorker::spawn(decoders)?;
        Ok(NativeSourcePlayback {
            registry: NativeSourceRegistry {
                sources: registry,
                dimensions,
                retained_rgba16f_bytes,
                limits,
            },
            worker,
            failed: false,
        })
    }

    /// Synchronous daemon wrapper for bounded source-prefix preflight.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound, decode-contract, decode, or
    /// normalization failure.
    pub fn preflight_resolved_sources_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourceRegistry, NativeSourcePreflightError> {
        block_on(self.preflight_resolved_sources_local_blocking(
            adapter,
            sources,
            clock_domain,
            selector,
            limits,
        ))
    }

    /// Synchronous daemon wrapper for bounded source-playback preflight.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    pub fn preflight_resolved_source_playback_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        block_on(self.preflight_resolved_source_playback_local_blocking(
            adapter,
            sources,
            clock_domain,
            selector,
            limits,
        ))
    }

    /// Synchronous wrapper for mixed retained-frame/local-video preflight.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    pub fn preflight_resolved_source_playback_mixed_blocking(
        &self,
        adapter: Option<&Adapter>,
        sources: impl IntoIterator<Item = NativeResolvedSource>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        block_on(
            self.preflight_resolved_source_playback_mixed_local_blocking(
                adapter,
                sources,
                clock_domain,
                selector,
                limits,
            ),
        )
    }

    /// Drains completed CPU decode pages without waiting, normalizes complete
    /// pages on this runtime, evicts frames before each source's floor anchor,
    /// and schedules bounded low-watermark refill.
    ///
    /// The returned value is `true` only when every source is at EOS or has a
    /// latest rebased PTS at or beyond `deadline`. A non-EOS source is never
    /// considered safe merely because it has a last retained frame.
    ///
    /// # Errors
    ///
    /// Returns a typed, fatal source contract, decode, normalization, or worker
    /// failure. Playback remains failed after the first error.
    pub async fn service_source_playback(
        &self,
        playback: &mut NativeSourcePlayback,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        if playback.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        let result = self.service_source_playback_inner(playback, deadline).await;
        if result.is_err() {
            playback.failed = true;
        }
        result
    }

    /// Replaces the retained GPU frame for one live CPU source.
    ///
    /// Source timing is preserved exactly. Updates must retain the source clock
    /// and advance both PTS and sequence, though queue-induced sequence gaps are
    /// accepted. The operation keeps one frame and therefore does not grow the
    /// registry's retained-byte charge.
    ///
    /// # Errors
    ///
    /// Returns a typed source-kind, timeline, dimension, normalization, or
    /// previously-failed playback error. Playback remains failed after an error.
    pub async fn ingest_live_video_frame(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        frame: CpuVideoFrame,
    ) -> Result<(), NativeSourcePlaybackError> {
        if playback.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        let result = self
            .ingest_live_video_frame_inner(playback, input, frame)
            .await;
        if result.is_err() {
            playback.failed = true;
        }
        result
    }

    async fn ingest_live_video_frame_inner(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        frame: CpuVideoFrame,
    ) -> Result<(), NativeSourcePlaybackError> {
        let prefix = playback
            .registry
            .sources
            .get(&input)
            .ok_or(NativeSourcePlaybackError::SourceNotLive { input })?;
        if prefix.kind != NativeVideoSourceKind::Live {
            return Err(NativeSourcePlaybackError::SourceNotLive { input });
        }
        validate_live_update_timing(
            input,
            &frame,
            prefix.last_source_pts,
            prefix.last_sequence,
            prefix.clock_domain,
        )?;
        let (expected_width, expected_height) = playback
            .registry
            .dimensions
            .ok_or(NativeSourceError::InvalidTimeline { input })?;
        let dimensions = frame.payload().dimensions();
        if (dimensions.width(), dimensions.height()) != (expected_width, expected_height) {
            return Err(NativeSourceError::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width: dimensions.width(),
                actual_height: dimensions.height(),
            }
            .into());
        }
        let timing = frame.timing();
        let normalized = self
            .normalizer
            .normalize(&self.context, &frame)
            .await
            .map_err(|error| NativeSourcePlaybackError::Normalize { input, error })?;
        let prefix = playback
            .registry
            .sources
            .get_mut(&input)
            .ok_or(NativeSourcePlaybackError::SourceNotLive { input })?;
        prefix.frames.clear();
        prefix.frames.push(normalized);
        prefix.offsets_ns.clear();
        prefix.offsets_ns.push(0);
        prefix.last_source_pts = timing.presentation_timestamp().as_nanos();
        prefix.last_sequence = timing.sequence().get();
        Ok(())
    }

    /// Synchronous daemon wrapper for [`Self::ingest_live_video_frame`].
    ///
    /// # Errors
    ///
    /// Returns the same typed failures as the asynchronous operation.
    pub fn ingest_live_video_frame_blocking(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        frame: CpuVideoFrame,
    ) -> Result<(), NativeSourcePlaybackError> {
        block_on(self.ingest_live_video_frame(playback, input, frame))
    }

    #[allow(clippy::too_many_lines)]
    async fn service_source_playback_inner(
        &self,
        playback: &mut NativeSourcePlayback,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        let mut completed = Vec::with_capacity(playback.registry.sources.len());
        loop {
            match playback.worker.results.try_recv() {
                Ok(result) => completed.push(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(playback.worker.disconnected_error());
                }
            }
        }

        for completed in completed {
            let input = completed.input;
            let window = completed
                .window
                .map_err(|error| NativeSourcePlaybackError::Decode { input, error })?;
            let prefix = playback
                .registry
                .sources
                .get(&input)
                .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
            if prefix.kind != NativeVideoSourceKind::Decoded
                || prefix.in_flight != Some(completed.count)
            {
                return Err(NativeSourcePlaybackError::DecodeContract { input });
            }
            let requested = usize::try_from(completed.count.get()).unwrap_or(usize::MAX);
            if window.frames.len() > requested
                || (window.frames.is_empty() && !window.end_of_stream)
                || (!window.end_of_stream && window.frames.len() != requested)
            {
                return Err(NativeSourcePlaybackError::DecodeContract { input });
            }

            if window.frames.is_empty() {
                let prefix = playback
                    .registry
                    .sources
                    .get_mut(&input)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
                prefix.in_flight = None;
                prefix.end_of_stream = true;
                continue;
            }

            let expected_sequence = prefix
                .last_sequence
                .checked_add(1)
                .ok_or(NativeSourceError::InvalidTimeline { input })?;
            let (offsets_ns, last_source_pts, last_sequence) = validate_page_timing(
                input,
                &window.frames,
                prefix.source_pts_origin,
                Some(prefix.last_source_pts),
                expected_sequence,
                prefix.clock_domain,
            )?;
            let (expected_width, expected_height) = playback
                .registry
                .dimensions
                .ok_or(NativeSourceError::InvalidTimeline { input })?;
            for frame in &window.frames {
                let dimensions = frame.payload().dimensions();
                if (dimensions.width(), dimensions.height()) != (expected_width, expected_height) {
                    return Err(NativeSourceError::DimensionMismatch {
                        input,
                        expected_width,
                        expected_height,
                        actual_width: dimensions.width(),
                        actual_height: dimensions.height(),
                    }
                    .into());
                }
            }
            if prefix.frames.len().saturating_add(window.frames.len())
                > usize::try_from(playback.registry.limits.max_video_frames_per_source.get())
                    .unwrap_or(usize::MAX)
            {
                return Err(NativeSourceError::TooManyFrames {
                    input,
                    actual: prefix.frames.len().saturating_add(window.frames.len()),
                    maximum: playback.registry.limits.max_video_frames_per_source.get(),
                }
                .into());
            }
            let frame_bytes = rgba16f_frame_bytes(input, expected_width, expected_height)?;
            let batch_bytes = frame_bytes
                .checked_mul(u64::try_from(window.frames.len()).unwrap_or(u64::MAX))
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                })?;
            let required_bytes = playback
                .registry
                .retained_rgba16f_bytes
                .checked_add(batch_bytes)
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                })?;
            if required_bytes > playback.registry.limits.max_retained_rgba16f_bytes {
                return Err(NativeSourceError::RetainedBytesExceeded {
                    required: required_bytes,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                }
                .into());
            }

            let mut normalized = Vec::with_capacity(window.frames.len());
            for frame in &window.frames {
                normalized.push(
                    self.normalizer
                        .normalize(&self.context, frame)
                        .await
                        .map_err(|error| NativeSourcePlaybackError::Normalize { input, error })?,
                );
            }

            let prefix = playback
                .registry
                .sources
                .get_mut(&input)
                .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
            prefix.frames.append(&mut normalized);
            prefix.offsets_ns.extend(offsets_ns);
            prefix.last_source_pts = last_source_pts;
            prefix.last_sequence = last_sequence;
            prefix.end_of_stream = window.end_of_stream;
            prefix.in_flight = None;
            playback.registry.retained_rgba16f_bytes = required_bytes;
        }

        if let Some((width, height)) = playback.registry.dimensions {
            let Some(accounting_input) = playback.registry.sources.keys().next().copied() else {
                return Ok(true);
            };
            let frame_bytes = rgba16f_frame_bytes(accounting_input, width, height)?;
            for (input, prefix) in &mut playback.registry.sources {
                let remove = floor_anchor_eviction_count(&prefix.offsets_ns, deadline);
                prefix.frames.drain(..remove);
                prefix.offsets_ns.drain(..remove);
                let removed_bytes = frame_bytes
                    .checked_mul(u64::try_from(remove).unwrap_or(u64::MAX))
                    .ok_or(NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    })?;
                playback.registry.retained_rgba16f_bytes = playback
                    .registry
                    .retained_rgba16f_bytes
                    .checked_sub(removed_bytes)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input: *input })?;
            }

            let mut reserved_bytes = playback
                .registry
                .sources
                .values()
                .filter_map(|prefix| prefix.in_flight)
                .try_fold(0_u64, |reserved, count| {
                    frame_bytes
                        .checked_mul(u64::from(count.get()))
                        .and_then(|page| reserved.checked_add(page))
                })
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                })?;
            let inputs = playback
                .registry
                .sources
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for input in inputs {
                let prefix = playback
                    .registry
                    .sources
                    .get(&input)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
                if prefix.kind != NativeVideoSourceKind::Decoded {
                    continue;
                }
                let allocated = playback
                    .registry
                    .retained_rgba16f_bytes
                    .checked_add(reserved_bytes)
                    .ok_or(NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    })?;
                let budget_frames = playback
                    .registry
                    .limits
                    .max_retained_rgba16f_bytes
                    .saturating_sub(allocated)
                    / frame_bytes;
                let Some(count) = refill_page_size(
                    prefix.frames.len(),
                    prefix.in_flight.is_some(),
                    prefix.end_of_stream,
                    playback.registry.limits.max_video_frames_per_source.get(),
                    budget_frames,
                ) else {
                    continue;
                };
                let request = NativeDecodeRequest { input, count };
                let Some(sender) = playback.worker.requests.as_ref() else {
                    return Err(playback.worker.disconnected_error());
                };
                match sender.try_send(request) {
                    Ok(()) => {}
                    Err(TrySendError::Disconnected(_)) => {
                        return Err(playback.worker.disconnected_error());
                    }
                    Err(TrySendError::Full(_)) => {
                        return Err(NativeSourcePlaybackError::WorkerQueueFull);
                    }
                }
                playback
                    .registry
                    .sources
                    .get_mut(&input)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?
                    .in_flight = Some(count);
                let page_bytes = frame_bytes.checked_mul(u64::from(count.get())).ok_or(
                    NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    },
                )?;
                reserved_bytes = reserved_bytes.checked_add(page_bytes).ok_or(
                    NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    },
                )?;
            }
        }

        Ok(playback
            .registry
            .sources
            .values()
            .all(|prefix| prefix.covers_deadline(deadline)))
    }

    /// Synchronous wrapper for [`Self::service_source_playback`].
    ///
    /// # Errors
    ///
    /// Returns a typed, fatal source contract, decode, normalization, or worker
    /// failure.
    pub fn service_source_playback_blocking(
        &self,
        playback: &mut NativeSourcePlayback,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        block_on(self.service_source_playback(playback, deadline))
    }

    /// Renders the engine's authoritative frame from retained GPU source
    /// prefixes. Source frames are selected by rebased PTS at the exact output
    /// deadline, with the final retained frame held only after confirmed EOS.
    /// A frame without a secondary is rendered as
    /// `Cut(primary, primary)`; a frame with one is rendered as a `Fade` with
    /// its exact numerator and denominator. This method performs no decode,
    /// normalization, source upload, or CPU readback.
    ///
    /// # Errors
    ///
    /// Returns a typed missing-source, registry-dimension, invalid-mix, or
    /// native compositor failure.
    pub async fn render_frame_result(
        &self,
        registry: &NativeSourceRegistry,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        let plan = native_mix_plan(frame.program).map_err(NativeSourceRenderError::InvalidMix)?;
        let primary = registry_frame(registry, plan.primary, frame.deadline)?;
        let secondary = registry_frame(registry, plan.secondary, frame.deadline)?;
        self.renderer
            .render(
                &self.context,
                plan.transition,
                primary.texture(),
                secondary.texture(),
            )
            .await
            .map_err(NativeSourceRenderError::Compositor)
    }

    /// Synchronous daemon wrapper for one authoritative program render.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-registry or compositor failure.
    pub fn render_frame_result_blocking(
        &self,
        registry: &NativeSourceRegistry,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        block_on(self.render_frame_result(registry, frame))
    }

    /// Renders a GPU-resident Cut or Fade between canonical RGBA16-float
    /// working frames. This production operation performs no CPU readback.
    ///
    /// # Errors
    ///
    /// Returns a typed compositor or GPU validation failure.
    pub async fn render_cut_or_fade(
        &self,
        plan: TransitionPlan,
        from: &NativeWorkingFrame,
        to: &NativeWorkingFrame,
    ) -> Result<NativeTexture, NativeMediaError> {
        self.renderer
            .render(&self.context, plan, from.texture(), to.texture())
            .await
            .map_err(Into::into)
    }

    /// Creates a reusable fixed-size SDR Program readback owner on this
    /// runtime's context.
    ///
    /// Nonzero dimensions are further validated against the selected native
    /// adapter's texture limits by the existing GPU target API.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU target or SDR transform pipeline failure.
    pub async fn create_program_readback(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<NativeProgramReadback, NativeMediaError> {
        let target = self
            .context
            .create_rgba8_render_target(width.get(), height.get())
            .await?;
        let transform = NativeSdrOutputTransform::new(&self.context).await?;
        Ok(NativeProgramReadback { target, transform })
    }

    /// Synchronously creates a reusable fixed-size SDR Program readback owner.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU target or SDR transform pipeline failure.
    pub fn create_program_readback_blocking(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<NativeProgramReadback, NativeMediaError> {
        block_on(self.create_program_readback(width, height))
    }

    /// Transforms canonical `Rgba16Float` Program light to explicit
    /// sRGB-encoded Rec.709 in the owner's reusable `Rgba8Unorm` target, then
    /// returns tightly packed RGBA8 pixels.
    ///
    /// Existing transform and readback APIs validate source format, source and
    /// owner context, target role, and dimensions. This correctness path polls
    /// and maps synchronously and may block for up to the native readback
    /// timeout. Its exclusive owner borrow keeps transform plus readback
    /// single-flight for the reusable target. It is not a zero-copy production
    /// encoder bridge.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU ownership, format, submission, polling, mapping,
    /// timeout, or layout failure.
    pub async fn readback_program(
        &self,
        owner: &mut NativeProgramReadback,
        program: &NativeTexture,
    ) -> Result<DiagnosticReadback, NativeMediaError> {
        owner
            .transform
            .transform(&self.context, program, &owner.target)
            .await?;
        self.context
            .readback_rgba8(&owner.target)
            .await
            .map_err(Into::into)
    }

    /// Synchronous wrapper for [`Self::readback_program`].
    ///
    /// This blocking correctness path may wait for up to the native readback
    /// timeout and is not a zero-copy production encoder bridge.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU ownership, format, submission, polling, mapping,
    /// timeout, or layout failure.
    pub fn readback_program_blocking(
        &self,
        owner: &mut NativeProgramReadback,
        program: &NativeTexture,
    ) -> Result<DiagnosticReadback, NativeMediaError> {
        block_on(self.readback_program(owner, program))
    }

    /// Returns portable adapter identification for diagnostics and tests.
    #[must_use]
    pub const fn diagnostic_adapter_info(&self) -> &NativeAdapterInfo {
        self.context.adapter_info()
    }

    /// Reads a native texture back to CPU memory for diagnostics and tests.
    /// Production preroll and rendering never call this method.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU polling, mapping, or validation failure.
    pub async fn diagnostic_readback(
        &self,
        texture: &NativeTexture,
    ) -> Result<NativeTextureReadback, NativeMediaError> {
        self.context.readback(texture).await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU128;

    use fm_command::{Revision, RuntimeGeneration};
    #[cfg(target_os = "macos")]
    use fm_frame::{
        AlphaMode, ChromaLocation, ColorMetadata, ColorPrimaries, MatrixCoefficients, SignalRange,
        TransferFunction, VideoFrameMetadata,
    };
    use fm_frame::{
        Channel, ChannelLayout, CpuVideoPayload, MediaTimestamp, NormalizedDuration,
        NormalizedTimestamp, OriginalTimestamp, PixelFormat, SampleRate, SequenceNumber, TimeBase,
        VideoDimensions,
    };
    use fm_scheduler::FrameNumber;

    use super::*;

    fn input(value: u128) -> InputId {
        InputId::new(NonZeroU128::new(value).unwrap())
    }

    fn retained_frame(
        clock_domain: ClockDomainId,
        sequence: u64,
        presentation_timestamp: i64,
    ) -> CpuVideoFrame {
        let timing = MediaTiming::new(
            OriginalTimestamp::new(
                MediaTimestamp::new(presentation_timestamp),
                TimeBase::new(1, 1_000_000_000).unwrap(),
            ),
            NormalizedTimestamp::from_nanos(presentation_timestamp),
            NormalizedDuration::from_nanos(1).unwrap(),
            clock_domain,
            SequenceNumber::new(sequence),
        )
        .unwrap();
        let payload =
            CpuVideoPayload::allocate(PixelFormat::Rgba8, VideoDimensions::new(1, 1).unwrap())
                .unwrap();
        CpuVideoFrame::new(timing, payload)
    }

    fn mono_audio_format() -> AudioFormat {
        AudioFormat {
            sample_rate: SampleRate::new(48_000).unwrap(),
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        }
    }

    fn audio_block(
        sequence: u64,
        start_sample: u64,
        samples: &[f32],
        format: &AudioFormat,
        clock_domain: ClockDomainId,
    ) -> AudioBlock {
        let end_sample = start_sample + u64::try_from(samples.len()).unwrap();
        AudioBlock::new(
            output_audio_timing(
                sequence,
                start_sample,
                end_sample,
                format.sample_rate.hertz(),
                clock_domain,
            )
            .unwrap(),
            format.sample_rate,
            format.channels.clone(),
            vec![samples.to_vec()],
        )
        .unwrap()
    }

    fn audio_source(chunks: Vec<NativeAudioChunk>, end_of_stream: bool) -> NativeAudioSource {
        let next_sample = chunks.last().map_or(0, |chunk| chunk.end_sample);
        NativeAudioSource {
            explicit_silence: false,
            chunks: chunks.into(),
            source_origin_sample: Some(0),
            next_sample,
            next_sequence: 0,
            end_of_stream,
            in_flight: None,
        }
    }

    fn audio_chunk(start_sample: u64, samples: &[f32]) -> NativeAudioChunk {
        NativeAudioChunk {
            start_sample,
            end_sample: start_sample + u64::try_from(samples.len()).unwrap(),
            planes: vec![samples.to_vec()],
        }
    }

    fn frame_result(frame: u64, primary: InputId, secondary: Option<InputId>) -> FrameResult {
        frame_result_with_mix(frame, primary, secondary, u32::from(secondary.is_some()), 2)
    }

    fn frame_result_with_mix(
        frame: u64,
        primary: InputId,
        secondary: Option<InputId>,
        mix_numerator: u32,
        mix_denominator: u32,
    ) -> FrameResult {
        let mix_end_numerator = if secondary.is_some() {
            mix_numerator.saturating_add(1).min(mix_denominator)
        } else {
            0
        };
        frame_result_with_interval(
            frame,
            primary,
            secondary,
            mix_numerator,
            mix_denominator,
            mix_numerator,
            mix_end_numerator,
        )
    }

    fn frame_result_with_interval(
        frame: u64,
        primary: InputId,
        secondary: Option<InputId>,
        mix_numerator: u32,
        mix_denominator: u32,
        mix_start_numerator: u32,
        mix_end_numerator: u32,
    ) -> FrameResult {
        FrameResult {
            frame: FrameNumber::new(frame),
            deadline: ClockTime::ZERO,
            program: ProgramFrame {
                primary,
                secondary,
                mix_numerator,
                mix_denominator,
                mix_start_numerator,
                mix_end_numerator,
            },
            events: Vec::new(),
            revision: Revision::new(0),
            runtime_generation: RuntimeGeneration::new(0),
        }
    }

    fn audio_test_master(sources: &[(InputId, f32)], sink_blocks: usize) -> NativeMasterRuntime {
        let format = mono_audio_format();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        let mut audio_sources = BTreeMap::new();
        let samples_per_source = 3 * 1_920;
        for &(input, sample) in sources {
            mixer
                .add_input(
                    input,
                    format.clone(),
                    ChannelMap::identity(1).unwrap(),
                    InputState {
                        follow_video: true,
                        ..InputState::default()
                    },
                )
                .unwrap();
            audio_sources.insert(
                input,
                audio_source(
                    vec![audio_chunk(0, &vec![sample; samples_per_source])],
                    true,
                ),
            );
        }
        let retained_samples = sources.len() * samples_per_source;
        NativeMasterRuntime {
            format,
            frame_rate: FrameRate::new(25, 1).unwrap(),
            clock_domain: ClockDomainId::new(NonZeroU128::new(9).unwrap()),
            expected_next_frame: 0,
            ready_frame: None,
            mixer,
            sink: CollectingAudioSink::new(sink_blocks, OverflowPolicy::DropOldest).unwrap(),
            sources: audio_sources,
            worker: NativeAudioDecodeWorker::spawn(BTreeMap::new()).unwrap(),
            retained: NativeAudioCharge {
                blocks: sources.len(),
                samples: retained_samples,
                bytes: retained_samples * size_of::<f32>(),
            },
            limits: NativeAudioLimits {
                sink_blocks,
                ..NativeAudioLimits::default()
            },
            failed: false,
        }
    }

    fn silent_test_master(input: InputId, sink_blocks: usize) -> NativeMasterRuntime {
        let format = mono_audio_format();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                input,
                format.clone(),
                ChannelMap::identity(1).unwrap(),
                InputState {
                    follow_video: true,
                    ..InputState::default()
                },
            )
            .unwrap();
        NativeMasterRuntime {
            format,
            frame_rate: FrameRate::new(25, 1).unwrap(),
            clock_domain: ClockDomainId::new(NonZeroU128::new(9).unwrap()),
            expected_next_frame: 0,
            ready_frame: None,
            mixer,
            sink: CollectingAudioSink::new(sink_blocks, OverflowPolicy::DropOldest).unwrap(),
            sources: BTreeMap::from([(input, NativeAudioSource::silence())]),
            worker: NativeAudioDecodeWorker::spawn(BTreeMap::new()).unwrap(),
            retained: NativeAudioCharge::default(),
            limits: NativeAudioLimits {
                sink_blocks,
                ..NativeAudioLimits::default()
            },
            failed: false,
        }
    }

    #[test]
    fn aggregate_errors_preserve_typed_sources_without_paths() {
        let error = NativeMediaError::from(fm_codec_ffmpeg::Error::InputNotFound);
        assert!(error.source().is_some());
        assert_eq!(
            error.to_string(),
            "local media decode failed: input file was not found"
        );
    }

    #[test]
    fn program_readback_blocking_api_has_owned_private_target_contract() {
        let _: fn(
            &NativeMediaRuntime,
            NonZeroU32,
            NonZeroU32,
        ) -> Result<NativeProgramReadback, NativeMediaError> =
            NativeMediaRuntime::create_program_readback_blocking;
        let _: fn(
            &NativeMediaRuntime,
            &mut NativeProgramReadback,
            &NativeTexture,
        ) -> Result<DiagnosticReadback, NativeMediaError> =
            NativeMediaRuntime::readback_program_blocking;
    }

    #[test]
    fn resolved_source_accessors_preserve_full_width_ids() {
        let local_input = input(1);
        let retained_input = input((1_u128 << 96) + 1);
        let live_input = input((1_u128 << 112) + 1);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let local = NativeResolvedSource::LocalVideo {
            input: local_input,
            path: PathBuf::from("video.mov"),
        };
        let retained = NativeResolvedSource::RetainedFrame {
            input: retained_input,
            frame: retained_frame(clock_domain, 0, -42),
        };
        let live = NativeResolvedSource::LiveFrame {
            input: live_input,
            frame: retained_frame(clock_domain, 41, -21),
        };

        assert_eq!(local.input(), local_input);
        assert_eq!(retained.input(), retained_input);
        assert_eq!(live.input(), live_input);
        assert!(matches!(local, NativeResolvedSource::LocalVideo { .. }));
        assert!(matches!(
            retained,
            NativeResolvedSource::RetainedFrame { .. }
        ));
        assert!(matches!(live, NativeResolvedSource::LiveFrame { .. }));
    }

    #[test]
    fn mixed_source_validation_precedes_adapter_use() {
        let duplicate = input((1_u128 << 80) + 3);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let sources = vec![
            NativeResolvedSource::LocalVideo {
                input: duplicate,
                path: PathBuf::from("video.mov"),
            },
            NativeResolvedSource::RetainedFrame {
                input: duplicate,
                frame: retained_frame(clock_domain, 0, 0),
            },
        ];
        assert!(matches!(
            validate_resolved_sources(&sources, None, 2),
            Err(NativeSourcePreflightError::Source(
                NativeSourceError::DuplicateSource(input)
            )) if input == duplicate
        ));

        let local = input(4);
        let sources = [NativeResolvedSource::LocalVideo {
            input: local,
            path: PathBuf::from("video.mov"),
        }];
        assert!(matches!(
            validate_resolved_sources(&sources, None, 1),
            Err(NativeSourcePreflightError::CodecAdapterRequired { input }) if input == local
        ));

        let retained = [NativeResolvedSource::RetainedFrame {
            input: local,
            frame: retained_frame(clock_domain, 0, 0),
        }];
        assert!(validate_resolved_sources(&retained, None, 1).is_ok());
        let live = [NativeResolvedSource::LiveFrame {
            input: local,
            frame: retained_frame(clock_domain, 9, 10),
        }];
        assert!(validate_resolved_sources(&live, None, 1).is_ok());
    }

    #[test]
    fn retained_frame_timing_rebases_pts_and_requires_domain_and_sequence_zero() {
        let source = input(1);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let other_domain = ClockDomainId::new(NonZeroU128::new(8).unwrap());
        let frame = retained_frame(clock_domain, 0, -42);
        assert_eq!(
            validate_retained_frame_timing(source, &frame, clock_domain),
            Ok((vec![0], -42, 0))
        );
        assert_eq!(
            validate_retained_frame_timing(
                source,
                &retained_frame(clock_domain, 1, 42),
                clock_domain
            ),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
        assert_eq!(
            validate_retained_frame_timing(
                source,
                &retained_frame(other_domain, 0, 42),
                clock_domain
            ),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
    }

    #[test]
    fn live_timing_preserves_source_clock_and_accepts_sequence_gaps() {
        let source = input(1);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let other_domain = ClockDomainId::new(NonZeroU128::new(8).unwrap());
        let seed = retained_frame(clock_domain, 40, -42);
        assert_eq!(
            validate_live_seed_timing(source, &seed),
            Ok((vec![0], -42, 40, clock_domain))
        );
        assert_eq!(
            validate_live_update_timing(
                source,
                &retained_frame(clock_domain, 44, 10),
                -42,
                40,
                clock_domain,
            ),
            Ok(())
        );
        for frame in [
            retained_frame(clock_domain, 40, 10),
            retained_frame(clock_domain, 44, -42),
            retained_frame(other_domain, 44, 10),
        ] {
            assert_eq!(
                validate_live_update_timing(source, &frame, -42, 40, clock_domain),
                Err(NativeSourceError::InvalidTimeline { input: source })
            );
        }
    }

    #[test]
    fn source_validation_preserves_full_width_ids_and_charges_exact_bytes() {
        let low = input(1);
        let high = input((1_u128 << 64) + 1);
        let sources = [(low, 2, 3, 1), (high, 2, 3, 2)];
        let limits = NativeSourceLimits {
            max_media_inputs: 2,
            max_video_frames_per_source: NonZeroU32::new(2).unwrap(),
            max_retained_rgba16f_bytes: 144,
        };

        assert_eq!(
            validate_source_layouts(&sources, limits),
            Ok((Some((2, 3)), 144))
        );
        let ids = sources
            .into_iter()
            .map(|(id, _, _, _)| (id, true))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(ids.len(), 2);
        assert!(registered_source(&ids, low).is_ok());
        assert!(registered_source(&ids, high).is_ok());
    }

    #[test]
    fn source_validation_rejects_count_duplicates_budget_and_dimensions() {
        let first = input(1);
        let second = input(2);
        let limits = NativeSourceLimits {
            max_media_inputs: 2,
            max_video_frames_per_source: NonZeroU32::new(1).unwrap(),
            max_retained_rgba16f_bytes: 64,
        };

        assert_eq!(
            validate_source_layouts(
                &[(first, 2, 2, 1), (second, 2, 2, 1), (input(3), 2, 2, 1)],
                limits
            ),
            Err(NativeSourceError::TooManySources {
                actual: 3,
                maximum: 2
            })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 1), (first, 2, 2, 1)], limits),
            Err(NativeSourceError::DuplicateSource(first))
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 0)], limits),
            Err(NativeSourceError::InvalidTimeline { input: first })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 2)], limits),
            Err(NativeSourceError::TooManyFrames {
                input: first,
                actual: 2,
                maximum: 1
            })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 3, 2, 1), (second, 3, 2, 1)], limits),
            Err(NativeSourceError::RetainedBytesExceeded {
                required: 96,
                maximum: 64
            })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 1), (second, 3, 2, 1)], limits),
            Err(NativeSourceError::DimensionMismatch {
                input: second,
                expected_width: 2,
                expected_height: 2,
                actual_width: 3,
                actual_height: 2
            })
        );
    }

    #[test]
    fn source_validation_reports_checked_frame_charge_overflow() {
        let source = input(1);
        assert_eq!(
            validate_source_layouts(
                &[(source, u32::MAX, u32::MAX, 1)],
                NativeSourceLimits {
                    max_media_inputs: 1,
                    max_video_frames_per_source: NonZeroU32::MIN,
                    max_retained_rgba16f_bytes: u64::MAX,
                }
            ),
            Err(NativeSourceError::FrameByteSizeOverflow {
                input: source,
                width: u32::MAX,
                height: u32::MAX
            })
        );
    }

    #[test]
    fn missing_source_lookup_is_typed_and_uses_full_width_id() {
        let missing = input((1_u128 << 64) + 1);
        let sources = BTreeMap::<InputId, bool>::new();
        assert!(matches!(
            registered_source(&sources, missing),
            Err(NativeSourceRenderError::MissingSource { input }) if input == missing
        ));
    }

    #[test]
    fn program_frame_maps_exactly_to_cut_or_fade() {
        let primary = input(1);
        let secondary = input((1_u128 << 64) + 1);
        let cut = native_mix_plan(ProgramFrame {
            primary,
            secondary: None,
            mix_numerator: u32::MAX,
            mix_denominator: 0,
            mix_start_numerator: u32::MAX,
            mix_end_numerator: u32::MAX,
        })
        .unwrap();
        assert_eq!(cut.primary, primary);
        assert_eq!(cut.secondary, primary);
        assert_eq!(cut.transition.kind(), TransitionKind::Cut);
        assert_eq!(cut.transition.numerator(), 0);
        assert_eq!(cut.transition.denominator(), 1);

        let identical = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(primary),
            mix_numerator: u32::MAX,
            mix_denominator: 0,
            mix_start_numerator: u32::MAX,
            mix_end_numerator: u32::MAX,
        })
        .unwrap();
        assert_eq!(identical.primary, primary);
        assert_eq!(identical.secondary, primary);
        assert_eq!(identical.transition.kind(), TransitionKind::Cut);

        let fade = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(secondary),
            mix_numerator: 7,
            mix_denominator: 11,
            mix_start_numerator: 7,
            mix_end_numerator: 8,
        })
        .unwrap();
        assert_eq!(fade.primary, primary);
        assert_eq!(fade.secondary, secondary);
        assert_eq!(fade.transition.kind(), TransitionKind::Fade);
        assert_eq!(fade.transition.numerator(), 7);
        assert_eq!(fade.transition.denominator(), 11);
        assert_eq!(
            native_mix_plan(ProgramFrame {
                primary,
                secondary: Some(secondary),
                mix_numerator: 1,
                mix_denominator: 0,
                mix_start_numerator: 1,
                mix_end_numerator: 1,
            }),
            Err(TransitionError::ZeroDenominator)
        );
    }

    #[test]
    fn prefix_selection_rebases_vfr_pts_and_holds_boundaries_and_end() {
        let source = input(1);
        let offsets = rebased_offsets(source, &[-20_000_000, 20_000_000, 100_000_000]).unwrap();
        assert_eq!(offsets, [0, 40_000_000, 120_000_000]);
        for (deadline, expected) in [
            (0, 0),
            (39_999_999, 0),
            (40_000_000, 1),
            (119_999_999, 1),
            (120_000_000, 2),
            (u64::MAX, 2),
        ] {
            assert_eq!(frame_index_at_deadline(&offsets, deadline), Some(expected));
        }
        assert_eq!(frame_index_at_deadline(&[], 0), None);
        assert_eq!(
            rebased_offsets(source, &[0, 0]),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
        assert_eq!(
            rebased_offsets(source, &[i64::MIN, i64::MAX]),
            Ok(vec![0, u64::MAX])
        );
    }

    #[test]
    fn bounded_preflight_request_is_video_only() {
        let request = prefix_decode_request(
            ClockDomainId::new(NonZeroU128::new(1).unwrap()),
            StreamSelector::Best,
            NonZeroU32::new(8).unwrap(),
        );
        assert_eq!(request.video.unwrap().count.get(), 8);
        assert!(request.audio.is_none());
    }

    #[test]
    fn eviction_retains_floor_anchor_and_every_future_frame() {
        let offsets = [0, 40, 125, 210];
        assert_eq!(floor_anchor_eviction_count(&offsets, ClockTime::ZERO), 0);
        assert_eq!(
            floor_anchor_eviction_count(&offsets, ClockTime::from_nanos(124)),
            1
        );
        assert_eq!(
            floor_anchor_eviction_count(&offsets, ClockTime::from_nanos(125)),
            2
        );
        assert_eq!(
            floor_anchor_eviction_count(&offsets, ClockTime::from_nanos(u64::MAX)),
            3
        );
    }

    #[test]
    fn coverage_requires_latest_pts_until_eos() {
        assert!(!source_covers_deadline(None, false, ClockTime::ZERO));
        assert!(source_covers_deadline(
            Some(100),
            false,
            ClockTime::from_nanos(100)
        ));
        assert!(!source_covers_deadline(
            Some(100),
            false,
            ClockTime::from_nanos(101)
        ));
        assert!(source_covers_deadline(
            Some(100),
            true,
            ClockTime::from_nanos(u64::MAX)
        ));
    }

    #[test]
    fn refill_state_obeys_watermark_ring_budget_and_single_flight() {
        assert_eq!(
            refill_page_size(4, false, false, 8, u64::MAX),
            NonZeroU32::new(4)
        );
        assert_eq!(
            refill_page_size(3, false, false, 5, u64::MAX),
            NonZeroU32::new(2)
        );
        assert_eq!(refill_page_size(1, false, false, 8, 1), NonZeroU32::new(1));
        assert_eq!(refill_page_size(5, false, false, 8, 8), None);
        assert_eq!(refill_page_size(1, true, false, 8, 8), None);
        assert_eq!(refill_page_size(1, false, true, 8, 8), None);
        assert_eq!(refill_page_size(1, false, false, 8, 0), None);
    }

    #[test]
    fn retained_eos_uses_an_idle_worker_and_never_schedules_refill() {
        assert_eq!(refill_page_size(1, false, true, 8, u64::MAX), None);
        drop(NativeDecodeWorker::spawn(BTreeMap::new()).unwrap());
    }

    #[test]
    fn vfr_page_seams_preserve_origin_pts_and_global_sequence() {
        let source = input(1);
        let first =
            validate_timing_values(source, &[-20, 20, 100], &[0, 1, 2], -20, None, 0).unwrap();
        assert_eq!(first, (vec![0, 40, 120], 100, 2));
        let second =
            validate_timing_values(source, &[175, 260], &[3, 4], -20, Some(100), 3).unwrap();
        assert_eq!(second, (vec![195, 280], 260, 4));

        assert_eq!(
            validate_timing_values(source, &[100], &[3], -20, Some(100), 3),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
        assert_eq!(
            validate_timing_values(source, &[175], &[4], -20, Some(100), 3),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
    }

    #[test]
    fn absolute_audio_spans_are_exact_at_integer_and_fractional_rates() {
        let rate = FrameRate::new(25, 1).unwrap();
        assert_eq!(
            absolute_frame_sample_span(0, 48_000, rate).unwrap(),
            (0, 1_920)
        );
        assert_eq!(
            absolute_frame_sample_span(123, 48_000, rate).unwrap(),
            (236_160, 238_080)
        );

        let ntsc = FrameRate::new(60_000, 1_001).unwrap();
        assert_eq!(
            absolute_frame_sample_span(0, 48_000, ntsc).unwrap(),
            (0, 800)
        );
        assert_eq!(
            absolute_frame_sample_span(1, 48_000, ntsc).unwrap(),
            (800, 1_601)
        );
        assert_eq!(
            absolute_frame_sample_span(59_999, 48_000, ntsc).unwrap(),
            (48_047_199, 48_048_000)
        );
    }

    #[test]
    fn output_audio_timing_uses_absolute_samples_and_contiguous_normalized_endpoints() {
        let clock_domain = ClockDomainId::new(NonZeroU128::new(3).unwrap());
        let rate = FrameRate::new(60_000, 1_001).unwrap();
        let first_span = absolute_frame_sample_span(41, 48_000, rate).unwrap();
        let second_span = absolute_frame_sample_span(42, 48_000, rate).unwrap();
        let first =
            output_audio_timing(41, first_span.0, first_span.1, 48_000, clock_domain).unwrap();
        let second =
            output_audio_timing(42, second_span.0, second_span.1, 48_000, clock_domain).unwrap();

        assert_eq!(first.original_timestamp().timestamp().ticks(), 32_832);
        assert_eq!(
            first.original_timestamp().time_base(),
            TimeBase::new(1, 48_000).unwrap()
        );
        assert_eq!(first.sequence().get(), 41);
        assert_eq!(second.sequence().get(), 42);
        assert_eq!(
            first.presentation_timestamp().as_nanos()
                + i64::try_from(first.duration().as_nanos()).unwrap(),
            second.presentation_timestamp().as_nanos()
        );
    }

    #[test]
    fn audio_pages_validate_global_sequence_and_sample_continuity_transactionally() {
        let source_id = input(1);
        let format = mono_audio_format();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(3).unwrap());
        let mut source = NativeAudioSource::decoded();
        let first = [
            audio_block(0, 100, &[1.0, 2.0], &format, clock_domain),
            audio_block(1, 102, &[3.0], &format, clock_domain),
        ];
        let page = validate_audio_page(source_id, &source, &first, &format, clock_domain).unwrap();
        assert_eq!(page.next_sample, 3);
        assert_eq!(page.next_sequence, 2);
        commit_audio_page(&mut source, page);

        let invalid = [audio_block(3, 103, &[4.0], &format, clock_domain)];
        assert!(matches!(
            validate_audio_page(source_id, &source, &invalid, &format, clock_domain),
            Err(NativeMasterError::InvalidTimeline { input }) if input == source_id
        ));
        assert_eq!(source.next_sample, 3);
        assert_eq!(source.next_sequence, 2);
        assert_eq!(source.chunks.len(), 2);
    }

    #[test]
    fn source_coalescing_crosses_blocks_and_handles_partial_fronts() {
        let format = mono_audio_format();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(3).unwrap());
        let source = audio_source(
            vec![
                audio_chunk(0, &[1.0, 2.0, 3.0]),
                audio_chunk(3, &[4.0, 5.0, 6.0, 7.0]),
            ],
            false,
        );
        let timing = output_audio_timing(0, 2, 6, 48_000, clock_domain).unwrap();
        let block = coalesce_source(&source, timing, 2, 6, &format).unwrap();
        assert_eq!(block.plane(0).unwrap(), &[3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn eos_tail_is_silence_but_missing_pre_eos_coverage_stalls() {
        let format = mono_audio_format();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(3).unwrap());
        let eos = audio_source(vec![audio_chunk(0, &[1.0, 2.0, 3.0])], true);
        let timing = output_audio_timing(0, 1, 5, 48_000, clock_domain).unwrap();
        let block = coalesce_source(&eos, timing, 1, 5, &format).unwrap();
        assert_eq!(block.plane(0).unwrap(), &[2.0, 3.0, 0.0, 0.0]);

        let live = audio_source(vec![audio_chunk(0, &[1.0, 2.0, 3.0])], false);
        assert!(!live.covers(1, 5));
        assert!(matches!(
            coalesce_source(&live, timing, 1, 5, &format),
            Err(NativeMasterError::FrameNotReady(0))
        ));
    }

    #[test]
    fn audio_ring_bounds_and_partial_eviction_charge_exactly() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        let chunk = audio_chunk(0, &[1.0, 2.0, 3.0, 4.0]);
        master
            .sources
            .insert(source_id, audio_source(vec![chunk], false));
        master.retained = NativeAudioCharge {
            blocks: 1,
            samples: 4,
            bytes: 16,
        };
        master.evict_before(2).unwrap();
        let front = master.sources[&source_id].chunks.front().unwrap();
        assert_eq!(front.start_sample, 2);
        assert_eq!(front.planes[0], [3.0, 4.0]);
        let retained_bytes = chunk_charge(front).unwrap().bytes;
        assert_eq!(
            master.retained,
            NativeAudioCharge {
                blocks: 1,
                samples: 2,
                bytes: retained_bytes
            }
        );

        let limits = NativeAudioLimits {
            max_retained_blocks: 1,
            max_retained_samples: 2,
            max_retained_bytes: 8,
            ..NativeAudioLimits::default()
        };
        assert!(validate_retained_bounds(master.retained, limits).is_ok());
        assert!(matches!(
            validate_retained_bounds(
                NativeAudioCharge {
                    blocks: 2,
                    samples: 2,
                    bytes: 8
                },
                limits
            ),
            Err(NativeMasterError::BoundsExceeded)
        ));
    }

    #[test]
    fn uncovered_tiny_blocks_fail_at_the_per_source_ring_bound() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        let chunks = (0..8)
            .map(|sample| audio_chunk(sample, &[1.0]))
            .collect::<Vec<_>>();
        master
            .sources
            .insert(source_id, audio_source(chunks, false));
        master.retained = NativeAudioCharge {
            blocks: 8,
            samples: 8,
            bytes: 8 * size_of::<f32>(),
        };
        master.limits.max_blocks_per_source = NonZeroU32::new(8).unwrap();
        master.limits.max_blocks_per_page = NonZeroU32::new(4).unwrap();

        assert!(matches!(
            master.service_next_frame(),
            Err(NativeMasterError::BoundsExceeded)
        ));
    }

    #[test]
    fn audio_mix_plan_maps_cut_and_exact_fade_interval_endpoints() {
        let old = input(1);
        let new = input(2);
        assert_eq!(
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(new),
                mix_numerator: 2,
                mix_denominator: 4,
                mix_start_numerator: 2,
                mix_end_numerator: 3,
            })
            .unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2, 1, 4).unwrap(),
                secondary: Some((new, SourceGain::new(2, 3, 4).unwrap())),
            }
        );
        assert_eq!(
            native_audio_mix_plan(ProgramFrame {
                primary: new,
                secondary: None,
                mix_numerator: u32::MAX,
                mix_denominator: 0,
                mix_start_numerator: u32::MAX,
                mix_end_numerator: u32::MAX,
            })
            .unwrap(),
            NativeAudioMixPlan {
                primary: new,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
        assert!(
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(new),
                mix_numerator: 1,
                mix_denominator: 0,
                mix_start_numerator: 1,
                mix_end_numerator: 1,
            })
            .is_err()
        );
        assert_eq!(
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(old),
                mix_numerator: u32::MAX,
                mix_denominator: 0,
                mix_start_numerator: u32::MAX,
                mix_end_numerator: u32::MAX,
            })
            .unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
    }

    #[test]
    fn identical_fade_sources_render_once_at_unity_without_poisoning_runtime() {
        let source = input(1);
        let mut master = audio_test_master(&[(source, 0.25)], 2);

        assert!(master.service_next_frame().unwrap());
        let output = master
            .render_frame_audio(&frame_result_with_interval(
                0,
                source,
                Some(source),
                u32::MAX,
                0,
                u32::MAX,
                u32::MAX,
            ))
            .unwrap();
        assert!(
            output
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.25)
        );
        assert_eq!(master.expected_next_frame(), 1);

        assert!(master.service_next_frame().unwrap());
        master
            .render_frame_audio(&frame_result_with_mix(1, source, None, 0, 1))
            .unwrap();
        assert_eq!(master.expected_next_frame(), 2);
    }

    #[test]
    fn t_bar_master_audio_holds_reverses_and_accepts_irregular_ratios() {
        let old = input(1);
        let new = input(2);
        let mut master = audio_test_master(&[(old, 1.0), (new, -1.0)], 3);

        assert!(master.service_next_frame().unwrap());
        let held = master
            .render_frame_audio(&frame_result_with_interval(
                0,
                old,
                Some(new),
                7_500,
                10_000,
                7_500,
                7_500,
            ))
            .unwrap();
        assert!(held.plane(0).unwrap().iter().all(|sample| *sample == -0.5));

        assert!(master.service_next_frame().unwrap());
        let reversed = master
            .render_frame_audio(&frame_result_with_interval(
                1,
                old,
                Some(new),
                2_500,
                10_000,
                7_500,
                2_500,
            ))
            .unwrap();
        let reversed = reversed.plane(0).unwrap();
        assert!((reversed[0] - (-0.5 + 1.0 / 1_920.0)).abs() < 1.0e-6);
        assert_eq!(reversed[1_919], 0.5);

        assert!(master.service_next_frame().unwrap());
        let irregular = master
            .render_frame_audio(&frame_result_with_interval(
                2,
                old,
                Some(new),
                7_333,
                10_000,
                2_500,
                7_333,
            ))
            .unwrap();
        assert!((irregular.plane(0).unwrap()[1_919] - -0.4666).abs() < 1.0e-6);
    }

    #[test]
    fn fade_master_audio_is_linear_continuous_and_reaches_cut_endpoint() {
        let old = input(1);
        let new = input(2);
        let mut master = audio_test_master(&[(old, 1.0), (new, -1.0)], 3);

        assert!(master.service_next_frame().unwrap());
        let first = master
            .render_frame_audio(&frame_result_with_mix(0, old, Some(new), 0, 2))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        let second = master
            .render_frame_audio(&frame_result_with_mix(1, old, Some(new), 1, 2))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        let cut = master
            .render_frame_audio(&frame_result_with_mix(2, new, None, 0, 1))
            .unwrap();

        let first = first.plane(0).unwrap();
        let second = second.plane(0).unwrap();
        let cut = cut.plane(0).unwrap();
        let step = 1.0 / 1_920.0;
        assert!((first[0] - (1.0 - step)).abs() < 1.0e-6);
        assert_eq!(first[1_919], 0.0);
        assert!((second[0] + step).abs() < 1.0e-6);
        assert_eq!(second[1_919], -1.0);
        assert_eq!(cut[0], -1.0);
        assert!((second[0] - first[1_919] + step).abs() < 1.0e-6);
    }

    #[test]
    fn fade_master_audio_uses_exact_fractional_cadence_intervals() {
        let old = input(1);
        let new = input(2);
        let mut master = audio_test_master(&[(old, 1.0), (new, -1.0)], 3);
        master.frame_rate = FrameRate::new(30_000, 1_001).unwrap();

        assert!(master.service_next_frame().unwrap());
        let first = master
            .render_frame_audio(&frame_result_with_mix(0, old, Some(new), 0, 2))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        let second = master
            .render_frame_audio(&frame_result_with_mix(1, old, Some(new), 1, 2))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        let cut = master
            .render_frame_audio(&frame_result_with_mix(2, new, None, 0, 1))
            .unwrap();

        assert_eq!(first.sample_count(), 1_601);
        assert_eq!(second.sample_count(), 1_602);
        assert_eq!(cut.sample_count(), 1_601);
        assert!((first.plane(0).unwrap()[0] - (1.0 - 1.0 / 1_601.0)).abs() < 1.0e-6);
        assert_eq!(first.plane(0).unwrap()[1_600], 0.0);
        assert!((second.plane(0).unwrap()[0] + 1.0 / 1_602.0).abs() < 1.0e-6);
        assert_eq!(second.plane(0).unwrap()[1_601], -1.0);
        assert_eq!(cut.plane(0).unwrap()[0], -1.0);
    }

    #[test]
    fn fake_audio_sink_is_bounded_and_reports_drop_oldest_telemetry() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(0, source_id, None))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(1, source_id, None))
            .unwrap();

        assert_eq!(master.sink_len(), 1);
        let telemetry = master.sink_telemetry();
        assert_eq!(telemetry.received(), 2);
        assert_eq!(telemetry.accepted(), 2);
        assert_eq!(telemetry.dropped_oldest(), 1);
        assert_eq!(telemetry.high_watermark(), 1);
        let only = master.collected_audio().next().unwrap();
        assert_eq!(only.timing().sequence().get(), 1);
    }

    #[test]
    fn returned_master_audio_exactly_matches_sink_sequence_and_sample_span() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        master.expected_next_frame = 123;

        assert!(master.service_next_frame().unwrap());
        let returned = master
            .render_frame_audio(&frame_result(123, source_id, None))
            .unwrap();

        assert_eq!(master.collected_audio().next(), Some(&returned));
        assert_eq!(returned.timing().sequence().get(), 123);
        assert_eq!(returned.sample_count(), 1_920);
        assert_eq!(master.expected_next_frame(), 124);
    }

    #[test]
    fn returned_master_audio_sink_failure_is_sticky_and_transactional() {
        let source_id = input(1);
        let secondary = input(2);
        let mut master = audio_test_master(&[(source_id, 1.0), (secondary, -1.0)], 1);
        master.sink = CollectingAudioSink::new(1, OverflowPolicy::Reject).unwrap();
        master
            .mixer
            .set_input_state(
                source_id,
                InputState {
                    gain: fm_audio::Gain::SILENCE,
                    follow_video: true,
                    ..InputState::default()
                },
                3_840,
            )
            .unwrap();

        assert!(master.service_next_frame().unwrap());
        let first = master
            .render_frame_audio(&frame_result_with_mix(0, source_id, Some(secondary), 0, 2))
            .unwrap();
        let gain_after_first = master.mixer.current_linear_gain(source_id);
        assert!((gain_after_first.unwrap() - 0.5).abs() < 1.0e-5);
        assert!(master.service_next_frame().unwrap());
        let ready = master.ready_frame;

        assert!(matches!(
            master.render_frame_audio(&frame_result_with_mix(1, source_id, Some(secondary), 1, 2,)),
            Err(NativeMasterError::SinkRejected)
        ));
        assert_eq!(master.expected_next_frame(), 1);
        assert_eq!(master.ready_frame, ready);
        assert_eq!(master.collected_audio().next(), Some(&first));
        assert_eq!(
            master.mixer.current_linear_gain(source_id),
            gain_after_first
        );
        assert!(matches!(
            master.render_frame_audio(&frame_result_with_mix(1, source_id, Some(secondary), 1, 2,)),
            Err(NativeMasterError::Failed)
        ));
        assert_eq!(master.expected_next_frame(), 1);
        assert_eq!(master.ready_frame, ready);
        assert_eq!(master.collected_audio().next(), Some(&first));
        assert_eq!(
            master.mixer.current_linear_gain(source_id),
            gain_after_first
        );
    }

    #[test]
    fn restored_master_cursor_services_absolute_frame_without_allocator_replay() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        master.expected_next_frame = 123;

        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(123, source_id, None))
            .unwrap();

        assert_eq!(master.expected_next_frame(), 124);
        let output = master.collected_audio().next().unwrap();
        assert_eq!(output.timing().sequence().get(), 123);
        assert_eq!(
            output.timing().original_timestamp().timestamp().ticks(),
            236_160
        );
        assert_eq!(output.sample_count(), 1_920);
    }

    #[test]
    fn decoder_and_worker_messages_are_send() {
        fn assert_send<T: Send>() {}

        assert_send::<LocalVideoDecoder>();
        assert_send::<LocalAudioDecoder>();
        assert_send::<NativeDecodeRequest>();
        assert_send::<NativeDecodeResult>();
        assert_send::<NativeAudioDecodeRequest>();
        assert_send::<NativeAudioDecodeResult>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a native macOS Metal adapter"]
    fn native_metal_program_readback_is_tightly_packed_and_reusable() {
        let runtime = NativeMediaRuntime::new_blocking([NativeBackend::Metal]).unwrap();
        let mut owner = runtime
            .create_program_readback_blocking(
                NonZeroU32::new(3).unwrap(),
                NonZeroU32::new(2).unwrap(),
            )
            .unwrap();
        assert_eq!((owner.width(), owner.height()), (3, 2));

        let source = retained_frame(ClockDomainId::new(NonZeroU128::new(7).unwrap()), 0, 0)
            .with_metadata(VideoFrameMetadata::new(
                ColorMetadata {
                    primaries: ColorPrimaries::Bt709,
                    transfer: TransferFunction::Srgb,
                    matrix: MatrixCoefficients::Identity,
                    range: SignalRange::Full,
                    chroma_location: ChromaLocation::Center,
                },
                Some(AlphaMode::Straight),
            ))
            .unwrap();
        let working = block_on(runtime.normalizer.normalize(runtime.context(), &source)).unwrap();
        let first = runtime
            .readback_program_blocking(&mut owner, working.texture())
            .unwrap();
        let second = runtime
            .readback_program_blocking(&mut owner, working.texture())
            .unwrap();

        assert_eq!((first.width, first.height, first.stride), (3, 2, 12));
        assert_eq!(first.rgba.len(), 24);
        assert!(
            first
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [0, 0, 0, 255])
        );
        assert_eq!(second, first);
    }
}
