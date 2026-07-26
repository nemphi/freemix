use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use fm_frame::{EncodedPacket, NormalizedTimestamp, StreamId};

use crate::{EncodedFormat, QueueCapacity, SubmitStatus};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ContainerFormat(String);

impl ContainerFormat {
    pub const MAX_LENGTH: usize = 64;

    /// Creates a portable container identifier such as `container/mp4`.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, and non-portable identifiers.
    pub fn new(value: impl Into<String>) -> Result<Self, ContainerFormatError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ContainerFormatError::Empty);
        }
        if value.len() > Self::MAX_LENGTH {
            return Err(ContainerFormatError::TooLong {
                actual: value.len(),
                maximum: Self::MAX_LENGTH,
            });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
        {
            return Err(ContainerFormatError::Invalid);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerFormatError {
    Empty,
    TooLong { actual: usize, maximum: usize },
    Invalid,
}

impl fmt::Display for ContainerFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("container identifier must not be empty"),
            Self::TooLong { actual, maximum } => {
                write!(
                    formatter,
                    "container identifier length {actual} exceeds {maximum}"
                )
            }
            Self::Invalid => formatter.write_str("container identifier contains invalid bytes"),
        }
    }
}

impl std::error::Error for ContainerFormatError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamDescriptor {
    stream_id: StreamId,
    format: EncodedFormat,
}

impl StreamDescriptor {
    #[must_use]
    pub const fn new(stream_id: StreamId, format: EncodedFormat) -> Self {
        Self { stream_id, format }
    }

