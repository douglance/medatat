//! The coalescing outbox.
//!
//! The primary key is `(case_id, field_id)`, so forty keystrokes in one field collapse to
//! one row: sync volume is bounded by *fields touched*, not by keystrokes. That is the
//! load-bearing detail of the write path (`docs/04-SYNC.md`).

use crate::error::StoreError;
use medatat_core::{CaseId, CaseRev, FieldId, Value};
use rusqlite::{Connection, params};
use std::time::Duration;

/// One queued field edit, ready to send.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub case_id: CaseId,
    pub field_id: FieldId,
    /// Bumped on every enqueue. `confirm` drops a row only if this still matches what was
    /// sent, so an edit made while the field was in flight survives instead of being
    /// deleted with the value it replaced.
    pub seq: i64,
    pub value: Value,
    pub base_rev: CaseRev,
    pub attempts: i64,
    pub next_attempt_at: String,
    pub last_error: Option<String>,
}

pub(crate) fn enqueue(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
    v: &Value,
    base_rev: CaseRev,
    now: &str,
) -> Result<(), StoreError> {
    let blob = postcard::to_allocvec(v)?;
    // `base_rev` is deliberately *not* refreshed on conflict. It records the server rev
    // the edit chain for this field started from, which is what per-field conflict
    // detection compares against; advancing it here would hide a genuine same-field race.
    // `seq` advances past whatever is there, so a row re-enqueued while its previous
    // value is in flight cannot be confirmed away by the response to that older send.
    conn.prepare_cached(
        "INSERT INTO outbox (case_id, field_id, value_blob, base_rev, attempts, next_attempt_at, seq) \
         VALUES (?1, ?2, ?3, ?4, 0, ?5, \
                 COALESCE((SELECT MAX(seq) FROM outbox), 0) + 1) \
         ON CONFLICT(case_id, field_id) DO UPDATE SET \
           value_blob = excluded.value_blob, \
           attempts = 0, \
           next_attempt_at = excluded.next_attempt_at, \
           last_error = NULL, \
           seq = excluded.seq",
    )?
    .execute(params![
        case_id.to_string(),
        field_id.to_string(),
        blob,
        base_rev.0,
        now
    ])?;
    Ok(())
}

/// Rows whose backoff has elapsed, oldest first.
pub(crate) fn next_batch(
    conn: &Connection,
    limit: usize,
    now: &str,
) -> Result<Vec<OutboxRow>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT case_id, field_id, value_blob, base_rev, attempts, next_attempt_at, last_error, seq \
         FROM outbox WHERE next_attempt_at <= ?1 ORDER BY next_attempt_at LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![now, limit as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let raw_case: String = row.get(0)?;
        let raw_field: String = row.get(1)?;
        let blob: Vec<u8> = row.get(2)?;
        out.push(OutboxRow {
            case_id: CaseId::parse(&raw_case).map_err(|_| StoreError::BadId(raw_case))?,
            field_id: FieldId::parse(&raw_field).map_err(|_| StoreError::BadId(raw_field))?,
            value: postcard::from_bytes(&blob)?,
            base_rev: CaseRev(row.get(3)?),
            attempts: row.get(4)?,
            next_attempt_at: row.get(5)?,
            last_error: row.get(6)?,
            seq: row.get(7)?,
        });
    }
    Ok(out)
}

pub(crate) fn drop_rows(
    conn: &Connection,
    case_id: CaseId,
    fields: &[FieldId],
) -> Result<(), StoreError> {
    let mut stmt =
        conn.prepare_cached("DELETE FROM outbox WHERE case_id = ?1 AND field_id = ?2")?;
    for f in fields {
        stmt.execute(params![case_id.to_string(), f.to_string()])?;
    }
    Ok(())
}

/// Drops a queued edit **only if it is still the one that was sent**.
///
/// A row whose `seq` has moved on was re-enqueued while the older value was in flight, and
/// now holds a newer value the server has not seen. Deleting it would clear `pending` on a
/// field whose current value will never be sent — a silent lost update, which is the one
/// failure class this design exists to rule out. Leaving it queued costs at most one
/// redundant send.
pub(crate) fn drop_rows_at_seq(
    conn: &Connection,
    case_id: CaseId,
    sent: &[(FieldId, i64)],
) -> Result<usize, StoreError> {
    let mut stmt = conn
        .prepare_cached("DELETE FROM outbox WHERE case_id = ?1 AND field_id = ?2 AND seq = ?3")?;
    let mut dropped = 0;
    for (field_id, seq) in sent {
        dropped += stmt.execute(params![case_id.to_string(), field_id.to_string(), seq])?;
    }
    Ok(dropped)
}

/// The sequence currently queued for a field, if any.
pub(crate) fn seq_of(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
) -> Result<Option<i64>, StoreError> {
    use rusqlite::OptionalExtension as _;
    Ok(conn
        .prepare_cached("SELECT seq FROM outbox WHERE case_id = ?1 AND field_id = ?2")?
        .query_row(params![case_id.to_string(), field_id.to_string()], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Records a failed send and schedules the retry.
///
/// The schedule is `min(60s, 2^attempts * 500ms)`. Jitter is applied by `medatat-sync`,
/// not here: the store is deterministic so a test can assert the schedule exactly, and
/// the ±20% that keeps clients from retrying in lockstep belongs with the loop that
/// actually sleeps.
pub(crate) fn bump_attempts(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
    err: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    let attempts: i64 = conn
        .prepare_cached("SELECT attempts FROM outbox WHERE case_id = ?1 AND field_id = ?2")?
        .query_row(params![case_id.to_string(), field_id.to_string()], |r| {
            r.get(0)
        })?;
    let next = now + chrono::Duration::from_std(backoff(attempts)).unwrap_or_default();
    conn.prepare_cached(
        "UPDATE outbox SET attempts = attempts + 1, last_error = ?3, next_attempt_at = ?4 \
         WHERE case_id = ?1 AND field_id = ?2",
    )?
    .execute(params![
        case_id.to_string(),
        field_id.to_string(),
        err,
        crate::format_time(next)
    ])?;
    Ok(())
}

pub(crate) fn backoff(attempts: i64) -> Duration {
    let capped = attempts.clamp(0, 8) as u32;
    Duration::from_millis(500 * 2u64.pow(capped)).min(Duration::from_secs(60))
}
