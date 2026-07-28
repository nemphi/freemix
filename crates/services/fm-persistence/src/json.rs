mod decoder;
mod encoder;
mod reader;

use crate::ProjectValidationError;

pub(crate) use decoder::{
    decode, decode_v2, decode_v3, decode_v4, decode_v5, decode_v6, decode_v7,
};
pub(crate) use encoder::encode;

#[derive(Debug)]
pub(crate) enum DecodeError {
    Syntax { offset: usize, message: String },
    Validation(ProjectValidationError),
}
