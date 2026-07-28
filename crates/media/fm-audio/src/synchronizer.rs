use core::{
    fmt,
    num::{NonZeroU128, NonZeroUsize},
};
use std::collections::VecDeque;

use fm_clock::{ClockDomainId as MappingClockDomainId, ClockMapping};
use fm_frame::{
    AudioBlock, MediaFlags, MediaTiming, NormalizedDuration, NormalizedTimestamp, SequenceNumber,
};
use fm_types::{ChannelLayout, SampleRate};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const BYTES_PER_SAMPLE: usize = size_of::<f32>();

/// Hard ceiling for queued source blocks.
pub const MAX_SYNCHRONIZER_BLOCKS: usize = 256;
/// Hard ceiling for queued samples per channel.
pub const MAX_SYNCHRONIZER_SAMPLES: usize = 2_097_152;
/// Hard ceiling for preallocated source PCM storage.
pub const MAX_SYNCHRONIZER_BYTES: usize = 256 * 1024 * 1024;
/// Hard ceiling for one caller-buffer render.
pub const MAX_SYNCHRONIZER_OUTPUT_SAMPLES: usize = 48_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizerLimit {
    Blocks,
    Samples,
    Bytes,
    OutputSamples,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferLimit {
    Blocks,
    Samples,
    Bytes,
}

/// A continuity failure that requires an explicit reset or a corrected request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizerDiscontinuity {
    FlaggedInput,
    Sequence {
        expected: SequenceNumber,
        actual: SequenceNumber,
    },
    SourcePts {
        expected: NormalizedTimestamp,
        actual: NormalizedTimestamp,
    },
    MasterPts {
        expected: NormalizedTimestamp,
        actual: NormalizedTimestamp,
    },
    RequestedBeforeBuffer {
        requested_sample: i128,
        first_buffered_sample: u64,
    },
}

/// Errors from the bounded clock-mapped synchronizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioSynchronizerError {
    InvalidLimit {
        limit: SynchronizerLimit,
        value: usize,
        maximum: usize,
    },
    ByteCapacityTooSmall {
        minimum: usize,
        actual: usize,
    },
    SourceRateOutOfRange(u32),
    OutputRateOutOfRange(u32),
    ChannelCountOutOfRange(usize),
    DuplicateChannel,
    SourceRateMismatch {
        expected: SampleRate,
        actual: SampleRate,
    },
    ChannelLayoutMismatch,
    SourceClockMismatch {
        expected: NonZeroU128,
        actual: NonZeroU128,
    },
    MasterClockMismatch {
        expected: NonZeroU128,
        actual: NonZeroU128,
    },
    CorruptedInput,
    NonFiniteSample {
        channel: usize,
        sample: usize,
    },
    SourceDurationMismatch {
        expected_nanos: u64,
        actual_nanos: u64,
    },
    MasterDurationMismatch {
        expected_nanos: u64,
        actual_nanos: u64,
    },
    OutputPlaneCountMismatch {
        expected: usize,
        actual: usize,
    },
    OutputPlaneLengthMismatch {
        plane: usize,
        expected: usize,
        actual: usize,
    },
    OutputSampleCountOutOfRange {
        actual: usize,
        maximum: usize,
    },
    BufferOverflow {
        limit: BufferLimit,
        capacity: usize,
        attempted: usize,
    },
    NeedMoreInput {
        required_sample: u64,
        buffered_end_sample: u64,
    },
    StaleRenderPlan,
    Discontinuity(SynchronizerDiscontinuity),
    ArithmeticOverflow,
}

impl fmt::Display for AudioSynchronizerError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit {
                limit,
                value,
                maximum,
            } => write!(
                formatter,
                "synchronizer {limit:?} limit {value} is outside 1..={maximum}"
            ),
            Self::ByteCapacityTooSmall { minimum, actual } => write!(
                formatter,
                "synchronizer byte capacity {actual} cannot hold one sample frame ({minimum} bytes)"
            ),
            Self::SourceRateOutOfRange(rate) => {
                write!(formatter, "source sample rate {rate} Hz is unsupported")
            }
            Self::OutputRateOutOfRange(rate) => {
                write!(formatter, "output sample rate {rate} Hz is unsupported")
            }
            Self::ChannelCountOutOfRange(channels) => {
                write!(
                    formatter,
                    "synchronizer channel count {channels} is unsupported"
                )
            }
            Self::DuplicateChannel => formatter.write_str("channel layout contains duplicates"),
            Self::SourceRateMismatch { expected, actual } => write!(
                formatter,
                "source rate {} Hz does not match configured {} Hz",
                actual.hertz(),
                expected.hertz()
            ),
            Self::ChannelLayoutMismatch => {
                formatter.write_str("source channel layout does not match the configured layout")
            }
            Self::SourceClockMismatch { expected, actual } => write!(
                formatter,
                "source clock domain {actual} does not match configured domain {expected}"
            ),
            Self::MasterClockMismatch { expected, actual } => write!(
                formatter,
                "Master clock domain {actual} does not match configured domain {expected}"
            ),
            Self::CorruptedInput => formatter.write_str("source block is marked corrupted"),
            Self::NonFiniteSample { channel, sample } => {
                write!(
                    formatter,
                    "source sample {sample} in channel {channel} is not finite"
                )
            }
            Self::SourceDurationMismatch {
                expected_nanos,
                actual_nanos,
            } => write!(
                formatter,
                "source duration {actual_nanos} ns does not match expected {expected_nanos} ns"
            ),
            Self::MasterDurationMismatch {
                expected_nanos,
                actual_nanos,
            } => write!(
                formatter,
                "Master duration {actual_nanos} ns does not match expected {expected_nanos} ns"
            ),
            Self::OutputPlaneCountMismatch { expected, actual } => {
                write!(formatter, "output has {actual} planes; expected {expected}")
            }
            Self::OutputPlaneLengthMismatch {
                plane,
                expected,
                actual,
            } => write!(
                formatter,
                "output plane {plane} has {actual} samples; expected {expected}"
            ),
            Self::OutputSampleCountOutOfRange { actual, maximum } => write!(
                formatter,
                "output sample count {actual} is outside 1..={maximum}"
            ),
            Self::BufferOverflow {
                limit,
                capacity,
                attempted,
            } => write!(
                formatter,
                "source buffer {limit:?} capacity {capacity} cannot accept {attempted}"
            ),
            Self::NeedMoreInput {
                required_sample,
                buffered_end_sample,
            } => write!(
                formatter,
                "source sample {required_sample} is required but buffered input ends at {buffered_end_sample}"
            ),
            Self::StaleRenderPlan => formatter.write_str("audio render plan is stale"),
            Self::Discontinuity(discontinuity) => {
                write!(formatter, "audio discontinuity: {discontinuity:?}")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("audio synchronizer arithmetic overflow")
            }
        }
    }
}

