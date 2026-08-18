//! Store errors.

use medatat_core::{CaseId, FieldId, FormId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    /// The local database exists but cannot be decrypted, or is not a database at all.
    ///
    /// Reported to the user as "cannot unlock local data". The caller must **never**
    /// respond by recreating the file: to an abstractor with a week of unsynced work that
    /// is indistinguishable from total data loss. See `docs/04-SYNC.md`.
    #[error("cannot unlock local data")]
    Locked,

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("stored value could not be decoded: {0}")]
    Codec(#[from] postcard::Error),

    #[error("stored form definition could not be decoded: {0}")]
    FormCodec(String),

    #[error("form {0} is not in the local store")]
    FormNotFound(FormId),

    #[error("case {0} is not in the local store")]
    CaseNotFound(CaseId),

    #[error("no conflict recorded for field {field_id} of case {case_id}")]
    ConflictNotFound { case_id: CaseId, field_id: FieldId },

    /// A `value_kind` discriminant that this build does not know, or a discriminant whose
    /// typed column is NULL. Both mean the row was written by something else.
    #[error("field {field_id} has a value this build cannot read ({detail})")]
    CorruptValue { field_id: FieldId, detail: String },

    #[error("malformed identifier in the local store: {0}")]
    BadId(String),

    #[error("database schema is version {found}, newer than this build's {expected}")]
    SchemaTooNew { found: i64, expected: i64 },

    /// A previous holder of the write connection panicked mid-transaction.
    #[error("the store lock was poisoned by a panic in another thread")]
    Poisoned,

    /// The directory the database lives in did not exist and could not be created.
    #[error("could not create the data directory {path}: {detail}")]
    DataDir { path: String, detail: String },

    /// The key file beside the database could not be read, created, or generated.
    ///
    /// `docs/02-DATA-MODEL.md` requires the caller to fall back to `open_in_memory` and
    /// re-sync rather than write plaintext where encryption was asked for. A *malformed*
    /// key file is [`StoreError::Locked`] instead — re-keying would present as total data
    /// loss.
    #[cfg(feature = "phi")]
    #[error("database key file: {0}")]
    KeyFile(String),
}

impl StoreError {
    pub(crate) fn form_codec(e: serde_json::Error) -> Self {
        StoreError::FormCodec(e.to_string())
    }

    /// True when SQLite reported `SQLITE_NOTADB` — the file is encrypted with a different
    /// key, or is not a SQLite database.
    pub(crate) fn from_open(e: rusqlite::Error) -> Self {
        match &e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::NotADatabase => {
                StoreError::Locked
            }
            _ => StoreError::Sqlite(e),
        }
    }
}
