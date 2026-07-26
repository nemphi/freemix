mod decoder;
mod encoder;
mod reader;

use crate::ProjectValidationError;

pub(crate) use decoder::{decode, decode_v1, decode_v2};
pub(crate) use encoder::encode;

#[derive(Debug)]
pub(crate) enum DecodeError {
    Syntax { offset: usize, message: String },
    Validation(ProjectValidationError),
}
