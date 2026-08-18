//! The `Transport` implementation: `medatat-sync` over HTTP.
//!
//! This crate exists because [`medatat_sync::Transport`] is deliberately a trait with no
//! network dependency — that is what lets the sync engine be tested against a mock with no
//! server, and it is a decision worth keeping. But a trait with only mock implementations
//! is a design that has never been connected to anything, which is what this fixes.
//!
//! Everything here is mechanical: build a URL, attach the bearer token, parse the
//! documented envelope from `docs/03-API.md`. The judgement lives in `medatat-sync`.

use async_trait::async_trait;
use medatat_core::ids::{CaseId, CaseRev, ConfigRev};
use medatat_core::wire::{
    CasePage, CaseQuery, ConfigDelta, Envelope, ErrorCode, PutValuesReq, PutValuesResp, ValuePage,
};
use medatat_sync::{Transport, TransportError};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A session token that can be replaced without rebuilding the client — a re-auth after an
/// idle timeout must not require tearing down the sync engine and losing its state.
#[derive(Clone, Default)]
pub struct TokenHolder(Arc<RwLock<Option<String>>>);

impl TokenHolder {
    pub fn new(token: Option<String>) -> Self {
        TokenHolder(Arc::new(RwLock::new(token)))
    }
    pub fn set(&self, token: Option<String>) {
        *self.0.write().expect("token lock") = token;
    }
    pub fn get(&self) -> Option<String> {
        self.0.read().expect("token lock").clone()
    }
}

pub struct HttpTransport {
    base: String,
    client: reqwest::Client,
    token: TokenHolder,
}

impl HttpTransport {
    pub fn new(base: impl Into<String>, token: TokenHolder) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            // A hung request must not stall the drain loop indefinitely; the loop's own
            // backoff is the retry policy, not a socket that never closes.
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| TransportError::Malformed(e.to_string()))?;
        Ok(HttpTransport {
            base: base.into().trim_end_matches('/').to_string(),
            client,
            token,
        })
    }

    pub fn token_holder(&self) -> TokenHolder {
        self.token.clone()
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&(impl serde::Serialize + ?Sized)>,
    ) -> Result<Option<T>, TransportError> {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base, path));
        if let Some(t) = self.token.get() {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        let resp = req.send().await.map_err(classify_send)?;
        let status = resp.status();

        // 304 is a legitimate "nothing changed" for the config poll, not an error.
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }

        let text = resp.text().await.map_err(classify_send)?;
        let env: Envelope<T> = serde_json::from_str(&text)
            .map_err(|e| TransportError::Malformed(format!("{e}: {text}")))?;

        if env.ok {
            return Ok(env.data);
        }
        Err(from_api_error(status.as_u16(), env.error))
    }
}

/// A transport-level failure. Anything that is not a clean HTTP exchange is treated as
/// `Offline` — a normal operating state the UI shows quietly rather than an error that
/// blocks work. See `docs/04-SYNC.md`.
fn classify_send(e: reqwest::Error) -> TransportError {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        TransportError::Offline
    } else {
        TransportError::Malformed(e.to_string())
    }
}

fn from_api_error(status: u16, err: Option<medatat_core::wire::ApiError>) -> TransportError {
    let message = err
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_else(|| format!("HTTP {status}"));
    match err.as_ref().map(|e| e.code) {
        Some(ErrorCode::Unauthorized) => TransportError::Unauthorized,
        Some(ErrorCode::RateLimited) => TransportError::RateLimited(Duration::from_secs(60)),
        _ => TransportError::Server { status, message },
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn config(&self, since: ConfigRev) -> Result<Option<ConfigDelta>, TransportError> {
        self.send::<ConfigDelta>(
            reqwest::Method::GET,
            &format!("/config?since_rev={}", since.0),
            None::<&()>,
        )
        .await
    }

    async fn list_cases(&self, q: CaseQuery) -> Result<CasePage, TransportError> {
        let mut path = String::from("/cases?");
        if let Some(a) = &q.assignee {
            path.push_str(&format!("assignee={}&", urlencode(a)));
        }
        if let Some(c) = &q.since {
            path.push_str(&format!("since={}&", urlencode(c)));
        }
        if let Some(l) = q.limit {
            path.push_str(&format!("limit={l}"));
        }
        self.send(reqwest::Method::GET, &path, None::<&()>)
            .await?
            .ok_or_else(|| TransportError::Malformed("empty case page".into()))
    }

    async fn get_values(
        &self,
        case_id: CaseId,
        since_rev: CaseRev,
    ) -> Result<ValuePage, TransportError> {
        self.send(
            reqwest::Method::GET,
            &format!("/cases/{case_id}/values?since_rev={}", since_rev.0),
            None::<&()>,
        )
        .await?
        .ok_or_else(|| TransportError::Malformed("empty value page".into()))
    }

    async fn put_values(
        &self,
        case_id: CaseId,
        req: PutValuesReq,
    ) -> Result<PutValuesResp, TransportError> {
        // A 409 is a documented outcome carrying the server's rows, not a failure — so it
        // is decoded from the error envelope rather than surfaced as TransportError.
        let path = format!("/cases/{case_id}/values");
        let mut r = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&req);
        if let Some(t) = self.token.get() {
            r = r.bearer_auth(t);
        }
        let resp = r.send().await.map_err(classify_send)?;
        let status = resp.status();
        let text = resp.text().await.map_err(classify_send)?;

        if status == reqwest::StatusCode::CONFLICT {
            let env: Envelope<serde_json::Value> = serde_json::from_str(&text)
                .map_err(|e| TransportError::Malformed(format!("{e}: {text}")))?;
            let detail = env
                .error
                .and_then(|e| e.detail)
                .ok_or_else(|| TransportError::Malformed("409 without detail".into()))?;
            return serde_json::from_value::<PutValuesResp>(detail)
                .map_err(|e| TransportError::Malformed(e.to_string()));
        }

        let env: Envelope<PutValuesResp> = serde_json::from_str(&text)
            .map_err(|e| TransportError::Malformed(format!("{e}: {text}")))?;
        if env.ok {
            env.data
                .ok_or_else(|| TransportError::Malformed("empty put response".into()))
        } else {
            Err(from_api_error(status.as_u16(), env.error))
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_can_be_replaced_without_rebuilding_the_client() {
        // Re-auth after an idle timeout must not require tearing down the sync engine.
        let h = TokenHolder::new(None);
        assert_eq!(h.get(), None);
        h.set(Some("abc".into()));
        assert_eq!(h.get().as_deref(), Some("abc"));
    }

    #[test]
    fn unauthorized_maps_to_its_own_variant_so_reauth_can_trigger() {
        let e = from_api_error(
            401,
            Some(medatat_core::wire::ApiError::new(
                ErrorCode::Unauthorized,
                "nope",
            )),
        );
        assert!(matches!(e, TransportError::Unauthorized));
    }

    #[test]
    fn server_errors_carry_their_status_for_the_retry_policy() {
        let e = from_api_error(
            503,
            Some(medatat_core::wire::ApiError::new(
                ErrorCode::Internal,
                "down",
            )),
        );
        match e {
            TransportError::Server { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn base_url_trailing_slash_does_not_double_up() {
        let t = HttpTransport::new("http://x/", TokenHolder::default()).unwrap();
        assert_eq!(t.base, "http://x");
    }
}
