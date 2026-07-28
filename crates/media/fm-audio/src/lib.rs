//! Deterministic, sample-accurate reference audio processing.
//!
//! This crate deliberately contains no device I/O, threads, or asynchronous
//! work. Its planar `f32` path is suitable for simulation and as a reference
//! against which later real-time backends can be tested.

use std::collections::BTreeMap;
use std::fmt;

use fm_types::{AudioFormat, ChannelLayout, FrameRate, InputId, SampleFormat, SampleRate};

mod synchronizer;

pub use synchronizer::{
    AudioCadenceOrigin, AudioRenderPlan, AudioSilenceSpan, AudioSynchronizerError,
    AudioSynchronizerLimits, AudioSynchronizerTelemetry, BufferLimit, ClockMappedAudioSynchronizer,
    MAX_SYNCHRONIZER_BLOCKS, MAX_SYNCHRONIZER_BYTES, MAX_SYNCHRONIZER_OUTPUT_SAMPLES,
    MAX_SYNCHRONIZER_SAMPLES, MasterAudioInterval, SynchronizerDiscontinuity, SynchronizerLimit,
};

/// Maximum number of channels accepted by a block or mixer.
pub const MAX_CHANNELS: usize = 32;
/// Maximum number of samples per channel accepted in one operation.
pub const MAX_SAMPLES_PER_BLOCK: usize = 48_000;
/// Maximum duration of a scheduled gain ramp, in samples.
pub const MAX_RAMP_SAMPLES: usize = 48_000;
/// Lowest non-silent gain accepted by [`Gain::from_db`].
pub const MIN_GAIN_DB: f32 = -120.0;
/// Highest gain accepted by [`Gain::from_db`].
pub const MAX_GAIN_DB: f32 = 24.0;

/// Errors returned by the bounded reference audio path.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioError {
    AudioBlock(fm_frame::AudioBlockError),
    UnsupportedSampleFormat(SampleFormat),
    ChannelCountOutOfRange(usize),
    SampleCountOutOfRange(usize),
    PlaneCountMismatch {
        expected: usize,
        actual: usize,
    },
    PlaneLengthMismatch {
        plane: usize,
        expected: usize,
        actual: usize,
    },
    NonFiniteSample {
        channel: usize,
        sample: usize,
    },
    InvalidGainDb(f32),
    InvalidLinearGain(f32),
    InvalidFrequency(f32),
    InvalidAmplitude(f32),
    ChannelIndexOutOfRange {
        channel: usize,
        channels: usize,
    },
    MappingChannelCountMismatch,
    FormatMismatch,
    SampleCountMismatch {
        expected: usize,
        actual: usize,
    },
    TimingSampleCountMismatch {
        samples: usize,
        sample_rate_hz: u32,
        duration_nanos: u64,
    },
    DuplicateInput(InputId),
    UnknownInput(InputId),
    RampTooLong(usize),
    InvalidSourceGain {
        start_numerator: u32,
        end_numerator: u32,
        denominator: u32,
    },
    CadenceBlockTooLarge(u128),
}

impl fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AudioBlock(error) => write!(formatter, "canonical audio block error: {error}"),
            Self::UnsupportedSampleFormat(format) => {
                write!(formatter, "unsupported audio sample format: {format:?}")
            }
            Self::ChannelCountOutOfRange(count) => {
                write!(
                    formatter,
                    "channel count {count} is outside 1..={MAX_CHANNELS}"
                )
            }
            Self::SampleCountOutOfRange(count) => write!(
                formatter,
                "sample count {count} exceeds the per-block limit of {MAX_SAMPLES_PER_BLOCK}"
            ),
            Self::PlaneCountMismatch { expected, actual } => {
                write!(formatter, "expected {expected} planes, received {actual}")
            }
            Self::PlaneLengthMismatch {
                plane,
                expected,
                actual,
            } => write!(
                formatter,
                "plane {plane} has {actual} samples; expected {expected}"
            ),
            Self::NonFiniteSample { channel, sample } => {
                write!(
                    formatter,
                    "sample {sample} in channel {channel} is not finite"
                )
            }
            Self::InvalidGainDb(db) => write!(formatter, "invalid gain in dB: {db}"),
            Self::InvalidLinearGain(gain) => write!(formatter, "invalid linear gain: {gain}"),
            Self::InvalidFrequency(frequency) => {
                write!(formatter, "invalid sine frequency: {frequency}")
            }
            Self::InvalidAmplitude(amplitude) => {
                write!(formatter, "invalid impulse amplitude: {amplitude}")
            }
            Self::ChannelIndexOutOfRange { channel, channels } => write!(
                formatter,
                "channel index {channel} is outside a {channels}-channel layout"
            ),
            Self::MappingChannelCountMismatch => {
                formatter.write_str("channel map does not match the configured formats")
            }
            Self::FormatMismatch => formatter.write_str("audio format or layout mismatch"),
            Self::SampleCountMismatch { expected, actual } => write!(
                formatter,
                "audio block has {actual} samples per channel; expected {expected}"
            ),
            Self::TimingSampleCountMismatch {
                samples,
                sample_rate_hz,
                duration_nanos,
            } => write!(
                formatter,
                "audio timing duration {duration_nanos} ns does not represent {samples} samples at {sample_rate_hz} Hz"
            ),
            Self::DuplicateInput(id) => write!(formatter, "duplicate audio input {id}"),
            Self::UnknownInput(id) => write!(formatter, "unknown audio input {id}"),
            Self::RampTooLong(samples) => write!(
                formatter,
                "gain ramp of {samples} samples exceeds the limit of {MAX_RAMP_SAMPLES}"
            ),
            Self::InvalidSourceGain {
                start_numerator,
                end_numerator,
                denominator,
            } => write!(
                formatter,
                "source gain {start_numerator}/{denominator}..{end_numerator}/{denominator} is outside 0.0..=1.0"
            ),
            Self::CadenceBlockTooLarge(samples) => write!(
                formatter,
                "video cadence requires up to {samples} audio samples in one block"
            ),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AudioBlock(error) => Some(error),
            _ => None,
        }
    }
}

impl From<fm_frame::AudioBlockError> for AudioError {
    fn from(value: fm_frame::AudioBlockError) -> Self {
        Self::AudioBlock(value)
    }
}

/// A validated gain value.
///
/// Silence is represented by zero linear gain and negative infinity in dB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gain(f32);

impl Gain {
    pub const UNITY: Self = Self(1.0);
    pub const SILENCE: Self = Self(0.0);

    /// Creates a gain from decibels.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidGainDb`] for NaN, positive infinity, or a
    /// finite value outside [`MIN_GAIN_DB`] through [`MAX_GAIN_DB`]. Negative
    /// infinity is accepted as silence.
    pub fn from_db(db: f32) -> Result<Self, AudioError> {
        if db == f32::NEG_INFINITY {
            return Ok(Self::SILENCE);
        }
        if !db.is_finite() || !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&db) {
            return Err(AudioError::InvalidGainDb(db));
        }
        Ok(Self(10.0_f32.powf(db / 20.0)))
    }

    /// Creates a gain from a linear multiplier.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidLinearGain`] unless the value corresponds
    /// to the accepted dB range, or is exactly zero.
    pub fn from_linear(linear: f32) -> Result<Self, AudioError> {
        if linear == 0.0 {
            return Ok(Self::SILENCE);
        }
        if !linear.is_finite() || linear < 0.0 {
            return Err(AudioError::InvalidLinearGain(linear));
        }
        let db = 20.0 * linear.log10();
        if !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&db) {
            return Err(AudioError::InvalidLinearGain(linear));
        }
        Ok(Self(linear))
    }

    #[must_use]
    pub const fn linear(self) -> f32 {
        self.0
    }

    #[must_use]
    pub fn db(self) -> f32 {
        if self.0 == 0.0 {
            f32::NEG_INFINITY
        } else {
            20.0 * self.0.log10()
        }
    }
}

/// An immutable, owned planar `f32` reference/generator audio block.
///
/// Timed media interchange uses [`fm_frame::AudioBlock`]. This legacy block is
/// retained for reference generators and existing callers.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioBlock {
    format: AudioFormat,
    planes: Vec<Vec<f32>>,
    samples: usize,
}

