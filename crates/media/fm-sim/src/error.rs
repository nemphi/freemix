use core::fmt;
use fm_audio::AudioError;
use fm_frame::{AudioBlockError, TimingError, VideoFrameMetadataError, VideoPayloadError};
use fm_switcher::TransitionKind;
use fm_types::InputId;
use fm_video::{BlendError, FrameError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineConfigError {
    ZeroWidth,
    ZeroHeight,
    DimensionsExceedLimit {
        width: u32,
        height: u32,
        maximum_width: u32,
        maximum_height: u32,
    },
}

impl fmt::Display for PipelineConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => formatter.write_str("pipeline width must be nonzero"),
            Self::ZeroHeight => formatter.write_str("pipeline height must be nonzero"),
            Self::DimensionsExceedLimit {
                width,
                height,
                maximum_width,
                maximum_height,
            } => write!(
                formatter,
                "pipeline dimensions {width}x{height} exceed {maximum_width}x{maximum_height}"
            ),
        }
    }
}

impl std::error::Error for PipelineConfigError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    DuplicateSource(InputId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSource(input) => {
                write!(formatter, "source {input} is already registered")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoError {
    Frame(FrameError),
    Blend(BlendError),
}

impl fmt::Display for VideoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => error.fmt(formatter),
            Self::Blend(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for VideoError {}

impl From<FrameError> for VideoError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

impl From<BlendError> for VideoError {
    fn from(value: BlendError) -> Self {
        Self::Blend(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    MissingSource { input: InputId },
    MissingTransitionKind,
    UnsupportedTransition(TransitionKind),
    Video(VideoError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource { input } => write!(formatter, "source {input} is not registered"),
            Self::MissingTransitionKind => {
                formatter.write_str("program-frame transition kind is missing")
            }
            Self::UnsupportedTransition(kind) => {
                write!(formatter, "simulated transition {kind:?} is not supported")
            }
            Self::Video(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<VideoError> for RenderError {
    fn from(value: VideoError) -> Self {
        Self::Video(value)
    }
}

impl From<FrameError> for RenderError {
    fn from(value: FrameError) -> Self {
        Self::Video(VideoError::Frame(value))
    }
}

impl From<BlendError> for RenderError {
    fn from(value: BlendError) -> Self {
        Self::Video(VideoError::Blend(value))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaSourceError {
    ZeroWidth,
    ZeroHeight,
    DimensionOverflow,
    TimelineOverflow,
    Video(FrameError),
    VideoPayload(VideoPayloadError),
    FrameMetadata(VideoFrameMetadataError),
    Timing(TimingError),
    Audio(AudioError),
    AudioBlock(AudioBlockError),
}

impl fmt::Display for MediaSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => formatter.write_str("simulated video width must be nonzero"),
            Self::ZeroHeight => formatter.write_str("simulated video height must be nonzero"),
            Self::DimensionOverflow => formatter.write_str("simulated video dimensions overflow"),
            Self::TimelineOverflow => formatter.write_str("simulated media timeline overflow"),
            Self::Video(error) => error.fmt(formatter),
            Self::VideoPayload(error) => error.fmt(formatter),
            Self::FrameMetadata(error) => error.fmt(formatter),
            Self::Timing(error) => error.fmt(formatter),
            Self::Audio(error) => error.fmt(formatter),
            Self::AudioBlock(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MediaSourceError {}

impl From<FrameError> for MediaSourceError {
    fn from(value: FrameError) -> Self {
        Self::Video(value)
    }
}

impl From<VideoPayloadError> for MediaSourceError {
    fn from(value: VideoPayloadError) -> Self {
        Self::VideoPayload(value)
    }
}

impl From<VideoFrameMetadataError> for MediaSourceError {
    fn from(value: VideoFrameMetadataError) -> Self {
        Self::FrameMetadata(value)
    }
}

impl From<TimingError> for MediaSourceError {
    fn from(value: TimingError) -> Self {
        Self::Timing(value)
    }
}

impl From<AudioError> for MediaSourceError {
    fn from(value: AudioError) -> Self {
        Self::Audio(value)
    }
}

impl From<AudioBlockError> for MediaSourceError {
    fn from(value: AudioBlockError) -> Self {
        Self::AudioBlock(value)
    }
}
