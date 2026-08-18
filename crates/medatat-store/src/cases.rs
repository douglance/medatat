//! `patient_case` rows and the worklist query.

use crate::error::StoreError;
use medatat_core::{CaseId, CaseRev, FormId, wire::CaseSummary};
use rusqlite::{Connection, Row, params};

/// One row of the worklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseRow {
    pub case_id: CaseId,
    pub mrn: Option<String>,
    pub form_id: FormId,
    /// Local revision, monotonic. Ahead of `synced_rev` when there is unsynced work.
    pub rev: CaseRev,
    pub synced_rev: CaseRev,
    pub assignee: Option<String>,
    pub updated_at: String,
}

const SELECT: &str = "SELECT case_id, mrn, form_id, rev, synced_rev, assignee, updated_at \
     FROM patient_case";

fn row_to_case(row: &Row<'_>) -> Result<CaseRow, StoreError> {
    let raw_case: String = row.get(0)?;
    let raw_form: String = row.get(2)?;
    Ok(CaseRow {
        case_id: CaseId::parse(&raw_case).map_err(|_| StoreError::BadId(raw_case))?,
        mrn: row.get(1)?,
        form_id: FormId::parse(&raw_form).map_err(|_| StoreError::BadId(raw_form))?,
        rev: CaseRev(row.get(3)?),
        synced_rev: CaseRev(row.get(4)?),
        assignee: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// Inserts or refreshes a case from a server summary.
///
/// `rev` and `synced_rev` only ever move forward: a summary that arrived out of order, or
/// after local edits already bumped the local rev, must not rewind either counter.
/// `CaseSummary` carries no `synced_rev` — by construction its `rev` *is* what the server
/// has, so it becomes the new `synced_rev`.
pub(crate) fn upsert(conn: &Connection, c: &CaseSummary) -> Result<(), StoreError> {
    conn.prepare_cached(
        "INSERT INTO patient_case (case_id, mrn, form_id, rev, synced_rev, assignee, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6) \
         ON CONFLICT(case_id) DO UPDATE SET \
           mrn = excluded.mrn, form_id = excluded.form_id, assignee = excluded.assignee, \
           updated_at = excluded.updated_at, \
           rev = MAX(patient_case.rev, excluded.rev), \
           synced_rev = MAX(patient_case.synced_rev, excluded.synced_rev)",
    )?
    .execute(params![
        c.case_id.to_string(),
        c.mrn,
        c.form_id.to_string(),
        c.rev.0,
        c.assignee,
        c.updated_at,
    ])?;
    Ok(())
}

/// Most-recently-touched first, which is the order the caseload pre-sync walks
/// (`docs/04-SYNC.md`). Served by the `case_assignee` index.
pub(crate) fn worklist(
    conn: &Connection,
    assignee: &str,
    limit: usize,
) -> Result<Vec<CaseRow>, StoreError> {
    let sql = format!("{SELECT} WHERE assignee = ?1 ORDER BY updated_at DESC LIMIT ?2");
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params![assignee, limit as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_case(row)?);
    }
    Ok(out)
}

pub(crate) fn get(conn: &Connection, case_id: CaseId) -> Result<CaseRow, StoreError> {
    let sql = format!("{SELECT} WHERE case_id = ?1");
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params![case_id.to_string()])?;
    match rows.next()? {
        Some(row) => row_to_case(row),
        None => Err(StoreError::CaseNotFound(case_id)),
    }
}

/// Bumps the local revision after a local edit, and moves the case to the top of the
/// worklist.
pub(crate) fn touch_local(conn: &Connection, case_id: CaseId, now: &str) -> Result<(), StoreError> {
    let n = conn
        .prepare_cached(
            "UPDATE patient_case SET rev = rev + 1, updated_at = ?2 WHERE case_id = ?1",
        )?
        .execute(params![case_id.to_string(), now])?;
    if n == 0 {
        return Err(StoreError::CaseNotFound(case_id));
    }
    Ok(())
}

/// Records the server rev a case has been brought up to.
pub(crate) fn mark_synced(
    conn: &Connection,
    case_id: CaseId,
    rev: CaseRev,
) -> Result<(), StoreError> {
    conn.prepare_cached(
        "UPDATE patient_case SET rev = MAX(rev, ?2), synced_rev = MAX(synced_rev, ?2) \
         WHERE case_id = ?1",
    )?
    .execute(params![case_id.to_string(), rev.0])?;
    Ok(())
}
