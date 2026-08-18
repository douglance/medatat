//! [`CaseStore`] over a Durable Object's SQLite storage (`docs/02-DATA-MODEL.md`, store 3).
//!
//! **No DDL runs in the `CaseDO` constructor** — the constructor is on every cold-start
//! path. The schema is created lazily, on the first *write* only; reads check
//! `sqlite_master` and answer an empty case rather than materialising anything.
//!
//! One deviation from the documented DO schema, deliberately: `field_value` carries a
//! `kind` column. Radio, select, text, and textarea all land in `value_text`, so without
//! the kind tag a read cannot tell `Value::Opt("M")` from `Value::Text("M")` and the wire
//! round-trip in `docs/03-API.md` is lossy. The alternative — shipping field definitions
//! into the DO on every read — costs a D1 hit per GET to recover information the row could
//! have carried for a few bytes.

use crate::error::{LogicError, LogicResult};
use crate::logic::case_store::{CaseStore, FieldLookup};
use chrono::{SecondsFormat, Utc};
use medatat_core::ids::{ActorId, CaseRev, FieldId, OptionCode};
use medatat_core::value::{Value, parse_date, parse_decimal, parse_time_24};
use medatat_core::wire::{ValueChange, ValueRow};
use serde::Deserialize;
use std::cell::Cell;
use worker::{SqlStorage, SqlStorageValue};

const REV_KEY: &str = "rev";

pub struct SqlCaseStore<'a> {
    sql: SqlStorage,
    /// Present on write paths only. A write needs the field's kind to choose its storage
    /// column, and to tag a `Null` that carries no type of its own.
    defs: Option<&'a FieldLookup>,
    schema_ready: Cell<bool>,
}

impl<'a> SqlCaseStore<'a> {
    /// A read-only view. Never creates the schema.
    pub fn read_only(sql: SqlStorage) -> Self {
        SqlCaseStore {
            sql,
            defs: None,
            schema_ready: Cell::new(false),
        }
    }

    pub fn with_defs(sql: SqlStorage, defs: &'a FieldLookup) -> Self {
        SqlCaseStore {
            sql,
            defs: Some(defs),
            schema_ready: Cell::new(false),
        }
    }

    fn exec(&self, query: &str, bindings: Vec<SqlStorageValue>) -> LogicResult<worker::SqlCursor> {
        self.sql
            .exec(query, bindings)
            .map_err(|e| LogicError::Storage(format!("{e}")))
    }

    fn rows<T: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        bindings: Vec<SqlStorageValue>,
    ) -> LogicResult<Vec<T>> {
        self.exec(query, bindings)?
            .to_array::<T>()
            .map_err(|e| LogicError::Storage(format!("{e}")))
    }

    /// Whether this object has ever been written to. Keeps DDL off every read path.
    fn schema_exists(&self) -> LogicResult<bool> {
        if self.schema_ready.get() {
            return Ok(true);
        }
        let found: Vec<NameRow> = self.rows(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'field_value'",
            vec![],
        )?;
        let exists = !found.is_empty();
        self.schema_ready.set(exists);
        Ok(exists)
    }

    /// Create the schema. Called from the write path only, never from `new`.
    pub fn ensure_schema(&self) -> LogicResult<()> {
        if self.schema_ready.get() {
            return Ok(());
        }
        self.exec(
            "CREATE TABLE IF NOT EXISTS field_value (
               field_id      TEXT PRIMARY KEY,
               kind          TEXT NOT NULL,
               value_text    TEXT,
               value_numeric TEXT,
               value_date    TEXT,
               value_time    TEXT,
               rev           INTEGER NOT NULL,
               updated_at    TEXT NOT NULL,
               updated_by    TEXT NOT NULL
             ) WITHOUT ROWID",
            vec![],
        )?;
        self.exec(
            "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT)",
            vec![],
        )?;
        self.schema_ready.set(true);
        Ok(())
    }

    pub fn meta_get(&self, key: &str) -> LogicResult<Option<String>> {
        if !self.schema_exists()? {
            return Ok(None);
        }
        let rows: Vec<MetaRow> = self.rows(
            "SELECT v FROM meta WHERE k = ?",
            vec![SqlStorageValue::String(key.to_string())],
        )?;
        Ok(rows.into_iter().next().and_then(|r| r.v))
    }

    pub fn meta_set(&self, key: &str, value: &str) -> LogicResult<()> {
        self.ensure_schema()?;
        self.exec(
            "INSERT INTO meta (k, v) VALUES (?, ?)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            vec![
                SqlStorageValue::String(key.to_string()),
                SqlStorageValue::String(value.to_string()),
            ],
        )?;
        Ok(())
    }

    fn kind_tag_for(&self, field_id: FieldId, value: &Value) -> LogicResult<&'static str> {
        if let Some(defs) = self.defs
            && let Some(def) = defs.get(field_id)
        {
            return Ok(def.kind.tag());
        }
        // A write should always arrive with definitions; fall back to the value's own shape
        // rather than losing the row, and refuse only when even that is impossible.
        match value {
            Value::Text(_) => Ok("text"),
            Value::Opt(_) => Ok("select"),
            Value::Num(_) => Ok("numeric"),
            Value::Date(_) => Ok("date"),
            Value::Time(_) => Ok("time"),
            Value::Null => Err(LogicError::UnknownField(field_id)),
        }
    }
}

