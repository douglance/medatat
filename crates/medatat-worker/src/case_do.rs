//! `CaseDO` — one Durable Object per case (`docs/adr/0001-durable-object-per-case.md`).
//!
//! **No DDL runs in the constructor.** `new` is on every cold-start path; putting
//! `CREATE TABLE` there would pay for schema creation on every wake of every one of ~100k
//! objects. `store/case_sql.rs` creates the schema lazily, on the first write only.
//!
//! The DO is single-threaded, so `meta.rev` is a race-free counter with no locking — the
//! `SELECT ... FOR UPDATE` a relational design would need simply does not exist here.

use crate::error::LogicError;
use crate::logic::case_store::{CaseStore, FieldLookup};
use crate::logic::values::{handle_get_values, handle_put_values};
use crate::routes::{fail, json};
use crate::store::case_sql::SqlCaseStore;
use medatat_core::def::FieldDef;
use medatat_core::ids::{ActorId, CaseId, CaseRev, FormId};
use medatat_core::wire::{PutValuesReq, PutValuesResp};
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen;
use worker::{DurableObject, Env, Request, Response, Result, SqlStorage, State, durable_object};

/// The DO's internal protocol. Not part of the public API in `docs/03-API.md` — the Worker
/// is the only caller, and it always speaks this shape.
pub const BASE_URL: &str = "https://case.medatat.internal";

#[derive(Debug, Serialize, Deserialize)]
pub struct DoInitReq {
    pub case_id: CaseId,
    pub mrn: String,
    pub form_id: FormId,
    pub assignee: Option<String>,
}

/// `actor` is resolved from the session by the Worker before this is built. `defs` come
/// from D1, never from the client, so a client cannot widen its own validation rules.
#[derive(Debug, Serialize, Deserialize)]
pub struct DoPutReq {
    pub case_id: CaseId,
    pub actor: ActorId,
    pub defs: Vec<FieldDef>,
    pub req: PutValuesReq,
}

#[durable_object]
pub struct CaseDO {
    sql: SqlStorage,
    env: Env,
}

impl DurableObject for CaseDO {
    fn new(state: State, env: Env) -> Self {
        // NOTE: no DDL here, deliberately. See the module docs.
        CaseDO {
            sql: state.storage().sql(),
            env,
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        match (req.method(), path.as_str()) {
            (worker::Method::Post, "/init") => self.init(&mut req).await,
            (worker::Method::Get, "/values") => self.read_values(&req),
            (worker::Method::Post, "/values") => self.write_values(&mut req).await,
            (worker::Method::Get, "/rev") => self.read_rev(),
            _ => fail(LogicError::NotFound(format!("no DO route for {path}"))),
        }
    }
}

impl CaseDO {
    async fn init(&self, req: &mut Request) -> Result<Response> {
        let body: DoInitReq = match req.json().await {
            Ok(b) => b,
            Err(e) => return fail(LogicError::Validation(format!("malformed init: {e}"))),
        };
        let store = SqlCaseStore::read_only(self.sql.clone());
        let write = (|| {
            store.meta_set("case_id", &body.case_id.to_string())?;
            store.meta_set("mrn", &body.mrn)?;
            store.meta_set("form_id", &body.form_id.to_string())?;
            if let Some(a) = &body.assignee {
                store.meta_set("assignee", a)?;
            }
            store.meta_set(
                "created_at",
                &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )
        })();
        match write {
            Ok(()) => json(serde_json::json!({ "case_id": body.case_id }), 201),
            Err(e) => fail(e),
        }
    }

    fn read_rev(&self) -> Result<Response> {
        let store = SqlCaseStore::read_only(self.sql.clone());
        match store.rev() {
            Ok(rev) => json(serde_json::json!({ "rev": rev }), 200),
            Err(e) => fail(e),
        }
    }

    fn read_values(&self, req: &Request) -> Result<Response> {
        let case_id = match do_case_id(req) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // The Worker re-serialises an already-parsed value into this URL, so this cannot
        // be malformed today. It is still propagated rather than defaulted: the default
        // here is a full case read, and that is not a failure mode to leave on trust.
        let since = match crate::routes::query_i64(req, "since_rev") {
            Ok(v) => v.map(CaseRev),
            Err(e) => return fail(e),
        };
        let store = SqlCaseStore::read_only(self.sql.clone());
        match handle_get_values(&store, case_id, since) {
            Ok(page) => json(page, 200),
            Err(e) => fail(e),
        }
    }

    async fn write_values(&self, req: &mut Request) -> Result<Response> {
        let body: DoPutReq = match req.json().await {
            Ok(b) => b,
            Err(e) => return fail(LogicError::Validation(format!("malformed put: {e}"))),
        };
        let defs = FieldLookup::from_defs(body.defs);
        let store = SqlCaseStore::with_defs(self.sql.clone(), &defs);

        let outcome = handle_put_values(&store, body.req, &body.actor, &defs);
        match outcome {
            Err(e) => fail(e),
            Ok(resp) => {
                if let PutValuesResp::Applied { rev, .. } = &resp {
                    // The worklist index is updated after the value write and **may fail
                    // independently** — that is the eventual-consistency seam, repaired by
                    // POST /admin/reindex. A failure here must not fail the write.
                    self.touch_index(body.case_id, *rev).await;
                }
                // `http::put_values` owns the 200-vs-409 split and the conflict envelope.
                crate::routes::render(crate::http::put_values(resp))
            }
        }
    }

    async fn touch_index(&self, case_id: CaseId, rev: CaseRev) {
        let Ok(db) = crate::routes::db(&self.env) else {
            worker::console_error!("case_index not updated for {case_id}: no D1 binding");
            return;
        };
        if let Err(e) = db.touch_case(case_id, rev).await {
            worker::console_error!("case_index not updated for {case_id}: {e}");
        }
    }
}

fn do_case_id(req: &Request) -> std::result::Result<CaseId, LogicError> {
    let raw = crate::routes::query_param(req, "case_id")
        .ok_or_else(|| LogicError::Internal("DO call without case_id".into()))?;
    CaseId::parse(&raw).map_err(|_| LogicError::Internal("DO call with malformed case_id".into()))
}