impl std::error::Error for AudioSynchronizerError {}

/// Caller-selected queue and render bounds.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioSynchronizerLimits {
    max_blocks: usize,
    max_samples: usize,
    max_bytes: usize,
    max_output_samples: usize,
}

impl AudioSynchronizerLimits {
    /// Creates limits subject to the crate's hard memory and operation ceilings.
    ///
    /// # Errors
    ///
    /// Returns [`AudioSynchronizerError::InvalidLimit`] when any limit is zero
    /// or exceeds its corresponding hard ceiling.
    pub fn new(
        max_blocks: usize,
        max_samples: usize,
        max_bytes: usize,
        max_output_samples: usize,
    ) -> Result<Self, AudioSynchronizerError> {
        validate_limit(
            SynchronizerLimit::Blocks,
            max_blocks,
            MAX_SYNCHRONIZER_BLOCKS,
        )?;
        validate_limit(
            SynchronizerLimit::Samples,
            max_samples,
            MAX_SYNCHRONIZER_SAMPLES,
        )?;
        validate_limit(SynchronizerLimit::Bytes, max_bytes, MAX_SYNCHRONIZER_BYTES)?;
        validate_limit(
            SynchronizerLimit::OutputSamples,
            max_output_samples,
            MAX_SYNCHRONIZER_OUTPUT_SAMPLES,
        )?;
        Ok(Self {
            max_blocks,
            max_samples,
            max_bytes,
            max_output_samples,
        })
    }

    #[must_use]
    pub const fn max_blocks(self) -> usize {
        self.max_blocks
    }

    #[must_use]
    pub const fn max_samples(self) -> usize {
        self.max_samples
    }

    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }

    #[must_use]
    pub const fn max_output_samples(self) -> usize {
        self.max_output_samples
    }
}

impl Default for AudioSynchronizerLimits {
    fn default() -> Self {
        Self {
            max_blocks: 32,
            max_samples: 96_000,
            max_bytes: 32 * 1024 * 1024,
            max_output_samples: MAX_SYNCHRONIZER_OUTPUT_SAMPLES,
        }
    }
}

/// A known sample boundary on an absolute audio cadence.
///
/// `timestamp` is the normalized time at the start of `sample_index`. Keeping
/// the absolute index preserves floor-rounded endpoint durations and phase when
/// a stream starts or resets away from cadence sample zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioCadenceOrigin {
    timestamp: NormalizedTimestamp,
    sample_index: u64,
}

impl AudioCadenceOrigin {
    #[must_use]
    pub const fn new(timestamp: NormalizedTimestamp, sample_index: u64) -> Self {
        Self {
            timestamp,
            sample_index,
        }
    }

    #[must_use]
    pub const fn timestamp(self) -> NormalizedTimestamp {
        self.timestamp
    }

    #[must_use]
    pub const fn sample_index(self) -> u64 {
        self.sample_index
    }
}

/// One non-empty interval on the configured Master clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MasterAudioInterval {
    clock_domain: MappingClockDomainId,
    start: NormalizedTimestamp,
    duration: NormalizedDuration,
}

impl MasterAudioInterval {
    #[must_use]
    pub const fn new(
        clock_domain: MappingClockDomainId,
        start: NormalizedTimestamp,
        duration: NormalizedDuration,
    ) -> Self {
        Self {
            clock_domain,
            start,
            duration,
        }
    }

    #[must_use]
    pub const fn clock_domain(self) -> MappingClockDomainId {
        self.clock_domain
    }

    #[must_use]
    pub const fn start(self) -> NormalizedTimestamp {
        self.start
    }

    #[must_use]
    pub const fn duration(self) -> NormalizedDuration {
        self.duration
    }
}

/// Saturating cumulative counters plus instantaneous bounded occupancy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioSynchronizerTelemetry {
    accepted_blocks: u64,
    accepted_samples: u64,
    rejected_blocks: u64,
    rendered_intervals: u64,
    rendered_samples: u64,
    failed_renders: u64,
    need_more_input: u64,
    resets: u64,
    buffered_blocks: usize,
    buffered_samples: usize,
    buffered_bytes: usize,
    peak_buffered_blocks: usize,
    peak_buffered_samples: usize,
    peak_buffered_bytes: usize,
}

impl AudioSynchronizerTelemetry {
    #[must_use]
    pub const fn accepted_blocks(self) -> u64 {
        self.accepted_blocks
    }

    #[must_use]
    pub const fn accepted_samples(self) -> u64 {
        self.accepted_samples
    }

    #[must_use]
    pub const fn rejected_blocks(self) -> u64 {
        self.rejected_blocks
    }

    #[must_use]
    pub const fn rendered_intervals(self) -> u64 {
        self.rendered_intervals
    }

    #[must_use]
    pub const fn rendered_samples(self) -> u64 {
        self.rendered_samples
    }

    #[must_use]
    pub const fn failed_renders(self) -> u64 {
        self.failed_renders
    }

    #[must_use]
    pub const fn need_more_input(self) -> u64 {
        self.need_more_input
    }

    #[must_use]
    pub const fn resets(self) -> u64 {
        self.resets
    }

    #[must_use]
    pub const fn buffered_blocks(self) -> usize {
        self.buffered_blocks
    }

    #[must_use]
    pub const fn buffered_samples(self) -> usize {
        self.buffered_samples
    }

    #[must_use]
    pub const fn buffered_bytes(self) -> usize {
        self.buffered_bytes
    }

    #[must_use]
    pub const fn peak_buffered_blocks(self) -> usize {
        self.peak_buffered_blocks
    }

    #[must_use]
    pub const fn peak_buffered_samples(self) -> usize {
        self.peak_buffered_samples
    }

    #[must_use]
    pub const fn peak_buffered_bytes(self) -> usize {
        self.peak_buffered_bytes
    }
}

#[derive(Clone, Copy, Debug)]
struct BufferedBlock {
    remaining_samples: usize,
}

#[derive(Clone, Copy, Debug)]
struct InputCursor {
    next_sequence: SequenceNumber,
    next_sample: u64,
}

#[derive(Clone, Copy, Debug)]
struct OutputCursor {
    next_sample: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioRenderPlan {
    cursor: OutputCursor,
    output_samples: usize,
    discard_before: u64,
    generation: u64,
}

/// One timing-bearing silence span copied into preallocated synchronizer storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioSilenceSpan {
    timing: MediaTiming,
    sample_count: NonZeroUsize,
}

