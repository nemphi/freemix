//! Portable, bounded contracts for timed media and resource leases.

mod audio;
mod encoded;
mod lease;
mod preview;
mod timing;
mod video;

pub use audio::{AudioBlock, AudioBlockError};
pub use encoded::{
    CodecConfigGeneration, CodecId, EncodedPacket, EncodedPacketError, EncodedPacketMetadata,
    EncodedPayload, PacketFlagError, PacketFlags, StreamId,
};
pub use lease::{
    BridgeId, LeaseError, ReleaseOwner, ReleaseOwnerId, ReleaseOwnership, ResourceId,
    ResourceLease, SynchronizationId, SynchronizationToken,
};
pub use preview::{
    EngineInstanceId, LocalPreviewLease, LocalPreviewLeaseError, OsHandleReferenceId,
    PhysicalAdapterToken, PreviewImageDescriptor, PreviewImageDescriptorError, PreviewLeaseId,
    PreviewLeaseRegistry, PreviewLeaseRegistryError, PreviewReleaseAck, PreviewStreamDescriptor,
    PreviewStreamDescriptorError, PreviewStreamId, PreviewTarget, PreviewTransport,
};
pub use timing::{
    ClockDomainId, MediaFlagError, MediaFlags, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SequenceNumber, TimingError,
};
pub use video::{CpuVideoFrame, CpuVideoPayload, CpuVideoPlane, VideoPayloadError};

pub use fm_types::{
    AlphaMode, Channel, ChannelLayout, ChromaLocation, ColorMetadata, ColorPrimaries,
    MatrixCoefficients, MediaDuration, MediaTimestamp, MemoryDomain, PixelFormat, SampleRate,
    SignalRange, TimeBase, Timecode, TransferFunction, VideoDimensions, VideoFrameMetadata,
    VideoFrameMetadataError,
};
