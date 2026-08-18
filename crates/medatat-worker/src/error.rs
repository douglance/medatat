//! The Worker's error type and its mapping onto the wire contract.
//!
//! Every failure inside `logic/` and `store/` is a [`LogicError`]. The binding layer turns
//! one into an [`ApiError`] plus an HTTP status with [`LogicError::into_api_error`], so the
//! status codes in `docs/03-API.md` are produced in exactly one place.

use medatat_core::error::FieldError;
use medatat_core::ids::FieldId;
use medatat_core::wire::{ApiError, ErrorCode};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LogicError {
    /// Missing, expired, or invalid bearer token.
    #[error("unauthorized")]
    Unauthorized,

    /// A write reached the value path without an actor resolved from the session token.
    /// `actor_id` is never read from the request body, so this is a bug, not a client error.
    #[error("no authenticated actor for this write")]
    NoActor,

    /// Authenticated, but the role does not permit this.
    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    /// A field id in the batch is not in the configuration.
    #[error("unknown field: {0}")]
    UnknownField(FieldId),

    /// A value failed `medatat_core::validate`. Carries the offending field.
    #[error("field {} rejected: {}", .0.field_id, .0.error)]
    Invalid(FieldError),

    /// A validation failure that is not attributable to a single stored value —
    /// `col_span > columns`, a `kind` patch, a malformed body.
    #[error("{0}")]
    Validation(String),

    #[error("too many attempts")]
    RateLimited,

    /// Storage said no. Never surfaced verbatim to a client.
    #[error("storage failure: {0}")]
    Storage(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl LogicError {
    pub fn code(&self) -> ErrorCode {
        match self {
            LogicError::Unauthorized => ErrorCode::Unauthorized,
            // A write with no resolved actor is a server-side auth failure, not a 500:
            // the only way to reach it is an unauthenticated write path.
            LogicError::NoActor => ErrorCode::Unauthorized,
            LogicError::Forbidden(_) => ErrorCode::Forbidden,
            LogicError::NotFound(_) | LogicError::UnknownField(_) => ErrorCode::NotFound,
            LogicError::Invalid(_) | LogicError::Validation(_) => ErrorCode::Validation,
            LogicError::RateLimited => ErrorCode::RateLimited,
            LogicError::Storage(_) | LogicError::Internal(_) => ErrorCode::Internal,
        }
    }

    pub fn http_status(&self) -> u16 {
        self.code().http_status()
    }

    /// The client-facing rendering. Storage and internal details are deliberately dropped:
    /// they can carry query text, and query text can carry values.
    pub fn into_api_error(self) -> ApiError {
        let code = self.code();
        match self {
            LogicError::Invalid(fe) => ApiError::new(code, fe.error.to_string())
                .with_detail(serde_json::json!({ "field_id": fe.field_id, "error": fe.error })),
            LogicError::Storage(_) | LogicError::Internal(_) => {
                ApiError::new(code, "internal error")
            }
            other => ApiError::new(code, other.to_string()),
        }
    }

    pub fn invalid(field_id: FieldId, error: medatat_core::error::ValidationError) -> Self {
        LogicError::Invalid(FieldError { field_id, error })
    }
}

pub type LogicResult<T> = Result<T, LogicError>;

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::error::ValidationError;

    #[test]
    fn statuses_match_the_documented_table() {
        assert_eq!(LogicError::Unauthorized.http_status(), 401);
        assert_eq!(LogicError::NoActor.http_status(), 401);
        assert_eq!(LogicError::Forbidden("x".into()).http_status(), 403);
        assert_eq!(LogicError::NotFound("x".into()).http_status(), 404);
        assert_eq!(LogicError::Validation("x".into()).http_status(), 422);
        assert_eq!(LogicError::RateLimited.http_status(), 429);
        assert_eq!(LogicError::Storage("x".into()).http_status(), 500);
    }

    #[test]
    fn invalid_carries_the_offending_field_id() {
        let id = FieldId::new();
        let api = LogicError::invalid(id, ValidationError::WrongType).into_api_error();
        let detail = api.detail.expect("detail");
        assert_eq!(detail["field_id"], serde_json::json!(id));
    }

    #[test]
    fn storage_detail_never_reaches_the_client() {
        let api = LogicError::Storage("SELECT value_text FROM field_value".into()).into_api_error();
        assert_eq!(api.message, "internal error");
        assert!(api.detail.is_none());
    }
}
