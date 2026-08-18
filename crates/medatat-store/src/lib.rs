//! The client's system of record.
//!
//! Every user-triggered read and write in `medatat-ui` goes through this crate,
//! synchronously. Sync is background-only and never on the critical path — the UI reads
//! local SQLite and nothing else (`docs/04-SYNC.md`).
//!
//! Two invariants carry most of the weight:
//!
//! 1. [`Store::apply_local`] writes the value **and** its outbox row in one transaction,
//!    so a crash can never leave a value saved but un-enqueued.
//! 2. [`Store::apply_server_values`] never overwrites a row with `pending = 1`, because
//!    that row is an unsynced local edit.
//!
//! Default builds use plain SQLite. `--features phi` switches to SQLCipher with the key
//! in an owner-only file beside the database; nothing else about the schema or the API
//! changes. See `docs/12-PHI-READINESS.md`.

mod cases;
mod conflicts;
mod conn;
mod error;
mod forms;
mod migrations;
mod outbox;
mod schema;
mod values;

#[cfg(feature = "phi")]
mod keyring;

pub use cases::CaseRow;
pub use conflicts::ConflictRow;
pub use error::StoreError;
pub use outbox::OutboxRow;

use chrono::{DateTime, SecondsFormat, Utc};
use conn::Location;
use medatat_core::{
    CaseId, CaseRev, ConfigRev, FieldDef, FieldId, FormDef, FormId, Value,
    wire::{CaseSummary, ValueRow},
};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// One timestamp encoding for the whole store: RFC-3339 UTC, millisecond precision. Fixed
/// width, so string comparison is time comparison — which is what
/// `outbox.next_attempt_at <= ?` and the worklist ordering rely on.
pub(crate) fn format_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn now() -> String {
    format_time(Utc::now())
}

/// A local database.
///
/// Reads and writes use separate connections so a read never waits behind a write; WAL is
/// what makes that true. Both are behind a `Mutex` because `rusqlite::Connection` is
/// `Send` but not `Sync`, and a `Store` has to be `Sync` to sit in an `Arc` shared by the
/// UI thread and the sync task. Read contention is then between readers only, never with
/// the writer. (`docs/11-CRATE-GUIDE.md` sketches the read connection unwrapped; it cannot
/// be, for that reason.)
pub struct Store {
    read: Mutex<Connection>,
    write: Mutex<Connection>,
}

