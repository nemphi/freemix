use core::{fmt, num::NonZeroU128};
use std::{io, path::PathBuf};

use fm_frame::{
    CodecConfigGeneration, CodecId, EncodedPacket, EncodedPayload, MediaTiming, OriginalTimestamp,
    PacketFlags, StreamId,
};
use fm_types::{ChannelLayout, PixelFormat, SampleRate, VideoDimensions};

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(RecorderId);
stable_id!(ActionReceiptId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecorderState {
    Recording,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueLimits {
    pub max_events: usize,
    pub max_bytes: usize,
}

impl QueueLimits {
    #[must_use]
    pub const fn new(max_events: usize, max_bytes: usize) -> Self {
        Self {
            max_events,
            max_bytes,
        }
    }
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_events: 256,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SegmentPolicy {
    pub max_frames: Option<u64>,
    pub max_bytes: Option<u64>,
}

impl SegmentPolicy {
    #[must_use]
    pub const fn by_frames(max_frames: u64) -> Self {
        Self {
            max_frames: Some(max_frames),
            max_bytes: None,
        }
    }

    #[must_use]
    pub const fn by_bytes(max_bytes: u64) -> Self {
        Self {
            max_frames: None,
            max_bytes: Some(max_bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecorderConfig {
    pub queue: QueueLimits,
    pub segments: SegmentPolicy,
}

impl RecorderConfig {
    #[must_use]
    pub const fn new(queue: QueueLimits, segments: SegmentPolicy) -> Self {
        Self { queue, segments }
    }

    pub(crate) fn validate(self) -> Result<(), RecorderError> {
        if self.queue.max_events == 0 || self.queue.max_bytes == 0 {
            return Err(RecorderError::InvalidConfig(
                "queue event and byte limits must be nonzero",
            ));
        }
        if self.segments.max_frames == Some(0) || self.segments.max_bytes == Some(0) {
            return Err(RecorderError::InvalidConfig(
                "segment frame and byte limits must be nonzero",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketCommon {
    codec: CodecId,
    config_generation: CodecConfigGeneration,
    stream_id: StreamId,
    channel_index: Option<u16>,
    timing: MediaTiming,
    decode_timestamp: OriginalTimestamp,
    flags: PacketFlags,
}

impl PacketCommon {
    #[must_use]
    pub fn from_packet(packet: &EncodedPacket) -> Self {
        let metadata = packet.metadata();
        Self {
            codec: metadata.codec().clone(),
            config_generation: metadata.config_generation(),
            stream_id: metadata.stream_id(),
            channel_index: metadata.channel_index(),
            timing: metadata.timing(),
            decode_timestamp: metadata.decode_timestamp(),
            flags: metadata.flags(),
        }
    }

    #[must_use]
    pub const fn codec(&self) -> &CodecId {
        &self.codec
    }

    #[must_use]
    pub const fn config_generation(&self) -> CodecConfigGeneration {
        self.config_generation
    }

    #[must_use]
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn channel_index(&self) -> Option<u16> {
        self.channel_index
    }

    #[must_use]
    pub const fn timing(&self) -> MediaTiming {
        self.timing
    }

    #[must_use]
    pub const fn decode_timestamp(&self) -> OriginalTimestamp {
        self.decode_timestamp
    }

    #[must_use]
    pub const fn flags(&self) -> PacketFlags {
        self.flags
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPacketMetadata {
    pub common: PacketCommon,
    pub sample_rate: SampleRate,
    pub channels: ChannelLayout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoPacketMetadata {
    pub common: PacketCommon,
    pub dimensions: VideoDimensions,
    pub pixel_format: PixelFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimedPacketMetadata {
    pub common: PacketCommon,
    pub content_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Discontinuity {
    pub stream_id: Option<StreamId>,
    pub timing: MediaTiming,
    pub reason: String,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RecordEvent {
    Audio {
        metadata: AudioPacketMetadata,
        payload: Vec<u8>,
    },
    Video {
        metadata: VideoPacketMetadata,
        payload: Vec<u8>,
    },
    Timed {
        metadata: TimedPacketMetadata,
        payload: Vec<u8>,
    },
    Discontinuity(Discontinuity),
}

impl RecordEvent {
    /// Takes the owned byte payload from an encoded audio packet.
    ///
    /// # Errors
    ///
    /// Resource-backed packets are rejected because this std-only recorder has
    /// no resource mapping contract.
    pub fn audio(
        packet: EncodedPacket,
        sample_rate: SampleRate,
        channels: ChannelLayout,
    ) -> Result<Self, RecorderError> {
        let common = PacketCommon::from_packet(&packet);
        let payload = take_bytes(packet)?;
        Ok(Self::Audio {
            metadata: AudioPacketMetadata {
                common,
                sample_rate,
                channels,
            },
            payload,
        })
    }

    /// Takes the owned byte payload from an encoded video packet.
    ///
    /// # Errors
    ///
    /// Resource-backed packets are rejected.
    pub fn video(
        packet: EncodedPacket,
        dimensions: VideoDimensions,
        pixel_format: PixelFormat,
    ) -> Result<Self, RecorderError> {
        let common = PacketCommon::from_packet(&packet);
        let payload = take_bytes(packet)?;
        Ok(Self::Video {
            metadata: VideoPacketMetadata {
                common,
                dimensions,
                pixel_format,
            },
            payload,
        })
    }

    /// Takes the owned byte payload from an encoded timed-data packet.
    ///
    /// # Errors
    ///
    /// Empty or oversized content types and resource-backed packets are
    /// rejected.
    pub fn timed(
        packet: EncodedPacket,
        content_type: impl Into<String>,
    ) -> Result<Self, RecorderError> {
        let content_type = content_type.into();
        validate_text(&content_type, "timed content type")?;
        let common = PacketCommon::from_packet(&packet);
        let payload = take_bytes(packet)?;
        Ok(Self::Timed {
            metadata: TimedPacketMetadata {
                common,
                content_type,
            },
            payload,
        })
    }

    /// Creates an explicit stream or timeline discontinuity marker.
    ///
    /// # Errors
    ///
    /// An empty or oversized reason is rejected.
    pub fn discontinuity(discontinuity: Discontinuity) -> Result<Self, RecorderError> {
        validate_text(&discontinuity.reason, "discontinuity reason")?;
        Ok(Self::Discontinuity(discontinuity))
    }

    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Audio { payload, .. }
            | Self::Video { payload, .. }
            | Self::Timed { payload, .. } => payload.len(),
            Self::Discontinuity(_) => 0,
        }
    }

    #[must_use]
    pub const fn counts_as_frame(&self) -> bool {
        !matches!(self, Self::Discontinuity(_))
    }

    pub(crate) fn queue_bytes(&self) -> usize {
        self.payload_len().saturating_add(256)
    }
}

fn take_bytes(packet: EncodedPacket) -> Result<Vec<u8>, RecorderError> {
    match packet.into_payload() {
        EncodedPayload::Bytes(bytes) => Ok(bytes),
        EncodedPayload::Resource(_) => Err(RecorderError::ResourcePayloadUnsupported),
    }
}

fn validate_text(value: &str, name: &'static str) -> Result<(), RecorderError> {
    if value.is_empty() || value.len() > u16::MAX.into() {
        Err(RecorderError::InvalidMetadata(name))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecorderSnapshot {
    pub id: RecorderId,
    pub state: RecorderState,
    pub segment_index: Option<u64>,
    pub queued_events: usize,
    pub queued_bytes: usize,
    pub written_frames: u64,
    pub written_bytes: u64,
    pub failure: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredSegment {
    pub index: u64,
    pub records: u64,
    pub bytes: u64,
    pub truncated_bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    pub manifest_records: u64,
    pub manifest_truncated_bytes: u64,
    pub segments: Vec<RecoveredSegment>,
}

#[derive(Debug)]
pub enum EnqueueError {
    UnknownRecorder(Box<RecordEvent>),
    NotRecording {
        state: RecorderState,
        event: Box<RecordEvent>,
    },
    QueueFull(Box<RecordEvent>),
    EventTooLarge(Box<RecordEvent>),
}

impl EnqueueError {
    #[must_use]
    pub fn into_event(self) -> RecordEvent {
        match self {
            Self::UnknownRecorder(event)
            | Self::NotRecording { event, .. }
            | Self::QueueFull(event)
            | Self::EventTooLarge(event) => *event,
        }
    }
}

impl fmt::Display for EnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRecorder(_) => formatter.write_str("recorder is not registered"),
            Self::NotRecording { state, .. } => {
                write!(formatter, "recorder is not recording (state: {state:?})")
            }
            Self::QueueFull(_) => formatter.write_str("recorder queue is full"),
            Self::EventTooLarge(_) => {
                formatter.write_str("event exceeds recorder queue byte limit")
            }
        }
    }
}

impl std::error::Error for EnqueueError {}

#[derive(Debug)]
pub enum AppendError {
    Enqueue(EnqueueError),
    Recorder(RecorderError),
}

impl fmt::Display for AppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enqueue(error) => error.fmt(formatter),
            Self::Recorder(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Enqueue(error) => Some(error),
            Self::Recorder(error) => Some(error),
        }
    }
}

impl From<EnqueueError> for AppendError {
    fn from(error: EnqueueError) -> Self {
        Self::Enqueue(error)
    }
}

impl From<RecorderError> for AppendError {
    fn from(error: RecorderError) -> Self {
        Self::Recorder(error)
    }
}

#[derive(Debug)]
pub enum RecorderError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    InvalidConfig(&'static str),
    InvalidMetadata(&'static str),
    ResourcePayloadUnsupported,
    UnknownRecorder(RecorderId),
    InvalidState {
        id: RecorderId,
        state: RecorderState,
        operation: &'static str,
    },
    ReceiptConflict(ActionReceiptId),
    Corrupt {
        path: PathBuf,
        offset: u64,
        reason: &'static str,
    },
    FormatLimit(&'static str),
}

impl RecorderError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for RecorderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::InvalidConfig(message) => write!(formatter, "invalid recorder config: {message}"),
            Self::InvalidMetadata(name) => write!(formatter, "invalid {name}"),
            Self::ResourcePayloadUnsupported => {
                formatter.write_str("resource-backed encoded payloads cannot be recorded")
            }
            Self::UnknownRecorder(id) => write!(formatter, "unknown recorder {id}"),
            Self::InvalidState {
                id,
                state,
                operation,
            } => write!(
                formatter,
                "cannot {operation} recorder {id} in state {state:?}"
            ),
            Self::ReceiptConflict(receipt) => {
                write!(
                    formatter,
                    "action receipt {receipt} was used for a different action"
                )
            }
            Self::Corrupt {
                path,
                offset,
                reason,
            } => write!(
                formatter,
                "corrupt recorder file {} at byte {offset}: {reason}",
                path.display()
            ),
            Self::FormatLimit(message) => write!(formatter, "record format limit: {message}"),
        }
    }
}

impl std::error::Error for RecorderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