impl AudioBlock {
    /// Validates and owns a set of channel planes.
    ///
    /// # Errors
    ///
    /// Returns an error for non-`F32` formats, excessive dimensions,
    /// non-rectangular planes, non-finite samples, or a plane/layout mismatch.
    pub fn from_planar(format: AudioFormat, planes: Vec<Vec<f32>>) -> Result<Self, AudioError> {
        validate_format(&format)?;
        let channels = format.channels.channels().len();
        if planes.len() != channels {
            return Err(AudioError::PlaneCountMismatch {
                expected: channels,
                actual: planes.len(),
            });
        }
        let samples = planes.first().map_or(0, Vec::len);
        validate_sample_count(samples)?;
        for (channel, plane) in planes.iter().enumerate() {
            if plane.len() != samples {
                return Err(AudioError::PlaneLengthMismatch {
                    plane: channel,
                    expected: samples,
                    actual: plane.len(),
                });
            }
            if let Some(sample) = plane.iter().position(|value| !value.is_finite()) {
                return Err(AudioError::NonFiniteSample { channel, sample });
            }
        }
        Ok(Self {
            format,
            planes,
            samples,
        })
    }

    /// Creates a zero-filled block.
    ///
    /// # Errors
    ///
    /// Returns an error when the format or requested block size is unsupported.
    pub fn silence(format: AudioFormat, samples: usize) -> Result<Self, AudioError> {
        validate_format(&format)?;
        validate_sample_count(samples)?;
        let channels = format.channels.channels().len();
        Ok(Self {
            format,
            planes: vec![vec![0.0; samples]; channels],
            samples,
        })
    }

    #[must_use]
    pub const fn format(&self) -> &AudioFormat {
        &self.format
    }

    #[must_use]
    pub const fn samples(&self) -> usize {
        self.samples
    }

    #[must_use]
    pub fn channels(&self) -> usize {
        self.planes.len()
    }

    #[must_use]
    pub fn planes(&self) -> &[Vec<f32>] {
        &self.planes
    }

    #[must_use]
    pub fn plane(&self, channel: usize) -> Option<&[f32]> {
        self.planes.get(channel).map(Vec::as_slice)
    }
}

/// Deterministic source of planar blocks.
pub trait AudioGenerator {
    /// Generates the next contiguous block.
    ///
    /// # Errors
    ///
    /// Returns an error when `samples` exceeds [`MAX_SAMPLES_PER_BLOCK`].
    fn generate(&mut self, samples: usize) -> Result<AudioBlock, AudioError>;
}

/// A deterministic silence source.
#[derive(Clone, Debug)]
pub struct SilenceGenerator {
    format: AudioFormat,
}

impl SilenceGenerator {
    /// Creates a silence generator.
    ///
    /// # Errors
    ///
    /// Returns an error when `format` is not a supported planar float format.
    pub fn new(format: AudioFormat) -> Result<Self, AudioError> {
        validate_format(&format)?;
        Ok(Self { format })
    }
}

impl AudioGenerator for SilenceGenerator {
    fn generate(&mut self, samples: usize) -> Result<AudioBlock, AudioError> {
        AudioBlock::silence(self.format.clone(), samples)
    }
}

/// A phase-continuous deterministic sine source.
#[derive(Clone, Debug)]
pub struct SineGenerator {
    format: AudioFormat,
    frequency_hz: f32,
    amplitude: Gain,
    phase_radians: f32,
    sample_cursor: u64,
}

impl SineGenerator {
    /// Creates a sine generator whose channels carry identical samples.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported format or a frequency outside zero
    /// through the inclusive Nyquist frequency.
    #[allow(clippy::cast_precision_loss)]
    pub fn new(
        format: AudioFormat,
        frequency_hz: f32,
        amplitude: Gain,
    ) -> Result<Self, AudioError> {
        validate_format(&format)?;
        let nyquist = format.sample_rate.hertz() as f32 / 2.0;
        if !frequency_hz.is_finite() || !(0.0..=nyquist).contains(&frequency_hz) {
            return Err(AudioError::InvalidFrequency(frequency_hz));
        }
        Ok(Self {
            format,
            frequency_hz,
            amplitude,
            phase_radians: 0.0,
            sample_cursor: 0,
        })
    }

    pub fn reset(&mut self) {
        self.phase_radians = 0.0;
        self.sample_cursor = 0;
    }

    #[must_use]
    pub const fn sample_cursor(&self) -> u64 {
        self.sample_cursor
    }
}

impl AudioGenerator for SineGenerator {
    #[allow(clippy::cast_precision_loss)]
    fn generate(&mut self, samples: usize) -> Result<AudioBlock, AudioError> {
        validate_sample_count(samples)?;
        let channels = self.format.channels.channels().len();
        let mut plane = Vec::with_capacity(samples);
        let phase_increment =
            std::f32::consts::TAU * self.frequency_hz / self.format.sample_rate.hertz() as f32;
        for _ in 0..samples {
            plane.push(self.phase_radians.sin() * self.amplitude.linear());
            self.phase_radians =
                (self.phase_radians + phase_increment).rem_euclid(std::f32::consts::TAU);
        }
        self.sample_cursor = self.sample_cursor.saturating_add(samples as u64);
        AudioBlock::from_planar(self.format.clone(), vec![plane; channels])
    }
}

/// A deterministic one-shot impulse source.
#[derive(Clone, Debug)]
pub struct ImpulseGenerator {
    format: AudioFormat,
    channel: usize,
    impulse_sample: u64,
    amplitude: f32,
    sample_cursor: u64,
}

impl ImpulseGenerator {
    /// Creates a source with one impulse at an absolute sample offset.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported format, channel, or non-finite
    /// amplitude.
    pub fn new(
        format: AudioFormat,
        channel: usize,
        impulse_sample: u64,
        amplitude: f32,
    ) -> Result<Self, AudioError> {
        validate_format(&format)?;
        let channels = format.channels.channels().len();
        if channel >= channels {
            return Err(AudioError::ChannelIndexOutOfRange { channel, channels });
        }
        if !amplitude.is_finite() {
            return Err(AudioError::InvalidAmplitude(amplitude));
        }
        Ok(Self {
            format,
            channel,
            impulse_sample,
            amplitude,
            sample_cursor: 0,
        })
    }

    pub fn reset(&mut self) {
        self.sample_cursor = 0;
    }
}

impl AudioGenerator for ImpulseGenerator {
    fn generate(&mut self, samples: usize) -> Result<AudioBlock, AudioError> {
        let mut block = AudioBlock::silence(self.format.clone(), samples)?;
        let end = self.sample_cursor.saturating_add(samples as u64);
        if (self.sample_cursor..end).contains(&self.impulse_sample) {
            let offset = usize::try_from(self.impulse_sample - self.sample_cursor)
                .expect("impulse offset is bounded by the block size");
            block.planes[self.channel][offset] = self.amplitude;
        }
        self.sample_cursor = end;
        Ok(block)
    }
}

/// One source-to-destination channel route.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelRoute {
    pub source: usize,
    pub destination: usize,
    pub gain: Gain,
}

/// A validated channel routing matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelMap {
    source_channels: usize,
    destination_channels: usize,
    routes: Vec<ChannelRoute>,
}

impl ChannelMap {
    /// Creates a channel map. Multiple routes may feed the same destination.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported channel counts or an invalid route.
    pub fn new(
        source_channels: usize,
        destination_channels: usize,
        routes: Vec<ChannelRoute>,
    ) -> Result<Self, AudioError> {
        validate_channel_count(source_channels)?;
        validate_channel_count(destination_channels)?;
        for route in &routes {
            if route.source >= source_channels {
                return Err(AudioError::ChannelIndexOutOfRange {
                    channel: route.source,
                    channels: source_channels,
                });
            }
            if route.destination >= destination_channels {
                return Err(AudioError::ChannelIndexOutOfRange {
                    channel: route.destination,
                    channels: destination_channels,
                });
            }
        }
        Ok(Self {
            source_channels,
            destination_channels,
            routes,
        })
    }

    /// Creates an index-preserving map.
    ///
    /// # Errors
    ///
    /// Returns an error when `channels` is outside the supported range.
    pub fn identity(channels: usize) -> Result<Self, AudioError> {
        let routes = (0..channels)
            .map(|channel| ChannelRoute {
                source: channel,
                destination: channel,
                gain: Gain::UNITY,
            })
            .collect();
        Self::new(channels, channels, routes)
    }

    /// Maps channels with matching semantic labels.
    ///
    /// A destination without a matching source remains silent.
    ///
    /// # Errors
    ///
    /// Returns an error when either layout exceeds the supported channel count.
    pub fn matching_labels(
        source: &ChannelLayout,
        destination: &ChannelLayout,
    ) -> Result<Self, AudioError> {
        let source_channels = source.channels().len();
        let destination_channels = destination.channels().len();
        let mut routes = Vec::new();
        for (destination_index, destination_channel) in destination.channels().iter().enumerate() {
            if let Some(source_index) = source
                .channels()
                .iter()
                .position(|source_channel| source_channel == destination_channel)
            {
                routes.push(ChannelRoute {
                    source: source_index,
                    destination: destination_index,
                    gain: Gain::UNITY,
                });
            }
        }
        Self::new(source_channels, destination_channels, routes)
    }

    #[must_use]
    pub fn routes(&self) -> &[ChannelRoute] {
        &self.routes
    }
}

