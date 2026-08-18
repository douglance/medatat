//! `/cases/*` (R2, R16). List and create hit the D1 worklist index; values proxy to the
//! `CaseDO`, which owns the case's revision counter and its ~1000 values.

use super::{
    CASE_BINDING, body_json, db, fail, json, path_id, query_param, query_u32, require_session,
};
use crate::case_do::{BASE_URL, DoInitReq, DoPutReq};
use crate::error::LogicError;
use crate::logic::cases::{check_mrn, effective_limit, resolve_assignee};
use medatat_core::ids::{ActorId, CaseId, CaseRev};
use medatat_core::wire::{CreateCaseReq, CreateCaseResp, PutValuesReq};
use worker::{Env, Method, Request, RequestInit, Response, Result, RouteContext, Stub};

/// Addressed by name, so the same `case_id` always reaches the same object without an id
/// round-trip through D1.
///
/// `docs/02-DATA-MODEL.md` calls for `jurisdiction("us")`, but the Workers API allows a
/// jurisdiction only on `newUniqueId()` — it is explicitly incompatible with
/// `idFromName()`. Deterministic addressing is the property the rest of the system depends
/// on, so it wins; a jurisdiction constraint would require storing the generated id in
/// `case_index` and looking it up on every call.
fn case_stub(env: &Env, case_id: CaseId) -> std::result::Result<Stub, LogicError> {
    env.durable_object(CASE_BINDING)
        .and_then(|ns| ns.id_from_name(&case_id.to_string())?.get_stub())
        .map_err(|e| LogicError::Storage(format!("CASE binding: {e}")))
}

async fn call_do(
    stub: &Stub,
    method: Method,
    path: &str,
    body: Option<String>,
) -> std::result::Result<Response, LogicError> {
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));
    }
    let request = Request::new_with_init(&format!("{BASE_URL}{path}"), &init)
        .map_err(|e| LogicError::Internal(format!("DO request: {e}")))?;
    stub.fetch_with_request(request)
        .await
        .map_err(|e| LogicError::Storage(format!("DO fetch: {e}")))
}

/// Reads `case_index` in D1. **Eventually consistent** — see `docs/10-LIMITATIONS.md` #2.
pub async fn list_cases(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let session = match require_session(&req, &ctx.env).await {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let assignee = resolve_assignee(
        query_param(&req, "assignee").as_deref(),
        &session.account.user.user_id,
    );
    let result = async {
        db(&ctx.env)?
            .list_cases(
                assignee.as_deref(),
                query_param(&req, "since").as_deref(),
                effective_limit(query_u32(&req, "limit")?),
            )
            .await
    }
    .await;
    match result {
        Ok(page) => json(page, 200),
        Err(e) => fail(e),
    }
}

/// Inserts the `case_index` row and materialises the DO's metadata. The object itself only
/// truly exists once something is written to it.
pub async fn create_case(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_session(&req, &ctx.env).await {
        return fail(e);
    }
    let result: std::result::Result<CreateCaseResp, LogicError> = async {
        let body: CreateCaseReq = body_json(&mut req).await?;
        check_mrn(&body.mrn)?;
        let case_id = CaseId::new();
        db(&ctx.env)?
            .insert_case(case_id, &body.mrn, body.form_id, body.assignee.as_deref())
            .await?;

        let init = DoInitReq {
            case_id,
            mrn: body.mrn,
            form_id: body.form_id,
            assignee: body.assignee,
        };
        let payload = serde_json::to_string(&init)
            .map_err(|e| LogicError::Internal(format!("init encode: {e}")))?;
        let stub = case_stub(&ctx.env, case_id)?;
        call_do(&stub, Method::Post, "/init", Some(payload)).await?;

        Ok(CreateCaseResp {
            case_id,
            rev: CaseRev::ZERO,
        })
    }
    .await;
    match result {
        Ok(v) => json(v, 201),
        Err(e) => fail(e),
    }
}

pub async fn get_values(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_session(&req, &ctx.env).await {
        return fail(e);
    }
    let case_id = match path_id(ctx.param("case_id"), CaseId::parse, "case_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    // An unparseable `since_rev` is a client error rather than a silent full read, which
    // would quietly ship every value in the case.
    let since = match super::since_rev(&req) {
        Ok(v) => v.map(|r| format!("&since_rev={}", r.0)).unwrap_or_default(),
        Err(e) => return fail(e),
    };
    let path = format!("/values?case_id={case_id}{since}");

    match case_stub(&ctx.env, case_id) {
        Ok(stub) => match call_do(&stub, Method::Get, &path, None).await {
            Ok(resp) => Ok(resp),
            Err(e) => fail(e),
        },
        Err(e) => fail(e),
    }
}

/// The write path. `actor_id` is resolved here from the bearer token and travels to the DO
/// alongside the batch; the request body is never consulted for it. Field definitions are
/// loaded from D1 for the same reason — a client must not be able to widen the rules its
/// own values are checked against.
pub async fn put_values(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let session = match require_session(&req, &ctx.env).await {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let case_id = match path_id(ctx.param("case_id"), CaseId::parse, "case_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };

    let prepared: std::result::Result<String, LogicError> = async {
        let body: PutValuesReq = body_json(&mut req).await?;
        let defs = db(&ctx.env)?.field_defs().await?;
        let payload = DoPutReq {
            case_id,
            actor: ActorId::new(session.account.user.user_id.clone()),
            defs,
            req: body,
        };
        serde_json::to_string(&payload)
            .map_err(|e| LogicError::Internal(format!("put encode: {e}")))
    }
    .await;

    let payload = match prepared {
        Ok(p) => p,
        Err(e) => return fail(e),
    };

    match case_stub(&ctx.env, case_id) {
        Ok(stub) => {
            match call_do(
                &stub,
                Method::Post,
                &format!("/values?case_id={case_id}"),
                Some(payload),
            )
            .await
            {
                // The DO already produced the correct status — 200, 409, or 422 — so it is
                // returned verbatim rather than re-encoded here.
                Ok(resp) => Ok(resp),
                Err(e) => fail(e),
            }
        }
        Err(e) => fail(e),
    }
}
