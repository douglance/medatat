//! `/config/*` (R3, R4, R12). Reads are open to any authenticated user; every mutation is
//! admin-only and bumps `config_version.rev` in the same D1 batch.

use super::{
    body_json, body_value, db, fail, json, no_content, path_id, query_i64, require_admin,
    require_session,
};
use crate::error::LogicError;
use crate::logic::config::{
    check_kind_patch, clamps_for_columns, resolve_col_span, validate_columns, validate_field_kind,
    validate_placement,
};
use crate::logic::config_rows::PlacementRow;
use medatat_core::def::{FieldKind, FieldOption};
use medatat_core::ids::{ConfigRev, FieldId, FormId, SectionId};
use medatat_core::wire::{
    ConfigDelta, CreateFieldReq, CreateFormReq, CreateSectionReq, PatchSectionReq, PlaceFieldReq,
};
use serde::{Deserialize, Serialize};
use worker::{Request, Response, Result, RouteContext};

#[derive(Debug, Serialize)]
pub struct CreatedForm {
    pub form_id: FormId,
}

#[derive(Debug, Serialize)]
pub struct CreatedSection {
    pub section_id: SectionId,
}

#[derive(Debug, Serialize)]
pub struct CreatedField {
    pub field_id: FieldId,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchFormReq {
    pub name: Option<String>,
    pub archived: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchPlacementReq {
    pub ordinal: Option<i32>,
    pub col_span: Option<u8>,
    pub label: Option<String>,
    pub required: Option<bool>,
}

/// `304` when the client's `since_rev` is current. Config is small enough that partial sync
/// is not worth the complexity, so the answer is all-or-nothing.
pub async fn get_config(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_session(&req, &ctx.env).await {
        return fail(e);
    }
    let result: std::result::Result<Option<ConfigDelta>, LogicError> = async {
        let db = db(&ctx.env)?;
        let current = db.config_rev().await?;
        if query_i64(&req, "since_rev") == Some(current.0) {
            return Ok(None);
        }
        db.load_config().await.map(Some)
    }
    .await;

    match result {
        Ok(None) => super::not_modified(),
        Ok(Some(delta)) => json(delta, 200),
        Err(e) => fail(e),
    }
}

pub async fn create_form(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let result: std::result::Result<CreatedForm, LogicError> = async {
        let body: CreateFormReq = body_json(&mut req).await?;
        let form_id = db(&ctx.env)?.create_form(&body.key, &body.name).await?;
        Ok(CreatedForm { form_id })
    }
    .await;
    match result {
        Ok(v) => json(v, 201),
        Err(e) => fail(e),
    }
}

pub async fn patch_form(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let form_id = match path_id(ctx.param("form_id"), FormId::parse, "form_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let result: std::result::Result<(), LogicError> = async {
        let body: PatchFormReq = body_json(&mut req).await?;
        db(&ctx.env)?
            .patch_form(form_id, body.name.as_deref(), body.archived)
            .await
    }
    .await;
    match result {
        Ok(()) => no_content(),
        Err(e) => fail(e),
    }
}

pub async fn create_section(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let form_id = match path_id(ctx.param("form_id"), FormId::parse, "form_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let result: std::result::Result<CreatedSection, LogicError> = async {
        let body: CreateSectionReq = body_json(&mut req).await?;
        validate_columns(body.columns)?;
        let section_id = db(&ctx.env)?
            .create_section(form_id, &body.name, body.ordinal, body.columns)
            .await?;
        Ok(CreatedSection { section_id })
    }
    .await;
    match result {
        Ok(v) => json(v, 201),
        Err(e) => fail(e),
    }
}

/// R12: **reducing `columns` clamps every child `col_span` in the same transaction.**
pub async fn patch_section(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let section_id = match path_id(ctx.param("section_id"), SectionId::parse, "section_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let result: std::result::Result<(), LogicError> = async {
        let body: PatchSectionReq = body_json(&mut req).await?;
        let db = db(&ctx.env)?;
        if db.section_columns(section_id).await?.is_none() {
            return Err(LogicError::NotFound("unknown section".into()));
        }
        let clamps = match body.columns {
            None => Vec::new(),
            Some(columns) => {
                validate_columns(columns)?;
                let children = spans_of(&db.section_placements(section_id).await?)?;
                clamps_for_columns(&children, columns)
            }
        };
        db.patch_section(
            section_id,
            body.name.as_deref(),
            body.ordinal,
            body.columns,
            &clamps,
        )
        .await
    }
    .await;
    match result {
        Ok(()) => no_content(),
        Err(e) => fail(e),
    }
}

/// Cascades placements. **Never deletes values.**
pub async fn delete_section(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let section_id = match path_id(ctx.param("section_id"), SectionId::parse, "section_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    match db(&ctx.env) {
        Ok(db) => match db.delete_section(section_id).await {
            Ok(()) => no_content(),
            Err(e) => fail(e),
        },
        Err(e) => fail(e),
    }
}

pub async fn create_field(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let result: std::result::Result<CreatedField, LogicError> = async {
        let body: CreateFieldReq = body_json(&mut req).await?;
        validate_field_kind(&body.kind)?;
        let field_id = db(&ctx.env)?.create_field(&body.key, &body.kind).await?;
        Ok(CreatedField { field_id })
    }
    .await;
    match result {
        Ok(v) => json(v, 201),
        Err(e) => fail(e),
    }
}

/// **`kind` is immutable.** A patch that names a different one is a 422, not a silent
/// no-op: dropping it would let a builder believe it had re-typed a field in place.
pub async fn patch_field(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let field_id = match path_id(ctx.param("field_id"), FieldId::parse, "field_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let result: std::result::Result<(), LogicError> = async {
        let patch = body_value(&mut req).await?;
        let db = db(&ctx.env)?;
        let (existing, existing_config) = db
            .field_kind(field_id)
            .await?
            .ok_or_else(|| LogicError::NotFound("unknown field".into()))?;

        check_kind_patch(&existing, &patch)?;

        let merged = merge_field_patch(&existing, &existing_config, &patch)?;
        validate_field_kind(&merged)?;
        db.patch_field(field_id, &merged).await
    }
    .await;
    match result {
        Ok(()) => no_content(),
        Err(e) => fail(e),
    }
}

/// Apply `{config?, options?}` on top of the stored definition, keeping `kind` fixed.
fn merge_field_patch(
    existing: &FieldKind,
    existing_config: &str,
    patch: &serde_json::Value,
) -> std::result::Result<FieldKind, LogicError> {
    let mut config: serde_json::Value = if existing_config.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing_config)
            .map_err(|e| LogicError::Storage(format!("bad stored config: {e}")))?
    };