/// User-visible state of one input strip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputState {
    pub gain: Gain,
    pub muted: bool,
    pub follow_video: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            gain: Gain::UNITY,
            muted: false,
            follow_video: false,
        }
    }
}

/// Exact rational source gain endpoints for one Master sample interval.
///
/// The first rendered sample advances one of `samples` equal steps from the
/// start endpoint and the final sample is exactly the end endpoint. This is
/// the same endpoint convention used by input-strip gain ramps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceGain {
    start_numerator: u32,
    end_numerator: u32,
    denominator: u32,
}

impl SourceGain {
    pub const UNITY: Self = Self {
        start_numerator: 1,
        end_numerator: 1,
        denominator: 1,
    };

    /// Creates a linear source gain from exact rational endpoints.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidSourceGain`] for a zero denominator or an
    /// endpoint greater than one.
    pub const fn new(
        start_numerator: u32,
        end_numerator: u32,
        denominator: u32,
    ) -> Result<Self, AudioError> {
        if denominator == 0 || start_numerator > denominator || end_numerator > denominator {
            return Err(AudioError::InvalidSourceGain {
                start_numerator,
                end_numerator,
                denominator,
            });
        }
        Ok(Self {
            start_numerator,
            end_numerator,
            denominator,
        })
    }

    #[must_use]
    pub const fn start_numerator(self) -> u32 {
        self.start_numerator
    }

    #[must_use]
    pub const fn end_numerator(self) -> u32 {
        self.end_numerator
    }

    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.denominator
    }

    #[allow(clippy::cast_possible_truncation)]
    fn at_sample(self, sample: usize, samples: usize) -> f32 {
        let denominator = f64::from(self.denominator);
        let start = f64::from(self.start_numerator) / denominator;
        let end = f64::from(self.end_numerator) / denominator;
        let step = f64::from(u32::try_from(sample + 1).expect("sample count is bounded"));
        let steps = f64::from(u32::try_from(samples).expect("sample count is bounded"));
        (start + (end - start) * (step / steps)) as f32
    }
}

#[derive(Clone, Copy, Debug)]
struct GainRamp {
    current: f32,
    target: f32,
    remaining: u16,
}

impl GainRamp {
    const fn immediate(gain: Gain) -> Self {
        Self {
            current: gain.linear(),
            target: gain.linear(),
            remaining: 0,
        }
    }

    fn set(&mut self, target: Gain, samples: usize) {
        self.target = target.linear();
        self.remaining = u16::try_from(samples).expect("validated ramp length fits in u16");
        if samples == 0 {
            self.current = self.target;
        }
    }

    fn next(&mut self) -> f32 {
        if self.remaining != 0 {
            self.current += (self.target - self.current) / f32::from(self.remaining);
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }
}

#[derive(Clone, Debug)]
struct InputStrip {
    format: AudioFormat,
    map: ChannelMap,
    state: InputState,
    ramp: GainRamp,
}

/// Master summing behavior when a sample exceeds the normalized range.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClippingPolicy {
    /// Preserve the mathematical sum, including values outside `-1.0..=1.0`.
    Allow,
    /// Hard-clip the Master output to `-1.0..=1.0`.
    #[default]
    Clamp,
}

/// Peak and RMS values for one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChannelMeter {
    pub peak: f32,
    pub rms: f32,
}

/// Meter readings in channel-layout order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeterReadings {
    channels: Vec<ChannelMeter>,
}

impl MeterReadings {
    #[must_use]
    pub fn channels(&self) -> &[ChannelMeter] {
        &self.channels
    }
}

/// Result of one Master bus render.
#[derive(Clone, Debug, PartialEq)]
pub struct MasterOutput {
    pub block: AudioBlock,
    pub meters: MeterReadings,
}

/// Result of one timed Master bus render using the canonical media block.
#[derive(Clone, Debug, PartialEq)]
pub struct TimedMasterOutput {
    pub block: fm_frame::AudioBlock,
    pub meters: MeterReadings,
}

/// One borrowed planar source for allocation-free Master mixing.
#[derive(Clone, Copy, Debug)]
pub struct PlanarAudioSource<'a> {
    pub input: InputId,
    pub sample_rate: SampleRate,
    pub channel_layout: &'a ChannelLayout,
    pub planes: &'a [Vec<f32>],
    pub samples: usize,
    pub source_gain: SourceGain,
}

trait MixerBlockView {
    fn sample_rate(&self) -> SampleRate;
    fn channel_layout(&self) -> &ChannelLayout;
    fn sample_count(&self) -> usize;
    fn planes(&self) -> &[Vec<f32>];
}

trait MixerSubmission<B> {
    fn input(&self) -> InputId;
    fn block(&self) -> &B;
    fn source_gain(&self) -> SourceGain;
}

impl<B> MixerSubmission<B> for (InputId, &B) {
    fn input(&self) -> InputId {
        self.0
    }

    fn block(&self) -> &B {
        self.1
    }

    fn source_gain(&self) -> SourceGain {
        SourceGain::UNITY
    }
}

impl<B> MixerSubmission<B> for (InputId, &B, SourceGain) {
    fn input(&self) -> InputId {
        self.0
    }

    fn block(&self) -> &B {
        self.1
    }

    fn source_gain(&self) -> SourceGain {
        self.2
    }
}

impl MixerBlockView for AudioBlock {
    fn sample_rate(&self) -> SampleRate {
        self.format.sample_rate
    }

    fn channel_layout(&self) -> &ChannelLayout {
        &self.format.channels
    }

    fn sample_count(&self) -> usize {
        self.samples
    }

    fn planes(&self) -> &[Vec<f32>] {
        &self.planes
    }
}

impl MixerBlockView for fm_frame::AudioBlock {
    fn sample_rate(&self) -> SampleRate {
        self.sample_rate()
    }

    fn channel_layout(&self) -> &ChannelLayout {
        self.channel_layout()
    }

    fn sample_count(&self) -> usize {
        self.sample_count()
    }

    fn planes(&self) -> &[Vec<f32>] {
        self.planes()
    }
}

impl MixerBlockView for PlanarAudioSource<'_> {
    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn channel_layout(&self) -> &ChannelLayout {
        self.channel_layout
    }

    fn sample_count(&self) -> usize {
        self.samples
    }

    fn planes(&self) -> &[Vec<f32>] {
        self.planes
    }
}

impl<'a> MixerSubmission<PlanarAudioSource<'a>> for PlanarAudioSource<'a> {
    fn input(&self) -> InputId {
        self.input
    }

    fn block(&self) -> &PlanarAudioSource<'a> {
        self
    }

    fn source_gain(&self) -> SourceGain {
        self.source_gain
    }
}

struct MixedMaster {
    planes: Vec<Vec<f32>>,
    meters: MeterReadings,
}

/// Deterministic planar float mixer with one Master bus.
#[derive(Clone, Debug)]
pub struct MasterMixer {
    format: AudioFormat,
    inputs: BTreeMap<InputId, InputStrip>,
    clipping_policy: ClippingPolicy,
}

impl MasterMixer {
    /// Creates a Master bus.
    ///
    /// # Errors
    ///
    /// Returns an error unless `format` is bounded planar `F32`.
    pub fn new(format: AudioFormat) -> Result<Self, AudioError> {
        validate_format(&format)?;
        Ok(Self {
            format,
            inputs: BTreeMap::new(),
            clipping_policy: ClippingPolicy::default(),
        })
    }

    #[must_use]
    pub const fn format(&self) -> &AudioFormat {
        &self.format
    }

    #[must_use]
    pub const fn clipping_policy(&self) -> ClippingPolicy {
        self.clipping_policy
    }

    pub fn set_clipping_policy(&mut self, policy: ClippingPolicy) {
        self.clipping_policy = policy;
    }

    /// Adds one configured input atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate ID, format incompatibility, or channel
    /// map dimension mismatch. The mixer is unchanged on error.
    pub fn add_input(
        &mut self,
        id: InputId,
        format: AudioFormat,
        map: ChannelMap,
        state: InputState,
    ) -> Result<(), AudioError> {
        if self.inputs.contains_key(&id) {
            return Err(AudioError::DuplicateInput(id));
        }
        validate_format(&format)?;
        if format.sample_rate != self.format.sample_rate {
            return Err(AudioError::FormatMismatch);
        }
        if map.source_channels != format.channels.channels().len()
            || map.destination_channels != self.format.channels.channels().len()
        {
            return Err(AudioError::MappingChannelCountMismatch);
        }
        let ramp = GainRamp::immediate(state.gain);
        self.inputs.insert(
            id,
            InputStrip {
                format,
                map,
                state,
                ramp,
            },
        );
        Ok(())
    }

