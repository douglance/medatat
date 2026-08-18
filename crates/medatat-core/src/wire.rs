//! Wire types shared by client, Worker, and CLI.
//!
//! There is deliberately no separate proto crate: a single client shipped alongside its
//! server does not need independent schema evolution, and a duplicate schema is somewhere
//! for the two halves to drift apart.

use crate::def::{FieldKind, FormDef};
use crate::error::FieldError;
use crate::ids::{ActorId, CaseId, CaseRev, ConfigRev, FieldId, FormId};
use crate::value::Value;
use serde::{Deserialize, Serialize};

/// Every response body. `ok` is redundant with the HTTP status by design — CLI and shell
/// consumers assert on it without inspecting headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl<T> Envelope<T> {
    pub fn ok(data: T) -> Self {
        Envelope {
            ok: true,
            data: Some(data),
            error: None,
        }
    }
    pub fn err(error: ApiError) -> Self {
        Envelope {
            ok: false,
            data: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    Validation,
    RateLimited,
    Internal,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::Unauthorized => 401,
            ErrorCode::Forbidden => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::Validation => 422,
            ErrorCode::RateLimited => 429,
            ErrorCode::Internal => 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            code,
            message: message.into(),
            detail: None,
        }
    }
    pub fn with_detail(mut self, d: serde_json::Value) -> Self {
        self.detail = Some(d);
        self
    }
}

// ---------------------------------------------------------------- auth

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequestReq {
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthVerifyReq {
    pub email: String,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthVerifyResp {
    pub token: String,
    pub user: UserInfo,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub user_id: String,
    pub email: String,
    pub display_name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Abstractor,
    Admin,
}

impl Role {
    pub fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Abstractor => "abstractor",
            Role::Admin => "admin",
        }
    }
}

// ---------------------------------------------------------------- config

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDelta {
    pub config_rev: ConfigRev,
    pub forms: Vec<FormDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFormReq {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSectionReq {
    pub name: String,
    pub ordinal: i32,
    /// 1..=3 (R12).
    pub columns: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSectionReq {
    pub name: Option<String>,
    pub ordinal: Option<i32>,
    /// Reducing this clamps every child `col_span` in the same transaction.
    pub columns: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFieldReq {
    pub key: String,
    /// `kind` is immutable after creation — a change means a replacement field.
    #[serde(flatten)]
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceFieldReq {
    pub field_id: FieldId,
    pub ordinal: i32,
    pub col_span: u8,
    pub label: String,
    #[serde(default)]
    pub required: bool,
}

// ---------------------------------------------------------------- cases

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseSummary {
    pub case_id: CaseId,
    pub mrn: String,
    pub form_id: FormId,
    pub assignee: Option<String>,
    pub rev: CaseRev,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaseQuery {
    pub assignee: Option<String>,
    pub since: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasePage {
    pub cases: Vec<CaseSummary>,
    pub cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCaseReq {
    pub mrn: String,
    pub form_id: FormId,
    pub assignee: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCaseResp {
    pub case_id: CaseId,
    pub rev: CaseRev,
}

// ---------------------------------------------------------------- values

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueRow {
    pub field_id: FieldId,
    pub value: Value,
    pub rev: CaseRev,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<ActorId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueChange {
    pub field_id: FieldId,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValuePage {
    pub case_id: CaseId,
    pub rev: CaseRev,
    pub values: Vec<ValueRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutValuesReq {
    pub base_rev: CaseRev,
    pub changes: Vec<ValueChange>,
}

/// Success or conflict. Conflict detection is per *field*: only a genuine same-field race
/// rejects, so two abstractors in different sections both succeed. See `docs/04-SYNC.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PutValuesResp {
    Applied {
        rev: CaseRev,
        applied: Vec<FieldId>,
    },
    Conflict {
        server_rev: CaseRev,
        conflicts: Vec<ValueRow>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationFailure {
    pub errors: Vec<FieldError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
    pub config_rev: ConfigRev,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_omits_empty_fields() {
        let e = Envelope::ok(Health {
            version: "1".into(),
            config_rev: ConfigRev(1),
        });
        let j = serde_json::to_string(&e).unwrap();
        assert!(!j.contains("error"), "empty error must be omitted: {j}");
    }

    #[test]
    fn put_values_resp_discriminates_by_shape() {
        let applied = PutValuesResp::Applied {
            rev: CaseRev(1),
            applied: vec![],
        };
        let j = serde_json::to_string(&applied).unwrap();
        assert!(matches!(
            serde_json::from_str::<PutValuesResp>(&j).unwrap(),
            PutValuesResp::Applied { .. }
        ));

        let conflict = PutValuesResp::Conflict {
            server_rev: CaseRev(9),
            conflicts: vec![],
        };
        let j = serde_json::to_string(&conflict).unwrap();
        assert!(matches!(
            serde_json::from_str::<PutValuesResp>(&j).unwrap(),
            PutValuesResp::Conflict { .. }
        ));
    }

    #[test]
    fn error_codes_map_to_documented_statuses() {
        assert_eq!(ErrorCode::Conflict.http_status(), 409);
        assert_eq!(ErrorCode::Validation.http_status(), 422);
        assert_eq!(ErrorCode::Unauthorized.http_status(), 401);
    }
}
