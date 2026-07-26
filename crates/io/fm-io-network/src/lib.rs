//! Transport-neutral network output state, rendition planning, and HLS metadata.
//!
//! This crate deliberately performs no encoding and opens no sockets. Transport
//! adapters implement [`TransportSink`]; the state machines in this crate own
//! bounded queues, retries, telemetry, and failure isolation.

mod config;
mod hls;
mod impairment;
mod output;
mod rendition;

pub use config::{
    ConfigError, CredentialReference, DestinationConfig, DestinationId, Endpoint, MAX_DESTINATIONS,
    OutputProtocol, ReconnectPolicy, TlsConfig, TlsMinimumVersion, TlsVerification,
};
pub use hls::{
    HlsAbrCoordinator, HlsError, HlsPlaylist, HlsPlaylistMetadata, HlsPlaylistType,
    HlsSegmentMetadata, HlsVariantMetadata,
};
pub use impairment::{ImpairmentDecision, ImpairmentError, ImpairmentModel, ImpairmentTelemetry};
pub use output::{
    CongestionObservation, ConnectionObservation, DestinationEnqueue, DestinationState,
    EnqueueStatus, FailureRecord, FailureStage, NetworkTelemetry, OutputError, OutputPacket,
    OutputSet, PollEvent, SendObservation, SinkError, SinkWrite, TransportSink,
};
pub use rendition::{
    AbrLadder, AbrVariant, AudioRendition, ColorDescription, DestinationRenditions, FrameRate,
    PlannedRendition, RenditionError, RenditionId, RenditionPlan, RenditionPlanner,
    RenditionProfile, TimingProfile, VideoRendition,
};

pub use fm_codec_api::QueueCapacity;

#[cfg(test)]
mod tests;
