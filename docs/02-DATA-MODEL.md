# 02 — Data Model

Three stores. All schemas here are authoritative; migrations live beside the code that
owns them.

## Core principle: stable field identity

`field.id` is a UUID that is **global, stable, and never reused**. Values attach to
`field_id`, never to a placement. This is what delivers R2 and R3 with one mechanism:

- Moving a field between sections, relabelling it, or changing its column span touches
  only `section_field`. Stored values are untouched.
- Deleting a field placement never deletes values.
- **A field's `kind` is never re-typed in place.** Changing a text field to a numeric field
  creates a *new* field with a new id; the old field's values remain intact and viewable.
  This single invariant replaces the entire form-version lifecycle that was cut.

## Value representation

Seven field kinds map onto four storage columns:

| `FieldKind` | Column used | Rust type | Storage encoding |
|---|---|---|---|
| `Text` | `value_text` | `SharedStr` | UTF-8 |
| `Textarea` | `value_text` | `SharedStr` | UTF-8 |
| `Numeric` | `value_numeric` | `rust_decimal::Decimal` | **decimal string**, e.g. `"12.50"` |
| `Date` | `value_date` | `chrono::NaiveDate` | ISO-8601 `YYYY-MM-DD` |
| `Time` | `value_time` | `chrono::NaiveTime` | `HH:MM` (24hr, always zero-padded) |
| `Radio` | `value_text` | `OptionId` | the option's `code` |
| `Select` | `value_text` | `OptionId` | the option's `code` |

**Numeric is stored as TEXT, never REAL.** SQLite has no exact numeric type and IEEE-754
would silently corrupt clinical values. Parse with `rust_decimal::Decimal::from_str`.
This rule is non-negotiable and has a test.

**Typed columns, not one text column.** Exactly one column is non-NULL per row, determined
by the field's kind. This keeps type errors at write time rather than query time, and keeps
exports typed. NULL columns cost nothing meaningful in SQLite.

---

## Store 1 — Client (encrypted SQLite, SQLCipher)

Owned by `medatat-store`. This is the UI's system of record. Location:

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/medatat/medatat.db` |
| Linux | `$XDG_DATA_HOME/medatat/medatat.db` (fallback `~/.local/share/medatat/`) |
| Windows | `%APPDATA%\medatat\medatat.db` |

`field_value` carries one column the original sketch did not:
**`value_kind`**, holding the `Value` discriminant. It is necessary because Text, Radio,
and Select all land in `value_text`, so the typed columns alone cannot tell `Value::Text`
from `Value::Opt` on read. The alternative — looking the field up in the cached `FormDef` —
was rejected: it would put a join and a full form-definition decode on the R13 read path,
and would make a case unreadable whenever its form definition had not synced yet.

Form definitions are stored as **JSON**, not postcard. `FieldKind` is an internally-tagged
enum so the wire shape in [03-API.md](03-API.md) reads naturally, and internal tagging needs
a self-describing format — postcard returns `WontImplement`. Values still use postcard,
where density matters and the enum is externally tagged.

```sql
-- Applied immediately after PRAGMA key, before any other statement.
PRAGMA key = "x'<32 bytes from medatat.key, as hex>'";
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA cache_size   = -65536;   -- 64 MiB

CREATE TABLE schema_version (version INTEGER NOT NULL);

-- Form definitions. Not PHI. Serialised FormDef (JSON) for one-shot load.
CREATE TABLE form (
  form_id    TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  def_blob   BLOB NOT NULL,          -- JSON-encoded medatat_core::FormDef
  config_rev INTEGER NOT NULL,       -- server config version this was fetched at
  updated_at TEXT NOT NULL
);