impl Store {
    /// Opens (creating if needed) the database at `path`.
    ///
    /// Under `--features phi` the key comes from an owner-only file beside the database
    /// and `PRAGMA key` is the first statement issued. A wrong key is reported as
    /// [`StoreError::Locked`] — never as corruption, and never by recreating the file.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        #[cfg(feature = "phi")]
        {
            let key = keyring::get_or_create(path)?;
            Self::new(Location::file(path), Some(&key))
        }
        #[cfg(not(feature = "phi"))]
        Self::new(Location::file(path), None)
    }

    /// Opens with an explicit hex key, bypassing the key file. `phi` only.
    ///
    /// This exists for the wrong-key test and for tooling that manages its own key; the
    /// application path is [`Store::open`].
    #[cfg(feature = "phi")]
    pub fn open_with_key(path: &Path, key_hex: &str) -> Result<Self, StoreError> {
        Self::new(Location::file(path), Some(key_hex))
    }

    /// An in-memory database. Nothing is at rest, so it is never keyed.
    ///
    /// Used by tests, and as the fallback when the on-disk store cannot be opened — in
    /// which case the caseload is re-synced each launch rather than written unencrypted
    /// to disk.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::new(Location::unique_memory(), None)
    }

    fn new(loc: Location, key_hex: Option<&str>) -> Result<Self, StoreError> {
        // SQLite creates the database *file* but not the directory holding it, and on a
        // clean machine `~/Library/Application Support/medatat/` does not exist. Without
        // this the app falls back to an in-memory store and loses every edit on quit,
        // which is far worse than failing loudly at startup. Done here rather than in
        // `open` so `open_with_key` and any later constructor get it too.
        if let Location::File(path) = &loc
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::DataDir {
                path: parent.display().to_string(),
                detail: e.to_string(),
            })?;
        }

        let mut write = conn::open_conn(&loc, key_hex)?;
        migrations::apply(&mut write)?;
        let read = conn::open_conn(&loc, key_hex)?;
        Ok(Store {
            read: Mutex::new(read),
            write: Mutex::new(write),
        })
    }

    fn reader(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.read.lock().map_err(|_| StoreError::Poisoned)
    }

    fn writer(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.write.lock().map_err(|_| StoreError::Poisoned)
    }

    /// The schema version this build writes. A database at any lower version is migrated
    /// forward on open; one at a higher version is [`StoreError::SchemaTooNew`].
    pub const SCHEMA_VERSION: i64 = migrations::LATEST;

    /// The schema version on disk.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        migrations::read_version(&*self.reader()?)
    }

    // ----------------------------------------------------------------- forms

    pub fn save_form(&self, def: &FormDef, config_rev: ConfigRev) -> Result<(), StoreError> {
        forms::save(&*self.writer()?, def, config_rev, &now())
    }

    pub fn load_form(&self, form_id: FormId) -> Result<Arc<FormDef>, StoreError> {
        forms::load(&*self.reader()?, form_id)
    }

    pub fn load_all_forms(&self) -> Result<Vec<Arc<FormDef>>, StoreError> {
        forms::load_all(&*self.reader()?)
    }

    /// Mirrors `ConfigDelta::fields` — every field the server knows about, placed or not.
    ///
    /// Upsert only. A field is **never** removed here when it stops being placed: the row
    /// outliving the placement is the client's only route back to a field it has unplaced,
    /// and to the values still stored against it in `field_value`.
    pub fn save_fields(&self, fields: &[FieldDef]) -> Result<(), StoreError> {
        let mut conn = self.writer()?;
        let tx = conn.transaction()?;
        forms::save_fields(&tx, fields, &now())?;
        tx.commit()?;
        Ok(())
    }

    /// Every known field, ordered by `key`. Includes fields no form places, which is what
    /// lets the builder's "Unplaced fields" drawer survive a restart.
    pub fn all_fields(&self) -> Result<Vec<FieldDef>, StoreError> {
        forms::all_fields(&*self.reader()?)
    }

    // ----------------------------------------------------------------- cases

    pub fn upsert_case(&self, c: &CaseSummary) -> Result<(), StoreError> {
        cases::upsert(&*self.writer()?, c)
    }

    pub fn case(&self, case_id: CaseId) -> Result<CaseRow, StoreError> {
        cases::get(&*self.reader()?, case_id)
    }

    /// The server revision this case is known to be caught up to, or `None` if the case is
    /// not local yet.
    pub fn synced_rev(&self, case_id: CaseId) -> Result<Option<CaseRev>, StoreError> {
        match cases::get(&*self.reader()?, case_id) {
            Ok(c) => Ok(Some(c.synced_rev)),
            Err(StoreError::CaseNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The assignee's caseload, most recently touched first.
    pub fn worklist(&self, assignee: &str, limit: usize) -> Result<Vec<CaseRow>, StoreError> {
        cases::worklist(&*self.reader()?, assignee, limit)
    }

    // ----------------------------------------------------------------- values

    /// Every stored value for one case. R13's read: a primary-key range scan over the
    /// clustered `field_value` rows, no join and no decode of the form definition.
    pub fn load_case_values(&self, case_id: CaseId) -> Result<Vec<(FieldId, Value)>, StoreError> {
        values::load(&*self.reader()?, case_id)
    }

    /// One field's value, or `None` if the field has never been written for this case.
    pub fn value(&self, case_id: CaseId, field_id: FieldId) -> Result<Option<Value>, StoreError> {
        values::get_one(&*self.reader()?, case_id, field_id)
    }

    /// Commits local edits: the values and their outbox rows, in **one transaction**.
    ///
    /// This is the whole write path. There is no debounce — writing to local SQLite *is*
    /// the save, it costs microseconds, and a debounce window is a window in which a crash
    /// loses data.
    pub fn apply_local(
        &self,
        case_id: CaseId,
        changes: &[(FieldId, Value)],
        base_rev: CaseRev,
    ) -> Result<(), StoreError> {
        let mut conn = self.writer()?;
        let tx = conn.transaction()?;
        let now = now();
        for (field_id, value) in changes {
            values::upsert(&tx, case_id, *field_id, value, base_rev, true)?;
            outbox::enqueue(&tx, case_id, *field_id, value, base_rev, &now)?;
        }
        cases::touch_local(&tx, case_id, &now)?;
        tx.commit()?;
        Ok(())
    }

    /// Applies values received from the server.
    ///
    /// Rows with `pending = 1` are skipped: those are unsynced local edits, and the
    /// abstractor's work outranks an inbound snapshot. The UI is separately responsible
    /// for not applying to the field that currently has focus.
    pub fn apply_server_values(
        &self,
        case_id: CaseId,
        rows: &[ValueRow],
        rev: CaseRev,
    ) -> Result<(), StoreError> {
        let mut conn = self.writer()?;
        let tx = conn.transaction()?;
        for row in rows {
            values::upsert_unless_pending(&tx, case_id, row.field_id, &row.value, row.rev)?;
        }
        cases::mark_synced(&tx, case_id, rev)?;
        tx.commit()?;
        Ok(())
    }

    // ----------------------------------------------------------------- outbox

    /// The next queued edits whose backoff has elapsed, oldest first.
    pub fn next_outbox_batch(&self, limit: usize) -> Result<Vec<OutboxRow>, StoreError> {
        outbox::next_batch(&*self.reader()?, limit, &now())
    }

    /// The server accepted these fields at `rev`: clear `pending` and drop the queue rows.
    ///
    /// Known gap: if the abstractor edited one of these fields *after* it was sent, the
    /// outbox row now holds the newer value and dropping it loses that edit. The store
    /// cannot tell — it has no record of what was sent. `medatat-sync` must therefore
    /// confirm only fields it has not seen re-enqueued, the same way `FormInstance` tracks
    /// its in-flight set. Closing it inside the store needs a sequence number on the
    /// outbox row and a `confirm` that carries it.
    pub fn confirm(
        &self,
        case_id: CaseId,
        sent: &[(FieldId, i64)],
        rev: CaseRev,
    ) -> Result<(), StoreError> {
        let mut conn = self.writer()?;
        let tx = conn.transaction()?;
        for (field_id, seq) in sent {
            // Only clear `pending` where the queued edit is still the one that was sent.
            // A row whose `seq` moved on was re-edited mid-flight and must stay pending,
            // or its newer value sits in `field_value` with nothing left to send it.
            if outbox::seq_of(&tx, case_id, *field_id)? == Some(*seq) {
                values::mark_confirmed(&tx, case_id, *field_id, rev)?;
            }
        }
        outbox::drop_rows_at_seq(&tx, case_id, sent)?;
        cases::mark_synced(&tx, case_id, rev)?;
        tx.commit()?;
        Ok(())
    }

    /// Records a failed send against one queued edit and schedules its retry.
    pub fn bump_attempts(
        &self,
        case_id: CaseId,
        field_id: FieldId,
        err: &str,
    ) -> Result<(), StoreError> {
        outbox::bump_attempts(&*self.writer()?, case_id, field_id, err, Utc::now())
    }

    /// Every queued edit, whether or not its retry backoff has elapsed.
    ///
    /// Distinct from [`Store::next_outbox_batch`], which returns only rows that are *due*.
    /// The unsynced indicator must use this one: a row waiting out a backoff is still
    /// unsynced work, and reporting zero would tell the user their edits were safe when
    /// they were not.
    pub fn unsynced_count(&self) -> Result<usize, StoreError> {
        let c = self.reader()?;
        let n: i64 = c.query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn drop_outbox(&self, case_id: CaseId, fields: &[FieldId]) -> Result<(), StoreError> {
        outbox::drop_rows(&*self.writer()?, case_id, fields)
    }

    // ----------------------------------------------------------------- conflicts

    /// Records the server's version of fields it rejected, against the local value.
    pub fn record_conflicts(&self, case_id: CaseId, rows: &[ValueRow]) -> Result<(), StoreError> {
        let mut conn = self.writer()?;
        let tx = conn.transaction()?;
        for row in rows {
            let mine = values::get_one(&tx, case_id, row.field_id)?.unwrap_or(Value::Null);
            conflicts::record(
                &tx,
                case_id,
                row.field_id,
                &mine,
                &row.value,
                row.updated_by.as_ref().map(|a| a.as_str()),
                row.updated_at.as_deref(),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_conflicts(&self, case_id: CaseId) -> Result<Vec<ConflictRow>, StoreError> {
        conflicts::list(&*self.reader()?, case_id)
    }

    /// Resolves one conflict. Never automatic, never in bulk — the abstractor chooses per
    /// field.
    ///
    /// "Keep mine" re-enqueues the local value against the rev the server is now at, so
    /// the retry is no longer stale. "Take theirs" writes the server value and drops the
    /// queued edit.
    pub fn resolve_conflict(
        &self,
        case_id: CaseId,
        field_id: FieldId,
        keep_mine: bool,
    ) -> Result<(), StoreError> {
        let mut conn = self.writer()?;
        let tx = conn.transaction()?;
        let (mine, theirs) = conflicts::get(&tx, case_id, field_id)?
            .ok_or(StoreError::ConflictNotFound { case_id, field_id })?;
        let synced_rev = cases::get(&tx, case_id)?.synced_rev;
        if keep_mine {
            values::upsert(&tx, case_id, field_id, &mine, synced_rev, true)?;
            outbox::enqueue(&tx, case_id, field_id, &mine, synced_rev, &now())?;
        } else {
            values::upsert(&tx, case_id, field_id, &theirs, synced_rev, false)?;
            outbox::drop_rows(&tx, case_id, &[field_id])?;
        }
        conflicts::clear(&tx, case_id, field_id)?;
        tx.commit()?;
        Ok(())
    }

    // ----------------------------------------------------------------- sync state

    pub fn sync_state(&self, k: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .reader()?
            .prepare_cached("SELECT v FROM sync_state WHERE k = ?1")?
            .query_row(params![k], |r| r.get(0))
            .optional()?)
    }

    pub fn set_sync_state(&self, k: &str, v: &str) -> Result<(), StoreError> {
        self.writer()?
            .prepare_cached(
                "INSERT INTO sync_state (k, v) VALUES (?1, ?2) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )?
            .execute(params![k, v])?;
        Ok(())
    }

    /// Read-only escape hatch for tests and diagnostics. Not part of the supported API:
    /// production callers use the typed methods above.
    #[doc(hidden)]
    pub fn with_read<T>(&self, f: impl FnOnce(&Connection) -> T) -> Result<T, StoreError> {
        Ok(f(&*self.reader()?))
    }

    // ----------------------------------------------------------------- test hooks

    #[cfg(test)]
    fn exec(&self, sql: &str) -> Result<(), StoreError> {
        self.writer()?.execute_batch(sql)?;
        Ok(())
    }

    #[cfg(test)]
    fn query_i64(&self, sql: &str) -> Result<i64, StoreError> {
        Ok(self.reader()?.query_row(sql, [], |r| r.get(0))?)
    }

    #[cfg(test)]
    fn explain(&self, sql: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let mut rows = stmt.query(params![CaseId::nil().to_string()])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(r.get::<_, String>(3)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
