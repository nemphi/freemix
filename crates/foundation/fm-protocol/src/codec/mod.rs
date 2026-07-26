mod decode;
mod encode;
mod value;

pub use decode::{LineDecoder, decode_line};
pub use encode::encode_line;

use core::fmt;

pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_FIELDS_PER_MESSAGE: usize = 64;
pub const MAX_FIELD_NAME_BYTES: usize = 64;
pub const MAX_FIELD_VALUE_BYTES: usize = 48 * 1024;
pub const MAX_LIST_ITEMS: usize = 256;
pub const MAX_BATCH_EVENTS: usize = 256;
pub const MAX_MESSAGES_PER_PUSH: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodecError {
    LineTooLong,
    MissingNewline,
    MultipleLines,
    InvalidRecord,
    InvalidEscape,
    InvalidUtf8,
    DuplicateField(String),
    MissingField(&'static str),
    UnknownField(String),
    InvalidField { field: &'static str, value: String },
    UnknownMessage(String),
    TrailingData,
    TooManyFields,
    FieldNameTooLong,
    FieldValueTooLong,
    TooManyItems(&'static str),
    TooManyMessages,
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineTooLong => formatter.write_str("protocol line exceeds maximum length"),
            Self::MissingNewline => formatter.write_str("protocol record must end with newline"),
            Self::MultipleLines => formatter.write_str("decode_line accepts exactly one record"),
            Self::InvalidRecord => formatter.write_str("protocol record is malformed"),
            Self::InvalidEscape => formatter.write_str("field contains an invalid percent escape"),
            Self::InvalidUtf8 => formatter.write_str("field is not valid UTF-8"),
            Self::DuplicateField(field) => write!(formatter, "duplicate field {field}"),
            Self::MissingField(field) => write!(formatter, "required field {field} is missing"),
            Self::UnknownField(field) => write!(formatter, "unknown required field {field}"),
            Self::InvalidField { field, value } => {
                write!(formatter, "field {field} has invalid value {value}")
            }
            Self::UnknownMessage(kind) => write!(formatter, "unknown message type {kind}"),
            Self::TrailingData => formatter.write_str("stream ended with an incomplete record"),
            Self::TooManyFields => formatter.write_str("protocol record contains too many fields"),
            Self::FieldNameTooLong => formatter.write_str("protocol field name is too long"),
            Self::FieldValueTooLong => formatter.write_str("protocol field value is too long"),
            Self::TooManyItems(field) => write!(formatter, "field {field} contains too many items"),
            Self::TooManyMessages => formatter.write_str("stream chunk contains too many messages"),
        }
    }
}

impl std::error::Error for CodecError {}
