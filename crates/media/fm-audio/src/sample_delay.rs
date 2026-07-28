use core::fmt;

use crate::{MAX_CHANNELS, MAX_SAMPLES_PER_BLOCK};

const BYTES_PER_SAMPLE: usize = size_of::<f32>();

/// Maximum delay accepted by [`SampleDelay`], in samples.
///
/// The bound is independent of sample rate. At 48 kHz it represents one
/// second.
pub const MAX_SAMPLE_DELAY_SAMPLES: usize = 48_000;
/// Maximum ring-buffer allocation owned by one [`SampleDelay`].
pub const MAX_SAMPLE_DELAY_BYTES: usize =
    MAX_CHANNELS * MAX_SAMPLE_DELAY_SAMPLES * BYTES_PER_SAMPLE;

/// Identifies an input or output plane in a delay-processing error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleDelaySide {
    Input,
    Output,
}

/// Errors returned by the bounded planar sample delay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleDelayError {
    ChannelCountOutOfRange {
        actual: usize,
        maximum: usize,
    },
    DelayOutOfRange {
        actual: usize,
        maximum: usize,
    },
    SampleCountOutOfRange {
        actual: usize,
        maximum: usize,
    },
    PlaneCountMismatch {
        side: SampleDelaySide,
        expected: usize,
        actual: usize,
    },
    PlaneLengthMismatch {
        side: SampleDelaySide,
        plane: usize,
        expected: usize,
        actual: usize,
    },
    NonFiniteInput {
        channel: usize,
        sample: usize,
    },
    AllocationOverflow,
    AllocationFailed {
        samples: usize,
    },
}

impl fmt::Display for SampleDelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelCountOutOfRange { actual, maximum } => write!(
                formatter,
                "sample delay channel count {actual} is outside 1..={maximum}"
            ),
            Self::DelayOutOfRange { actual, maximum } => write!(
                formatter,
                "sample delay {actual} exceeds the limit of {maximum} samples"
            ),
            Self::SampleCountOutOfRange { actual, maximum } => write!(
                formatter,
                "sample delay block has {actual} samples per channel; maximum is {maximum}"
            ),
            Self::PlaneCountMismatch {
                side,
                expected,
                actual,
            } => write!(
                formatter,
                "sample delay {side:?} has {actual} planes; expected {expected}"
            ),
            Self::PlaneLengthMismatch {
                side,
                plane,
                expected,
                actual,
            } => write!(
                formatter,
                "sample delay {side:?} plane {plane} has {actual} samples; expected {expected}"
            ),
            Self::NonFiniteInput { channel, sample } => write!(
                formatter,
                "sample delay input sample {sample} in channel {channel} is not finite"
            ),
            Self::AllocationOverflow => {
                formatter.write_str("sample delay allocation arithmetic overflow")
            }
            Self::AllocationFailed { samples } => {
                write!(
                    formatter,
                    "could not allocate sample delay ring for {samples} samples"
                )
            }
        }
    }
}

impl std::error::Error for SampleDelayError {}

/// A deterministic, allocation-free steady-state planar `f32` sample delay.
///
/// Construction allocates one bounded contiguous ring. The configured channel
/// count and delay are immutable. Every processing call validates the complete
/// input and output before changing the ring, cursor, or output. A delay of
/// zero copies input directly to output.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleDelay {
    channels: usize,
    delay_samples: usize,
    cursor: usize,
    ring: Vec<f32>,
}

impl SampleDelay {
    /// Creates a delay with leading-silence state.
    ///
    /// # Errors
    ///
    /// Returns an error before allocation when the channel count or delay is
    /// outside its exported bound, or allocation-size arithmetic overflows.
    /// A bounded allocation failure is also reported rather than panicking.
    pub fn new(channels: usize, delay_samples: usize) -> Result<Self, SampleDelayError> {
        let capacity = validated_capacity(channels, delay_samples)?;
        let mut ring = Vec::new();
        ring.try_reserve_exact(capacity)
            .map_err(|_| SampleDelayError::AllocationFailed { samples: capacity })?;
        ring.resize(capacity, 0.0);
        Ok(Self {
            channels,
            delay_samples,
            cursor: 0,
            ring,
        })
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }

    #[must_use]
    pub const fn delay_samples(&self) -> usize {
        self.delay_samples
    }

