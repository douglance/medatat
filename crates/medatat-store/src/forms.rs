//! Form definitions, stored whole as a JSON blob.
//!
//! A form is loaded once and rendered thousands of times, so it is stored whole rather
//! than normalised into rows: one row read plus one decode, instead of a four-way join
//! reassembled in Rust on every open. Form definitions are configuration, not PHI.
//!
//! `docs/02-DATA-MODEL.md` specifies a postcard blob here. It cannot be one:
//! `medatat_core::FieldKind` is internally tagged (`#[serde(tag = "kind")]`, so the wire
//! shape in `docs/03-API.md` reads naturally), and serde's internally-tagged path
//! serialises through a map of *unknown* length — which postcard rejects with
//! `WontImplement`. Internal tagging needs a self-describing format, so the blob is JSON.
//! Compactness buys nothing here: the whole registry is a few hundred KB, read once at
//! startup. `Value` blobs in `outbox` and `conflict` stay postcard, where the density
//! does matter and the enum is externally tagged.

use crate::error::StoreError;
use medatat_core::{ConfigRev, FormDef, FormId};
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;

pub(crate) fn save(
    conn: &Connection,
    def: &FormDef,
    config_rev: ConfigRev,
    now: &str,
) -> Result<(), StoreError> {
    let blob = serde_json::to_vec(def).map_err(StoreError::form_codec)?;
    conn.prepare_cached(
        "INSERT INTO form (form_id, name, def_blob, config_rev, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(form_id) DO UPDATE SET \
           name = excluded.name, def_blob = excluded.def_blob, \
           config_rev = excluded.config_rev, updated_at = excluded.updated_at",
    )?
    .execute(params![
        def.form_id.to_string(),
        def.name,
        blob,
        config_rev.0,
        now
    ])?;
    Ok(())
}

pub(crate) fn load(conn: &Connection, form_id: FormId) -> Result<Arc<FormDef>, StoreError> {
    let blob: Vec<u8> = conn
        .prepare_cached("SELECT def_blob FROM form WHERE form_id = ?1")?
        .query_row(params![form_id.to_string()], |r| r.get(0))
        .optional()?
        .ok_or(StoreError::FormNotFound(form_id))?;
    decode(&blob)
}

pub(crate) fn load_all(conn: &Connection) -> Result<Vec<Arc<FormDef>>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT def_blob FROM form ORDER BY name")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        out.push(decode(&blob)?);
    }
    Ok(out)
}

/// `FormDef::by_id` and `field_count` are `#[serde(skip)]`, so a decoded definition has an
/// empty index and every `FieldIdx` at zero until `finalize` rebuilds them.
fn decode(blob: &[u8]) -> Result<Arc<FormDef>, StoreError> {
    let mut def: FormDef = serde_json::from_slice(blob).map_err(StoreError::form_codec)?;
    def.finalize();
    Ok(Arc::new(def))
}
