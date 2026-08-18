-- medatat D1 schema. See docs/02-DATA-MODEL.md.
-- Apply with: wrangler d1 migrations apply medatat --local|--remote
--
-- This database holds configuration, users, and a worklist index only.
-- The ~100M field values live in per-case Durable Objects (docs/adr/0001).

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------- forms (R3)
CREATE TABLE IF NOT EXISTS form (
  form_id     TEXT PRIMARY KEY,
  key         TEXT NOT NULL UNIQUE,
  name        TEXT NOT NULL,
  archived_at TEXT
);

-- ------------------------------------------------------------- sections (R12)
CREATE TABLE IF NOT EXISTS section (
  section_id TEXT PRIMARY KEY,
  form_id    TEXT NOT NULL REFERENCES form(form_id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  ordinal    INTEGER NOT NULL,
  columns    INTEGER NOT NULL DEFAULT 1 CHECK (columns BETWEEN 1 AND 3)
);
CREATE INDEX IF NOT EXISTS section_form ON section(form_id, ordinal);

-- --------------------------------------------------------------- fields (R2)
-- field_id is global, stable, and never reused. `kind` is immutable after
-- creation -- changing it means creating a new field. See docs/adr/0005.
CREATE TABLE IF NOT EXISTS field (
  field_id    TEXT PRIMARY KEY,
  key         TEXT NOT NULL UNIQUE,
  kind        TEXT NOT NULL CHECK (kind IN
                ('text','numeric','date','time','radio','select','textarea')),
  config      TEXT NOT NULL DEFAULT '{}',
  archived_at TEXT
);

-- ------------------------------------------------------ options (R9, R10)
CREATE TABLE IF NOT EXISTS field_option (
  field_id TEXT    NOT NULL REFERENCES field(field_id) ON DELETE CASCADE,
  code     TEXT    NOT NULL,
  label    TEXT    NOT NULL,
  ordinal  INTEGER NOT NULL,
  PRIMARY KEY (field_id, code)
);

-- ------------------------------------------------------------- placement
-- col_span <= section.columns is enforced in the Worker; SQLite cannot express
-- a cross-table CHECK. It is clamped again at render time.
CREATE TABLE IF NOT EXISTS section_field (
  section_id TEXT    NOT NULL REFERENCES section(section_id) ON DELETE CASCADE,
  field_id   TEXT    NOT NULL REFERENCES field(field_id),
  ordinal    INTEGER NOT NULL,
  col_span   INTEGER NOT NULL DEFAULT 1 CHECK (col_span BETWEEN 1 AND 3),
  label      TEXT    NOT NULL,
  required   INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (section_id, field_id)
);
CREATE INDEX IF NOT EXISTS section_field_order ON section_field(section_id, ordinal);

-- ----------------------------------------------------------------- users
CREATE TABLE IF NOT EXISTS app_user (
  user_id      TEXT PRIMARY KEY,
  email        TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  role         TEXT NOT NULL CHECK (role IN ('abstractor','admin')),
  is_active    INTEGER NOT NULL DEFAULT 1,
  created_at   TEXT NOT NULL
);

-- ------------------------------------------------------------ case index
-- Written by each CaseDO after a successful value write. EVENTUALLY CONSISTENT
-- by design -- a DO write can succeed while this update fails. Repaired by
-- POST /admin/reindex. See docs/10-LIMITATIONS.md #2.
-- Holds NO field values: this is a worklist index, not a shadow copy.
CREATE TABLE IF NOT EXISTS case_index (
  case_id    TEXT PRIMARY KEY,
  mrn        TEXT NOT NULL,
  form_id    TEXT NOT NULL,
  assignee   TEXT,
  rev        INTEGER NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS case_worklist ON case_index(assignee, updated_at DESC);
CREATE INDEX IF NOT EXISTS case_mrn      ON case_index(mrn);

-- -------------------------------------------------------- config version
-- Bumped on every form/section/field/option/placement mutation.
-- Clients poll GET /config?since_rev=<n> every 60s.
CREATE TABLE IF NOT EXISTS config_version (rev INTEGER NOT NULL);
INSERT INTO config_version (rev)
  SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM config_version);
