use std::fmt;
use std::io;

/// External program involved in an adapter operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
}

impl fmt::Display for Tool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ffmpeg => "ffmpeg",
            Self::Ffprobe => "ffprobe",
        })
    }
}

/// Why a configured tool is not usable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    Missing,
    PermissionDenied,
    InvalidExecutable,
    TimedOut,
    OutputLimit,
    Failed,
    MalformedVersion,
}

/// Coarse resource categories used by [`Error::LimitExceeded`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitKind {
    InputBytes,
    Streams,
    Width,
    Height,
    VideoFrames,
    AudioBlocks,
    AudioSamples,
    DecodedBytes,
    Stdout,
    Stderr,
}

/// Unsupported but well-formed source properties.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unsupported {
    StreamKind,
    AttachedPicture,
    NonSquarePixels,
    Rotation,
    InterlacedVideo,
    UnstableVideoFormat,
    AlphaVideo,
    HdrTransfer,
    PixelFormat,
    AudioLayout,
    AudioSeek,
    NegativeAudioAnchor,
}

/// Failure from the process-isolated local-file adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidConfig,
    ToolUnavailable {
        tool: Tool,
        reason: UnavailableReason,
    },
    InputNotFound,
    InputAccessDenied,
    InputNotRegularFile,
    InputOutsideAllowedRoot,
    LimitExceeded {
        kind: LimitKind,
        actual: u64,
        maximum: u64,
    },
    SourceChanged,
    ProcessTimedOut {
        tool: Tool,
    },
    ProcessOutputOverflow {
        tool: Tool,
        kind: LimitKind,
    },
    ProcessFailed {
        tool: Tool,
        status: Option<i32>,
        stderr: String,
    },
    ProcessIo {
        tool: Tool,
        kind: io::ErrorKind,
    },
    MalformedProbe,
    InvalidSelector,
    MissingStream,
    Unsupported(Unsupported),
    MissingFrames,
    IncompleteFrameMetadata,
    InvalidTimeline,
    OutputMismatch {
        expected: usize,
        actual: usize,
    },
    NonFiniteAudio,
    FrameConstruction,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => formatter.write_str("invalid FFmpeg adapter configuration"),
            Self::ToolUnavailable { tool, reason } => {
                write!(formatter, "{tool} is unavailable: {reason:?}")
            }
            Self::InputNotFound => formatter.write_str("input file was not found"),
            Self::InputAccessDenied => formatter.write_str("input file access was denied"),
            Self::InputNotRegularFile => formatter.write_str("input is not a regular file"),
            Self::InputOutsideAllowedRoot => {
                formatter.write_str("input is outside the configured allowed root")
            }
            Self::LimitExceeded {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "{kind:?} resource use {actual} exceeds configured limit {maximum}"
            ),
            Self::SourceChanged => formatter.write_str("input changed during the operation"),
            Self::ProcessTimedOut { tool } => write!(formatter, "{tool} timed out"),
            Self::ProcessOutputOverflow { tool, kind } => {
                write!(formatter, "{tool} exceeded its {kind:?} output limit")
            }
            Self::ProcessFailed {
                tool,
                status,
                stderr,
            } => write!(formatter, "{tool} failed with status {status:?}: {stderr}"),
            Self::ProcessIo { tool, kind } => {
                write!(formatter, "{tool} process I/O failed: {kind:?}")
            }
            Self::MalformedProbe => formatter.write_str("ffprobe returned malformed metadata"),
            Self::InvalidSelector => formatter.write_str("stream selector is invalid"),
            Self::MissingStream => formatter.write_str("requested stream is absent"),
            Self::Unsupported(reason) => write!(formatter, "unsupported source: {reason:?}"),
            Self::MissingFrames => formatter.write_str("source has fewer requested frames"),
            Self::IncompleteFrameMetadata => {
                formatter.write_str("bounded frame metadata is incomplete")
            }
            Self::InvalidTimeline => {
                formatter.write_str("source timeline is incomplete or invalid")
            }
            Self::OutputMismatch { expected, actual } => write!(
                formatter,
                "decoded output length {actual} does not match expected {expected}"
            ),
            Self::NonFiniteAudio => {
                formatter.write_str("decoded audio contains non-finite samples")
            }
            Self::FrameConstruction => {
                formatter.write_str("decoded media violates frame contracts")
            }
        }
    }
}

impl std::error::Error for Error {}