    #[must_use]
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn format(&self) -> &EncodedFormat {
        &self.format
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DemuxerCapabilities {
    containers: Vec<ContainerFormat>,
    seekable: bool,
}

impl DemuxerCapabilities {
    #[must_use]
    pub const fn new(containers: Vec<ContainerFormat>, seekable: bool) -> Self {
        Self {
            containers,
            seekable,
        }
    }

    #[must_use]
    pub fn containers(&self) -> &[ContainerFormat] {
        &self.containers
    }

    #[must_use]
    pub const fn seekable(&self) -> bool {
        self.seekable
    }
}

#[derive(Debug, PartialEq)]
pub enum DemuxerStatus {
    Packet(Box<EncodedPacket>),
    NeedData,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DemuxerErrorKind {
    InvalidContainer,
    CapabilityMismatch,
    InvalidState,
    SeekUnsupported,
    TimestampOutOfRange,
    AdapterFailure { code: u32, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DemuxerError {
    kind: DemuxerErrorKind,
}

impl DemuxerError {
    #[must_use]
    pub const fn new(kind: DemuxerErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> &DemuxerErrorKind {
        &self.kind
    }
}

impl fmt::Display for DemuxerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "demuxer failure: {:?}", self.kind)
    }
}

impl std::error::Error for DemuxerError {}

pub trait Demuxer: Send {
    fn streams(&self) -> &[StreamDescriptor];

    /// Polls the next interleaved packet without blocking.
    ///
    /// # Errors
    ///
    /// Returns a typed read, state, or adapter error.
    fn read_packet(&mut self) -> Result<DemuxerStatus, DemuxerError>;

    /// Seeks and invalidates previously returned stream ordering state.
    ///
    /// # Errors
    ///
    /// Returns [`DemuxerErrorKind::SeekUnsupported`] or a typed seek error.
    fn seek(&mut self, timestamp: NormalizedTimestamp) -> Result<(), DemuxerError>;

    /// Discards buffered packets while retaining the selected source.
    ///
    /// # Errors
    ///
    /// Returns a typed state or adapter error.
    fn flush(&mut self) -> Result<(), DemuxerError>;
}

pub trait DemuxerProvider: Send + Sync {
    fn capabilities(&self) -> &DemuxerCapabilities;

    /// Opens a container source supported by this provider.
    ///
    /// # Errors
    ///
    /// Returns a capability mismatch or adapter creation error.
    fn open(&self, container: &ContainerFormat) -> Result<Box<dyn Demuxer>, DemuxerError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentMode {
    Single,
    Segmented,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxerConfig {
    container: ContainerFormat,
    mode: SegmentMode,
    queue_capacity: QueueCapacity,
}

impl MuxerConfig {
    #[must_use]
    pub const fn new(
        container: ContainerFormat,
        mode: SegmentMode,
        queue_capacity: QueueCapacity,
    ) -> Self {
        Self {
            container,
            mode,
            queue_capacity,
        }
    }

    #[must_use]
    pub const fn container(&self) -> &ContainerFormat {
        &self.container
    }

    #[must_use]
    pub const fn mode(&self) -> SegmentMode {
        self.mode
    }

    #[must_use]
    pub const fn queue_capacity(&self) -> QueueCapacity {
        self.queue_capacity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxerCapabilities {
    containers: Vec<ContainerFormat>,
    codecs: Vec<fm_frame::CodecId>,
    maximum_streams: NonZeroU32,
    maximum_queue_capacity: QueueCapacity,
    segmented: bool,
    recoverable_finalization: bool,
}

impl MuxerCapabilities {
    #[must_use]
    pub const fn new(
        containers: Vec<ContainerFormat>,
        codecs: Vec<fm_frame::CodecId>,
        maximum_streams: NonZeroU32,
        maximum_queue_capacity: QueueCapacity,
    ) -> Self {
        Self {
            containers,
            codecs,
            maximum_streams,
            maximum_queue_capacity,
            segmented: false,
            recoverable_finalization: false,
        }
    }

    #[must_use]
    pub const fn with_segmentation(mut self, recoverable_finalization: bool) -> Self {
        self.segmented = true;
        self.recoverable_finalization = recoverable_finalization;
        self
    }

    #[must_use]
    pub fn supports_config(&self, config: &MuxerConfig) -> bool {
        self.containers.contains(config.container())
            && (config.mode() == SegmentMode::Single || self.segmented)
            && config.queue_capacity().get() <= self.maximum_queue_capacity.get()
    }

    #[must_use]
    pub fn supports_stream(&self, stream: &StreamDescriptor) -> bool {
        self.codecs.contains(stream.format().codec())
    }

    #[must_use]
    pub fn containers(&self) -> &[ContainerFormat] {
        &self.containers
    }

    #[must_use]
    pub fn codecs(&self) -> &[fm_frame::CodecId] {
        &self.codecs
    }

    #[must_use]
    pub const fn maximum_streams(&self) -> NonZeroU32 {
        self.maximum_streams
    }

    #[must_use]
    pub const fn maximum_queue_capacity(&self) -> QueueCapacity {
        self.maximum_queue_capacity
    }

    #[must_use]
    pub const fn recoverable_finalization(&self) -> bool {
        self.recoverable_finalization
    }

    #[must_use]
    pub const fn segmented(&self) -> bool {
        self.segmented
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SegmentNumber(NonZeroU64);

impl SegmentNumber {
    #[must_use]
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentPacketCount {
    stream_id: StreamId,
    packets: u64,
}

impl SegmentPacketCount {
    #[must_use]
    pub const fn new(stream_id: StreamId, packets: u64) -> Self {
        Self { stream_id, packets }
    }

    #[must_use]
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn packets(&self) -> u64 {
        self.packets
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentFinalization {
    Complete,
    RecoveredAfterError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentMetadata {
    number: SegmentNumber,
    start: Option<NormalizedTimestamp>,
    end: Option<NormalizedTimestamp>,
    packet_counts: Vec<SegmentPacketCount>,
    bytes_written: u64,
    independently_decodable: bool,
    finalization: SegmentFinalization,
}

impl SegmentMetadata {
    #[must_use]
    pub const fn new(
        number: SegmentNumber,
        start: Option<NormalizedTimestamp>,
        end: Option<NormalizedTimestamp>,
        packet_counts: Vec<SegmentPacketCount>,
        bytes_written: u64,
        independently_decodable: bool,
        finalization: SegmentFinalization,
    ) -> Self {
        Self {
            number,
            start,
            end,
            packet_counts,
            bytes_written,
            independently_decodable,
            finalization,
        }
    }

    #[must_use]
    pub const fn number(&self) -> SegmentNumber {
        self.number
    }

    #[must_use]
    pub const fn start(&self) -> Option<NormalizedTimestamp> {
        self.start
    }

    #[must_use]
    pub const fn end(&self) -> Option<NormalizedTimestamp> {
        self.end
    }

    #[must_use]
    pub fn packet_counts(&self) -> &[SegmentPacketCount] {
        &self.packet_counts
    }

    #[must_use]
    pub const fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    #[must_use]
    pub const fn independently_decodable(&self) -> bool {
        self.independently_decodable
    }

    #[must_use]
    pub const fn finalization(&self) -> SegmentFinalization {
        self.finalization
    }
}

#[derive(Debug, PartialEq)]
pub enum MuxerStatus {
    NeedInput,
    SegmentFinalized(SegmentMetadata),
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MuxerRecovery {
    None,
    FinalizeCurrentSegment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MuxerErrorKind {
    InvalidContainer,
    CapabilityMismatch,
    InvalidState,
    DuplicateStream,
    UnknownStream,
    TimestampRegression,
    AdapterFailure { code: u32, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxerError {
    kind: MuxerErrorKind,
    recovery: MuxerRecovery,
}

impl MuxerError {
    #[must_use]
    pub const fn new(kind: MuxerErrorKind, recovery: MuxerRecovery) -> Self {
        Self { kind, recovery }
    }

    #[must_use]
    pub const fn fatal(kind: MuxerErrorKind) -> Self {
        Self::new(kind, MuxerRecovery::None)
    }

    #[must_use]
    pub const fn kind(&self) -> &MuxerErrorKind {
        &self.kind
    }

    #[must_use]
    pub const fn recovery(&self) -> MuxerRecovery {
        self.recovery
    }
}

impl fmt::Display for MuxerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "container adapter failure: {:?} (recovery: {:?})",
            self.kind, self.recovery
        )
    }
}

impl std::error::Error for MuxerError {}

pub trait Muxer: Send {
    fn config(&self) -> &MuxerConfig;
    fn streams(&self) -> &[StreamDescriptor];

    /// Adds a stream before the muxer is started.
    ///
    /// # Errors
    ///
    /// Returns a capability, duplicate-stream, state, or adapter error.
    fn add_stream(&mut self, stream: StreamDescriptor) -> Result<(), MuxerError>;

    /// Seals stream configuration and starts writing.
    ///
    /// # Errors
    ///
    /// Returns a configuration, state, or adapter error.
    fn start(&mut self) -> Result<(), MuxerError>;

    /// Submits one packet or returns it unchanged under backpressure.
    ///
    /// # Errors
    ///
    /// Returns a stream, timestamp, state, or adapter error with recovery
    /// information for the current segment.
    fn submit_packet(
        &mut self,
        packet: EncodedPacket,
    ) -> Result<SubmitStatus<EncodedPacket>, MuxerError>;

    /// Advances pending writes and reports finalized segment metadata.
    ///
    /// # Errors
    ///
    /// Returns a typed write error and its available recovery action.
    fn poll(&mut self) -> Result<MuxerStatus, MuxerError>;

    /// Forces buffered bytes to the sink without ending a segment.
    ///
    /// # Errors
    ///
    /// Returns a typed write error and its available recovery action.
    fn flush(&mut self) -> Result<(), MuxerError>;

    /// Requests finalization of the current segment.
    ///
    /// # Errors
    ///
    /// Returns a state or adapter error if a valid segment cannot be emitted.
    fn finalize_segment(&mut self) -> Result<(), MuxerError>;

    /// Drains all accepted packets and requests the terminal status.
    ///
    /// # Errors
    ///
    /// Returns a state or adapter error with segment recovery information.
    fn finish(&mut self) -> Result<(), MuxerError>;
}

pub trait MuxerProvider: Send + Sync {
    fn capabilities(&self) -> &MuxerCapabilities;

    /// Opens a muxer after matching the requested configuration.
    ///
    /// # Errors
    ///
    /// Returns a capability mismatch or adapter creation error.
    fn create_muxer(&self, config: MuxerConfig) -> Result<Box<dyn Muxer>, MuxerError>;
}
