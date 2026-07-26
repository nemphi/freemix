//! Deterministic simulated source and compositor adapter.

mod error;
mod media_source;
mod pipeline;
mod sink;
mod source;

pub use error::{MediaSourceError, PipelineConfigError, RegistryError, RenderError, VideoError};
pub use fm_video::{ImageFrame, Rgba8};
pub use media_source::{
    AudioPattern, FaultSchedule, SimulatedAudioSource, SimulatedTimedVideoSource,
    SimulatedVideoSource, SourceEvent, audio_block_hash, video_frame_hash,
};
pub use pipeline::SimulatedPipeline;
pub use sink::{
    CollectError, CollectOutcome, CollectingAudioSink, CollectingSink, CollectingVideoSink,
    OverflowPolicy, SinkConfigError, SinkTelemetry,
};
pub use source::{SimulatedSource, SourcePattern};