    /// Removes an input.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownInput`] when `id` is not configured.
    pub fn remove_input(&mut self, id: InputId) -> Result<(), AudioError> {
        self.inputs
            .remove(&id)
            .map(|_| ())
            .ok_or(AudioError::UnknownInput(id))
    }

    #[must_use]
    pub fn input_state(&self, id: InputId) -> Option<InputState> {
        self.inputs.get(&id).map(|strip| strip.state)
    }

    #[must_use]
    pub fn current_linear_gain(&self, id: InputId) -> Option<f32> {
        self.inputs.get(&id).map(|strip| strip.ramp.current)
    }

    /// Replaces input state and schedules a linear gain ramp.
    ///
    /// Mute and follow-video changes take effect at the next rendered sample.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown input or excessive ramp. No state is
    /// changed on error.
    pub fn set_input_state(
        &mut self,
        id: InputId,
        state: InputState,
        ramp_samples: usize,
    ) -> Result<(), AudioError> {
        if ramp_samples > MAX_RAMP_SAMPLES {
            return Err(AudioError::RampTooLong(ramp_samples));
        }
        let strip = self
            .inputs
            .get_mut(&id)
            .ok_or(AudioError::UnknownInput(id))?;
        strip.ramp.set(state.gain, ramp_samples);
        strip.state = state;
        Ok(())
    }

    /// Copies mutable strip and clipping state from an identically configured mixer.
    ///
    /// This performs no allocation and is intended for reusable transactional
    /// mixer pairs.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::FormatMismatch`] unless both mixers have identical
    /// formats and configured input strips.
    pub fn copy_runtime_state_from(&mut self, other: &Self) -> Result<(), AudioError> {
        if self.format != other.format || self.inputs.len() != other.inputs.len() {
            return Err(AudioError::FormatMismatch);
        }
        for (id, strip) in &mut self.inputs {
            let source = other.inputs.get(id).ok_or(AudioError::FormatMismatch)?;
            if strip.format != source.format || strip.map != source.map {
                return Err(AudioError::FormatMismatch);
            }
            strip.state = source.state;
            strip.ramp = source.ramp;
        }
        self.clipping_policy = other.clipping_policy;
        Ok(())
    }

    /// Renders submitted blocks into the Master bus.
    ///
    /// Inputs not submitted for this call contribute silence. Every submitted
    /// block is validated before output or ramp state is changed, so all errors
    /// are transactional.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive sample count, duplicate/unknown inputs,
    /// or any block whose exact format or sample count does not match.
    pub fn mix(
        &mut self,
        samples: usize,
        blocks: &[(InputId, &AudioBlock)],
        active_video_input: Option<InputId>,
    ) -> Result<MasterOutput, AudioError> {
        let active_video_inputs = active_video_input.as_slice();
        let mixed = self.mix_block_views(samples, blocks, active_video_inputs)?;
        let block = AudioBlock::from_planar(self.format.clone(), mixed.planes)?;
        Ok(MasterOutput {
            block,
            meters: mixed.meters,
        })
    }

    /// Renders canonical timed blocks into the Master bus.
    ///
    /// Input timing is not compared with `output_timing`; callers select or
    /// slice inputs for the requested interval. The output carries
    /// `output_timing` unchanged. Inputs not submitted contribute silence.
    /// Every submission is validated before output allocation or ramp state is
    /// changed, so validation errors are transactional.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive sample count, duplicate/unknown inputs,
    /// a sample-rate, channel-layout, or sample-count mismatch, non-finite
    /// samples, or failure to construct the canonical output block.
    pub fn mix_timed(
        &mut self,
        output_timing: fm_frame::MediaTiming,
        samples: usize,
        blocks: &[(InputId, &fm_frame::AudioBlock)],
        active_video_input: Option<InputId>,
    ) -> Result<TimedMasterOutput, AudioError> {
        let active_video_inputs = active_video_input.as_slice();
        validate_sample_count(samples)?;
        validate_canonical_output(&self.format, samples)?;
        validate_timed_duration(output_timing, self.format.sample_rate, samples)?;
        let mixed = self.mix_block_views(samples, blocks, active_video_inputs)?;
        self.timed_output(output_timing, mixed)
    }

    /// Renders timed blocks with independent linear source envelopes.
    ///
    /// Source gain is multiplied by the persistent input-strip and channel-map
    /// gains. Every ID in `active_video_inputs` satisfies follow-video for this
    /// interval. All submissions are validated before output or strip-ramp
    /// state changes, so failures are transactional.
    ///
    /// # Errors
    ///
    /// Returns the same validation and construction errors as
    /// [`Self::mix_timed`].
    pub fn mix_timed_with_source_gains(
        &mut self,
        output_timing: fm_frame::MediaTiming,
        samples: usize,
        blocks: &[(InputId, &fm_frame::AudioBlock, SourceGain)],
        active_video_inputs: &[InputId],
    ) -> Result<TimedMasterOutput, AudioError> {
        validate_sample_count(samples)?;
        validate_canonical_output(&self.format, samples)?;
        validate_timed_duration(output_timing, self.format.sample_rate, samples)?;
        let mixed = self.mix_block_views(samples, blocks, active_video_inputs)?;
        self.timed_output(output_timing, mixed)
    }

    /// Mixes borrowed planar sources into caller-owned preallocated planes.
    ///
    /// Only the first `samples` values of each source and output plane are used.
    /// Validation completes before output or strip-ramp state changes.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::mix_timed_with_source_gains`]
    /// plus output plane shape errors.
    pub fn mix_planar_timed_into(
        &mut self,
        output_timing: fm_frame::MediaTiming,
        samples: usize,
        sources: &[PlanarAudioSource<'_>],
        active_video_inputs: &[InputId],
        output: &mut [Vec<f32>],
    ) -> Result<(), AudioError> {
        validate_sample_count(samples)?;
        validate_canonical_output(&self.format, samples)?;
        validate_timed_duration(output_timing, self.format.sample_rate, samples)?;
        self.mix_block_views_into(samples, sources, active_video_inputs, output, false)
            .map(drop)
    }

    fn timed_output(
        &self,
        output_timing: fm_frame::MediaTiming,
        mixed: MixedMaster,
    ) -> Result<TimedMasterOutput, AudioError> {
        let block = fm_frame::AudioBlock::new(
            output_timing,
            self.format.sample_rate,
            self.format.channels.clone(),
            mixed.planes,
        )?;
        Ok(TimedMasterOutput {
            block,
            meters: mixed.meters,
        })
    }

    fn mix_block_views<B: MixerBlockView, S: MixerSubmission<B>>(
        &mut self,
        samples: usize,
        blocks: &[S],
        active_video_inputs: &[InputId],
    ) -> Result<MixedMaster, AudioError> {
        let channels = self.format.channels.channels().len();
        let mut output = vec![vec![0.0; samples]; channels];
        let meters = self
            .mix_block_views_into(samples, blocks, active_video_inputs, &mut output, true)?
            .expect("metering was requested");
        Ok(MixedMaster {
            planes: output,
            meters,
        })
    }

    #[allow(clippy::needless_range_loop)]
    fn mix_block_views_into<B: MixerBlockView, S: MixerSubmission<B>>(
        &mut self,
        samples: usize,
        blocks: &[S],
        active_video_inputs: &[InputId],
        output: &mut [Vec<f32>],
        measure_output: bool,
    ) -> Result<Option<MeterReadings>, AudioError> {
        validate_sample_count(samples)?;
        for (index, submission) in blocks.iter().enumerate() {
            let id = submission.input();
            let block = submission.block();
            if blocks[..index]
                .iter()
                .any(|previous| previous.input() == id)
            {
                return Err(AudioError::DuplicateInput(id));
            }
            let strip = self.inputs.get(&id).ok_or(AudioError::UnknownInput(id))?;
            if block.sample_rate() != strip.format.sample_rate
                || block.channel_layout() != &strip.format.channels
            {
                return Err(AudioError::FormatMismatch);
            }
            if block.sample_count() != samples {
                return Err(AudioError::SampleCountMismatch {
                    expected: samples,
                    actual: block.sample_count(),
                });
            }
            validate_finite_sample_prefix(block.planes(), samples)?;
        }

        let channels = self.format.channels.channels().len();
        if output.len() != channels {
            return Err(AudioError::PlaneCountMismatch {
                expected: channels,
                actual: output.len(),
            });
        }
        for (plane, values) in output.iter().enumerate() {
            if values.len() < samples {
                return Err(AudioError::PlaneLengthMismatch {
                    plane,
                    expected: samples,
                    actual: values.len(),
                });
            }
        }
        for plane in output.iter_mut() {
            plane[..samples].fill(0.0);
        }
        for (id, strip) in &self.inputs {
            let block = blocks.iter().find_map(|submission| {
                (submission.input() == *id).then(|| (submission.block(), submission.source_gain()))
            });
            let audible = !strip.state.muted
                && (!strip.state.follow_video || active_video_inputs.contains(id));
            let mut ramp = strip.ramp;
            if let Some((block, source_gain)) = block
                && audible
            {
                for sample in 0..samples {
                    let strip_gain = ramp.next();
                    let source_gain = source_gain.at_sample(sample, samples);
                    for route in &strip.map.routes {
                        output[route.destination][sample] += block.planes()[route.source][sample]
                            * strip_gain
                            * source_gain
                            * route.gain.linear();
                    }
                }
            } else {
                for _ in 0..samples {
                    ramp.next();
                }
            }
        }

        if self.clipping_policy == ClippingPolicy::Clamp {
            for plane in output.iter_mut() {
                for sample in &mut plane[..samples] {
                    *sample = sample.clamp(-1.0, 1.0);
                }
            }
        }
        validate_finite_sample_prefix(output, samples)?;
        let meters = measure_output.then(|| measure_plane_prefix(output, samples));
        for strip in self.inputs.values_mut() {
            for _ in 0..samples {
                strip.ramp.next();
            }
        }
        Ok(meters)
    }
}

/// Measures sample peak and unweighted RMS independently for each channel.
#[must_use]
pub fn measure(block: &AudioBlock) -> MeterReadings {
    measure_planes(&block.planes)
}

#[allow(clippy::cast_possible_truncation)]
fn measure_planes(planes: &[Vec<f32>]) -> MeterReadings {
    let channels = planes
        .iter()
        .map(|plane| {
            if plane.is_empty() {
                return ChannelMeter::default();
            }
            let mut peak = 0.0_f32;
            let mut squares = 0.0_f64;
            for sample in plane {
                peak = peak.max(sample.abs());
                squares += f64::from(*sample) * f64::from(*sample);
            }
            let sample_count = f64::from(
                u32::try_from(plane.len()).expect("audio plane length is bounded below u32::MAX"),
            );
            ChannelMeter {
                peak,
                rms: (squares / sample_count).sqrt() as f32,
            }
        })
        .collect();
    MeterReadings { channels }
}

/// Allocates integer audio samples against rational video frames without drift.
///
/// The allocation after `n` frames is exactly
/// `floor(n * sample_rate * frame_rate.denominator / frame_rate.numerator)`.
#[derive(Clone, Debug)]
pub struct FrameSampleAllocator {
    samples_per_frame_numerator: u128,
    frame_rate_numerator: u128,
    remainder: u128,
    allocated_samples: u128,
    allocated_frames: u64,
}

impl FrameSampleAllocator {
    /// Creates a cumulative rational sample allocator.
    ///
    /// # Errors
    ///
    /// Returns an error if any individual frame could exceed the block bound.
    pub fn new(sample_rate: SampleRate, frame_rate: FrameRate) -> Result<Self, AudioError> {
        let samples_per_frame_numerator =
            u128::from(sample_rate.hertz()) * u128::from(frame_rate.denominator());
        let frame_rate_numerator = u128::from(frame_rate.numerator());
        let maximum = samples_per_frame_numerator.div_ceil(frame_rate_numerator);
        if maximum > MAX_SAMPLES_PER_BLOCK as u128 {
            return Err(AudioError::CadenceBlockTooLarge(maximum));
        }
        Ok(Self {
            samples_per_frame_numerator,
            frame_rate_numerator,
            remainder: 0,
            allocated_samples: 0,
            allocated_frames: 0,
        })
    }

