//! Small, portable domain types shared across `FreeMix`.

mod audio;
mod color;
mod id;
mod memory;
mod time;
mod video;

pub use audio::{AudioFormat, Channel, ChannelLayout, SampleFormat, SampleRate};
pub use color::{
    AlphaMode, ChromaLocation, ColorMetadata, ColorPrimaries, MatrixCoefficients, SignalRange,
    TransferFunction,
};
pub use id::{BusId, InputId, OutputId, ProjectId, SceneId};
pub use memory::MemoryDomain;
pub use time::{
    FrameRate, MediaDuration, MediaTimestamp, RateError, TimeBase, Timecode, TimecodeError,
};
pub use video::{
    PixelFormat, ScanMode, VideoDimensions, VideoFormat, VideoFrameMetadata,
    VideoFrameMetadataError,
};
