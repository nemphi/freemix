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

pub const MAX_INPUT_NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenameInputError {
    UnknownInput(InputId),
    EmptyName,
    NameTooLong,
    DuplicateName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputOrderError {
    EmptyOrder,
    WrongLength { expected: usize, actual: usize },
    UnknownInput(InputId),
    DuplicateInput(InputId),
}

impl core::fmt::Display for InputOrderError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyOrder => formatter.write_str("input order must not be empty"),
            Self::WrongLength { expected, actual } => {
                write!(
                    formatter,
                    "input order must contain {expected} inputs, got {actual}"
                )
            }
            Self::UnknownInput(input) => write!(formatter, "input {input} does not exist"),
            Self::DuplicateInput(input) => write!(formatter, "input {input} occurs more than once"),
        }
    }
}

impl std::error::Error for InputOrderError {}

pub fn validate_input_order(
    current: &[InputId],
    requested: &[InputId],
) -> Result<(), InputOrderError> {
    if requested.is_empty() {
        return Err(InputOrderError::EmptyOrder);
    }
    if requested.len() != current.len() {
        return Err(InputOrderError::WrongLength {
            expected: current.len(),
            actual: requested.len(),
        });
    }
    for input in requested {
        if !current.contains(input) {
            return Err(InputOrderError::UnknownInput(*input));
        }
    }
    for (index, input) in requested.iter().enumerate() {
        if requested[..index].contains(input) {
            return Err(InputOrderError::DuplicateInput(*input));
        }
    }
    Ok(())
}

impl core::fmt::Display for RenameInputError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownInput(input) => write!(formatter, "input {input} does not exist"),
            Self::EmptyName => formatter.write_str("input name must not be empty"),
            Self::NameTooLong => {
                write!(
                    formatter,
                    "input name must not exceed {MAX_INPUT_NAME_BYTES} bytes"
                )
            }
            Self::DuplicateName => formatter.write_str("input name is already in use"),
        }
    }
}

impl std::error::Error for RenameInputError {}

pub fn validate_input_name(name: &str) -> Result<(), RenameInputError> {
    if name.trim().is_empty() {
        Err(RenameInputError::EmptyName)
    } else if name.len() > MAX_INPUT_NAME_BYTES {
        Err(RenameInputError::NameTooLong)
    } else {
        Ok(())
    }
}
