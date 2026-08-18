//! `/auth/*` (R1). Every decision is in `logic/auth.rs`; this file only moves bytes.

use super::{auth_kv, body_json, client_ip, db, env_var, fail, json, no_content, require_session};
use crate::error::LogicError;
use crate::logic::auth::{
    REQUESTS_PER_EMAIL, REQUESTS_PER_IP, RequestOutcome, VerifyOutcome, handle_auth_request,
    mint_session, normalize_email, verify_code,
};
use crate::mail::{BindingMailer, Mailer};
use chrono::Utc;
use medatat_core::wire::{AuthRequestReq, AuthVerifyReq, AuthVerifyResp};
use worker::{Env, Request, Response, Result, RouteContext};

/// **Always 204**, whether or not the account exists. The only exception is the rate limit,
/// which leaks nothing because it is scoped to an IP and an email the caller already named.
pub async fn request_code(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match issue_code(&mut req, &ctx.env).await {
        Ok(()) => no_content(),
        Err(e @ LogicError::RateLimited) => fail(e),
        // A storage or mail failure must not become an oracle either: the caller learns
        // nothing about the account from a 500 it could have provoked any other way.
        Err(e) => {
            worker::console_error!("auth/request failed: {e}");
            no_content()
        }
    }
}

async fn issue_code(req: &mut Request, env: &Env) -> std::result::Result<(), LogicError> {
    let body: AuthRequestReq = body_json(req).await?;
    let email = normalize_email(&body.email);
    let now = Utc::now();

    let kv = auth_kv(env)?;
    kv.bump_ip_rate(&client_ip(req), REQUESTS_PER_IP, now)
        .await?;
    kv.bump_email_rate(&email, REQUESTS_PER_EMAIL, now).await?;

    let account = db(env)?.account_by_email(&email).await?;
    match handle_auth_request(account.as_ref(), now)? {
        RequestOutcome::Silent => Ok(()),
        RequestOutcome::Issue {
            code,
            record,
            ttl_seconds,
        } => {
            kv.put_code(&email, &record, ttl_seconds).await?;
            let mailer = BindingMailer::new(
                env.send_email(super::EMAIL_BINDING)
                    .map_err(|e| LogicError::Storage(format!("send_email binding: {e}")))?,
                env_var(env, "MAIL_FROM", "noreply@cetify.email"),
                env_var(env, "MAIL_FROM_NAME", "medatat"),
            );
            mailer.send_code(&email, &code).await
        }
    }
}

pub async fn verify(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match do_verify(&mut req, &ctx.env).await {
        Ok(resp) => json(resp, 200),
        Err(e) => fail(e),
    }
}

async fn do_verify(
    req: &mut Request,
    env: &Env,
) -> std::result::Result<AuthVerifyResp, LogicError> {
    let body: AuthVerifyReq = body_json(req).await?;
    let email = normalize_email(&body.email);
    let now = Utc::now();

    let kv = auth_kv(env)?;
    let record = kv.get_code(&email).await?;
    let outcome = verify_code(record.as_ref(), &body.code, now);

    match outcome {
        VerifyOutcome::Retry { record } => {
            // Persist the attempt count before answering, so a client that hangs up early
            // does not get a free retry.
            kv.put_code(&email, &record, remaining_ttl(&record.issued_at, now))
                .await?;
            Err(LogicError::Unauthorized)
        }
        VerifyOutcome::NoCode => Err(LogicError::Unauthorized),
        VerifyOutcome::Expired | VerifyOutcome::Locked => {
            kv.delete_code(&email).await?;
            Err(LogicError::Unauthorized)
        }
        VerifyOutcome::Accept => {
            // Codes are single-use: the key goes before the session is minted.
            kv.delete_code(&email).await?;
            let account = db(env)?
                .account_by_email(&email)
                .await?
                .filter(|a| a.is_active)
                .ok_or(LogicError::Unauthorized)?;

            let session = mint_session(&account.user.user_id, now)?;
            kv.put_session(&session.token_hash, &session.record, session.ttl_seconds)
                .await?;
            Ok(AuthVerifyResp {
                token: session.token,
                user: account.user,
                expires_at: session.record.expires_at,
            })
        }
    }
}

/// Keep the remaining life of the original code rather than extending it on every wrong
/// guess — otherwise `MAX_ATTEMPTS` wrong guesses could keep a code alive indefinitely.
fn remaining_ttl(issued_at: &str, now: chrono::DateTime<Utc>) -> u64 {
    use crate::logic::auth::{CODE_TTL_SECONDS, parse_rfc3339};
    let Ok(issued) = parse_rfc3339(issued_at) else {
        return 1;
    };
    let elapsed = now.signed_duration_since(issued).num_seconds().max(0) as u64;
    CODE_TTL_SECONDS.saturating_sub(elapsed).max(1)
}

pub async fn logout(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let Ok(session) = require_session(&req, &ctx.env).await else {
        // Logging out without a valid session is already the desired end state.
        return no_content();
    };
    match auth_kv(&ctx.env) {
        Ok(kv) => match kv.delete_session(&session.token_hash).await {
            Ok(()) => no_content(),
            Err(e) => fail(e),
        },
        Err(e) => fail(e),
    }
}

pub async fn me(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match require_session(&req, &ctx.env).await {
        Ok(session) => {
            // `last_seen` is throttled to at most one write per minute: KV allows one write
            // per second per key and a per-request write would exceed it.
            if let Ok(kv) = auth_kv(&ctx.env) {
                let _ = kv
                    .touch_last_seen(&session.account.user.user_id, Utc::now())
                    .await;
            }
            json(session.account.user, 200)
        }
        Err(e) => fail(e),
    }
}
