//! Connection opening and PRAGMA setup.
//!
//! Two connections per store: one for reads (the UI thread) and one for writes. WAL is
//! what makes that pay — a reader never blocks on the writer.

use crate::error::StoreError;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Where a connection points. In-memory stores use a named shared-cache URI so the read
/// and write connections see the *same* database; two plain `:memory:` connections would
/// be two unrelated databases.
pub(crate) enum Location {
    File(std::path::PathBuf),
    Memory(String),
}

impl Location {
    pub(crate) fn file(p: &Path) -> Self {
        Location::File(p.to_path_buf())
    }

    /// A process-unique in-memory database name.
    pub(crate) fn unique_memory() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Location::Memory(format!(
            "file:medatat-{}-{n}?mode=memory&cache=shared",
            std::process::id()
        ))
    }
}

pub(crate) fn open_conn(loc: &Location, key_hex: Option<&str>) -> Result<Connection, StoreError> {
    let conn = match loc {
        Location::File(p) => Connection::open(p).map_err(StoreError::from_open)?,
        Location::Memory(uri) => Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(StoreError::from_open)?,
    };

    // PRAGMA key is the FIRST statement issued on the connection. Anything before it
    // touches the file unkeyed and fails.
    #[cfg(feature = "phi")]
    if let Some(k) = key_hex {
        // The raw-key form: SQLCipher takes the 32 bytes as given rather than running a
        // KDF over an ASCII passphrase.
        pragma(&conn, &format!("PRAGMA key = \"x'{k}'\""))?;
    }
    #[cfg(not(feature = "phi"))]
    let _ = key_hex;

    conn.busy_timeout(Duration::from_secs(5))?;
    // journal_mode returns a row; the others do not. `pragma` copes with both.
    pragma(&conn, "PRAGMA journal_mode = WAL")?;
    pragma(&conn, "PRAGMA synchronous = NORMAL")?;
    pragma(&conn, "PRAGMA foreign_keys = ON")?;
    pragma(&conn, "PRAGMA cache_size = -65536")?; // 64 MiB

    verify_readable(&conn)?;
    Ok(conn)
}

/// Runs a PRAGMA, tolerating the ones that return a row.
fn pragma(conn: &Connection, sql: &str) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(sql).map_err(StoreError::from_open)?;
    let mut rows = stmt.query([]).map_err(StoreError::from_open)?;
    rows.next().map_err(StoreError::from_open)?;
    Ok(())
}

/// Proves the key is right before any caller can mistake `SQLITE_NOTADB` for corruption.
/// SQLCipher decrypts lazily, so without this the failure would surface at some arbitrary
/// later query.
fn verify_readable(conn: &Connection) -> Result<(), StoreError> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|_| ())
    .map_err(StoreError::from_open)
}
