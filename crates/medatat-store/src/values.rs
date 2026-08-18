//! `field_value` encoding, reads, and the local write path.

use crate::error::StoreError;
use medatat_core::{
    CaseId, CaseRev, FieldId, OptionCode, Value, parse_date, parse_decimal, parse_time_24,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// The read that R13 depends on. `WHERE case_id = ?` against a `WITHOUT ROWID` table keyed
/// on `(case_id, field_id)` is a primary-key range scan over contiguous pages — see the
/// `explain_query_plan` test, which fails if this ever degrades to a table scan.
pub(crate) const LOAD_SQL: &str = "SELECT field_id, value_kind, value_text, value_numeric, \
     value_date, value_time \
     FROM field_value WHERE case_id = ?1";

/// One value, split across the typed columns.
pub(crate) struct Cols {
    pub kind: &'static str,
    pub text: Option<String>,
    pub numeric: Option<String>,
    pub date: Option<String>,
    pub time: Option<String>,
}

/// Splits a `Value` into its storage columns. Encoding is always
/// [`Value::to_storage_string`] — this function only chooses the column.
pub(crate) fn encode(v: &Value) -> Cols {
    let empty = Cols {
        kind: "null",
        text: None,
        numeric: None,
        date: None,
        time: None,
    };
    let s = v.to_storage_string();
    match v {
        Value::Null => empty,
        Value::Text(_) => Cols {
            kind: "text",
            text: s,
            ..empty
        },
        Value::Opt(_) => Cols {
            kind: "opt",
            text: s,
            ..empty
        },
        // Decimal strings, never REAL. SQLite has no exact numeric type and IEEE-754
        // would silently corrupt a dose or a lab result.
        Value::Num(_) => Cols {
            kind: "num",
            numeric: s,
            ..empty
        },
        Value::Date(_) => Cols {
            kind: "date",
            date: s,
            ..empty
        },
        Value::Time(_) => Cols {
            kind: "time",
            time: s,
            ..empty
        },
    }
}

/// Rebuilds the exact `Value` variant a row was written from.
pub(crate) fn decode(
    field_id: FieldId,
    kind: &str,
    cell: Option<String>,
) -> Result<Value, StoreError> {
    let corrupt = |detail: String| StoreError::CorruptValue { field_id, detail };
    let take = |k: &str| -> Result<String, StoreError> {
        cell.clone()
            .ok_or_else(|| corrupt(format!("kind '{k}' but its column is NULL")))
    };
    Ok(match kind {
        "null" => Value::Null,
        "text" => Value::Text(take("text")?),
        "opt" => Value::Opt(OptionCode::new(take("opt")?)),
        "num" => Value::Num(
            parse_decimal(&take("num")?).map_err(|e| corrupt(format!("bad decimal: {e}")))?,
        ),
        "date" => {
            Value::Date(parse_date(&take("date")?).map_err(|e| corrupt(format!("bad date: {e}")))?)
        }
        "time" => Value::Time(
            parse_time_24(&take("time")?).map_err(|e| corrupt(format!("bad time: {e}")))?,
        ),
        other => return Err(corrupt(format!("unknown value_kind '{other}'"))),
    })
}

/// Reads one row of [`LOAD_SQL`].
pub(crate) fn row_to_value(row: &Row<'_>) -> Result<(FieldId, Value), StoreError> {
    let raw_id: String = row.get(0)?;
    let field_id = FieldId::parse(&raw_id).map_err(|_| StoreError::BadId(raw_id))?;
    let kind: String = row.get(1)?;
    // Exactly one typed column is non-NULL, and `kind` says which.
    let cell: Option<String> = match kind.as_str() {
        "text" | "opt" => row.get(2)?,
        "num" => row.get(3)?,
        "date" => row.get(4)?,
        "time" => row.get(5)?,
        _ => None,
    };
    Ok((field_id, decode(field_id, &kind, cell)?))
}

pub(crate) fn load(
    conn: &Connection,
    case_id: CaseId,
) -> Result<Vec<(FieldId, Value)>, StoreError> {
    let mut stmt = conn.prepare_cached(LOAD_SQL)?;
    let mut rows = stmt.query(params![case_id.to_string()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_value(row)?);
    }
    Ok(out)
}

pub(crate) fn get_one(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
) -> Result<Option<Value>, StoreError> {
    let sql = format!("{LOAD_SQL} AND field_id = ?2");
    let mut stmt = conn.prepare_cached(&sql)?;
    let row = stmt
        .query_row(params![case_id.to_string(), field_id.to_string()], |r| {
            Ok(row_to_value(r))
        })
        .optional()?;
    match row {
        Some(r) => Ok(Some(r?.1)),
        None => Ok(None),
    }
}

const UPSERT_SQL: &str = "INSERT INTO field_value \
     (case_id, field_id, value_kind, value_text, value_numeric, value_date, value_time, rev, pending) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
     ON CONFLICT(case_id, field_id) DO UPDATE SET \
       value_kind = excluded.value_kind, value_text = excluded.value_text, \
       value_numeric = excluded.value_numeric, value_date = excluded.value_date, \
       value_time = excluded.value_time, rev = excluded.rev, pending = excluded.pending";

/// Writes a value unconditionally. Used by the local write path, which is by definition
/// the newest truth for that field.
pub(crate) fn upsert(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
    v: &Value,
    rev: CaseRev,
    pending: bool,
) -> Result<(), StoreError> {
    let c = encode(v);
    conn.prepare_cached(UPSERT_SQL)?.execute(params![
        case_id.to_string(),
        field_id.to_string(),
        c.kind,
        c.text,
        c.numeric,
        c.date,
        c.time,
        rev.0,
        i64::from(pending),
    ])?;
    Ok(())
}

/// Writes an inbound server value, **skipping any row with `pending = 1`**.
///
/// A pending row is an unsynced local edit; overwriting it would silently discard the
/// abstractor's work (`docs/04-SYNC.md`). The guard is the `WHERE` on the upsert's DO
/// UPDATE clause, so the skip is decided by SQLite inside the same statement rather than
/// by a read-then-write that another writer could interleave with.
pub(crate) fn upsert_unless_pending(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
    v: &Value,
    rev: CaseRev,
) -> Result<(), StoreError> {
    let c = encode(v);
    let sql = format!("{UPSERT_SQL} WHERE field_value.pending = 0");
    conn.prepare_cached(&sql)?.execute(params![
        case_id.to_string(),
        field_id.to_string(),
        c.kind,
        c.text,
        c.numeric,
        c.date,
        c.time,
        rev.0,
        0i64,
    ])?;
    Ok(())
}

/// Clears `pending` and stamps the server's rev, for fields the server has accepted.
pub(crate) fn mark_confirmed(
    conn: &Connection,
    case_id: CaseId,
    field_id: FieldId,
    rev: CaseRev,
) -> Result<(), StoreError> {
    conn.prepare_cached(
        "UPDATE field_value SET pending = 0, rev = ?3 WHERE case_id = ?1 AND field_id = ?2",
    )?
    .execute(params![case_id.to_string(), field_id.to_string(), rev.0])?;
    Ok(())
}