#[derive(Deserialize)]
struct NameRow {
    #[allow(dead_code)]
    name: String,
}

#[derive(Deserialize)]
struct MetaRow {
    v: Option<String>,
}

#[derive(Deserialize)]
struct SqlValueRow {
    field_id: String,
    kind: String,
    value_text: Option<String>,
    value_numeric: Option<String>,
    value_date: Option<String>,
    value_time: Option<String>,
    rev: i64,
    updated_at: String,
    updated_by: String,
}

impl SqlValueRow {
    fn into_wire(self) -> LogicResult<ValueRow> {
        let field_id = FieldId::parse(&self.field_id)
            .map_err(|e| LogicError::Storage(format!("bad field_id in row: {e}")))?;
        let value = self.decode_value(field_id)?;
        Ok(ValueRow {
            field_id,
            value,
            rev: CaseRev(self.rev),
            updated_by: Some(ActorId::new(self.updated_by)),
            updated_at: Some(self.updated_at),
        })
    }

    fn decode_value(&self, field_id: FieldId) -> LogicResult<Value> {
        let bad = |what: &str| LogicError::Storage(format!("undecodable {what} for {field_id}"));
        Ok(match self.kind.as_str() {
            "radio" | "select" => match &self.value_text {
                Some(code) => Value::Opt(OptionCode::new(code.clone())),
                None => Value::Null,
            },
            "text" | "textarea" => match &self.value_text {
                Some(s) => Value::Text(s.clone()),
                None => Value::Null,
            },
            "numeric" => match &self.value_numeric {
                Some(s) => Value::Num(parse_decimal(s).map_err(|_| bad("numeric"))?),
                None => Value::Null,
            },
            "date" => match &self.value_date {
                Some(s) => Value::Date(parse_date(s).map_err(|_| bad("date"))?),
                None => Value::Null,
            },
            "time" => match &self.value_time {
                Some(s) => Value::Time(parse_time_24(s).map_err(|_| bad("time"))?),
                None => Value::Null,
            },
            other => return Err(LogicError::Storage(format!("unknown kind tag {other:?}"))),
        })
    }
}

