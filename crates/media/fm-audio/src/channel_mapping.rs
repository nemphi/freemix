use core::fmt;

use fm_frame::AudioBlock;
use fm_types::{Channel, ChannelLayout};

use crate::MAX_SAMPLES_PER_BLOCK;

const BYTES_PER_SAMPLE: usize = size_of::<f32>();

/// Number of distinct semantic channel labels currently representable.
pub const MAX_CHANNEL_MAPPING_CHANNELS: usize = 7;
/// Maximum number of explicit source-to-destination routes in one mapping.
pub const MAX_CHANNEL_MAPPING_ROUTES: usize =
    MAX_CHANNEL_MAPPING_CHANNELS * MAX_CHANNEL_MAPPING_CHANNELS;
/// Maximum temporary PCM storage used by one mapping operation.
pub const MAX_CHANNEL_MAPPING_BYTES: usize =
    MAX_CHANNEL_MAPPING_CHANNELS * MAX_SAMPLES_PER_BLOCK * BYTES_PER_SAMPLE;

/// Identifies one side of a channel mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelMappingSide {
    Source,
    Destination,
}

/// One explicit contribution to a destination channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelMappingRoute {
    pub source: Channel,
    pub destination: Channel,
    pub coefficient: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CompiledRoute {
    route: ChannelMappingRoute,
    source_index: usize,
    destination_index: usize,
}

/// Errors returned by the bounded channel mapper.
#[derive(Clone, Debug, PartialEq)]
pub enum ChannelMappingError {
    ChannelCountOutOfRange {
        side: ChannelMappingSide,
        actual: usize,
        maximum: usize,
    },
    DuplicateLayoutChannel {
        side: ChannelMappingSide,
        channel: Channel,
    },
    TooManyRoutes {
        actual: usize,
        maximum: usize,
    },
    UnknownRouteChannel {
        side: ChannelMappingSide,
        channel: Channel,
    },
    DuplicateRoute {
        source: Channel,
        destination: Channel,
    },
    InvalidCoefficient {
        route: usize,
        coefficient: f32,
    },
    SourceLayoutMismatch,
    SampleCountOutOfRange {
        actual: usize,
        maximum: usize,
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
    NonFiniteInput {
        channel: usize,
        sample: usize,
    },
    NonFiniteOutput {
        channel: usize,
        sample: usize,
    },
    AllocationOverflow,
    AllocationTooLarge {
        required: usize,
        maximum: usize,
    },
    AudioBlock(fm_frame::AudioBlockError),
}

impl fmt::Display for ChannelMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelCountOutOfRange {
                side,
                actual,
                maximum,
            } => write!(
                formatter,
                "{side:?} channel count {actual} is outside 1..={maximum}"
            ),
            Self::DuplicateLayoutChannel { side, channel } => {
                write!(formatter, "{side:?} layout contains duplicate {channel:?}")
            }
            Self::TooManyRoutes { actual, maximum } => {
                write!(
                    formatter,
                    "channel mapping has {actual} routes; maximum is {maximum}"
                )
            }
            Self::UnknownRouteChannel { side, channel } => {
                write!(
                    formatter,
                    "channel mapping references missing {side:?} {channel:?}"
                )
            }
            Self::DuplicateRoute {
                source,
                destination,
            } => write!(
                formatter,
                "channel mapping repeats route {source:?} to {destination:?}"
            ),
            Self::InvalidCoefficient { route, coefficient } => {
                write!(
                    formatter,
                    "channel mapping route {route} has invalid coefficient {coefficient}"
                )
            }
            Self::SourceLayoutMismatch => {
                formatter.write_str("audio block does not have the mapping's source layout")
            }
            Self::SampleCountOutOfRange { actual, maximum } => write!(
                formatter,
                "audio block has {actual} samples per channel; maximum is {maximum}"
            ),
            Self::OutputPlaneCountMismatch { expected, actual } => {
                write!(
                    formatter,
                    "mapping output has {actual} planes; expected {expected}"
                )
            }
            Self::OutputPlaneLengthMismatch {
                plane,
                expected,
                actual,
            } => write!(
                formatter,
                "mapping output plane {plane} has {actual} samples; expected {expected}"
            ),
            Self::NonFiniteInput { channel, sample } => {
                write!(
                    formatter,
                    "input sample {sample} in channel {channel} is not finite"
                )
            }
            Self::NonFiniteOutput { channel, sample } => {
                write!(
                    formatter,
                    "mapped sample {sample} in channel {channel} is not finite"
                )
            }
            Self::AllocationOverflow => {
                formatter.write_str("channel mapping allocation arithmetic overflow")
            }
            Self::AllocationTooLarge { required, maximum } => write!(
                formatter,
                "channel mapping requires {required} temporary bytes; maximum is {maximum}"
            ),
            Self::AudioBlock(error) => write!(formatter, "mapped audio block error: {error}"),
        }
    }
}

