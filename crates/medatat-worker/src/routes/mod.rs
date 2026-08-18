//! The binding layer: parse, authenticate, call `logic/`, encode.
//!
//! Nothing here decides anything. If a rule is being applied in this directory rather than
//! delegated to `logic/`, it is in the wrong place and it is not being tested.

pub mod auth;
pub mod bulk;
pub mod cases;
pub mod config;

use crate::error::LogicError;
use crate::logic::auth::{Account, bearer_token, hash_token, session_is_live};
use crate::store::d1::Db;
use crate::store::kv::AuthKv;
use chrono::Utc;
use serde::Serialize;
use worker::{Env, Request, Response, Result};

pub const DB_BINDING: &str = "DB";
pub const AUTH_BINDING: &str = "AUTH";
pub const EMAIL_BINDING: &str = "EMAIL";
pub const CASE_BINDING: &str = "CASE";

pub fn db(env: &Env) -> std::result::Result<Db, LogicError> {
    env.d1(DB_BINDING)
        .map(Db::new)
        .map_err(|e| LogicError::Storage(format!("D1 binding: {e}")))
}

pub fn auth_kv(env: &Env) -> std::result::Result<AuthKv, LogicError> {
    env.kv(AUTH_BINDING)
        .map(AuthKv::new)
        .map_err(|e| LogicError::Storage(format!("KV binding: {e}")))
}

pub fn env_var(env: &Env, name: &str, fallback: &str) -> String {
    env.var(name)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| fallback.to_string())
}

/// Turn a decided [`crate::http::Rendered`] into a `worker::Response`. This is the only
/// place in the crate that constructs one, so the status table in `docs/03-API.md` has
/// exactly one implementation and it is the one `http.rs` tests.
pub fn render(r: crate::http::Rendered) -> Result<Response> {
    let builder = Response::builder().with_status(r.status);
    match r.body {
        None => Ok(builder.empty()),
        Some(body) => Ok(builder
            .with_header("content-type", "application/json")?
            .fixed(body.into_bytes())),
    }
}

/// Encode a success body. `Envelope.ok` is redundant with the status by design — CLI and
/// shell consumers assert on it without inspecting headers.
pub fn json<T: Serialize>(data: T, status: u16) -> Result<Response> {
    render(match status {
        201 => crate::http::created(data),
        _ => crate::http::ok(data),
    })
}

pub fn no_content() -> Result<Response> {
    render(crate::http::no_content())
}

pub fn not_modified() -> Result<Response> {
    render(crate::http::not_modified())
}

/// Encode a failure. The status comes from the error code table in `docs/03-API.md`, in
/// exactly one place.
pub fn fail(e: LogicError) -> Result<Response> {
    render(crate::http::error(e))
}

/// A caller proven to hold a live session.
pub struct Session {
    pub account: Account,
    pub token_hash: String,
}

/// Resolve the actor from the bearer token. **This is the only way an `actor_id` enters the
/// system** — no request body is ever consulted for it.
pub async fn require_session(req: &Request, env: &Env) -> std::result::Result<Session, LogicError> {
    let header = req
        .headers()
        .get("Authorization")
        .map_err(|e| LogicError::Internal(format!("headers: {e}")))?;
    let token = bearer_token(header.as_deref()).ok_or(LogicError::Unauthorized)?;
    let token_hash = hash_token(token);

    let record = auth_kv(env)?
        .get_session(&token_hash)
        .await?
        .ok_or(LogicError::Unauthorized)?;
    if !session_is_live(&record, Utc::now()) {
        return Err(LogicError::Unauthorized);
    }

    let account = db(env)?
        .account_by_id(&record.user_id)
        .await?
        .ok_or(LogicError::Unauthorized)?;
    if !account.is_active {
        return Err(LogicError::Unauthorized);
    }

    Ok(Session {
        account,
        token_hash,
    })
}

/// Config mutations and bulk endpoints are admin-only.
pub async fn require_admin(req: &Request, env: &Env) -> std::result::Result<Session, LogicError> {
    let session = require_session(req, env).await?;
    if !session.account.user.role.is_admin() {
        return Err(LogicError::Forbidden(
            "this endpoint requires the admin role".into(),
        ));
    }
    Ok(session)
}

/// One query parameter, by name.
pub fn query_param(req: &Request, key: &str) -> Option<String> {
    crate::http::query_value(&raw_query(req), key)
}

/// Numeric query parameters propagate their parse error. They deliberately do **not**
/// return a bare `Option`: `.ok().flatten()` on a strict parser turns "you sent nonsense"
/// into "you sent nothing", and every default in this crate — a full config read, a full
/// case read, a default page size — is more expensive than the request the client meant.
/// Keeping `Result` in the signature is what stops the next caller from re-introducing it.
pub fn query_i64(req: &Request, key: &str) -> std::result::Result<Option<i64>, LogicError> {
    crate::http::parse_i64(&raw_query(req), key)
}

pub fn query_u32(req: &Request, key: &str) -> std::result::Result<Option<u32>, LogicError> {
    crate::http::parse_u32(&raw_query(req), key)
}

/// An unparseable `since_rev` is a client error, not a silent full read — a full read
/// would quietly ship every value in the case.
pub fn since_rev(
    req: &Request,
) -> std::result::Result<Option<medatat_core::ids::CaseRev>, LogicError> {
    crate::http::parse_since_rev(&raw_query(req))
}

pub fn raw_query(req: &Request) -> String {
    req.url()
        .ok()
        .and_then(|u| u.query().map(str::to_string))
        .unwrap_or_default()
}

/// A path parameter that must parse as a UUID-backed id.
pub fn path_id<T, E>(
    raw: Option<&String>,
    parse: impl Fn(&str) -> std::result::Result<T, E>,
    what: &str,
) -> std::result::Result<T, LogicError> {
    let raw = raw.ok_or_else(|| LogicError::NotFound(format!("missing {what}")))?;
    parse(raw).map_err(|_| LogicError::NotFound(format!("malformed {what}")))
}

/// Deserialize a request body, turning a parse failure into a 422 rather than a 500.
pub async fn body_json<T: serde::de::DeserializeOwned>(
    req: &mut Request,
) -> std::result::Result<T, LogicError> {
    req.json::<T>()
        .await
        .map_err(|e| LogicError::Validation(format!("malformed request body: {e}")))
}

pub async fn body_value(req: &mut Request) -> std::result::Result<serde_json::Value, LogicError> {
    body_json(req).await
}

/// The connecting client's address, for IP-scoped rate limits.
pub fn client_ip(req: &Request) -> String {
    req.headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string())
}