    if let Some(incoming) = patch.get("config") {
        let incoming = incoming
            .as_object()
            .ok_or_else(|| LogicError::Validation("config must be an object".into()))?;
        let target = config
            .as_object_mut()
            .ok_or_else(|| LogicError::Storage("stored config is not an object".into()))?;
        for (k, v) in incoming {
            target.insert(k.clone(), v.clone());
        }
    }

    let options: Vec<FieldOption> = match patch.get("options") {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| LogicError::Validation(format!("malformed options: {e}")))?,
        None => existing.options().map(|o| o.to_vec()).unwrap_or_default(),
    };

    crate::logic::config_rows::kind_from_row(
        existing.tag(),
        &config.to_string(),
        std::sync::Arc::from(options),
    )
}

/// `col_span <= section.columns`, else `422`.
pub async fn place_field(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let section_id = match path_id(ctx.param("section_id"), SectionId::parse, "section_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let result: std::result::Result<(), LogicError> = async {
        let body: PlaceFieldReq = body_json(&mut req).await?;
        let db = db(&ctx.env)?;
        let columns = db
            .section_columns(section_id)
            .await?
            .ok_or_else(|| LogicError::NotFound("unknown section".into()))?;
        validate_placement(body.col_span, columns)?;
        db.place_field(
            section_id,
            body.field_id,
            body.ordinal,
            body.col_span,
            &body.label,
            body.required,
        )
        .await
    }
    .await;
    match result {
        Ok(()) => no_content(),
        Err(e) => fail(e),
    }
}

pub async fn patch_placement(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let section_id = match path_id(ctx.param("section_id"), SectionId::parse, "section_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let field_id = match path_id(ctx.param("field_id"), FieldId::parse, "field_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let result: std::result::Result<(), LogicError> = async {
        let body: PatchPlacementReq = body_json(&mut req).await?;
        let db = db(&ctx.env)?;
        let columns = db
            .section_columns(section_id)
            .await?
            .ok_or_else(|| LogicError::NotFound("unknown section".into()))?;

        // One read of the placement row carries everything the rewrite needs: the current
        // span, ordinal, label, and requiredness. An absent body field keeps its value.
        let fid = field_id.to_string();
        let current = db
            .section_placements(section_id)
            .await?
            .into_iter()
            .find(|r| r.field_id == fid)
            .ok_or_else(|| LogicError::NotFound("field is not placed in this section".into()))?;

        // Validated even when the body never mentions `col_span` — the stored row may
        // predate a `columns` reduction. `resolve_col_span` makes that unskippable.
        let col_span =
            resolve_col_span(body.col_span, current.col_span.clamp(1, 3) as u8, columns)?;

        db.place_field(
            section_id,
            field_id,
            body.ordinal.unwrap_or(current.ordinal as i32),
            col_span,
            body.label.as_deref().unwrap_or(&current.label),
            body.required.unwrap_or(current.required != 0),
        )
        .await
    }
    .await;
    match result {
        Ok(()) => no_content(),
        Err(e) => fail(e),
    }
}

/// The `(field_id, col_span)` pairs `clamps_for_columns` works over.
fn spans_of(rows: &[PlacementRow]) -> std::result::Result<Vec<(FieldId, u8)>, LogicError> {
    rows.iter()
        .map(|r| {
            let id = FieldId::parse(&r.field_id)
                .map_err(|_| LogicError::Storage(format!("bad field_id {:?}", r.field_id)))?;
            Ok((id, r.col_span.clamp(1, 3) as u8))
        })
        .collect()
}

/// Removes the placement only. **Values survive.**
pub async fn unplace_field(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = require_admin(&req, &ctx.env).await {
        return fail(e);
    }
    let section_id = match path_id(ctx.param("section_id"), SectionId::parse, "section_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let field_id = match path_id(ctx.param("field_id"), FieldId::parse, "field_id") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    match db(&ctx.env) {
        Ok(db) => match db.unplace_field(section_id, field_id).await {
            Ok(()) => no_content(),
            Err(e) => fail(e),
        },
        Err(e) => fail(e),
    }
}

pub async fn current_rev(env: &worker::Env) -> std::result::Result<ConfigRev, LogicError> {
    db(env)?.config_rev().await
}
