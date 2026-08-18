//! Error types shared across client and Worker.

use crate::ids::FieldId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single field failing validation. Rendered inline next to that field.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ValidationError {
    #[error("this field is required")]
    Required,
    #[error("wrong value type for this field")]
    WrongType,
    #[error("must be at most {max} characters (got {actual})")]
    TooLong { max: u32, actual: u32 },
    #[error("must be at least {min}")]
    BelowMin { min: String },
    #[error("must be at most {max}")]
    AboveMax { max: String },
    #[error("at most {max} decimal places")]
    TooManyDecimals { max: u8 },
    #[error("not a valid 24-hour time")]
    BadTime,
    #[error("not a valid date")]
    BadDate,
    #[error("not a valid number")]
    BadNumber,
    #[error("\"{0}\" is not one of the allowed options")]
    UnknownOption(String),
}

/// Validation failure carrying the field it belongs to, for API responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub field_id: FieldId,
    pub error: ValidationError,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoreError {
    #[error("unknown field: {0}")]
    UnknownField(FieldId),
    #[error("field index {0} out of range")]
    IndexOutOfRange(u32),
}