impl std::error::Error for ChannelMappingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AudioBlock(error) => Some(error),
            _ => None,
        }
    }
}

impl From<fm_frame::AudioBlockError> for ChannelMappingError {
    fn from(value: fm_frame::AudioBlockError) -> Self {
        Self::AudioBlock(value)
    }
}

/// An immutable, deterministic source-layout to destination-layout mapping.
///
/// Destinations without routes are silent. Duplication and mixing happen only
/// through explicit routes. Routes are compiled into layout indices and sorted
/// into a stable accumulation order when the mapping is created.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelMapping {
    source_layout: ChannelLayout,
    destination_layout: ChannelLayout,
    routes: Vec<CompiledRoute>,
}

impl ChannelMapping {
    /// Creates and validates an explicit channel mapping.
    ///
    /// Coefficients may be signed, but must be finite. A source/destination
    /// pair may appear only once.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported or duplicate layout labels, excessive
    /// route counts, missing route labels, repeated routes, or non-finite
    /// coefficients.
    pub fn new(
        source_layout: ChannelLayout,
        destination_layout: ChannelLayout,
        routes: Vec<ChannelMappingRoute>,
    ) -> Result<Self, ChannelMappingError> {
        validate_layout(ChannelMappingSide::Source, &source_layout)?;
        validate_layout(ChannelMappingSide::Destination, &destination_layout)?;
        if routes.len() > MAX_CHANNEL_MAPPING_ROUTES {
            return Err(ChannelMappingError::TooManyRoutes {
                actual: routes.len(),
                maximum: MAX_CHANNEL_MAPPING_ROUTES,
            });
        }

        let mut compiled = Vec::with_capacity(routes.len());
        for (route_index, route) in routes.into_iter().enumerate() {
            if !route.coefficient.is_finite() {
                return Err(ChannelMappingError::InvalidCoefficient {
                    route: route_index,
                    coefficient: route.coefficient,
                });
            }
            let source_index =
                channel_index(ChannelMappingSide::Source, &source_layout, route.source)?;
            let destination_index = channel_index(
                ChannelMappingSide::Destination,
                &destination_layout,
                route.destination,
            )?;
            if compiled.iter().any(|existing: &CompiledRoute| {
                existing.source_index == source_index
                    && existing.destination_index == destination_index
            }) {
                return Err(ChannelMappingError::DuplicateRoute {
                    source: route.source,
                    destination: route.destination,
                });
            }
            compiled.push(CompiledRoute {
                route,
                source_index,
                destination_index,
            });
        }
        compiled.sort_by_key(|route| (route.destination_index, route.source_index));

        Ok(Self {
            source_layout,
            destination_layout,
            routes: compiled,
        })
    }

    /// Creates a semantic identity/reorder map from matching channel labels.
    ///
    /// Destination labels absent from the source layout receive silence.
    ///
    /// # Errors
    ///
    /// Returns an error when either layout is unsupported or contains duplicate
    /// labels.
    pub fn matching(
        source_layout: ChannelLayout,
        destination_layout: ChannelLayout,
    ) -> Result<Self, ChannelMappingError> {
        validate_layout(ChannelMappingSide::Source, &source_layout)?;
        validate_layout(ChannelMappingSide::Destination, &destination_layout)?;
        let routes = destination_layout
            .channels()
            .iter()
            .filter(|destination| source_layout.channels().contains(destination))
            .map(|destination| ChannelMappingRoute {
                source: *destination,
                destination: *destination,
                coefficient: 1.0,
            })
            .collect();
        Self::new(source_layout, destination_layout, routes)
    }

    /// Creates an identity map for a semantic channel layout.
    ///
    /// # Errors
    ///
    /// Returns an error when the layout is unsupported or contains duplicate
    /// labels.
    pub fn identity(layout: ChannelLayout) -> Result<Self, ChannelMappingError> {
        Self::matching(layout.clone(), layout)
    }

    #[must_use]
    pub const fn source_layout(&self) -> &ChannelLayout {
        &self.source_layout
    }

    #[must_use]
    pub const fn destination_layout(&self) -> &ChannelLayout {
        &self.destination_layout
    }

