//! Versioned, forward-only migrations.
//!
//! There is no down path. A client that has already written data cannot usefully roll a
//! schema back, and pretending otherwise invites a migration that destroys local edits.

use crate::error::StoreError;
use crate::schema;
use rusqlite::{Connection, OptionalExtension};

/// The schema version this build writes and expects.
pub(crate) const LATEST: i64 = 2;

/// Brings `conn` up to [`LATEST`]. Idempotent.
pub(crate) fn apply(conn: &mut Connection) -> Result<i64, StoreError> {
    let current = read_version(conn)?;
    if current > LATEST {
        return Err(StoreError::SchemaTooNew {
            found: current,
            expected: LATEST,
        });
    }
    if current == LATEST {
        return Ok(current);
    }

    let tx = conn.transaction()?;
    if current < 1 {
        tx.execute_batch(schema::V1)?;
    }
    if current < 2 {
        tx.execute_batch(schema::V2)?;
    }
    // Later migrations append here, each guarded by `if current < N`.

    tx.execute("DELETE FROM schema_version", [])?;
    tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [LATEST])?;
    tx.commit()?;
    Ok(LATEST)
}

/// The version on disk, or 0 for a database that has never been migrated.
pub(crate) fn read_version(conn: &Connection) -> Result<i64, StoreError> {
    let has_table: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'schema_version'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(0);
    }
    Ok(conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}
