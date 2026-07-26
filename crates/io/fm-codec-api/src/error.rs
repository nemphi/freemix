use std::fmt;

use crate::CodecLifecycle;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    CreateDecoder,
    CreateEncoder,
    Submit,
    Receive,
    Drain,
    Flush,
    Reconfigure,
    RequestKeyframe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityMismatch {
    Codec,
    Profile,
    MediaKind,
    EncodedFormat,
    DecodedFormat,
    QueueCapacity,
    KeyframeRequests,
    Reconfiguration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodecErrorKind {
    CapabilityMismatch(CapabilityMismatch),
    InvalidState {
        state: CodecLifecycle,
        operation: Operation,
    },
    TimestampRegression,
    StreamMismatch,
    ReconfigureRejected,
    AdapterFailure {
        code: u32,
        message: String,
    },
}

/// A typed codec failure with an operation-independent classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecError {
    kind: CodecErrorKind,
}

impl CodecError {
    #[must_use]
    pub const fn new(kind: CodecErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> &CodecErrorKind {
        &self.kind
    }
}

impl From<CapabilityMismatch> for CodecError {
    fn from(value: CapabilityMismatch) -> Self {
        Self::new(CodecErrorKind::CapabilityMismatch(value))
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            CodecErrorKind::CapabilityMismatch(mismatch) => {
                write!(formatter, "codec capability mismatch: {mismatch:?}")
            }
            CodecErrorKind::InvalidState { state, operation } => {
                write!(
                    formatter,
                    "operation {operation:?} is invalid in state {state:?}"
                )
            }
            CodecErrorKind::TimestampRegression => formatter.write_str("media timestamp regressed"),
            CodecErrorKind::StreamMismatch => {
                formatter.write_str("packet belongs to a different stream")
            }
            CodecErrorKind::ReconfigureRejected => {
                formatter.write_str("codec rejected live reconfiguration")
            }
            CodecErrorKind::AdapterFailure { code, message } => {
                write!(formatter, "codec adapter failure {code}: {message}")
            }
        }
    }
}

impl std::error::Error for CodecError {}