    #[must_use]
    pub fn routes(&self) -> impl ExactSizeIterator<Item = ChannelMappingRoute> + '_ {
        self.routes.iter().map(|route| route.route)
    }

    /// Maps a canonical timed block into a newly allocated canonical block.
    ///
    /// Timing, sample rate, and sample count are preserved. The input block is
    /// not modified; its samples are transformed into destination planes.
    ///
    /// # Errors
    ///
    /// Returns an error before producing a block when the input does not match
    /// the mapping, violates operation bounds, contains non-finite samples, or
    /// produces a non-finite result.
    pub fn map(&self, input: &AudioBlock) -> Result<AudioBlock, ChannelMappingError> {
        let planes = self.render(input)?;
        Ok(AudioBlock::new(
            input.timing(),
            input.sample_rate(),
            self.destination_layout.clone(),
            planes,
        )?)
    }

    /// Maps into exact-size caller-owned destination planes.
    ///
    /// The destination is not modified unless all plan, input, numeric, shape,
    /// and allocation validation succeeds.
    ///
    /// # Errors
    ///
    /// Returns the same mapping errors as [`Self::map`], plus output plane
    /// count or length mismatches.
    pub fn map_into(
        &self,
        input: &AudioBlock,
        output: &mut [Vec<f32>],
    ) -> Result<(), ChannelMappingError> {
        let samples = self.validate_input(input)?;
        let expected_planes = self.destination_layout.channels().len();
        if output.len() != expected_planes {
            return Err(ChannelMappingError::OutputPlaneCountMismatch {
                expected: expected_planes,
                actual: output.len(),
            });
        }
        for (plane, values) in output.iter().enumerate() {
            if values.len() != samples {
                return Err(ChannelMappingError::OutputPlaneLengthMismatch {
                    plane,
                    expected: samples,
                    actual: values.len(),
                });
            }
        }

        let rendered = self.render_validated(input, samples)?;
        for (destination, mapped) in output.iter_mut().zip(rendered) {
            destination.copy_from_slice(&mapped);
        }
        Ok(())
    }

    fn render(&self, input: &AudioBlock) -> Result<Vec<Vec<f32>>, ChannelMappingError> {
        let samples = self.validate_input(input)?;
        self.render_validated(input, samples)
    }

    fn validate_input(&self, input: &AudioBlock) -> Result<usize, ChannelMappingError> {
        if input.channel_layout() != &self.source_layout {
            return Err(ChannelMappingError::SourceLayoutMismatch);
        }
        let samples = input.sample_count();
        if samples > MAX_SAMPLES_PER_BLOCK {
            return Err(ChannelMappingError::SampleCountOutOfRange {
                actual: samples,
                maximum: MAX_SAMPLES_PER_BLOCK,
            });
        }
        for (channel, plane) in input.planes().iter().enumerate() {
            if let Some(sample) = plane.iter().position(|value| !value.is_finite()) {
                return Err(ChannelMappingError::NonFiniteInput { channel, sample });
            }
        }
        Ok(samples)
    }

    fn render_validated(
        &self,
        input: &AudioBlock,
        samples: usize,
    ) -> Result<Vec<Vec<f32>>, ChannelMappingError> {
        let channels = self.destination_layout.channels().len();
        let required = channels
            .checked_mul(samples)
            .and_then(|values| values.checked_mul(BYTES_PER_SAMPLE))
            .ok_or(ChannelMappingError::AllocationOverflow)?;
        if required > MAX_CHANNEL_MAPPING_BYTES {
            return Err(ChannelMappingError::AllocationTooLarge {
                required,
                maximum: MAX_CHANNEL_MAPPING_BYTES,
            });
        }

        let mut output = vec![vec![0.0_f32; samples]; channels];
        for route in &self.routes {
            let source = &input.planes()[route.source_index];
            let destination = &mut output[route.destination_index];
            for sample in 0..samples {
                let mapped = destination[sample] + source[sample] * route.route.coefficient;
                if !mapped.is_finite() {
                    return Err(ChannelMappingError::NonFiniteOutput {
                        channel: route.destination_index,
                        sample,
                    });
                }
                destination[sample] = mapped;
            }
        }
        Ok(output)
    }

    pub(crate) fn compiled_routes(
        &self,
    ) -> impl ExactSizeIterator<Item = (usize, usize, f32)> + '_ {
        self.routes.iter().map(|route| {
            (
                route.source_index,
                route.destination_index,
                route.route.coefficient,
            )
        })
    }
}

pub(crate) fn validate_layout(
    side: ChannelMappingSide,
    layout: &ChannelLayout,
) -> Result<(), ChannelMappingError> {
    let channels = layout.channels();
    if !(1..=MAX_CHANNEL_MAPPING_CHANNELS).contains(&channels.len()) {
        return Err(ChannelMappingError::ChannelCountOutOfRange {
            side,
            actual: channels.len(),
            maximum: MAX_CHANNEL_MAPPING_CHANNELS,
        });
    }
    for (index, channel) in channels.iter().enumerate() {
        if channels[index + 1..].contains(channel) {
            return Err(ChannelMappingError::DuplicateLayoutChannel {
                side,
                channel: *channel,
            });
        }
    }
    Ok(())
}

fn channel_index(
    side: ChannelMappingSide,
    layout: &ChannelLayout,
    channel: Channel,
) -> Result<usize, ChannelMappingError> {
    layout
        .channels()
        .iter()
        .position(|candidate| *candidate == channel)
        .ok_or(ChannelMappingError::UnknownRouteChannel { side, channel })
}