    /// Returns the exact sample allocation for the next video frame.
    pub fn next_samples(&mut self) -> usize {
        self.remainder += self.samples_per_frame_numerator;
        let samples = self.remainder / self.frame_rate_numerator;
        self.remainder %= self.frame_rate_numerator;
        self.allocated_samples += samples;
        self.allocated_frames = self.allocated_frames.saturating_add(1);
        samples as usize
    }

    #[must_use]
    pub const fn allocated_samples(&self) -> u128 {
        self.allocated_samples
    }

    #[must_use]
    pub const fn allocated_frames(&self) -> u64 {
        self.allocated_frames
    }

    pub fn reset(&mut self) {
        self.remainder = 0;
        self.allocated_samples = 0;
        self.allocated_frames = 0;
    }
}

fn validate_format(format: &AudioFormat) -> Result<(), AudioError> {
    if format.sample_format != SampleFormat::F32 {
        return Err(AudioError::UnsupportedSampleFormat(format.sample_format));
    }
    validate_channel_count(format.channels.channels().len())
}

fn validate_channel_count(channels: usize) -> Result<(), AudioError> {
    if !(1..=MAX_CHANNELS).contains(&channels) {
        return Err(AudioError::ChannelCountOutOfRange(channels));
    }
    Ok(())
}

fn validate_sample_count(samples: usize) -> Result<(), AudioError> {
    if samples > MAX_SAMPLES_PER_BLOCK {
        return Err(AudioError::SampleCountOutOfRange(samples));
    }
    Ok(())
}

fn validate_finite_sample_prefix(planes: &[Vec<f32>], samples: usize) -> Result<(), AudioError> {
    for (channel, plane) in planes.iter().enumerate() {
        if plane.len() < samples {
            return Err(AudioError::PlaneLengthMismatch {
                plane: channel,
                expected: samples,
                actual: plane.len(),
            });
        }
        if let Some(sample) = plane[..samples].iter().position(|value| !value.is_finite()) {
            return Err(AudioError::NonFiniteSample { channel, sample });
        }
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn measure_plane_prefix(planes: &[Vec<f32>], samples: usize) -> MeterReadings {
    let channels = planes
        .iter()
        .map(|plane| {
            if samples == 0 {
                return ChannelMeter::default();
            }
            let mut peak = 0.0_f32;
            let mut squares = 0.0_f64;
            for sample in &plane[..samples] {
                peak = peak.max(sample.abs());
                squares += f64::from(*sample) * f64::from(*sample);
            }
            let sample_count = f64::from(
                u32::try_from(samples).expect("audio plane length is bounded below u32::MAX"),
            );
            ChannelMeter {
                peak,
                rms: (squares / sample_count).sqrt() as f32,
            }
        })
        .collect();
    MeterReadings { channels }
}

fn validate_canonical_output(format: &AudioFormat, samples: usize) -> Result<(), AudioError> {
    if format.sample_rate.hertz() > fm_frame::AudioBlock::MAX_SAMPLE_RATE_HZ {
        return Err(fm_frame::AudioBlockError::SampleRateTooHigh {
            actual: format.sample_rate.hertz(),
            maximum: fm_frame::AudioBlock::MAX_SAMPLE_RATE_HZ,
        }
        .into());
    }
    let channels = format.channels.channels();
    if channels
        .iter()
        .enumerate()
        .any(|(index, channel)| channels[index + 1..].contains(channel))
    {
        return Err(fm_frame::AudioBlockError::DuplicateChannel.into());
    }
    if samples == 0 {
        return Err(fm_frame::AudioBlockError::ZeroSamples.into());
    }
    Ok(())
}

fn validate_timed_duration(
    timing: fm_frame::MediaTiming,
    sample_rate: SampleRate,
    samples: usize,
) -> Result<(), AudioError> {
    let numerator = (samples as u128) * 1_000_000_000_u128;
    let denominator = u128::from(sample_rate.hertz());
    let floor = numerator / denominator;
    let ceil = numerator.div_ceil(denominator);
    let duration = u128::from(timing.duration().as_nanos());
    if duration == floor || duration == ceil {
        return Ok(());
    }
    Err(AudioError::TimingSampleCountMismatch {
        samples,
        sample_rate_hz: sample_rate.hertz(),
        duration_nanos: timing.duration().as_nanos(),
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU128;

    use fm_frame::{
        ClockDomainId, MediaFlags, MediaTiming, NormalizedDuration, NormalizedTimestamp,
        OriginalTimestamp, SequenceNumber,
    };
    use fm_types::{Channel, ChannelLayout};
    use fm_types::{MediaTimestamp, TimeBase};

    use super::*;

    fn stereo_format() -> AudioFormat {
        AudioFormat {
            sample_rate: SampleRate::new(48_000).unwrap(),
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::stereo(),
        }
    }

    fn mono_format() -> AudioFormat {
        AudioFormat {
            sample_rate: SampleRate::new(48_000).unwrap(),
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        }
    }

    fn input_id(value: u128) -> InputId {
        InputId::new(NonZeroU128::new(value).unwrap())
    }

    fn timing(sequence: u64) -> MediaTiming {
        MediaTiming::new(
            OriginalTimestamp::new(
                MediaTimestamp::new(i64::try_from(sequence).unwrap()),
                TimeBase::new(1, 48_000).unwrap(),
            ),
            NormalizedTimestamp::from_nanos(i64::try_from(sequence).unwrap() * 1_000_000),
            NormalizedDuration::from_nanos(1_000_000).unwrap(),
            ClockDomainId::new(NonZeroU128::new(7).unwrap()),
            SequenceNumber::new(sequence),
        )
        .unwrap()
    }

    fn timing_for_samples(sequence: u64, samples: usize) -> MediaTiming {
        let samples = u64::try_from(samples).unwrap();
        let start_sample = sequence.checked_mul(samples).unwrap();
        let end_sample = start_sample.checked_add(samples).unwrap();
        let start_nanos = start_sample.checked_mul(1_000_000_000).unwrap() / 48_000;
        let end_nanos = end_sample.checked_mul(1_000_000_000).unwrap() / 48_000;
        MediaTiming::new(
            OriginalTimestamp::new(
                MediaTimestamp::new(i64::try_from(start_sample).unwrap()),
                TimeBase::new(1, 48_000).unwrap(),
            ),
            NormalizedTimestamp::from_nanos(i64::try_from(start_nanos).unwrap()),
            NormalizedDuration::from_nanos(end_nanos - start_nanos).unwrap(),
            ClockDomainId::new(NonZeroU128::new(7).unwrap()),
            SequenceNumber::new(sequence),
        )
        .unwrap()
    }

    fn canonical_block(
        timing: MediaTiming,
        format: &AudioFormat,
        planes: Vec<Vec<f32>>,
    ) -> fm_frame::AudioBlock {
        fm_frame::AudioBlock::new(timing, format.sample_rate, format.channels.clone(), planes)
            .unwrap()
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
    }

    #[test]
    fn gain_conversion_is_validated() {
        let gain = Gain::from_db(-6.0).unwrap();
        assert_close(gain.linear(), 0.501_187_2);
        assert_close(gain.db(), -6.0);
        let silence_db = Gain::from_linear(0.0).unwrap().db();
        assert!(silence_db.is_infinite() && silence_db.is_sign_negative());
        assert!(Gain::from_db(f32::NAN).is_err());
        assert!(Gain::from_db(25.0).is_err());
        assert!(Gain::from_linear(-1.0).is_err());
    }

    #[test]
    fn silence_sine_and_impulse_are_deterministic() {
        let format = stereo_format();
        let mut silence = SilenceGenerator::new(format.clone()).unwrap();
        let silent = silence.generate(8).unwrap();
        assert!(
            silent
                .planes()
                .iter()
                .flatten()
                .all(|sample| *sample == 0.0)
        );

        let mut whole = SineGenerator::new(format.clone(), 1_000.0, Gain::UNITY).unwrap();
        let expected = whole.generate(16).unwrap();
        let mut split = SineGenerator::new(format.clone(), 1_000.0, Gain::UNITY).unwrap();
        let first = split.generate(7).unwrap();
        let second = split.generate(9).unwrap();
        let combined: Vec<_> = first
            .plane(0)
            .unwrap()
            .iter()
            .chain(second.plane(0).unwrap())
            .copied()
            .collect();
        assert_eq!(combined, expected.plane(0).unwrap());
        assert_eq!(expected.plane(0), expected.plane(1));

        let mut impulse = ImpulseGenerator::new(format, 1, 5, -0.75).unwrap();
        let first = impulse.generate(3).unwrap();
        let second = impulse.generate(5).unwrap();
        assert!(first.planes().iter().flatten().all(|sample| *sample == 0.0));
        assert_eq!(second.plane(1).unwrap(), &[0.0, 0.0, -0.75, 0.0, 0.0]);
        assert!(second.plane(0).unwrap().iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn cadence_accumulates_exactly_for_1001_frames() {
        let mut allocator = FrameSampleAllocator::new(
            SampleRate::new(48_000).unwrap(),
            FrameRate::new(60_000, 1_001).unwrap(),
        )
        .unwrap();
        let allocations: Vec<_> = (0..1_001).map(|_| allocator.next_samples()).collect();
        assert!(
            allocations
                .iter()
                .all(|samples| matches!(samples, 800 | 801))
        );
        assert_eq!(allocations.iter().sum::<usize>(), 801_600);
        assert_eq!(allocator.allocated_samples(), 801_600);
        assert_eq!(allocator.allocated_frames(), 1_001);
    }

    #[test]
    fn timed_mix_preserves_caller_timing_and_accepts_ffmpeg_shaped_stereo() {
        let format = stereo_format();
        let id = input_id(1);
        let input_timing = timing(1);
        let output_timing = timing_for_samples(42, 2).with_flags(MediaFlags::DISCONTINUITY);
        let input = canonical_block(
            input_timing,
            &format,
            vec![vec![0.25, -0.5], vec![0.75, -1.0]],
        );
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                id,
                format.clone(),
                ChannelMap::identity(2).unwrap(),
                InputState::default(),
            )
            .unwrap();

        let output = mixer
            .mix_timed(output_timing, 2, &[(id, &input)], None)
            .unwrap();

        assert_ne!(input.timing(), output_timing);
        assert_eq!(output.block.timing(), output_timing);
        assert_eq!(output.block.sample_rate(), format.sample_rate);
        assert_eq!(output.block.channel_layout(), &format.channels);
        assert_eq!(output.block.sample_count(), 2);
        assert_eq!(output.block.planes(), input.planes());
    }

    #[test]
    fn timed_mix_without_submitted_blocks_is_silence() {
        let format = stereo_format();
        let output_timing = timing_for_samples(9, 4);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();

        let output = mixer.mix_timed(output_timing, 4, &[], None).unwrap();

        assert_eq!(output.block.timing(), output_timing);
        assert_eq!(output.block.sample_rate(), format.sample_rate);
        assert_eq!(output.block.channel_layout(), &format.channels);
        assert!(
            output
                .block
                .planes()
                .iter()
                .flatten()
                .all(|sample| *sample == 0.0)
        );
        assert_eq!(
            output.meters.channels(),
            &[ChannelMeter::default(), ChannelMeter::default()]
        );
    }

    #[test]
    fn planar_mix_reuses_caller_storage_and_copies_transactional_state() {
        let format = mono_format();
        let source = input_id(1);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                source,
                format.clone(),
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        mixer
            .set_input_state(
                source,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();
        let mut pending = MasterMixer::new(format.clone()).unwrap();
        pending
            .add_input(
                source,
                format.clone(),
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        pending.copy_runtime_state_from(&mixer).unwrap();
        let planes = vec![vec![1.0, 1.0, f32::NAN, f32::NAN]];
        let mut output = vec![vec![9.0; 8]];
        let pointer = output[0].as_ptr();

        pending
            .mix_planar_timed_into(
                timing_for_samples(0, 2),
                2,
                &[PlanarAudioSource {
                    input: source,
                    sample_rate: format.sample_rate,
                    channel_layout: &format.channels,
                    planes: &planes,
                    samples: 2,
                    source_gain: SourceGain::UNITY,
                }],
                &[source],
                &mut output,
            )
            .unwrap();

        assert_eq!(output[0].as_ptr(), pointer);
        assert_close(output[0][0], 0.75);
        assert_close(output[0][1], 0.5);
        assert_close(output[0][2], 9.0);
        assert_eq!(mixer.current_linear_gain(source), Some(1.0));
        assert_eq!(pending.current_linear_gain(source), Some(0.5));
    }

    #[test]
    fn timed_source_gains_hit_endpoints_and_continue_linearly_across_intervals() {
        let format = mono_format();
        let primary = input_id(1);
        let secondary = input_id(2);
        let primary_block = canonical_block(timing(1), &format, vec![vec![1.0; 4]]);
        let secondary_block = canonical_block(timing(2), &format, vec![vec![-1.0; 4]]);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        for id in [primary, secondary] {
            mixer
                .add_input(
                    id,
                    format.clone(),
                    ChannelMap::identity(1).unwrap(),
                    InputState {
                        follow_video: true,
                        ..InputState::default()
                    },
                )
                .unwrap();
        }

        let first = mixer
            .mix_timed_with_source_gains(
                timing_for_samples(10, 4),
                4,
                &[
                    (primary, &primary_block, SourceGain::new(2, 1, 2).unwrap()),
                    (
                        secondary,
                        &secondary_block,
                        SourceGain::new(0, 1, 2).unwrap(),
                    ),
                ],
                &[primary, secondary],
            )
            .unwrap();
        let second = mixer
            .mix_timed_with_source_gains(
                timing_for_samples(11, 4),
                4,
                &[
                    (primary, &primary_block, SourceGain::new(1, 0, 2).unwrap()),
                    (
                        secondary,
                        &secondary_block,
                        SourceGain::new(1, 2, 2).unwrap(),
                    ),
                ],
                &[primary, secondary],
            )
            .unwrap();

        assert_eq!(first.block.plane(0).unwrap(), &[0.75, 0.5, 0.25, 0.0]);
        assert_eq!(second.block.plane(0).unwrap(), &[-0.25, -0.5, -0.75, -1.0]);
        let joined = first
            .block
            .plane(0)
            .unwrap()
            .iter()
            .chain(second.block.plane(0).unwrap());
        for (left, right) in joined.clone().zip(joined.skip(1)) {
            assert_close(*right - *left, -0.25);
        }
    }

    #[test]
    fn timed_source_gains_preserve_mute_and_follow_video() {
        let format = mono_format();
        let primary = input_id(1);
        let secondary = input_id(2);
        let muted = input_id(3);
        let inactive = input_id(4);
        let block = canonical_block(timing(1), &format, vec![vec![1.0; 2]]);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        for (id, is_muted) in [
            (primary, false),
            (secondary, false),
            (muted, true),
            (inactive, false),
        ] {
            mixer
                .add_input(
                    id,
                    format.clone(),
                    ChannelMap::identity(1).unwrap(),
                    InputState {
                        muted: is_muted,
                        follow_video: true,
                        ..InputState::default()
                    },
                )
                .unwrap();
        }

        let output = mixer
            .mix_timed_with_source_gains(
                timing_for_samples(3, 2),
                2,
                &[
                    (primary, &block, SourceGain::UNITY),
                    (secondary, &block, SourceGain::UNITY),
                    (muted, &block, SourceGain::UNITY),
                    (inactive, &block, SourceGain::UNITY),
                ],
                &[primary, secondary, muted],
            )
            .unwrap();

        assert_eq!(output.block.plane(0).unwrap(), &[2.0, 2.0]);
    }

    #[test]
    fn timed_source_gain_failures_are_transactional() {
        let format = mono_format();
        let id = input_id(1);
        assert_eq!(
            SourceGain::new(0, 1, 0),
            Err(AudioError::InvalidSourceGain {
                start_numerator: 0,
                end_numerator: 1,
                denominator: 0,
            })
        );
        assert!(SourceGain::new(0, 2, 1).is_err());

        let valid = canonical_block(timing(1), &format, vec![vec![1.0; 2]]);
        let wrong_count = canonical_block(timing(2), &format, vec![vec![1.0]]);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                id,
                format,
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        mixer
            .set_input_state(
                id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();

        assert_eq!(
            mixer.mix_timed_with_source_gains(
                timing_for_samples(4, 2),
                2,
                &[(id, &wrong_count, SourceGain::new(0, 1, 1).unwrap())],
                &[id],
            ),
            Err(AudioError::SampleCountMismatch {
                expected: 2,
                actual: 1,
            })
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        let rendered = mixer
            .mix_timed_with_source_gains(
                timing_for_samples(5, 2),
                2,
                &[(id, &valid, SourceGain::new(0, 1, 1).unwrap())],
                &[id],
            )
            .unwrap();
        assert_eq!(rendered.block.plane(0).unwrap(), &[0.375, 0.5]);
    }

    #[test]
    fn timed_gain_mute_afv_ramps_and_meters_match_legacy_mix() {
        let format = mono_format();
        let gain_id = input_id(1);
        let muted_id = input_id(2);
        let afv_id = input_id(3);
        let planes = vec![vec![0.5, -0.5, 0.25, -0.25]];
        let legacy_block = AudioBlock::from_planar(format.clone(), planes.clone()).unwrap();
        let timed_block = canonical_block(timing(1), &format, planes);
        let mut legacy_mixer = MasterMixer::new(format.clone()).unwrap();
        legacy_mixer.set_clipping_policy(ClippingPolicy::Allow);
        for (id, state) in [
            (gain_id, InputState::default()),
            (
                muted_id,
                InputState {
                    muted: true,
                    ..InputState::default()
                },
            ),
            (
                afv_id,
                InputState {
                    follow_video: true,
                    ..InputState::default()
                },
            ),
        ] {
            legacy_mixer
                .add_input(id, format.clone(), ChannelMap::identity(1).unwrap(), state)
                .unwrap();
        }
        legacy_mixer
            .set_input_state(
                gain_id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();
        let mut timed_mixer = legacy_mixer.clone();

        let legacy = legacy_mixer
            .mix(
                4,
                &[
                    (gain_id, &legacy_block),
                    (muted_id, &legacy_block),
                    (afv_id, &legacy_block),
                ],
                Some(afv_id),
            )
            .unwrap();
        let timed = timed_mixer
            .mix_timed(
                timing_for_samples(2, 4),
                4,
                &[
                    (gain_id, &timed_block),
                    (muted_id, &timed_block),
                    (afv_id, &timed_block),
                ],
                Some(afv_id),
            )
            .unwrap();

        assert_eq!(timed.block.planes(), legacy.block.planes());
        assert_eq!(timed.meters, legacy.meters);
        assert_eq!(
            timed_mixer.current_linear_gain(gain_id),
            legacy_mixer.current_linear_gain(gain_id)
        );
        assert_eq!(
            timed_mixer.current_linear_gain(muted_id),
            legacy_mixer.current_linear_gain(muted_id)
        );
        assert_eq!(
            timed_mixer.current_linear_gain(afv_id),
            legacy_mixer.current_linear_gain(afv_id)
        );
    }

    #[test]
    fn timed_validation_errors_leave_ramps_unchanged() {
        let format = mono_format();
        let id = input_id(1);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        mixer
            .add_input(
                id,
                format.clone(),
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        mixer
            .set_input_state(
                id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();

        let output_timing = timing_for_samples(10, 2);
        let valid = canonical_block(timing(1), &format, vec![vec![1.0; 2]]);
        let wrong_rate = fm_frame::AudioBlock::new(
            timing(2),
            SampleRate::new(44_100).unwrap(),
            format.channels.clone(),
            vec![vec![1.0; 2]],
        )
        .unwrap();
        let wrong_layout = fm_frame::AudioBlock::new(
            timing(3),
            format.sample_rate,
            ChannelLayout::new(vec![Channel::Center]).unwrap(),
            vec![vec![1.0; 2]],
        )
        .unwrap();
        let wrong_count = canonical_block(timing(4), &format, vec![vec![1.0]]);
        let non_finite = canonical_block(timing(5), &format, vec![vec![1.0, f32::NAN]]);

        assert_eq!(
            mixer.mix_timed(output_timing, 2, &[(id, &wrong_rate)], None),
            Err(AudioError::FormatMismatch)
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        assert_eq!(
            mixer.mix_timed(output_timing, 2, &[(id, &wrong_layout)], None),
            Err(AudioError::FormatMismatch)
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        assert_eq!(
            mixer.mix_timed(output_timing, 2, &[(id, &wrong_count)], None),
            Err(AudioError::SampleCountMismatch {
                expected: 2,
                actual: 1,
            })
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        assert_eq!(
            mixer.mix_timed(output_timing, 2, &[(id, &non_finite)], None),
            Err(AudioError::NonFiniteSample {
                channel: 0,
                sample: 1,
            })
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        assert_eq!(
            mixer.mix_timed(output_timing, 2, &[(id, &valid), (id, &valid)], None),
            Err(AudioError::DuplicateInput(id))
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        let unknown = input_id(99);
        assert_eq!(
            mixer.mix_timed(output_timing, 2, &[(unknown, &valid)], None),
            Err(AudioError::UnknownInput(unknown))
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        assert_eq!(
            mixer.mix_timed(output_timing, MAX_SAMPLES_PER_BLOCK + 1, &[], None),
            Err(AudioError::SampleCountOutOfRange(MAX_SAMPLES_PER_BLOCK + 1))
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));

        let rendered = mixer
            .mix_timed(output_timing, 2, &[(id, &valid)], None)
            .unwrap();
        assert_eq!(rendered.block.plane(0).unwrap(), &[0.75, 0.5]);
    }

    #[test]
    fn timed_mix_rejects_duration_unrelated_to_sample_count() {
        let mut mixer = MasterMixer::new(mono_format()).unwrap();

        assert_eq!(
            mixer.mix_timed(timing(1), 2, &[], None),
            Err(AudioError::TimingSampleCountMismatch {
                samples: 2,
                sample_rate_hz: 48_000,
                duration_nanos: 1_000_000,
            })
        );
    }

    #[test]
    fn numeric_render_errors_leave_ramps_unchanged() {
        let format = mono_format();
        let id = input_id(1);
        let maximum_gain = Gain::from_db(MAX_GAIN_DB).unwrap();
        let input = canonical_block(timing(1), &format, vec![vec![f32::MAX]]);
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        mixer
            .add_input(
                id,
                format,
                ChannelMap::identity(1).unwrap(),
                InputState {
                    gain: maximum_gain,
                    ..InputState::default()
                },
            )
            .unwrap();
        mixer
            .set_input_state(
                id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();

        assert_eq!(
            mixer.mix_timed(timing_for_samples(1, 1), 1, &[(id, &input)], None),
            Err(AudioError::NonFiniteSample {
                channel: 0,
                sample: 0,
            })
        );
        assert_eq!(mixer.current_linear_gain(id), Some(maximum_gain.linear()));
    }

    #[test]
    fn canonical_audio_block_errors_are_typed_sources() {
        let mut mixer = MasterMixer::new(mono_format()).unwrap();
        let error = mixer.mix_timed(timing(1), 0, &[], None).unwrap_err();

        assert_eq!(
            error,
            AudioError::AudioBlock(fm_frame::AudioBlockError::ZeroSamples)
        );
        assert!(std::error::Error::source(&error).is_some());

        let mut high_rate_format = mono_format();
        high_rate_format.sample_rate =
            SampleRate::new(fm_frame::AudioBlock::MAX_SAMPLE_RATE_HZ + 1).unwrap();
        let id = input_id(1);
        let mut mixer = MasterMixer::new(high_rate_format.clone()).unwrap();
        mixer
            .add_input(
                id,
                high_rate_format,
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        mixer
            .set_input_state(
                id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();

        assert_eq!(
            mixer.mix_timed(timing(2), 2, &[], None),
            Err(AudioError::AudioBlock(
                fm_frame::AudioBlockError::SampleRateTooHigh {
                    actual: fm_frame::AudioBlock::MAX_SAMPLE_RATE_HZ + 1,
                    maximum: fm_frame::AudioBlock::MAX_SAMPLE_RATE_HZ,
                }
            ))
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
    }

    #[test]
    fn gain_mute_and_follow_video_control_inputs() {
        let format = mono_format();
        let first_id = input_id(1);
        let second_id = input_id(2);
        let block = AudioBlock::from_planar(format.clone(), vec![vec![0.5; 4]]).unwrap();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        mixer
            .add_input(
                first_id,
                format.clone(),
                ChannelMap::identity(1).unwrap(),
                InputState {
                    gain: Gain::from_db(-6.020_6).unwrap(),
                    muted: false,
                    follow_video: false,
                },
            )
            .unwrap();
        mixer
            .add_input(
                second_id,
                format,
                ChannelMap::identity(1).unwrap(),
                InputState {
                    gain: Gain::UNITY,
                    muted: false,
                    follow_video: true,
                },
            )
            .unwrap();

        let output = mixer
            .mix(
                4,
                &[(first_id, &block), (second_id, &block)],
                Some(first_id),
            )
            .unwrap();
        for sample in output.block.plane(0).unwrap() {
            assert_close(*sample, 0.25);
        }
        let muted = InputState {
            gain: Gain::UNITY,
            muted: true,
            follow_video: true,
        };
        mixer.set_input_state(second_id, muted, 0).unwrap();
        let output = mixer
            .mix(4, &[(second_id, &block)], Some(second_id))
            .unwrap();
        assert!(
            output
                .block
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.0)
        );
    }

    #[test]
    fn arbitrary_channel_routes_are_summed() {
        let input_format = stereo_format();
        let output_format = mono_format();
        let id = input_id(1);
        let map = ChannelMap::new(
            2,
            1,
            vec![
                ChannelRoute {
                    source: 0,
                    destination: 0,
                    gain: Gain::UNITY,
                },
                ChannelRoute {
                    source: 1,
                    destination: 0,
                    gain: Gain::from_db(-6.020_6).unwrap(),
                },
            ],
        )
        .unwrap();
        let block =
            AudioBlock::from_planar(input_format.clone(), vec![vec![0.25, 0.5], vec![0.5, 0.5]])
                .unwrap();
        let mut mixer = MasterMixer::new(output_format).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        mixer
            .add_input(id, input_format, map, InputState::default())
            .unwrap();
        let output = mixer.mix(2, &[(id, &block)], None).unwrap();
        assert_close(output.block.plane(0).unwrap()[0], 0.5);
        assert_close(output.block.plane(0).unwrap()[1], 0.75);
    }

    #[test]
    fn ramps_are_linear_and_continue_across_blocks() {
        let format = mono_format();
        let id = input_id(1);
        let block = AudioBlock::from_planar(format.clone(), vec![vec![1.0; 2]]).unwrap();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        mixer
            .add_input(
                id,
                format,
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        mixer
            .set_input_state(
                id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();
        let first = mixer.mix(2, &[(id, &block)], None).unwrap();
        assert_eq!(first.block.plane(0).unwrap(), &[0.75, 0.5]);
        let second = mixer.mix(2, &[(id, &block)], None).unwrap();
        assert_eq!(second.block.plane(0).unwrap(), &[0.25, 0.0]);
        assert_eq!(mixer.current_linear_gain(id), Some(0.0));
    }

    #[test]
    fn clipping_policy_is_explicit() {
        let format = mono_format();
        let id = input_id(1);
        let block = AudioBlock::from_planar(format.clone(), vec![vec![0.75]]).unwrap();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                id,
                format,
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        let clamped = mixer.mix(1, &[(id, &block)], None).unwrap();
        assert_eq!(clamped.block.plane(0).unwrap(), &[0.75]);

        let second = input_id(2);
        mixer
            .add_input(
                second,
                mono_format(),
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        let clamped = mixer
            .mix(1, &[(id, &block), (second, &block)], None)
            .unwrap();
        assert_eq!(clamped.block.plane(0).unwrap(), &[1.0]);
        mixer.set_clipping_policy(ClippingPolicy::Allow);
        let allowed = mixer
            .mix(1, &[(id, &block), (second, &block)], None)
            .unwrap();
        assert_eq!(allowed.block.plane(0).unwrap(), &[1.5]);
    }

    #[test]
    fn meters_report_peak_and_rms_per_channel() {
        let block = AudioBlock::from_planar(
            stereo_format(),
            vec![vec![1.0, -1.0, 0.0, 0.0], vec![0.5, -0.5, 0.5, -0.5]],
        )
        .unwrap();
        let meters = measure(&block);
        assert_close(meters.channels()[0].peak, 1.0);
        assert_close(meters.channels()[0].rms, 2.0_f32.sqrt() / 2.0);
        assert_close(meters.channels()[1].peak, 0.5);
        assert_close(meters.channels()[1].rms, 0.5);
    }

    #[test]
    fn errors_leave_configuration_and_ramps_unchanged() {
        let format = mono_format();
        let id = input_id(1);
        let block = AudioBlock::from_planar(format.clone(), vec![vec![1.0; 2]]).unwrap();
        let wrong =
            AudioBlock::from_planar(stereo_format(), vec![vec![1.0; 2], vec![1.0; 2]]).unwrap();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                id,
                format.clone(),
                ChannelMap::identity(1).unwrap(),
                InputState::default(),
            )
            .unwrap();
        mixer
            .set_input_state(
                id,
                InputState {
                    gain: Gain::SILENCE,
                    ..InputState::default()
                },
                4,
            )
            .unwrap();
        assert_eq!(
            mixer.mix(2, &[(id, &wrong)], None),
            Err(AudioError::FormatMismatch)
        );
        assert_eq!(mixer.current_linear_gain(id), Some(1.0));
        let prior = mixer.input_state(id);
        assert_eq!(
            mixer.set_input_state(id, InputState::default(), MAX_RAMP_SAMPLES + 1),
            Err(AudioError::RampTooLong(MAX_RAMP_SAMPLES + 1))
        );
        assert_eq!(mixer.input_state(id), prior);
        assert_eq!(
            mixer.add_input(
                id,
                format,
                ChannelMap::identity(1).unwrap(),
                InputState::default()
            ),
            Err(AudioError::DuplicateInput(id))
        );
        let rendered = mixer.mix(2, &[(id, &block)], None).unwrap();
        assert_eq!(rendered.block.plane(0).unwrap(), &[0.75, 0.5]);
    }

    #[test]
    fn malformed_or_unbounded_blocks_are_rejected() {
        let mut wrong_format = mono_format();
        wrong_format.sample_format = SampleFormat::I16;
        assert_eq!(
            AudioBlock::from_planar(wrong_format, vec![vec![0.0]]),
            Err(AudioError::UnsupportedSampleFormat(SampleFormat::I16))
        );
        assert!(matches!(
            AudioBlock::from_planar(stereo_format(), vec![vec![0.0]]),
            Err(AudioError::PlaneCountMismatch { .. })
        ));
        assert!(matches!(
            AudioBlock::from_planar(stereo_format(), vec![vec![0.0], vec![]]),
            Err(AudioError::PlaneLengthMismatch { .. })
        ));
        assert!(matches!(
            AudioBlock::silence(mono_format(), MAX_SAMPLES_PER_BLOCK + 1),
            Err(AudioError::SampleCountOutOfRange(_))
        ));
    }
}
