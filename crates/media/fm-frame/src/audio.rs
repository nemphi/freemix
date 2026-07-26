use core::fmt;

use fm_types::{ChannelLayout, SampleRate};

use crate::MediaTiming;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioBlock {
    timing: MediaTiming,
    sample_rate: SampleRate,
    channel_layout: ChannelLayout,
    sample_count: usize,
    planes: Vec<Vec<f32>>,
}

impl AudioBlock {
    pub const MAX_SAMPLE_RATE_HZ: u32 = 768_000;
    pub const MAX_CHANNELS: usize = 64;
    pub const MAX_SAMPLES_PER_CHANNEL: usize = 1_048_576;
    pub const MAX_ALLOCATION_BYTES: usize = 256 * 1024 * 1024;

    /// Creates an immutable planar-f32 audio block.
    ///
    /// # Errors
    ///
    /// Returns a typed error for invalid channel layouts, plane lengths, sample
    /// counts, or allocation limits.
    pub fn new(
        timing: MediaTiming,
        sample_rate: SampleRate,
        channel_layout: ChannelLayout,
        planes: Vec<Vec<f32>>,
    ) -> Result<Self, AudioBlockError> {
        let sample_count = planes.first().map_or(0, Vec::len);
        validate_layout(sample_rate, &channel_layout, planes.len(), sample_count)?;
        for (index, plane) in planes.iter().enumerate() {
            if plane.len() != sample_count {
                return Err(AudioBlockError::PlaneLengthMismatch {
                    plane: index,
                    expected: sample_count,
                    actual: plane.len(),
                });
            }
        }
        Ok(Self {
            timing,
            sample_rate,
            channel_layout,
            sample_count,
            planes,
        })
    }

    /// Allocates a bounded block initialized to silence.
    ///
    /// # Errors
    ///
    /// Returns an error before allocating if the requested block exceeds a
    /// contract limit.
    pub fn silence(
        timing: MediaTiming,
        sample_rate: SampleRate,
        channel_layout: ChannelLayout,
        sample_count: usize,
    ) -> Result<Self, AudioBlockError> {
        let channels = channel_layout.channels().len();
        validate_layout(sample_rate, &channel_layout, channels, sample_count)?;
        let planes = vec![vec![0.0; sample_count]; channels];
        Self::new(timing, sample_rate, channel_layout, planes)
    }

    #[must_use]
    pub const fn timing(&self) -> MediaTiming {
        self.timing
    }

    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    #[must_use]
    pub const fn channel_layout(&self) -> &ChannelLayout {
        &self.channel_layout
    }

    #[must_use]
    pub const fn sample_count(&self) -> usize {
        self.sample_count
    }

    #[must_use]
    pub fn planes(&self) -> &[Vec<f32>] {
        &self.planes
    }

    #[must_use]
    pub fn plane(&self, channel: usize) -> Option<&[f32]> {
        self.planes.get(channel).map(Vec::as_slice)
    }

    #[must_use]
    pub fn sample(&self, channel: usize, sample: usize) -> Option<f32> {
        self.plane(channel)?.get(sample).copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioBlockError {
    SampleRateTooHigh {
        actual: u32,
        maximum: u32,
    },
    TooManyChannels {
        actual: usize,
        maximum: usize,
    },
    DuplicateChannel,
    PlaneCountMismatch {
        expected: usize,
        actual: usize,
    },
    ZeroSamples,
    TooManySamples {
        actual: usize,
        maximum: usize,
    },
    PlaneLengthMismatch {
        plane: usize,
        expected: usize,
        actual: usize,
    },
    AllocationOverflow,
    AllocationTooLarge {
        required: usize,
        maximum: usize,
    },
}

impl fmt::Display for AudioBlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleRateTooHigh { actual, maximum } => {
                write!(formatter, "sample rate {actual} Hz exceeds {maximum} Hz")
            }
            Self::TooManyChannels { actual, maximum } => {
                write!(formatter, "channel count {actual} exceeds {maximum}")
            }
            Self::DuplicateChannel => formatter.write_str("channel layout contains duplicates"),
            Self::PlaneCountMismatch { expected, actual } => {
                write!(
                    formatter,
                    "audio plane count {actual} does not match {expected}"
                )
            }
            Self::ZeroSamples => formatter.write_str("audio block must contain samples"),
            Self::TooManySamples { actual, maximum } => {
                write!(formatter, "sample count {actual} exceeds {maximum}")
            }
            Self::PlaneLengthMismatch {
                plane,
                expected,
                actual,
            } => write!(
                formatter,
                "audio plane {plane} length {actual} does not match {expected}"
            ),
            Self::AllocationOverflow => formatter.write_str("audio allocation arithmetic overflow"),
            Self::AllocationTooLarge { required, maximum } => {
                write!(
                    formatter,
                    "audio allocation {required} bytes exceeds {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for AudioBlockError {}

fn validate_layout(
    sample_rate: SampleRate,
    channel_layout: &ChannelLayout,
    plane_count: usize,
    sample_count: usize,
) -> Result<(), AudioBlockError> {
    if sample_rate.hertz() > AudioBlock::MAX_SAMPLE_RATE_HZ {
        return Err(AudioBlockError::SampleRateTooHigh {
            actual: sample_rate.hertz(),
            maximum: AudioBlock::MAX_SAMPLE_RATE_HZ,
        });
    }
    let channels = channel_layout.channels();
    if channels.len() > AudioBlock::MAX_CHANNELS {
        return Err(AudioBlockError::TooManyChannels {
            actual: channels.len(),
            maximum: AudioBlock::MAX_CHANNELS,
        });
    }
    if channels
        .iter()
        .enumerate()
        .any(|(index, channel)| channels[index + 1..].contains(channel))
    {
        return Err(AudioBlockError::DuplicateChannel);
    }
    if plane_count != channels.len() {
        return Err(AudioBlockError::PlaneCountMismatch {
            expected: channels.len(),
            actual: plane_count,
        });
    }
    if sample_count == 0 {
        return Err(AudioBlockError::ZeroSamples);
    }
    if sample_count > AudioBlock::MAX_SAMPLES_PER_CHANNEL {
        return Err(AudioBlockError::TooManySamples {
            actual: sample_count,
            maximum: AudioBlock::MAX_SAMPLES_PER_CHANNEL,
        });
    }
    let required = channels
        .len()
        .checked_mul(sample_count)
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or(AudioBlockError::AllocationOverflow)?;
    if required > AudioBlock::MAX_ALLOCATION_BYTES {
        return Err(AudioBlockError::AllocationTooLarge {
            required,
            maximum: AudioBlock::MAX_ALLOCATION_BYTES,
        });
    }
    Ok(())
}
