use std::collections::BTreeSet;

use fm_audio::{
    AudioGenerator, FrameSampleAllocator, Gain, ImpulseGenerator, SilenceGenerator, SineGenerator,
};
use fm_frame::{
    AlphaMode, AudioBlock, ChannelLayout, ChromaLocation, ClockDomainId, ColorMetadata,
    ColorPrimaries, CpuVideoFrame, CpuVideoPayload, CpuVideoPlane, MatrixCoefficients, MediaFlags,
    MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    PixelFormat, SampleRate, SequenceNumber, SignalRange, TimeBase, TransferFunction,
    VideoDimensions, VideoFrameMetadata,
};
use fm_types::{AudioFormat, FrameRate, SampleFormat};
use fm_video::{solid_color, vertical_color_bars};

use crate::{MediaSourceError, SourcePattern};

const SAMPLE_RATE_HZ: u32 = 48_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FaultSchedule {
    discontinuities: BTreeSet<u64>,
    corruptions: BTreeSet<u64>,
    signal_losses: BTreeSet<u64>,
}

impl FaultSchedule {
    #[must_use]
    pub fn discontinuity_at(mut self, sequence: u64) -> Self {
        self.discontinuities.insert(sequence);
        self
    }

    #[must_use]
    pub fn corruption_at(mut self, sequence: u64) -> Self {
        self.corruptions.insert(sequence);
        self
    }

    #[must_use]
    pub fn signal_loss_at(mut self, sequence: u64) -> Self {
        self.signal_losses.insert(sequence);
        self
    }

    pub fn add_discontinuity(&mut self, sequence: u64) {
        self.discontinuities.insert(sequence);
    }

    pub fn add_corruption(&mut self, sequence: u64) {
        self.corruptions.insert(sequence);
    }

    pub fn add_signal_loss(&mut self, sequence: u64) {
        self.signal_losses.insert(sequence);
    }

    #[must_use]
    pub fn is_discontinuity(&self, sequence: u64) -> bool {
        self.discontinuities.contains(&sequence)
    }

    #[must_use]
    pub fn is_corrupted(&self, sequence: u64) -> bool {
        self.corruptions.contains(&sequence)
    }

