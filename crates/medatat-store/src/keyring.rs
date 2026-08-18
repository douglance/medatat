//! Local database key material. `phi` feature only.
//!
//! The key is 32 bytes from the OS CSPRNG, hex-encoded, in `medatat.key` beside the
//! database with mode 0600.
//!
//! **Not the OS keychain** — the user has forbidden it (AGENTS.md rule 11). The honest
//! consequence is recorded as a downgrade in `docs/12-PHI-READINESS.md` checklist item 6:
//! a key file beside the ciphertext protects a copied database only if the key is not
//! copied along with it, and protects nothing against another process running as the same
//! user. That is adequate for the synthetic corpus this system handles today, and is a
//! decision to revisit before real patient data — not a gap to discover later.

use crate::error::StoreError;
use std::path::{Path, PathBuf};

const KEY_FILE: &str = "medatat.key";

/// Reads the key beside `db_path`, creating one on first run.
pub(crate) fn get_or_create(db_path: &Path) -> Result<String, StoreError> {
    let path = key_path(db_path);

    match std::fs::read_to_string(&path) {
        Ok(existing) => {
            let key = existing.trim();
            if is_valid_key(key) {
                return Ok(key.to_string());
            }
            // A malformed key file can only have come from a partial write. Minting a
            // replacement would make the database it guarded permanently unreadable,
            // which presents to the user as total data loss — so this reports "locked"
            // and leaves both files alone.
            Err(StoreError::Locked)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = mint()?;
            write_private(&path, &key)?;
            Ok(key)
        }
        Err(e) => Err(StoreError::KeyFile(e.to_string())),
    }
}

fn key_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|dir| dir.join(KEY_FILE))
        .unwrap_or_else(|| PathBuf::from(KEY_FILE))
}

/// Writes the key readable only by its owner. On Unix the mode is set as part of the
/// `open`, not afterwards, so there is no window in which the key is world-readable.
fn write_private(path: &Path, key: &str) -> Result<(), StoreError> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StoreError::KeyFile(e.to_string()))?;
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }

    let mut f = opts
        .open(path)
        .map_err(|e| StoreError::KeyFile(e.to_string()))?;
    f.write_all(key.as_bytes())
        .map_err(|e| StoreError::KeyFile(e.to_string()))?;
    Ok(())
}

fn mint() -> Result<String, StoreError> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw)
        .map_err(|e| StoreError::KeyFile(format!("could not generate a key: {e}")))?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

fn is_valid_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_is_enforced() {
        assert!(is_valid_key(&"a".repeat(64)));
        assert!(!is_valid_key(&"a".repeat(63)));
        assert!(!is_valid_key(&"z".repeat(64)));
        assert!(!is_valid_key(""));
    }

    #[test]
    fn minted_keys_are_valid_and_distinct() {
        let a = mint().unwrap();
        let b = mint().unwrap();
        assert!(is_valid_key(&a));
        assert_ne!(a, b, "32 bytes of OsRng must not repeat");
    }

    #[test]
    fn key_is_created_once_and_then_reused() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("medatat.db");
        let first = get_or_create(&db).unwrap();
        assert_eq!(
            get_or_create(&db).unwrap(),
            first,
            "reopening must not re-key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("medatat.db");
        get_or_create(&db).unwrap();
        let mode = std::fs::metadata(key_path(&db))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the key file must not be group or world readable"
        );
    }

    #[test]
    fn a_malformed_key_file_locks_rather_than_re_keying() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("medatat.db");
        std::fs::write(key_path(&db), "not-a-key").unwrap();
        assert!(matches!(get_or_create(&db), Err(StoreError::Locked)));
    }
}
