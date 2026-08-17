//! Transport-neutral network output state, rendition planning, and HLS metadata.
//!
//! The state machines here own bounded queues, retries, telemetry, and failure
//! isolation, and they encode nothing and open nothing themselves: transport
//! adapters implement [`TransportSink`]. [`rtmp`] is the one module that does
//! open a transport, mapping that trait onto one bounded `FFmpeg` child per
//! connection attempt so a live RTMP output gets the retry, backoff, and
//! backup-endpoint behaviour modelled here on top of a real connection.

mod config;
mod hls;
mod impairment;
mod output;
mod rendition;
pub mod rtmp;

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
    CongestionObservation, ConnectionObservation, ConnectionTarget, DestinationEnqueue,
    DestinationState, EnqueueStatus, FailureRecord, FailureStage, NetworkTelemetry, OutputError,
    OutputPacket, OutputSet, PollEvent, SendObservation, SinkError, SinkWrite, TransportSink,
};
pub use rendition::{
    AbrLadder, AbrVariant, AudioRendition, ColorDescription, DestinationRenditions, FrameRate,
    PlannedRendition, RenditionError, RenditionId, RenditionPlan, RenditionPlanner,
    RenditionProfile, TimingProfile, VideoRendition,
};

pub use fm_codec_api::QueueCapacity;

#[cfg(test)]
mod tests;