    #[must_use]
    pub fn is_signal_lost(&self, sequence: u64) -> bool {
        self.signal_losses.contains(&sequence)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceEvent<T> {
    Frame(T),
    SignalLost { timing: MediaTiming },
}

impl<T> SourceEvent<T> {
    #[must_use]
    pub const fn signal_loss_timing(&self) -> Option<MediaTiming> {
        match self {
            Self::Frame(_) => None,
            Self::SignalLost { timing } => Some(*timing),
        }
    }

    #[must_use]
    pub const fn is_signal_lost(&self) -> bool {
        matches!(self, Self::SignalLost { .. })
    }

    #[must_use]
    pub fn into_frame(self) -> Option<T> {
        match self {
            Self::Frame(frame) => Some(frame),
            Self::SignalLost { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SimulatedTimedVideoSource {
    width: u32,
    height: u32,
    frame_rate: FrameRate,
    clock_domain: ClockDomainId,
    pattern: SourcePattern,
    faults: FaultSchedule,
    sequence: u64,
    recovering: bool,
}

pub type SimulatedVideoSource = SimulatedTimedVideoSource;

impl SimulatedTimedVideoSource {
    /// Creates a deterministic progressive RGBA source starting at PTS zero.
    ///
    /// # Errors
    ///
    /// Returns a typed error for invalid or unsupported dimensions.
    pub fn new(
        width: u32,
        height: u32,
        frame_rate: FrameRate,
        clock_domain: ClockDomainId,
        pattern: SourcePattern,
    ) -> Result<Self, MediaSourceError> {
        if width == 0 {
            return Err(MediaSourceError::ZeroWidth);
        }
        if height == 0 {
            return Err(MediaSourceError::ZeroHeight);
        }
        let dimensions =
            VideoDimensions::new(width, height).ok_or(MediaSourceError::DimensionOverflow)?;
        let stride = usize::try_from(width)
            .ok()
            .and_then(|value| value.checked_mul(4))
            .ok_or(MediaSourceError::DimensionOverflow)?;
        CpuVideoPayload::new(
            PixelFormat::Rgba8,
            dimensions,
            vec![CpuVideoPlane::new(
                stride,
                vec![
                    0;
                    stride
                        .checked_mul(
                            usize::try_from(height)
                                .map_err(|_| MediaSourceError::DimensionOverflow)?
                        )
                        .ok_or(MediaSourceError::DimensionOverflow)?
                ],
            )?],
        )?;
        Ok(Self {
            width,
            height,
            frame_rate,
            clock_domain,
            pattern,
            faults: FaultSchedule::default(),
            sequence: 0,
            recovering: false,
        })
    }

    #[must_use]
    pub const fn sequence(&self) -> SequenceNumber {
        SequenceNumber::new(self.sequence)
    }

    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    #[must_use]
    pub const fn faults(&self) -> &FaultSchedule {
        &self.faults
    }

    pub fn set_faults(&mut self, faults: FaultSchedule) {
        self.faults = faults;
    }

    /// Advances one source interval and reports either a frame or signal loss.
    ///
    /// # Errors
    ///
    /// Returns a typed error if timeline, payload, or metadata construction fails.
    pub fn next_event(&mut self) -> Result<SourceEvent<CpuVideoFrame>, MediaSourceError> {
        let sequence = self.sequence;
        let timing = frame_timing(
            sequence,
            self.frame_rate,
            self.clock_domain,
            scheduled_flags(&self.faults, sequence, self.recovering),
        )?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(MediaSourceError::TimelineOverflow)?;

        if self.faults.is_signal_lost(sequence) {
            self.recovering = true;
            return Ok(SourceEvent::SignalLost { timing });
        }
        self.recovering = false;

        let image = match self.pattern {
            SourcePattern::Bars => vertical_color_bars(self.width, self.height, sequence)?,
            SourcePattern::Solid(color) => solid_color(self.width, self.height, color)?,
        };
        let dimensions = VideoDimensions::new(self.width, self.height)
            .ok_or(MediaSourceError::DimensionOverflow)?;
        let plane = CpuVideoPlane::new(image.stride(), image.pixels().to_vec())?;
        let payload = CpuVideoPayload::new(PixelFormat::Rgba8, dimensions, vec![plane])?;
        let frame = CpuVideoFrame::new(timing, payload).with_metadata(simulated_video_metadata())?;
        Ok(SourceEvent::Frame(frame))
    }

    /// Advances one interval, returning `None` for an injected signal loss.
    ///
    /// # Errors
    ///
    /// Returns a typed source construction or timeline error.
    pub fn next_frame(&mut self) -> Result<Option<CpuVideoFrame>, MediaSourceError> {
        Ok(self.next_event()?.into_frame())
    }

    pub fn reset(&mut self) {
        self.sequence = 0;
        self.recovering = false;
    }

    pub fn restart(&mut self) {
        self.reset();
    }
}

const fn simulated_video_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioPattern {
    Silence,
    Sine {
        frequency_hz: f32,
        gain: Gain,
    },
    Impulse {
        channel: usize,
        sample: u64,
        amplitude: f32,
    },
}

#[derive(Clone, Debug)]
enum Generator {
    Silence(SilenceGenerator),
    Sine(SineGenerator),
    Impulse(ImpulseGenerator),
}

impl Generator {
    fn generate(&mut self, samples: usize) -> Result<fm_audio::AudioBlock, fm_audio::AudioError> {
        match self {
            Self::Silence(generator) => generator.generate(samples),
            Self::Sine(generator) => generator.generate(samples),
            Self::Impulse(generator) => generator.generate(samples),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Silence(_) => {}
            Self::Sine(generator) => generator.reset(),
            Self::Impulse(generator) => generator.reset(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SimulatedAudioSource {
    frame_rate: FrameRate,
    clock_domain: ClockDomainId,
    channel_layout: ChannelLayout,
    allocator: FrameSampleAllocator,
    generator: Generator,
    faults: FaultSchedule,
    sequence: u64,
    sample_cursor: u64,
    recovering: bool,
}

impl SimulatedAudioSource {
    /// Creates a deterministic 48 kHz planar-f32 source with video-frame cadence.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported pattern, layout, or cadence.
    pub fn new(
        frame_rate: FrameRate,
        channel_layout: ChannelLayout,
        clock_domain: ClockDomainId,
        pattern: AudioPattern,
    ) -> Result<Self, MediaSourceError> {
        let sample_rate = sample_rate();
        let format = AudioFormat {
            sample_rate,
            sample_format: SampleFormat::F32,
            channels: channel_layout.clone(),
        };
        let generator = match pattern {
            AudioPattern::Silence => Generator::Silence(SilenceGenerator::new(format)?),
            AudioPattern::Sine { frequency_hz, gain } => {
                Generator::Sine(SineGenerator::new(format, frequency_hz, gain)?)
            }
            AudioPattern::Impulse {
                channel,
                sample,
                amplitude,
            } => Generator::Impulse(ImpulseGenerator::new(format, channel, sample, amplitude)?),
        };
        Ok(Self {
            frame_rate,
            clock_domain,
            channel_layout,
            allocator: FrameSampleAllocator::new(sample_rate, frame_rate)?,
            generator,
            faults: FaultSchedule::default(),
            sequence: 0,
            sample_cursor: 0,
            recovering: false,
        })
    }

    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        sample_rate()
    }

    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    #[must_use]
    pub const fn sequence(&self) -> SequenceNumber {
        SequenceNumber::new(self.sequence)
    }

    #[must_use]
    pub const fn cumulative_samples(&self) -> u64 {
        self.sample_cursor
    }

    #[must_use]
    pub const fn faults(&self) -> &FaultSchedule {
        &self.faults
    }

    pub fn set_faults(&mut self, faults: FaultSchedule) {
        self.faults = faults;
    }

    /// Advances exactly one rational video-frame interval of audio.
    ///
    /// The generator advances during signal loss, so recovery remains sample-
    /// continuous and carries a discontinuity flag.
    ///
    /// # Errors
    ///
    /// Returns a typed generation or timeline error.
    pub fn next_event(&mut self) -> Result<SourceEvent<AudioBlock>, MediaSourceError> {
        let sequence = self.sequence;
        let samples = self.allocator.next_samples();
        let start_sample = self.sample_cursor;
        let end_sample = start_sample
            .checked_add(u64::try_from(samples).map_err(|_| MediaSourceError::TimelineOverflow)?)
            .ok_or(MediaSourceError::TimelineOverflow)?;
        let timing = audio_timing(
            sequence,
            start_sample,
            samples,
            self.clock_domain,
            scheduled_flags(&self.faults, sequence, self.recovering),
        )?;
        let generated = self.generator.generate(samples)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(MediaSourceError::TimelineOverflow)?;
        self.sample_cursor = end_sample;

        if self.faults.is_signal_lost(sequence) {
            self.recovering = true;
            return Ok(SourceEvent::SignalLost { timing });
        }
        self.recovering = false;
        let block = AudioBlock::new(
            timing,
            sample_rate(),
            self.channel_layout.clone(),
            generated.planes().to_vec(),
        )?;
        Ok(SourceEvent::Frame(block))
    }

    /// Advances one interval, returning `None` for an injected signal loss.
    ///
    /// # Errors
    ///
    /// Returns a typed generation or timeline error.
    pub fn next_block(&mut self) -> Result<Option<AudioBlock>, MediaSourceError> {
        Ok(self.next_event()?.into_frame())
    }

    pub fn reset(&mut self) {
        self.sequence = 0;
        self.sample_cursor = 0;
        self.recovering = false;
        self.allocator.reset();
        self.generator.reset();
    }

    pub fn restart(&mut self) {
        self.reset();
    }
}

fn scheduled_flags(faults: &FaultSchedule, sequence: u64, recovering: bool) -> MediaFlags {
    let mut flags = MediaFlags::NONE;
    if recovering || faults.is_discontinuity(sequence) {
        flags |= MediaFlags::DISCONTINUITY;
    }
    if faults.is_corrupted(sequence) {
        flags |= MediaFlags::CORRUPTED;
    }
    flags
}

fn frame_timing(
    sequence: u64,
    frame_rate: FrameRate,
    clock_domain: ClockDomainId,
    flags: MediaFlags,
) -> Result<MediaTiming, MediaSourceError> {
    let current = scale_timestamp(sequence, frame_rate.denominator(), frame_rate.numerator())?;
    let next_sequence = sequence
        .checked_add(1)
        .ok_or(MediaSourceError::TimelineOverflow)?;
    let next = scale_timestamp(
        next_sequence,
        frame_rate.denominator(),
        frame_rate.numerator(),
    )?;
    let duration = u64::try_from(next - current).map_err(|_| MediaSourceError::TimelineOverflow)?;
    let original_ticks = i64::try_from(sequence).map_err(|_| MediaSourceError::TimelineOverflow)?;
    Ok(MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(original_ticks),
            TimeBase::new(frame_rate.denominator(), frame_rate.numerator())
                .expect("a frame rate has nonzero components"),
        ),
        NormalizedTimestamp::from_nanos(current),
        NormalizedDuration::from_nanos(duration)?,
        clock_domain,
        SequenceNumber::new(sequence),
    )?
    .with_flags(flags))
}

fn audio_timing(
    sequence: u64,
    start_sample: u64,
    samples: usize,
    clock_domain: ClockDomainId,
    flags: MediaFlags,
) -> Result<MediaTiming, MediaSourceError> {
    let current = scale_timestamp(start_sample, 1, SAMPLE_RATE_HZ)?;
    let end_sample = start_sample
        .checked_add(u64::try_from(samples).map_err(|_| MediaSourceError::TimelineOverflow)?)
        .ok_or(MediaSourceError::TimelineOverflow)?;
    let next = scale_timestamp(end_sample, 1, SAMPLE_RATE_HZ)?;
    let duration = u64::try_from(next - current).map_err(|_| MediaSourceError::TimelineOverflow)?;
    let original_ticks =
        i64::try_from(start_sample).map_err(|_| MediaSourceError::TimelineOverflow)?;
    Ok(MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(original_ticks),
            TimeBase::new(1, SAMPLE_RATE_HZ).expect("the sample rate is nonzero"),
        ),
        NormalizedTimestamp::from_nanos(current),
        NormalizedDuration::from_nanos(duration)?,
        clock_domain,
        SequenceNumber::new(sequence),
    )?
    .with_flags(flags))
}

fn scale_timestamp(ticks: u64, numerator: u32, denominator: u32) -> Result<i64, MediaSourceError> {
    let nanos = u128::from(ticks)
        .checked_mul(u128::from(numerator))
        .and_then(|value| value.checked_mul(NANOS_PER_SECOND))
        .ok_or(MediaSourceError::TimelineOverflow)?
        / u128::from(denominator);
    i64::try_from(nanos).map_err(|_| MediaSourceError::TimelineOverflow)
}

const fn sample_rate() -> SampleRate {
    match SampleRate::new(SAMPLE_RATE_HZ) {
        Some(rate) => rate,
        None => unreachable!(),
    }
}

/// Returns a stable FNV-1a hash of video payload bytes.
#[must_use]
pub fn video_frame_hash(frame: &CpuVideoFrame) -> u64 {
    frame
        .payload()
        .planes()
        .iter()
        .fold(FNV_OFFSET, |hash, plane| {
            fnv_bytes(hash, plane.bytes().iter().copied())
        })
}

/// Returns a stable FNV-1a hash of planar sample bit patterns.
#[must_use]
pub fn audio_block_hash(block: &AudioBlock) -> u64 {
    block
        .planes()
        .iter()
        .flatten()
        .fold(FNV_OFFSET, |hash, sample| {
            fnv_bytes(hash, sample.to_bits().to_le_bytes())
        })
}

fn fnv_bytes(bytes_hash: u64, bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(bytes_hash, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}