-- Every field that exists, whether or not a form places it. Mirrors D1's `field` table
-- and is populated from ConfigDelta::fields.
--
-- Rows are never deleted when a field is unplaced. `form.def_blob` can only ever describe
-- *placed* fields, so without this table unplacing a field erases the client's last route
-- to it — while its values sit on in `field_value`, referenced by nothing. That is what
-- makes the builder's "Unplaced fields" drawer survive a restart
-- ([06-FORM-BUILDER.md](06-FORM-BUILDER.md) acceptance item 9). Archival, if it is ever
-- wanted, is a flag here rather than a DELETE.
--
-- `key` is indexed but not UNIQUE, unlike the D1 table: key uniqueness is the server's
-- invariant to enforce at the point of change, and a client that also enforced it could
-- reject a whole inbound batch over a transient collision partway through a rename.
CREATE TABLE field (
  field_id   TEXT PRIMARY KEY,
  key        TEXT NOT NULL,
  def_blob   BLOB NOT NULL,          -- JSON-encoded medatat_core::FieldDef
  updated_at TEXT NOT NULL
);
CREATE INDEX field_key ON field(key);

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

-- The hot table. WITHOUT ROWID makes the PK the clustered storage order, so one
-- case's ~1000 values occupy a handful of contiguous pages. This is what makes R13 real.
CREATE TABLE field_value (
  case_id       TEXT    NOT NULL REFERENCES patient_case(case_id) ON DELETE CASCADE,
  field_id      TEXT    NOT NULL,
  value_kind    TEXT    NOT NULL     -- Value discriminant; see the note above
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

CREATE TABLE sync_state (k TEXT PRIMARY KEY, v TEXT);  -- cursor, last_full_sync, config_rev
```

### Encryption — `phi` feature only

**Default builds use plain SQLite.** This system handles synthetic data today, so the
`PRAGMA key` line above is emitted only under `--features phi`. See
[12-PHI-READINESS.md](12-PHI-READINESS.md).

Under `--features phi`: `rusqlite` gains `bundled-sqlcipher`. On first run, generate 32
bytes from `OsRng`, hex-encode, and write it to `medatat.key` beside the database with mode
`0600`. On every open, read the key and issue `PRAGMA key` as the **first** statement.

**Not the OS keychain**, at the user's explicit instruction. The trade-off and when to
revisit it are recorded in [12-PHI-READINESS.md](12-PHI-READINESS.md).

Verify the key by reading `schema_version`; a wrong key surfaces as `SQLITE_NOTADB`, which
must be reported as "cannot unlock local data", never as corruption — recreating the
database would look like total data loss.

**If the key cannot be established or read:** fall back to an in-memory database and
re-sync each launch. Log a clear warning. Never silently write plaintext when the caller
asked for encryption. A *malformed* key file is reported as `Locked` rather than re-keyed —
re-keying would present to the user as total data loss.

---

## Store 2 — Server D1 (config, users, case index)

Owned by `medatat-worker`. Migrations in `crates/medatat-worker/migrations/`.

```sql
CREATE TABLE form (
  form_id     TEXT PRIMARY KEY,
  key         TEXT NOT NULL UNIQUE,
  name        TEXT NOT NULL,
  archived_at TEXT
);

CREATE TABLE section (                                        -- R12
  section_id TEXT PRIMARY KEY,
  form_id    TEXT NOT NULL REFERENCES form(form_id),
  name       TEXT NOT NULL,
  ordinal    INTEGER NOT NULL,
  columns    INTEGER NOT NULL DEFAULT 1 CHECK (columns BETWEEN 1 AND 3)
);
CREATE INDEX section_form ON section(form_id, ordinal);

CREATE TABLE field (                                          -- R2
  field_id    TEXT PRIMARY KEY,
  key         TEXT NOT NULL UNIQUE,
  kind        TEXT NOT NULL CHECK (kind IN
                ('text','numeric','date','time','radio','select','textarea')),  -- R5–R11
  config      TEXT NOT NULL DEFAULT '{}',   -- JSON: min,max,scale,max_len,rows
  archived_at TEXT
);

CREATE TABLE field_option (                                   -- R9, R10
  field_id TEXT    NOT NULL REFERENCES field(field_id),
  code     TEXT    NOT NULL,
  label    TEXT    NOT NULL,
  ordinal  INTEGER NOT NULL,
  PRIMARY KEY (field_id, code)
);

CREATE TABLE section_field (                                  -- placement + presentation
  section_id TEXT    NOT NULL REFERENCES section(section_id) ON DELETE CASCADE,
  field_id   TEXT    NOT NULL REFERENCES field(field_id),
  ordinal    INTEGER NOT NULL,
  col_span   INTEGER NOT NULL DEFAULT 1 CHECK (col_span BETWEEN 1 AND 3),
  label      TEXT    NOT NULL,
  required   INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (section_id, field_id)
);

CREATE TABLE app_user (
  user_id      TEXT PRIMARY KEY,
  email        TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  role         TEXT NOT NULL CHECK (role IN ('abstractor','admin')),
  is_active    INTEGER NOT NULL DEFAULT 1,
  created_at   TEXT NOT NULL
);

-- Written by each CaseDO after a successful value write. Eventually consistent by design.
CREATE TABLE case_index (
  case_id    TEXT PRIMARY KEY,
  mrn        TEXT NOT NULL,
  form_id    TEXT NOT NULL,
  assignee   TEXT,
  rev        INTEGER NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX case_worklist ON case_index(assignee, updated_at DESC);
CREATE INDEX case_mrn      ON case_index(mrn);

-- Bumped on any form/section/field/option/placement mutation. Clients poll it.
CREATE TABLE config_version (rev INTEGER NOT NULL);
```

**`col_span <= columns` is enforced on write** in the Worker, not by a CHECK (SQLite cannot
express a cross-table CHECK). It is also clamped defensively at render.

### D1 sizing

Config is a few thousand rows. `case_index` is 100k rows at ~120 bytes ≈ **12 MB**. The
entire D1 stays under 100 MB, three orders of magnitude below the 10 GB cap. The 100M
values never touch D1 — that is the whole point of the topology.

---

## Store 3 — CaseDO SQLite (R16)

One Durable Object per `case_id`, addressed by `idFromName(case_id)` so the same case
always reaches the same object without an id round-trip through D1.

**No `jurisdiction("us")`.** The Workers API accepts a jurisdiction only on `newUniqueId()`
and rejects it with `idFromName()`. Deterministic addressing is what the rest of the system
depends on, so it wins; pinning a jurisdiction would mean storing each generated id in
`case_index` and looking it up on every call. If data residency becomes a requirement, that
is the trade to revisit.

```rust
#[durable_object]
pub struct CaseDO { sql: SqlStorage, env: Env }

impl DurableObject for CaseDO {
    fn new(state: State, env: Env) -> Self {
        // NOTE: no DDL here. Schema is created lazily on the first write.
        // Running CREATE TABLE in the constructor puts DDL on every cold-start path.
        // `env` is kept so the DO can update its own `case_index` row in D1 after a write.
        Self { sql: state.storage().sql(), env }
    }
    async fn fetch(&self, req: Request) -> Result<Response> { /* see 03-API.md */ }
}
```

```sql
-- Created lazily, on the first write only. `POST /cases` writes `meta`, so in practice the
-- schema appears when the case is created rather than when its first value lands.
CREATE TABLE IF NOT EXISTS field_value (
  field_id      TEXT PRIMARY KEY,     -- case_id is implicit: it IS this object
  kind          TEXT NOT NULL,        -- FieldKind::tag(); see the note below
  value_text    TEXT,
  value_numeric TEXT,
  value_date    TEXT,
  value_time    TEXT,
  rev           INTEGER NOT NULL,
  updated_at    TEXT NOT NULL,
  updated_by    TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
-- keys: rev, case_id, form_id, mrn, assignee, created_at
```

The DO's `kind` column solves the same problem as the client's `value_kind`, but stores a
different thing, and the two are easy to confuse:

| | client `field_value.value_kind` | DO `field_value.kind` |
|---|---|---|
| stores | the `Value` discriminant (`text`, `num`, `opt`, …) | `FieldKind::tag()` (`text`, `numeric`, `select`, …) |
| recovered from | the value alone | the field definition sent with the write |

Both exist because Text, Radio, and Select all land in `value_text`, so the typed columns
cannot tell `Value::Text` from `Value::Opt` on read. The alternative for the DO — shipping
field definitions in on every read — would cost a D1 hit per `GET` to recover information
the row can carry in a few bytes.

### Revision semantics

`meta.rev` is a per-case counter, incremented once per accepted write batch. **The DO is
single-threaded, so this is race-free with no locking** — the `SELECT ... FOR UPDATE` that
a relational design would need simply does not exist here. Each written `field_value.rev`
records the case rev at which that field last changed, which is what makes per-field
conflict detection precise (see [04-SYNC.md](04-SYNC.md)).

### Sizing

~1000 values × ~60 bytes ≈ **60–100 KB per DO**, against a 10 GB per-object limit —
roughly 100,000× headroom. 100k DOs ≈ 10 GB total at $0.20/GB-mo ≈ **$2/month**.

---

## Rust type mapping

Defined in `medatat-core`. These are the wire types too; there is no separate proto crate.

```rust
pub struct FieldId(pub Uuid);    // globally stable, never reused
pub struct CaseId(pub Uuid);
pub struct FieldIdx(pub u32);    // dense index within one FormDef — O(1) on hot paths
pub struct CaseRev(pub i64);

pub enum FieldKind {
    Text     { max_len: Option<u32> },
    Numeric  { min: Option<Decimal>, max: Option<Decimal>, scale: u8 },
    Date,
    Time,
    Radio    { options: Arc<[FieldOption]> },
    Select   { options: Arc<[FieldOption]>, searchable: bool },
    Textarea { rows: u16, max_len: Option<u32> },
}

pub enum Value {
    Null,
    Text(SharedStr),
    Num(Decimal),
    Date(NaiveDate),
    Time(NaiveTime),
    Opt(OptionCode),
}

pub struct FormDef {
    pub form_id: FormId,
    pub name: SharedStr,
    pub sections: Vec<SectionDef>,
    pub by_id: HashMap<FieldId, FieldIdx>,   // built once at load
    pub field_count: usize,
}
pub struct SectionDef {
    pub section_id: SectionId,
    pub title: SharedStr,
    pub columns: u8,                          // 1..=3, R12
    pub default_collapsed: bool,
    pub fields: Vec<SectionField>,
}
pub struct SectionField {
    pub idx: FieldIdx,
    pub field: Arc<FieldDef>,
    pub label: SharedStr,
    pub col_span: u8,                         // 1..=3, clamped to section.columns
    pub required: bool,
}
```

### PHI hygiene — `phi` feature only

Under `--features phi`, `Value` gets a redacting `Debug`. Default builds derive an ordinary
`Debug` so values are printable during development, which is the whole point of the flag.

```rust
#[cfg(feature = "phi")]
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null      => write!(f, "Null"),
            Value::Text(s)   => write!(f, "Text(<redacted, {} chars>)", s.len()),
            Value::Num(_)    => write!(f, "Num(<redacted>)"),
            Value::Date(_)   => write!(f, "Date(<redacted>)"),
            Value::Time(_)   => write!(f, "Time(<redacted>)"),
            Value::Opt(_)    => write!(f, "Opt(<redacted>)"),
        }
    }
}
```

Also under `phi`, `Value` implements `Drop` to zeroize heap payloads, and a clippy lint
bans `#[derive(Debug)]` on any type containing `Value`.
`core::tests::debug_redacts` runs under `--features phi` and asserts no plaintext appears in
`format!("{:?}", v)`. The CI job that builds with the feature is what keeps this path from
rotting while it is switched off.
