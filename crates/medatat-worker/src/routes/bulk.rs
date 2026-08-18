//! `/bulk/*` and `/admin/*` — seeding, export, and index repair. Admin only.

use super::{body_json, db, fail, json, query_param, query_u32, require_admin};
use crate::error::LogicError;
use crate::logic::cases::{check_bulk_size, check_mrn, effective_limit};
use medatat_core::ids::{CaseId, CaseRev, FormId};
use medatat_core::wire::CreateCaseReq;
use serde::{Deserialize, Serialize};
use worker::{Request, Response, Result, RouteContext};

#[derive(Debug, Deserialize)]
pub struct BulkCasesReq {
    pub cases: Vec<CreateCaseReq>,
}

#[derive(Debug, Serialize)]
pub struct BulkCasesResp {
    pub created: Vec<CaseId>,
}

pub async fn create_cases(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let result: std::result::Result<BulkCasesResp, LogicError> = async {
        let body: BulkCasesReq = body_json(&mut req).await?;
        check_bulk_size(body.cases.len())?;
        for case in &body.cases {
            check_mrn(&case.mrn)?;
        }
        let db = db(&ctx.env)?;
        let mut created = Vec::with_capacity(body.cases.len());
        for case in body.cases {
            let case_id = CaseId::new();
            db.insert_case(case_id, &case.mrn, case.form_id, case.assignee.as_deref())
                .await?;
            created.push(case_id);
        }
        Ok(BulkCasesResp { created })
    }
    .await;
    match result {
        Ok(v) => json(v, 201),
        Err(e) => fail(e),
    }
}

/// Streams the worklist index as NDJSON. Values stay in their DOs; this is a manifest, not
/// an export of clinical data. See `docs/10-LIMITATIONS.md` #1.
pub async fn export(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let result: std::result::Result<String, LogicError> = async {
        let form_id = match query_param(&req, "form_id") {
            Some(raw) => Some(
                FormId::parse(&raw)
                    .map_err(|_| LogicError::Validation("malformed form_id".into()))?,
            ),
            None => None,
        };
        let page = db(&ctx.env)?
            .export_page(
                form_id,
                query_param(&req, "cursor").as_deref(),
                effective_limit(query_u32(&req, "limit")),
            )
            .await?;
        let mut out = String::new();
        for case in &page.cases {
            out.push_str(
                &serde_json::to_string(case)
                    .map_err(|e| LogicError::Internal(format!("export encode: {e}")))?,
            );
            out.push('\n');
        }
        Ok(out)
    }
    .await;

    match result {
        Ok(body) => Ok(Response::builder()
            .with_header("content-type", "application/x-ndjson")?
            .fixed(body.into_bytes())),
        Err(e) => fail(e),
    }
}

#[derive(Debug, Serialize)]
pub struct ReindexJob {
    pub job_id: String,
    pub note: &'static str,
}

/// Walks the index and re-reads each DO's rev. Long-running, so it answers with a job id.
///
/// The walk itself is not implemented: it needs a Queue or an Alarm to survive the Worker
/// CPU budget across 100k objects, and neither is bound yet. Returning a job id for work
/// that has not started would be a lie, so this reports the gap instead.
pub async fn reindex(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let _ = CaseRev::ZERO;
    fail(LogicError::NotFound(
        "reindex is not implemented: it needs a Queue or Alarm binding to walk 100k objects \
         without exceeding the Worker CPU budget"
            .into(),
    ))
}