    /// Delays one exact-size planar block into caller-owned output.
    ///
    /// Blocks may contain any number of samples through
    /// [`MAX_SAMPLES_PER_BLOCK`]. Partitioning a stream across calls does not
    /// change its output. Successful processing performs no allocation.
    ///
    /// # Errors
    ///
    /// Returns an error for a channel-count mismatch, non-rectangular input or
    /// output planes, unequal input/output sample counts, an oversized block,
    /// or a non-finite input sample. Failure leaves both state and output
    /// unchanged.
    pub fn process_into<InputPlane, OutputPlane>(
        &mut self,
        input: &[InputPlane],
        output: &mut [OutputPlane],
    ) -> Result<(), SampleDelayError>
    where
        InputPlane: AsRef<[f32]>,
        OutputPlane: AsRef<[f32]> + AsMut<[f32]>,
    {
        let samples = self.validate_operation(input, output)?;
        if self.delay_samples == 0 {
            for (source, destination) in input.iter().zip(output) {
                destination.as_mut().copy_from_slice(source.as_ref());
            }
            return Ok(());
        }

        for (channel, (source, destination)) in input.iter().zip(output).enumerate() {
            let source = source.as_ref();
            let destination = destination.as_mut();
            let channel_base = channel * self.delay_samples;
            for sample in 0..samples {
                let ring_index = channel_base + (self.cursor + sample) % self.delay_samples;
                destination[sample] = self.ring[ring_index];
                self.ring[ring_index] = source[sample];
            }
        }
        self.cursor = (self.cursor + samples % self.delay_samples) % self.delay_samples;
        Ok(())
    }

    /// Clears retained samples and returns to leading-silence state.
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.cursor = 0;
    }

    fn validate_operation<InputPlane, OutputPlane>(
        &self,
        input: &[InputPlane],
        output: &[OutputPlane],
    ) -> Result<usize, SampleDelayError>
    where
        InputPlane: AsRef<[f32]>,
        OutputPlane: AsRef<[f32]>,
    {
        validate_plane_count(SampleDelaySide::Input, input.len(), self.channels)?;
        validate_plane_count(SampleDelaySide::Output, output.len(), self.channels)?;

        let samples = input[0].as_ref().len();
        if samples > MAX_SAMPLES_PER_BLOCK {
            return Err(SampleDelayError::SampleCountOutOfRange {
                actual: samples,
                maximum: MAX_SAMPLES_PER_BLOCK,
            });
        }
        for (plane, values) in input.iter().enumerate() {
            let values = values.as_ref();
            if values.len() != samples {
                return Err(SampleDelayError::PlaneLengthMismatch {
                    side: SampleDelaySide::Input,
                    plane,
                    expected: samples,
                    actual: values.len(),
                });
            }
        }
        for (plane, values) in output.iter().enumerate() {
            let actual = values.as_ref().len();
            if actual != samples {
                return Err(SampleDelayError::PlaneLengthMismatch {
                    side: SampleDelaySide::Output,
                    plane,
                    expected: samples,
                    actual,
                });
            }
        }
        for (channel, plane) in input.iter().enumerate() {
            if let Some(sample) = plane.as_ref().iter().position(|sample| !sample.is_finite()) {
                return Err(SampleDelayError::NonFiniteInput { channel, sample });
            }
        }
        Ok(samples)
    }
}

fn validated_capacity(channels: usize, delay_samples: usize) -> Result<usize, SampleDelayError> {
    if !(1..=MAX_CHANNELS).contains(&channels) {
        return Err(SampleDelayError::ChannelCountOutOfRange {
            actual: channels,
            maximum: MAX_CHANNELS,
        });
    }
    let capacity = channels
        .checked_mul(delay_samples)
        .ok_or(SampleDelayError::AllocationOverflow)?;
    capacity
        .checked_mul(BYTES_PER_SAMPLE)
        .ok_or(SampleDelayError::AllocationOverflow)?;
    if delay_samples > MAX_SAMPLE_DELAY_SAMPLES {
        return Err(SampleDelayError::DelayOutOfRange {
            actual: delay_samples,
            maximum: MAX_SAMPLE_DELAY_SAMPLES,
        });
    }
    Ok(capacity)
}

fn validate_plane_count(
    side: SampleDelaySide,
    actual: usize,
    expected: usize,
) -> Result<(), SampleDelayError> {
    if actual != expected {
        return Err(SampleDelayError::PlaneCountMismatch {
            side,
            expected,
            actual,
        });
    }
    Ok(())
}
