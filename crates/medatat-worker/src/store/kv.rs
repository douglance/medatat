//! Auth codes, sessions, and rate windows in Workers KV.
//!
//! Only hashes are stored: `sha256(code)` under `auth:{email}` and `sha256(token)` under
//! `session:{hash}`. Expiry is KV's `expirationTtl`; `logic/auth.rs` re-checks it so the
//! rule is testable without a live namespace.

use crate::error::{LogicError, LogicResult};
use crate::logic::auth::{
    AuthCodeRecord, RATE_WINDOW_SECONDS, RateWindow, SessionRecord, code_key, email_rate_key,
    ip_rate_key, session_key,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use worker::kv::KvStore;

pub struct AuthKv {
    kv: KvStore,
}

impl AuthKv {
    pub fn new(kv: KvStore) -> Self {
        AuthKv { kv }
    }

    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> LogicResult<Option<T>> {
        self.kv
            .get(key)
            .json::<T>()
            .await
            .map_err(|e| LogicError::Storage(format!("kv get {key}: {e}")))
    }

    async fn put_json<T: Serialize>(&self, key: &str, value: &T, ttl: u64) -> LogicResult<()> {
        let body = serde_json::to_string(value)
            .map_err(|e| LogicError::Internal(format!("kv encode {key}: {e}")))?;
        self.kv
            .put(key, body)
            .map_err(|e| LogicError::Storage(format!("kv put {key}: {e}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| LogicError::Storage(format!("kv put {key}: {e}")))
    }

    async fn delete(&self, key: &str) -> LogicResult<()> {
        self.kv
            .delete(key)
            .await
            .map_err(|e| LogicError::Storage(format!("kv delete {key}: {e}")))
    }

    pub async fn get_code(&self, email: &str) -> LogicResult<Option<AuthCodeRecord>> {
        self.get_json(&code_key(email)).await
    }

    pub async fn put_code(&self, email: &str, rec: &AuthCodeRecord, ttl: u64) -> LogicResult<()> {
        self.put_json(&code_key(email), rec, ttl).await
    }

    pub async fn delete_code(&self, email: &str) -> LogicResult<()> {
        self.delete(&code_key(email)).await
    }

    pub async fn get_session(&self, token_hash: &str) -> LogicResult<Option<SessionRecord>> {
        self.get_json(&session_key(token_hash)).await
    }

    pub async fn put_session(
        &self,
        token_hash: &str,
        rec: &SessionRecord,
        ttl: u64,
    ) -> LogicResult<()> {
        self.put_json(&session_key(token_hash), rec, ttl).await
    }

    pub async fn delete_session(&self, token_hash: &str) -> LogicResult<()> {
        self.delete(&session_key(token_hash)).await
    }

    /// Advance a fixed rate window, or fail with [`LogicError::RateLimited`].
    async fn bump(&self, key: &str, limit: u32, now: DateTime<Utc>) -> LogicResult<()> {
        let current: Option<RateWindow> = self.get_json(key).await?;
        let next = crate::logic::auth::check_rate(current.as_ref(), now, limit)?;
        self.put_json(key, &next, RATE_WINDOW_SECONDS as u64).await
    }

    pub async fn bump_email_rate(
        &self,
        email: &str,
        limit: u32,
        now: DateTime<Utc>,
    ) -> LogicResult<()> {
        self.bump(&email_rate_key(email), limit, now).await
    }

    pub async fn bump_ip_rate(&self, ip: &str, limit: u32, now: DateTime<Utc>) -> LogicResult<()> {
        self.bump(&ip_rate_key(ip), limit, now).await
    }

    /// `last_seen` is updated at most once per minute: KV allows one write per second per
    /// key, and a write on every request would blow straight through that.
    pub async fn touch_last_seen(&self, user_id: &str, now: DateTime<Utc>) -> LogicResult<()> {
        let key = format!("seen:{user_id}");
        if self.get_json::<String>(&key).await?.is_some() {
            return Ok(());
        }
        self.put_json(&key, &crate::logic::auth::rfc3339(now), 60)
            .await
    }
}
