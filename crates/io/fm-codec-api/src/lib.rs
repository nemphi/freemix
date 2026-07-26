//! Backend-independent decoder, encoder, demuxer, and muxer contracts.
//!
//! Adapters expose portable capabilities and implement synchronous push/poll
//! state machines. Input is never silently dropped: a backpressure result
//! returns ownership of the packet or frame that was not accepted.

mod codec;
mod container;
mod error;
mod format;
mod queue;

pub use codec::{
    CodecCapabilities, CodecLifecycle, CodecProvider, Decoder, DecoderCapability, DecoderConfig,
    Encoder, EncoderCapability, EncoderConfig, KeyframeRequest, OutputStatus, SubmitStatus,
};
pub use container::{
    ContainerFormat, ContainerFormatError, Demuxer, DemuxerCapabilities, DemuxerError,
    DemuxerErrorKind, DemuxerProvider, DemuxerStatus, Muxer, MuxerCapabilities, MuxerConfig,
    MuxerError, MuxerErrorKind, MuxerProvider, MuxerRecovery, MuxerStatus, SegmentFinalization,
    SegmentMetadata, SegmentMode, SegmentNumber, SegmentPacketCount, StreamDescriptor,
};
pub use error::{CapabilityMismatch, CodecError, CodecErrorKind, Operation};
pub use format::{
    CodecLevel, CodecProfile, DecodedAudioFormat, DecodedFormat, DecodedFrame, DecodedVideoFormat,
    EncodedFormat, FormatError, KnownCodec, MediaKind,
};
pub use queue::{BoundedQueue, QueueCapacity, QueueCapacityError, QueueFull};

pub use fm_capabilities::{Health, Provider};
pub use fm_frame::{CodecId, EncodedPacket, StreamId};

#[cfg(test)]
mod tests;
