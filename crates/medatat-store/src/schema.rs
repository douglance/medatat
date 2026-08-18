//! The client schema, embedded.
//!
//! This is the DDL from `docs/02-DATA-MODEL.md`, verbatim except for one addition
//! documented on `field_value.value_kind` below. It is embedded rather than read from a
//! file so a shipped binary can create its database with no external assets.

/// Migration 1 — the initial schema.
pub(crate) const V1: &str = r#"
CREATE TABLE schema_version (version INTEGER NOT NULL);

-- Form definitions. Not PHI. Serialised FormDef (postcard) for one-shot load.
CREATE TABLE form (
  form_id    TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  def_blob   BLOB NOT NULL,
  config_rev INTEGER NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE patient_case (
  case_id    TEXT PRIMARY KEY,
  mrn        TEXT,
  form_id    TEXT NOT NULL REFERENCES form(form_id),
  rev        INTEGER NOT NULL,       -- local revision, monotonic
  synced_rev INTEGER NOT NULL,       -- last rev confirmed by the server
  assignee   TEXT,
  updated_at TEXT NOT NULL
);
CREATE INDEX case_recent   ON patient_case(updated_at DESC);
CREATE INDEX case_assignee ON patient_case(assignee, updated_at DESC);

-- The hot table. WITHOUT ROWID makes the PK the clustered storage order, so one case's
-- ~1000 values occupy a handful of contiguous pages. This is what makes R13 real, and
-- `load_case_values` asserts the plan is a PK range scan.
--
-- `value_kind` is an addition to the DDL in 02-DATA-MODEL.md. Reading a row back has to
-- reconstruct the exact `Value` variant, and the four typed columns cannot do that alone:
-- Text, Radio, and Select all land in `value_text`, so a read cannot tell `Value::Text`
-- from `Value::Opt`. The two ways to recover it are (a) this discriminant column or
-- (b) looking the field up in the cached FormDef. (b) was rejected: it would put a
-- `form` join and a postcard decode of the whole form definition on the R13 read path,
-- and would make a case unreadable whenever its form definition had not been synced yet.
-- The column stores the `Value` discriminant, not `FieldKind::tag()`, because the variant
-- is what a read must rebuild and it is derivable from the value alone.
CREATE TABLE field_value (
  case_id       TEXT    NOT NULL REFERENCES patient_case(case_id) ON DELETE CASCADE,
  field_id      TEXT    NOT NULL,
  value_kind    TEXT    NOT NULL
                CHECK (value_kind IN ('null','text','num','date','time','opt')),
  value_text    TEXT,
  value_numeric TEXT,                -- decimal string, never REAL
  value_date    TEXT,                -- YYYY-MM-DD
  value_time    TEXT,                -- HH:MM
  rev           INTEGER NOT NULL,
  pending       INTEGER NOT NULL DEFAULT 0,   -- 1 = local edit not yet acked by server
  PRIMARY KEY (case_id, field_id)
) WITHOUT ROWID;

-- Coalescing outbox: PK is (case_id, field_id) so 40 keystrokes collapse to one row.
CREATE TABLE outbox (
  case_id         TEXT    NOT NULL,
  field_id        TEXT    NOT NULL,
  value_blob      BLOB    NOT NULL,  -- postcard-encoded medatat_core::Value
  base_rev        INTEGER NOT NULL,  -- server rev the edit was made against
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT    NOT NULL,
  last_error      TEXT,
  PRIMARY KEY (case_id, field_id)
) WITHOUT ROWID;
CREATE INDEX outbox_ready ON outbox(next_attempt_at);

-- Unresolved per-field conflicts. Survives restart; rendered as an inline strip.
CREATE TABLE conflict (
  case_id    TEXT NOT NULL,
  field_id   TEXT NOT NULL,
  mine       BLOB NOT NULL,
  theirs     BLOB NOT NULL,
  theirs_by  TEXT,
  theirs_at  TEXT,
  PRIMARY KEY (case_id, field_id)
) WITHOUT ROWID;

CREATE TABLE sync_state (k TEXT PRIMARY KEY, v TEXT);
"#;

/// Migration 2 — the `field` table.
///
/// The client had no mirror of D1's `field` table, so a field existed locally only inside
/// `form.def_blob`. Unplacing it from every section therefore erased the client's only
/// route to it: the values stayed safe in `field_value`, keyed by `field_id`, but nothing
/// referenced them any more and the builder's "Unplaced fields" drawer could not survive a
/// restart (`docs/06-FORM-BUILDER.md` acceptance item 9).
pub(crate) const V2: &str = r#"
-- Every field that exists, whether or not any form places it. Populated from
-- ConfigDelta::fields.
--
-- Rows are never deleted on unplacement. That is the entire point: the row outliving the
-- placement is what gives the builder a way back to a field, and to the values still
-- stored against it. Archival, if it is ever wanted, belongs here as a flag, never as a
-- DELETE.
--
-- `key` is indexed but not UNIQUE, unlike the D1 table it mirrors. Key uniqueness is the
-- server's invariant to enforce at the point of change; a client that also enforced it
-- could reject a whole inbound batch because two rows collided partway through a rename,
-- and a mirror that refuses to mirror is worse than one that lags.
CREATE TABLE field (
  field_id   TEXT PRIMARY KEY,
  key        TEXT NOT NULL,
  def_blob   BLOB NOT NULL,          -- JSON-encoded medatat_core::FieldDef
  updated_at TEXT NOT NULL
);
CREATE INDEX field_key ON field(key);
"#;

/// V3 — a monotonic sequence on each queued edit.
///
/// Without it, `confirm` drops an outbox row by `(case_id, field_id)` alone. If the
/// abstractor edits that field again *while the first value is in flight*, `enqueue`
/// upserts the row in place and `confirm` then deletes the newer value — which sits in
/// `field_value` with `pending` cleared, so nothing ever sends it. A silent lost update.
///
/// The sequence is stored rather than held in memory on purpose: the outbox is on disk
/// precisely so a crash cannot lose queued work, and an in-memory in-flight set would
/// reintroduce the same class of loss through a different door.
pub(crate) const V3: &str = r#"
ALTER TABLE outbox ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
"#;