impl AudioSilenceSpan {
    #[must_use]
    pub const fn new(timing: MediaTiming, sample_count: NonZeroUsize) -> Self {
        Self {
            timing,
            sample_count,
        }
    }

    #[must_use]
    pub const fn timing(self) -> MediaTiming {
        self.timing
    }

    #[must_use]
    pub const fn sample_count(self) -> usize {
        self.sample_count.get()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SamplePhase {
    source_sample: u64,
    fraction: f64,
}

/// Fixed-format, fixed-clock linear synchronizer and resampler.
///
/// Construction preallocates all internal PCM and block metadata. [`Self::push`]
/// copies canonical planar samples into that storage, and [`Self::render_into`]
/// writes directly into equally sized caller planes. The type performs no
/// channel conversion and contains no clock-estimation policy.
#[derive(Debug)]
pub struct ClockMappedAudioSynchronizer {
    source_rate: SampleRate,
    output_rate: SampleRate,
    channel_layout: ChannelLayout,
    mapping: ClockMapping,
    source_origin: AudioCadenceOrigin,
    master_origin: AudioCadenceOrigin,
    limits: AudioSynchronizerLimits,
    bytes_per_sample_frame: usize,
    sample_capacity: usize,
    planes: Vec<Vec<f32>>,
    phases: Vec<SamplePhase>,
    blocks: VecDeque<BufferedBlock>,
    read_position: usize,
    first_sample_index: u64,
    buffered_samples: usize,
    buffered_bytes: usize,
    input_cursor: Option<InputCursor>,
    output_cursor: Option<OutputCursor>,
    render_generation: u64,
    telemetry: AudioSynchronizerTelemetry,
}

impl ClockMappedAudioSynchronizer {
    /// Preallocates a synchronizer for one source format and affine clock map.
    /// `source_origin` and `master_origin` identify the first accepted sample
    /// boundaries on their independent absolute cadences.
    ///
    /// # Errors
    ///
    /// Returns a typed configuration error for unsupported rates or layouts,
    /// insufficient byte capacity, or allocation-size arithmetic overflow.
    pub fn new(
        source_rate: SampleRate,
        output_rate: SampleRate,
        channel_layout: ChannelLayout,
        mapping: ClockMapping,
        source_origin: AudioCadenceOrigin,
        master_origin: AudioCadenceOrigin,
        limits: AudioSynchronizerLimits,
    ) -> Result<Self, AudioSynchronizerError> {
        validate_rate(source_rate, true)?;
        validate_rate(output_rate, false)?;
        let channels = channel_layout.channels();
        if !(1..=crate::MAX_CHANNELS).contains(&channels.len()) {
            return Err(AudioSynchronizerError::ChannelCountOutOfRange(
                channels.len(),
            ));
        }
        if channels
            .iter()
            .enumerate()
            .any(|(index, channel)| channels[index + 1..].contains(channel))
        {
            return Err(AudioSynchronizerError::DuplicateChannel);
        }
        let bytes_per_sample_frame = channels
            .len()
            .checked_mul(BYTES_PER_SAMPLE)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        if limits.max_bytes < bytes_per_sample_frame {
            return Err(AudioSynchronizerError::ByteCapacityTooSmall {
                minimum: bytes_per_sample_frame,
                actual: limits.max_bytes,
            });
        }
        let sample_capacity = limits
            .max_samples
            .min(limits.max_bytes / bytes_per_sample_frame);
        let mut planes = Vec::with_capacity(channels.len());
        for _ in channels {
            planes.push(vec![0.0; sample_capacity]);
        }
        Ok(Self {
            source_rate,
            output_rate,
            channel_layout,
            mapping,
            source_origin,
            master_origin,
            limits,
            bytes_per_sample_frame,
            sample_capacity,
            planes,
            phases: vec![SamplePhase::default(); limits.max_output_samples],
            blocks: VecDeque::with_capacity(limits.max_blocks),
            read_position: 0,
            first_sample_index: source_origin.sample_index,
            buffered_samples: 0,
            buffered_bytes: 0,
            input_cursor: None,
            output_cursor: None,
            render_generation: 0,
            telemetry: AudioSynchronizerTelemetry::default(),
        })
    }

    #[must_use]
    pub const fn source_rate(&self) -> SampleRate {
        self.source_rate
    }

    #[must_use]
    pub const fn output_rate(&self) -> SampleRate {
        self.output_rate
    }

    #[must_use]
    pub const fn channel_layout(&self) -> &ChannelLayout {
        &self.channel_layout
    }

    #[must_use]
    pub const fn mapping(&self) -> ClockMapping {
        self.mapping
    }

    #[must_use]
    pub const fn source_origin(&self) -> AudioCadenceOrigin {
        self.source_origin
    }

    #[must_use]
    pub const fn master_origin(&self) -> AudioCadenceOrigin {
        self.master_origin
    }

    #[must_use]
    pub const fn limits(&self) -> AudioSynchronizerLimits {
        self.limits
    }

    #[must_use]
    pub const fn telemetry(&self) -> AudioSynchronizerTelemetry {
        self.telemetry
    }

    /// Validates and copies one contiguous canonical source block atomically.
    /// The first block must begin at the configured source cadence origin.
    ///
    /// # Errors
    ///
    /// Returns a typed format, timing, continuity, bounds, or arithmetic error.
    /// The input cursor and buffered samples remain unchanged on error.
    pub fn push(&mut self, block: &AudioBlock) -> Result<(), AudioSynchronizerError> {
        self.push_batch(core::slice::from_ref(block))
    }

    /// Validates a contiguous batch without changing buffered media or cursors.
    ///
    /// # Errors
    ///
    /// Returns the same format, timing, continuity, and bound errors as
    /// [`Self::push`].
    pub fn preflight_push_batch(
        &self,
        blocks: &[AudioBlock],
    ) -> Result<(), AudioSynchronizerError> {
        let mut cursor = self.input_cursor;
        let mut block_count = self.blocks.len();
        let mut samples = self.buffered_samples;
        let mut bytes = self.buffered_bytes;
        for block in blocks {
            let (next_cursor, block_bytes) =
                self.validate_push_at(block, cursor, block_count, samples, bytes)?;
            cursor = Some(next_cursor);
            block_count = block_count
                .checked_add(1)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            samples = samples
                .checked_add(block.sample_count())
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            bytes = bytes
                .checked_add(block_bytes)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        }
        Ok(())
    }

    /// Atomically validates and copies a contiguous batch into preallocated storage.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::preflight_push_batch`]. No block from
    /// the batch is accepted when validation fails.
    pub fn push_batch(&mut self, blocks: &[AudioBlock]) -> Result<(), AudioSynchronizerError> {
        if let Err(error) = self.preflight_push_batch(blocks) {
            self.telemetry.rejected_blocks = self
                .telemetry
                .rejected_blocks
                .saturating_add(u64::try_from(blocks.len()).unwrap_or(u64::MAX));
            return Err(error);
        }
        for block in blocks {
            let result = self.validate_push(block);
            let Ok((next_cursor, block_bytes)) = result else {
                unreachable!("unchanged synchronizer passed batch preflight");
            };

            let write_position =
                (self.read_position + self.buffered_samples) % self.sample_capacity;
            for (channel, source) in block.planes().iter().enumerate() {
                let first = source.len().min(self.sample_capacity - write_position);
                self.planes[channel][write_position..write_position + first]
                    .copy_from_slice(&source[..first]);
                self.planes[channel][..source.len() - first].copy_from_slice(&source[first..]);
            }
            self.blocks.push_back(BufferedBlock {
                remaining_samples: block.sample_count(),
            });
            self.buffered_samples += block.sample_count();
            self.buffered_bytes += block_bytes;
            self.input_cursor = Some(next_cursor);
            self.telemetry.accepted_blocks = self.telemetry.accepted_blocks.saturating_add(1);
            self.telemetry.accepted_samples = self
                .telemetry
                .accepted_samples
                .saturating_add(u64::try_from(block.sample_count()).unwrap_or(u64::MAX));
        }
        self.update_occupancy_telemetry();
        Ok(())
    }

    /// Validates contiguous silence metadata without changing buffered media.
    ///
    /// # Errors
    ///
    /// Returns a timing, continuity, capacity, or arithmetic error.
    pub fn preflight_push_silence_batch(
        &self,
        spans: &[AudioSilenceSpan],
    ) -> Result<(), AudioSynchronizerError> {
        let mut cursor = self.input_cursor;
        let mut block_count = self.blocks.len();
        let mut samples = self.buffered_samples;
        let mut bytes = self.buffered_bytes;
        for span in spans {
            let (next_cursor, block_bytes) = self.validate_push_timing_at(
                span.timing,
                span.sample_count(),
                cursor,
                block_count,
                samples,
                bytes,
            )?;
            cursor = Some(next_cursor);
            block_count = block_count
                .checked_add(1)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            samples = samples
                .checked_add(span.sample_count())
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            bytes = bytes
                .checked_add(block_bytes)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        }
        Ok(())
    }

    /// Atomically writes contiguous silence into preallocated PCM and block storage.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::preflight_push_silence_batch`].
    pub fn push_silence_batch(
        &mut self,
        spans: &[AudioSilenceSpan],
    ) -> Result<(), AudioSynchronizerError> {
        if let Err(error) = self.preflight_push_silence_batch(spans) {
            self.telemetry.rejected_blocks = self
                .telemetry
                .rejected_blocks
                .saturating_add(u64::try_from(spans.len()).unwrap_or(u64::MAX));
            return Err(error);
        }
        for span in spans {
            let result = self.validate_push_timing_at(
                span.timing,
                span.sample_count(),
                self.input_cursor,
                self.blocks.len(),
                self.buffered_samples,
                self.buffered_bytes,
            );
            let Ok((next_cursor, block_bytes)) = result else {
                unreachable!("unchanged synchronizer passed silence batch preflight");
            };
            let write_position =
                (self.read_position + self.buffered_samples) % self.sample_capacity;
            let first = span
                .sample_count()
                .min(self.sample_capacity - write_position);
            for plane in &mut self.planes {
                plane[write_position..write_position + first].fill(0.0);
                plane[..span.sample_count() - first].fill(0.0);
            }
            self.blocks.push_back(BufferedBlock {
                remaining_samples: span.sample_count(),
            });
            self.buffered_samples += span.sample_count();
            self.buffered_bytes += block_bytes;
            self.input_cursor = Some(next_cursor);
            self.telemetry.accepted_blocks = self.telemetry.accepted_blocks.saturating_add(1);
            self.telemetry.accepted_samples = self
                .telemetry
                .accepted_samples
                .saturating_add(u64::try_from(span.sample_count()).unwrap_or(u64::MAX));
        }
        self.update_occupancy_telemetry();
        Ok(())
    }

    /// Renders one exact, contiguous Master interval into caller-owned planes.
    /// The first interval must begin at the configured Master cadence origin.
    ///
    /// Every output plane must have the same non-zero length. Validation,
    /// mapping, lookahead, and post-render cursor arithmetic are preflighted
    /// before the first output sample is written. On error the output and all
    /// stream cursors remain unchanged; failure telemetry still increments.
    ///
    /// # Errors
    ///
    /// Returns a typed caller-buffer, Master interval, clock mapping,
    /// continuity, missing-input, or arithmetic error.
    pub fn render_into(
        &mut self,
        interval: MasterAudioInterval,
        output: &mut [&mut [f32]],
    ) -> Result<(), AudioSynchronizerError> {
        let output_samples = output.first().map_or(0, |plane| plane.len());
        if let Err(error) = self.validate_output(output, output_samples) {
            self.record_render_failure(&error);
            return Err(error);
        }
        let plan = match self.plan_render(interval, output_samples) {
            Ok(plan) => plan,
            Err(error) => {
                self.record_render_failure(&error);
                return Err(error);
            }
        };
        if let Err(error) = self.render_planned_into(plan, output) {
            self.record_render_failure(&error);
            return Err(error);
        }
        self.commit_render(plan)
    }

    /// Plans one render into internal preallocated scratch without advancing
    /// buffered media, output continuity, or telemetry.
    ///
    /// # Errors
    ///
    /// Returns a Master timing, mapping, bound, or missing-input error.
    pub fn plan_render(
        &mut self,
        interval: MasterAudioInterval,
        output_samples: usize,
    ) -> Result<AudioRenderPlan, AudioSynchronizerError> {
        self.prepare_render(interval, output_samples)
    }

    /// Writes a previously planned render without advancing synchronizer state.
    ///
    /// # Errors
    ///
    /// Returns an output-shape error or [`AudioSynchronizerError::StaleRenderPlan`].
    pub fn render_planned_into(
        &self,
        plan: AudioRenderPlan,
        output: &mut [&mut [f32]],
    ) -> Result<(), AudioSynchronizerError> {
        self.validate_plan(plan)?;
        self.validate_output(output, plan.output_samples)?;
        for output_sample in 0..plan.output_samples {
            let phase = self.phases[output_sample];
            for (channel, plane) in output.iter_mut().enumerate() {
                let first = f64::from(self.buffered_sample(channel, phase.source_sample));
                let value = if phase.fraction == 0.0 {
                    first
                } else {
                    let second = f64::from(
                        self.buffered_sample(channel, phase.source_sample.saturating_add(1)),
                    );
                    first + (second - first) * phase.fraction
                };
                plane[output_sample] = interpolated_sample(value);
            }
        }
        Ok(())
    }

    /// Writes a plan at `output_offset` in preallocated planar vectors without
    /// advancing synchronizer state.
    ///
    /// # Errors
    ///
    /// Returns an output-shape error or [`AudioSynchronizerError::StaleRenderPlan`].
    pub fn render_planned_planes(
        &self,
        plan: AudioRenderPlan,
        output: &mut [Vec<f32>],
        output_offset: usize,
    ) -> Result<(), AudioSynchronizerError> {
        self.validate_plan(plan)?;
        let channels = self.channel_layout.channels().len();
        if output.len() != channels {
            return Err(AudioSynchronizerError::OutputPlaneCountMismatch {
                expected: channels,
                actual: output.len(),
            });
        }
        let required = output_offset
            .checked_add(plan.output_samples)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        for (plane, samples) in output.iter().enumerate() {
            if samples.len() < required {
                return Err(AudioSynchronizerError::OutputPlaneLengthMismatch {
                    plane,
                    expected: required,
                    actual: samples.len(),
                });
            }
        }
        for output_sample in 0..plan.output_samples {
            let phase = self.phases[output_sample];
            for (channel, plane) in output.iter_mut().enumerate() {
                let first = f64::from(self.buffered_sample(channel, phase.source_sample));
                let value = if phase.fraction == 0.0 {
                    first
                } else {
                    let second = f64::from(
                        self.buffered_sample(channel, phase.source_sample.saturating_add(1)),
                    );
                    first + (second - first) * phase.fraction
                };
                plane[output_offset + output_sample] = interpolated_sample(value);
            }
        }
        Ok(())
    }

    /// Verifies that a render plan can still be committed without changing state.
    ///
    /// # Errors
    ///
    /// Returns [`AudioSynchronizerError::StaleRenderPlan`] if another render or
    /// reset invalidated the plan.
    pub fn preflight_commit_render(
        &self,
        plan: AudioRenderPlan,
    ) -> Result<(), AudioSynchronizerError> {
        self.validate_plan(plan)
    }

    /// Commits the cursor and occupancy effects of a previously rendered plan.
    ///
    /// # Errors
    ///
    /// Returns [`AudioSynchronizerError::StaleRenderPlan`] if another render or
    /// reset invalidated the plan.
    pub fn commit_render(&mut self, plan: AudioRenderPlan) -> Result<(), AudioSynchronizerError> {
        self.validate_plan(plan)?;
        self.output_cursor = Some(plan.cursor);
        self.discard_before(plan.discard_before);
        self.render_generation = self.render_generation.wrapping_add(1);
        self.telemetry.rendered_intervals = self.telemetry.rendered_intervals.saturating_add(1);
        self.telemetry.rendered_samples = self
            .telemetry
            .rendered_samples
            .saturating_add(u64::try_from(plan.output_samples).unwrap_or(u64::MAX));
        self.update_occupancy_telemetry();
        Ok(())
    }

    /// Drops buffered media and rearms both absolute cadences without reallocating.
    /// Cumulative telemetry is retained and the reset count increments.
    pub fn reset(&mut self, source_origin: AudioCadenceOrigin, master_origin: AudioCadenceOrigin) {
        self.blocks.clear();
        self.read_position = 0;
        self.first_sample_index = source_origin.sample_index;
        self.buffered_samples = 0;
        self.buffered_bytes = 0;
        self.input_cursor = None;
        self.output_cursor = None;
        self.render_generation = self.render_generation.wrapping_add(1);
        self.source_origin = source_origin;
        self.master_origin = master_origin;
        self.telemetry.resets = self.telemetry.resets.saturating_add(1);
        self.update_occupancy_telemetry();
    }

    #[allow(clippy::too_many_lines)]
    fn validate_push(
        &self,
        block: &AudioBlock,
    ) -> Result<(InputCursor, usize), AudioSynchronizerError> {
        self.validate_push_at(
            block,
            self.input_cursor,
            self.blocks.len(),
            self.buffered_samples,
            self.buffered_bytes,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn validate_push_at(
        &self,
        block: &AudioBlock,
        input_cursor: Option<InputCursor>,
        buffered_blocks: usize,
        buffered_samples: usize,
        buffered_bytes: usize,
    ) -> Result<(InputCursor, usize), AudioSynchronizerError> {
        if block.sample_rate() != self.source_rate {
            return Err(AudioSynchronizerError::SourceRateMismatch {
                expected: self.source_rate,
                actual: block.sample_rate(),
            });
        }
        if block.channel_layout() != &self.channel_layout {
            return Err(AudioSynchronizerError::ChannelLayoutMismatch);
        }
        let timing = block.timing();
        for (channel, plane) in block.planes().iter().enumerate() {
            if let Some(sample) = plane.iter().position(|sample| !sample.is_finite()) {
                return Err(AudioSynchronizerError::NonFiniteSample { channel, sample });
            }
        }

        self.validate_push_timing_at(
            timing,
            block.sample_count(),
            input_cursor,
            buffered_blocks,
            buffered_samples,
            buffered_bytes,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn validate_push_timing_at(
        &self,
        timing: MediaTiming,
        sample_count: usize,
        input_cursor: Option<InputCursor>,
        buffered_blocks: usize,
        buffered_samples: usize,
        buffered_bytes: usize,
    ) -> Result<(InputCursor, usize), AudioSynchronizerError> {
        let expected_clock = self.mapping.source_domain().get();
        let actual_clock = timing.clock_domain().get();
        if actual_clock != expected_clock {
            return Err(AudioSynchronizerError::SourceClockMismatch {
                expected: expected_clock,
                actual: actual_clock,
            });
        }
        if timing.flags().contains(MediaFlags::DISCONTINUITY) {
            return Err(AudioSynchronizerError::Discontinuity(
                SynchronizerDiscontinuity::FlaggedInput,
            ));
        }
        if timing.flags().contains(MediaFlags::CORRUPTED) {
            return Err(AudioSynchronizerError::CorruptedInput);
        }

        let sample_count_u64 =
            u64::try_from(sample_count).map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;
        let (first_sample, next_sequence) = if let Some(cursor) = input_cursor {
            if timing.sequence() != cursor.next_sequence {
                return Err(AudioSynchronizerError::Discontinuity(
                    SynchronizerDiscontinuity::Sequence {
                        expected: cursor.next_sequence,
                        actual: timing.sequence(),
                    },
                ));
            }
            let expected_pts =
                cadence_timestamp(self.source_origin, cursor.next_sample, self.source_rate)?;
            if timing.presentation_timestamp() != expected_pts {
                return Err(AudioSynchronizerError::Discontinuity(
                    SynchronizerDiscontinuity::SourcePts {
                        expected: expected_pts,
                        actual: timing.presentation_timestamp(),
                    },
                ));
            }
            let next = timing
                .sequence()
                .checked_next()
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            (cursor.next_sample, next)
        } else {
            let expected_pts = cadence_timestamp(
                self.source_origin,
                self.source_origin.sample_index,
                self.source_rate,
            )?;
            if timing.presentation_timestamp() != expected_pts {
                return Err(AudioSynchronizerError::Discontinuity(
                    SynchronizerDiscontinuity::SourcePts {
                        expected: expected_pts,
                        actual: timing.presentation_timestamp(),
                    },
                ));
            }
            let next = timing
                .sequence()
                .checked_next()
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            (self.source_origin.sample_index, next)
        };
        let expected_duration =
            sample_span_duration(first_sample, sample_count_u64, self.source_rate)?;
        if timing.duration().as_nanos() != expected_duration {
            return Err(AudioSynchronizerError::SourceDurationMismatch {
                expected_nanos: expected_duration,
                actual_nanos: timing.duration().as_nanos(),
            });
        }

        let block_bytes = sample_count
            .checked_mul(self.bytes_per_sample_frame)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        Self::check_buffer_limit(
            BufferLimit::Blocks,
            buffered_blocks,
            1,
            self.limits.max_blocks,
        )?;
        Self::check_buffer_limit(
            BufferLimit::Samples,
            buffered_samples,
            sample_count,
            self.limits.max_samples,
        )?;
        Self::check_buffer_limit(
            BufferLimit::Bytes,
            buffered_bytes,
            block_bytes,
            self.limits.max_bytes,
        )?;
        let next_sample = first_sample
            .checked_add(sample_count_u64)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        Ok((
            InputCursor {
                next_sequence,
                next_sample,
            },
            block_bytes,
        ))
    }

    fn check_buffer_limit(
        limit: BufferLimit,
        current: usize,
        added: usize,
        capacity: usize,
    ) -> Result<(), AudioSynchronizerError> {
        let attempted = current
            .checked_add(added)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        if attempted > capacity {
            return Err(AudioSynchronizerError::BufferOverflow {
                limit,
                capacity,
                attempted,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn prepare_render(
        &mut self,
        interval: MasterAudioInterval,
        output_samples: usize,
    ) -> Result<AudioRenderPlan, AudioSynchronizerError> {
        // Planning reuses shared phase scratch, so every attempt invalidates
        // prior plans even when this attempt later fails validation.
        self.render_generation = self.render_generation.wrapping_add(1);
        if output_samples == 0 || output_samples > self.limits.max_output_samples {
            return Err(AudioSynchronizerError::OutputSampleCountOutOfRange {
                actual: output_samples,
                maximum: self.limits.max_output_samples,
            });
        }
        let expected_master_clock = self.mapping.master_domain().get();
        let actual_master_clock = interval.clock_domain.get();
        if actual_master_clock != expected_master_clock {
            return Err(AudioSynchronizerError::MasterClockMismatch {
                expected: expected_master_clock,
                actual: actual_master_clock,
            });
        }

        let first_output_sample = if let Some(cursor) = self.output_cursor {
            let expected_start =
                cadence_timestamp(self.master_origin, cursor.next_sample, self.output_rate)?;
            if interval.start != expected_start {
                return Err(AudioSynchronizerError::Discontinuity(
                    SynchronizerDiscontinuity::MasterPts {
                        expected: expected_start,
                        actual: interval.start,
                    },
                ));
            }
            cursor.next_sample
        } else {
            let expected_start = cadence_timestamp(
                self.master_origin,
                self.master_origin.sample_index,
                self.output_rate,
            )?;
            if interval.start != expected_start {
                return Err(AudioSynchronizerError::Discontinuity(
                    SynchronizerDiscontinuity::MasterPts {
                        expected: expected_start,
                        actual: interval.start,
                    },
                ));
            }
            self.master_origin.sample_index
        };
        let output_samples_u64 = u64::try_from(output_samples)
            .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;
        let expected_duration =
            sample_span_duration(first_output_sample, output_samples_u64, self.output_rate)?;
        if interval.duration.as_nanos() != expected_duration {
            return Err(AudioSynchronizerError::MasterDurationMismatch {
                expected_nanos: expected_duration,
                actual_nanos: interval.duration.as_nanos(),
            });
        }
        let interval_end = interval
            .start
            .as_nanos()
            .checked_add(
                i64::try_from(interval.duration.as_nanos())
                    .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?,
            )
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        self.mapping
            .source_nanos_at_master(interval.start.as_nanos())
            .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;
        self.mapping
            .source_nanos_at_master(interval_end)
            .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;

        let next_output_sample = first_output_sample
            .checked_add(output_samples_u64)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        let mut required_source_sample = None;
        for offset in 0..output_samples_u64 {
            let output_sample = first_output_sample
                .checked_add(offset)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
            let (source_sample, remainder, denominator) = self.source_phase(output_sample)?;
            let (source_sample, required_sample) =
                self.source_sample_requirement(source_sample, remainder != 0)?;
            required_source_sample = Some(
                required_source_sample
                    .map_or(required_sample, |current: u64| current.max(required_sample)),
            );
            let phase_index =
                usize::try_from(offset).map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;
            self.phases[phase_index] = SamplePhase {
                source_sample,
                fraction: fraction_as_f64(remainder, denominator),
            };
        }
        let buffered_end_sample = self
            .first_sample_index
            .checked_add(
                u64::try_from(self.buffered_samples)
                    .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?,
            )
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        if required_source_sample.is_some_and(|required| required >= buffered_end_sample) {
            return Err(AudioSynchronizerError::NeedMoreInput {
                required_sample: required_source_sample.expect("checked as present"),
                buffered_end_sample,
            });
        }
        let (next_source_sample, _, _) = self.source_phase(next_output_sample)?;
        let discard_before = if next_source_sample <= 0 {
            0
        } else {
            u64::try_from(next_source_sample)
                .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?
        };
        Ok(AudioRenderPlan {
            cursor: OutputCursor {
                next_sample: next_output_sample,
            },
            output_samples,
            discard_before,
            generation: self.render_generation,
        })
    }

    fn validate_plan(&self, plan: AudioRenderPlan) -> Result<(), AudioSynchronizerError> {
        if plan.generation != self.render_generation {
            return Err(AudioSynchronizerError::StaleRenderPlan);
        }
        Ok(())
    }

    fn validate_output(
        &self,
        output: &[&mut [f32]],
        output_samples: usize,
    ) -> Result<(), AudioSynchronizerError> {
        let channels = self.channel_layout.channels().len();
        if output.len() != channels {
            return Err(AudioSynchronizerError::OutputPlaneCountMismatch {
                expected: channels,
                actual: output.len(),
            });
        }
        for (plane, samples) in output.iter().enumerate() {
            if samples.len() != output_samples {
                return Err(AudioSynchronizerError::OutputPlaneLengthMismatch {
                    plane,
                    expected: output_samples,
                    actual: samples.len(),
                });
            }
        }
        Ok(())
    }

    fn record_render_failure(&mut self, error: &AudioSynchronizerError) {
        self.telemetry.failed_renders = self.telemetry.failed_renders.saturating_add(1);
        if matches!(error, AudioSynchronizerError::NeedMoreInput { .. }) {
            self.telemetry.need_more_input = self.telemetry.need_more_input.saturating_add(1);
        }
    }

    fn source_phase(
        &self,
        output_sample: u64,
    ) -> Result<(i128, i128, i128), AudioSynchronizerError> {
        let source_rate = i128::from(self.source_rate.hertz());
        let master_timestamp =
            cadence_timestamp(self.master_origin, output_sample, self.output_rate)?;
        let source_timestamp = self
            .mapping
            .source_nanos_at_master(master_timestamp.as_nanos())
            .map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;
        let elapsed = i128::from(source_timestamp)
            .checked_sub(i128::from(self.source_origin.timestamp.as_nanos()))
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        let source_origin_boundary = cadence_boundary(
            i128::from(self.source_origin.sample_index),
            self.source_rate,
        )?;
        let absolute_position = elapsed
            .checked_add(source_origin_boundary)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        // This is the signed counterpart of "latest cadence boundary at or
        // before time" and preserves floor behavior on both sides of zero.
        let sample_numerator = absolute_position
            .checked_add(1)
            .and_then(|value| value.checked_mul(source_rate))
            .and_then(|value| value.checked_sub(1))
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        let (source_sample, _, _) = floor_div_rem(sample_numerator, NANOS_PER_SECOND)?;
        let source_boundary = cadence_boundary(source_sample, self.source_rate)?
            .checked_sub(source_origin_boundary)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        let next_boundary = cadence_boundary(
            source_sample
                .checked_add(1)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?,
            self.source_rate,
        )?
        .checked_sub(source_origin_boundary)
        .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        let remainder = elapsed
            .checked_sub(source_boundary)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        let denominator = next_boundary
            .checked_sub(source_boundary)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        Ok((source_sample, remainder, denominator))
    }

    fn source_sample_requirement(
        &self,
        source_sample: i128,
        requires_lookahead: bool,
    ) -> Result<(u64, u64), AudioSynchronizerError> {
        if source_sample < i128::from(self.first_sample_index) {
            return Err(AudioSynchronizerError::Discontinuity(
                SynchronizerDiscontinuity::RequestedBeforeBuffer {
                    requested_sample: source_sample,
                    first_buffered_sample: self.first_sample_index,
                },
            ));
        }
        let source_sample =
            u64::try_from(source_sample).map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?;
        let required_sample = if requires_lookahead {
            source_sample
                .checked_add(1)
                .ok_or(AudioSynchronizerError::ArithmeticOverflow)?
        } else {
            source_sample
        };
        Ok((source_sample, required_sample))
    }

    fn buffered_sample(&self, channel: usize, absolute_sample: u64) -> f32 {
        let offset = usize::try_from(absolute_sample - self.first_sample_index)
            .expect("preflighted buffered sample offset fits usize");
        let position = (self.read_position + offset) % self.sample_capacity;
        self.planes[channel][position]
    }

    fn discard_before(&mut self, requested_sample: u64) {
        let buffered_end = self
            .first_sample_index
            .saturating_add(u64::try_from(self.buffered_samples).unwrap_or(u64::MAX));
        let target = requested_sample.min(buffered_end);
        let discard = usize::try_from(target.saturating_sub(self.first_sample_index))
            .unwrap_or(self.buffered_samples)
            .min(self.buffered_samples);
        if discard == 0 {
            return;
        }
        self.read_position = (self.read_position + discard) % self.sample_capacity;
        self.first_sample_index = self
            .first_sample_index
            .saturating_add(u64::try_from(discard).unwrap_or(u64::MAX));
        self.buffered_samples -= discard;
        self.buffered_bytes -= discard * self.bytes_per_sample_frame;

        let mut remaining = discard;
        while remaining > 0 {
            let Some(front) = self.blocks.front_mut() else {
                break;
            };
            if remaining < front.remaining_samples {
                front.remaining_samples -= remaining;
                break;
            }
            remaining -= front.remaining_samples;
            self.blocks.pop_front();
        }
    }

    fn update_occupancy_telemetry(&mut self) {
        self.telemetry.buffered_blocks = self.blocks.len();
        self.telemetry.buffered_samples = self.buffered_samples;
        self.telemetry.buffered_bytes = self.buffered_bytes;
        self.telemetry.peak_buffered_blocks = self
            .telemetry
            .peak_buffered_blocks
            .max(self.telemetry.buffered_blocks);
        self.telemetry.peak_buffered_samples = self
            .telemetry
            .peak_buffered_samples
            .max(self.telemetry.buffered_samples);
        self.telemetry.peak_buffered_bytes = self
            .telemetry
            .peak_buffered_bytes
            .max(self.telemetry.buffered_bytes);
    }
}

fn validate_limit(
    limit: SynchronizerLimit,
    value: usize,
    maximum: usize,
) -> Result<(), AudioSynchronizerError> {
    if value == 0 || value > maximum {
        return Err(AudioSynchronizerError::InvalidLimit {
            limit,
            value,
            maximum,
        });
    }
    Ok(())
}

fn validate_rate(rate: SampleRate, source: bool) -> Result<(), AudioSynchronizerError> {
    if rate.hertz() > AudioBlock::MAX_SAMPLE_RATE_HZ {
        return Err(if source {
            AudioSynchronizerError::SourceRateOutOfRange(rate.hertz())
        } else {
            AudioSynchronizerError::OutputRateOutOfRange(rate.hertz())
        });
    }
    Ok(())
}

fn cadence_timestamp(
    origin: AudioCadenceOrigin,
    sample: u64,
    rate: SampleRate,
) -> Result<NormalizedTimestamp, AudioSynchronizerError> {
    let origin_boundary = cadence_boundary(i128::from(origin.sample_index), rate)?;
    let sample_boundary = cadence_boundary(i128::from(sample), rate)?;
    let offset = sample_boundary
        .checked_sub(origin_boundary)
        .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
    let timestamp = i128::from(origin.timestamp.as_nanos())
        .checked_add(offset)
        .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
    Ok(NormalizedTimestamp::from_nanos(
        i64::try_from(timestamp).map_err(|_| AudioSynchronizerError::ArithmeticOverflow)?,
    ))
}

fn cadence_boundary(sample: i128, rate: SampleRate) -> Result<i128, AudioSynchronizerError> {
    floor_div_rem(
        sample
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?,
        i128::from(rate.hertz()),
    )
    .map(|(boundary, _, _)| boundary)
}

fn sample_span_duration(
    first_sample: u64,
    sample_count: u64,
    rate: SampleRate,
) -> Result<u64, AudioSynchronizerError> {
    let last_sample = first_sample
        .checked_add(sample_count)
        .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
    let nanos = NANOS_PER_SECOND.cast_unsigned();
    let denominator = u128::from(rate.hertz());
    let start = u128::from(first_sample)
        .checked_mul(nanos)
        .ok_or(AudioSynchronizerError::ArithmeticOverflow)?
        / denominator;
    let end = u128::from(last_sample)
        .checked_mul(nanos)
        .ok_or(AudioSynchronizerError::ArithmeticOverflow)?
        / denominator;
    u64::try_from(end - start).map_err(|_| AudioSynchronizerError::ArithmeticOverflow)
}

fn floor_div_rem(
    numerator: i128,
    denominator: i128,
) -> Result<(i128, i128, i128), AudioSynchronizerError> {
    if denominator <= 0 {
        return Err(AudioSynchronizerError::ArithmeticOverflow);
    }
    let mut quotient = numerator / denominator;
    let mut remainder = numerator % denominator;
    if remainder < 0 {
        quotient = quotient
            .checked_sub(1)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
        remainder = remainder
            .checked_add(denominator)
            .ok_or(AudioSynchronizerError::ArithmeticOverflow)?;
    }
    Ok((quotient, remainder, denominator))
}

#[allow(clippy::cast_precision_loss)]
fn fraction_as_f64(remainder: i128, denominator: i128) -> f64 {
    remainder as f64 / denominator as f64
}

#[allow(clippy::cast_possible_truncation)]
fn interpolated_sample(value: f64) -> f32 {
    value as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_clock::{ClockSnapshot, ClockTime};
    use fm_frame::{ClockDomainId, MediaTimestamp, OriginalTimestamp, TimeBase};
    use fm_types::Channel;

    fn silence_timing(sequence: u64, start: u64, samples: usize) -> MediaTiming {
        let rate = SampleRate::new(48_000).unwrap();
        MediaTiming::new(
            OriginalTimestamp::new(
                MediaTimestamp::new(i64::try_from(start).unwrap()),
                TimeBase::new(1, rate.hertz()).unwrap(),
            ),
            cadence_timestamp(
                AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
                start,
                rate,
            )
            .unwrap(),
            NormalizedDuration::from_nanos(
                sample_span_duration(start, u64::try_from(samples).unwrap(), rate).unwrap(),
            )
            .unwrap(),
            ClockDomainId::new(NonZeroU128::new(1).unwrap()),
            SequenceNumber::new(sequence),
        )
        .unwrap()
    }

    #[test]
    fn silence_batch_reuses_preallocated_pcm_and_block_storage() {
        let source_domain = MappingClockDomainId::new(NonZeroU128::new(1).unwrap());
        let master_domain = MappingClockDomainId::new(NonZeroU128::new(2).unwrap());
        let mapping = ClockMapping::new(
            ClockSnapshot::new(source_domain, ClockTime::ZERO),
            ClockSnapshot::new(master_domain, ClockTime::ZERO),
            0,
        )
        .unwrap();
        let rate = SampleRate::new(48_000).unwrap();
        let mut synchronizer = ClockMappedAudioSynchronizer::new(
            rate,
            rate,
            ChannelLayout::new(vec![Channel::Mono]).unwrap(),
            mapping,
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            AudioSynchronizerLimits::new(4, 8, 8 * size_of::<f32>(), 4).unwrap(),
        )
        .unwrap();
        let pcm_pointer = synchronizer.planes[0].as_ptr();
        let pcm_capacity = synchronizer.planes[0].capacity();
        let block_capacity = synchronizer.blocks.capacity();
        let spans = [
            AudioSilenceSpan::new(silence_timing(0, 0, 2), NonZeroUsize::new(2).unwrap()),
            AudioSilenceSpan::new(silence_timing(1, 2, 2), NonZeroUsize::new(2).unwrap()),
        ];
        let interval = MasterAudioInterval::new(
            master_domain,
            NormalizedTimestamp::from_nanos(0),
            NormalizedDuration::from_nanos(sample_span_duration(0, 4, rate).unwrap()).unwrap(),
        );
        assert!(matches!(
            synchronizer.plan_render(interval, 4),
            Err(AudioSynchronizerError::NeedMoreInput {
                required_sample: 3,
                buffered_end_sample: 0,
            })
        ));

        synchronizer.push_silence_batch(&spans).unwrap();
        let plan = synchronizer.plan_render(interval, 4).unwrap();
        let mut output = [1.0; 4];
        synchronizer
            .render_planned_into(plan, &mut [&mut output])
            .unwrap();

        assert_eq!(synchronizer.planes[0].as_ptr(), pcm_pointer);
        assert_eq!(synchronizer.planes[0].capacity(), pcm_capacity);
        assert_eq!(synchronizer.blocks.capacity(), block_capacity);
        assert_eq!(synchronizer.telemetry().buffered_samples(), 4);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
        assert!(
            synchronizer.planes[0][..4]
                .iter()
                .all(|sample| sample.abs() < f32::EPSILON)
        );
    }
}
