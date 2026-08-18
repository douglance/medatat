//! Unresolved per-field conflicts.
//!
//! Conflicts persist across restarts and are rendered as an inline strip next to the
//! field. Never modal, never resolved automatically — this is clinical data, and a silent
//! last-write-wins is unacceptable (`docs/04-SYNC.md`).

use crate::error::StoreError;
use medatat_core::{CaseId, FieldId, Value};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone)]
pub struct ConflictRow {
    pub case_id: CaseId,
    pub field_id: FieldId,
    /// The local value at the moment the server rejected it.
    pub mine: Value,
    pub theirs: Value,
    pub theirs_by: Option<String>,
    pub theirs_at: Option<String>,
}

pub(crate) fn record(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
    mine: &Value,
    theirs: &Value,
    theirs_by: Option<&str>,
    theirs_at: Option<&str>,
) -> Result<(), StoreError> {
    conn.prepare_cached(
        "INSERT INTO conflict (case_id, field_id, mine, theirs, theirs_by, theirs_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(case_id, field_id) DO UPDATE SET \
           theirs = excluded.theirs, theirs_by = excluded.theirs_by, \
           theirs_at = excluded.theirs_at",
    )?
    .execute(params![
        case_id.to_string(),
        field_id.to_string(),
        postcard::to_allocvec(mine)?,
        postcard::to_allocvec(theirs)?,
        theirs_by,
        theirs_at,
    ])?;
    Ok(())
}

pub(crate) fn list(conn: &Connection, case_id: CaseId) -> Result<Vec<ConflictRow>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT field_id, mine, theirs, theirs_by, theirs_at FROM conflict WHERE case_id = ?1",
    )?;
    let mut rows = stmt.query(params![case_id.to_string()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let raw_field: String = row.get(0)?;
        let mine: Vec<u8> = row.get(1)?;
        let theirs: Vec<u8> = row.get(2)?;
        out.push(ConflictRow {
            case_id,
            field_id: FieldId::parse(&raw_field).map_err(|_| StoreError::BadId(raw_field))?,
            mine: postcard::from_bytes(&mine)?,
            theirs: postcard::from_bytes(&theirs)?,
            theirs_by: row.get(3)?,
            theirs_at: row.get(4)?,
        });
    }
    Ok(out)
}

pub(crate) fn get(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
) -> Result<Option<(Value, Value)>, StoreError> {
    let blobs: Option<(Vec<u8>, Vec<u8>)> = conn
        .prepare_cached("SELECT mine, theirs FROM conflict WHERE case_id = ?1 AND field_id = ?2")?
        .query_row(params![case_id.to_string(), field_id.to_string()], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    match blobs {
        Some((m, t)) => Ok(Some((postcard::from_bytes(&m)?, postcard::from_bytes(&t)?))),
        None => Ok(None),
    }
}

pub(crate) fn clear(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
) -> Result<(), StoreError> {
    conn.prepare_cached("DELETE FROM conflict WHERE case_id = ?1 AND field_id = ?2")?
        .execute(params![case_id.to_string(), field_id.to_string()])?;
    Ok(())
}