/// The four typed columns, exactly one of which is non-NULL.
///
/// The slot is chosen by `Value::column()`, which is `medatat-core`'s own
/// `FieldKind`→column mapping — there is deliberately no second one here.
fn columns_for(value: &Value) -> [SqlStorageValue; 4] {
    use medatat_core::value::ValueColumn;
    let mut cols = [
        SqlStorageValue::Null,
        SqlStorageValue::Null,
        SqlStorageValue::Null,
        SqlStorageValue::Null,
    ];
    if let (Some(column), Some(stored)) = (value.column(), value.to_storage_string()) {
        let slot = match column {
            ValueColumn::Text => 0,
            ValueColumn::Numeric => 1,
            ValueColumn::Date => 2,
            ValueColumn::Time => 3,
        };
        cols[slot] = SqlStorageValue::String(stored);
    }
    cols
}

impl CaseStore for SqlCaseStore<'_> {
    fn rev(&self) -> LogicResult<CaseRev> {
        match self.meta_get(REV_KEY)? {
            None => Ok(CaseRev::ZERO),
            Some(s) => s
                .parse::<i64>()
                .map(CaseRev)
                .map_err(|e| LogicError::Storage(format!("bad meta.rev: {e}"))),
        }
    }

    fn changed_since(&self, fields: &[FieldId], since: CaseRev) -> LogicResult<Vec<ValueRow>> {
        if fields.is_empty() || !self.schema_exists()? {
            return Ok(Vec::new());
        }
        // Per-field conflict detection, and nothing wider: only rows among the fields this
        // batch actually touches can conflict with it.
        let placeholders = std::iter::repeat_n("?", fields.len())
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT field_id, kind, value_text, value_numeric, value_date, value_time,
                    rev, updated_at, updated_by
               FROM field_value
              WHERE field_id IN ({placeholders}) AND rev > ?"
        );
        let mut bindings: Vec<SqlStorageValue> = fields
            .iter()
            .map(|f| SqlStorageValue::String(f.to_string()))
            .collect();
        bindings.push(SqlStorageValue::Integer(since.0));

        self.rows::<SqlValueRow>(&query, bindings)?
            .into_iter()
            .map(SqlValueRow::into_wire)
            .collect()
    }

    fn get_all(&self, since: CaseRev) -> LogicResult<Vec<ValueRow>> {
        if !self.schema_exists()? {
            return Ok(Vec::new());
        }
        self.rows::<SqlValueRow>(
            "SELECT field_id, kind, value_text, value_numeric, value_date, value_time,
                    rev, updated_at, updated_by
               FROM field_value
              WHERE rev > ?
              ORDER BY field_id",
            vec![SqlStorageValue::Integer(since.0)],
        )?
        .into_iter()
        .map(SqlValueRow::into_wire)
        .collect()
    }

    fn put(&self, changes: &[ValueChange], rev: CaseRev, actor: &ActorId) -> LogicResult<()> {
        if actor.as_str().trim().is_empty() {
            return Err(LogicError::NoActor);
        }
        // First write on this object: this is the only place the schema is created.
        self.ensure_schema()?;

        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        for change in changes {
            let tag = self.kind_tag_for(change.field_id, &change.value)?;
            let [text, numeric, date, time] = columns_for(&change.value);
            self.exec(
                "INSERT INTO field_value
                   (field_id, kind, value_text, value_numeric, value_date, value_time,
                    rev, updated_at, updated_by)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(field_id) DO UPDATE SET
                   kind = excluded.kind,
                   value_text = excluded.value_text,
                   value_numeric = excluded.value_numeric,
                   value_date = excluded.value_date,
                   value_time = excluded.value_time,
                   rev = excluded.rev,
                   updated_at = excluded.updated_at,
                   updated_by = excluded.updated_by",
                vec![
                    SqlStorageValue::String(change.field_id.to_string()),
                    SqlStorageValue::String(tag.to_string()),
                    text,
                    numeric,
                    date,
                    time,
                    SqlStorageValue::Integer(rev.0),
                    SqlStorageValue::String(now.clone()),
                    SqlStorageValue::String(actor.as_str().to_string()),
                ],
            )?;
        }
        self.meta_set(REV_KEY, &rev.0.to_string())?;
        Ok(())
    }
}
