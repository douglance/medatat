//! The network seam.
//!
//! This trait is why `medatat-sync` has no `reqwest` dependency: the engine tests against
//! a mock with a fake clock, and the same engine drives the real client and the CLI.

use async_trait::async_trait;
use medatat_core::ids::{CaseId, CaseRev, ConfigRev};
use medatat_core::wire::{
    CasePage, CaseQuery, ConfigDelta, PutValuesReq, PutValuesResp, ValuePage,
};
use std::time::Duration;
use thiserror::Error;

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn config(&self, since: ConfigRev) -> Result<Option<ConfigDelta>, TransportError>;
    async fn list_cases(&self, q: CaseQuery) -> Result<CasePage, TransportError>;
    async fn get_values(
        &self,
        case_id: CaseId,
        since_rev: CaseRev,
    ) -> Result<ValuePage, TransportError>;
    async fn put_values(
        &self,
        case_id: CaseId,
        req: PutValuesReq,
    ) -> Result<PutValuesResp, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    /// A normal operating state, not a failure. The UI shows an unobtrusive indicator and
    /// nothing blocks.
    #[error("offline")]
    Offline,
    /// The session expired. Triggers in-app re-auth; form state stays in memory.
    #[error("unauthorized")]
    Unauthorized,
    #[error("rate limited, retry after {0:?}")]
    RateLimited(Duration),
    #[error("server error {status}: {message}")]
    Server { status: u16, message: String },
    #[error("malformed response: {0}")]
    Malformed(String),
}

impl TransportError {
    /// Whether retrying this request could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            TransportError::Offline | TransportError::RateLimited(_) => true,
            TransportError::Server { status, .. } => *status >= 500,
            TransportError::Unauthorized | TransportError::Malformed(_) => false,
        }
    }
}
