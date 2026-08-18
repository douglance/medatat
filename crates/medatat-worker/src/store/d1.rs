//! D1: configuration, users, and the worklist index (`docs/02-DATA-MODEL.md`, store 2).
//!
//! This file is queries and nothing else. Rows in, `logic::config_rows` and `logic::cases`
//! turn them into wire types — that keeps the join tested natively and keeps a second
//! `FieldKind`→column mapping from ever appearing here.
//!
//! `case_index` is written by the DO after a successful value write and is **eventually
//! consistent by design** — see `docs/10-LIMITATIONS.md` #2. It holds no field values.

use crate::error::{LogicError, LogicResult};
use crate::logic::auth::Account;
use crate::logic::cases::build_page;
use crate::logic::config::validate_field_kind;
use crate::logic::config_rows::{
    FieldRow, FormRow, OptionRow, PlacementRow, SectionRow, assemble_config, field_defs,
    kind_from_row, kind_to_row,
};
use medatat_core::def::{FieldDef, FieldKind, FieldOption};
use medatat_core::ids::{CaseId, CaseRev, ConfigRev, FieldId, FormId, SectionId};
use medatat_core::wire::{CasePage, CaseSummary, ConfigDelta, Role, UserInfo};
use serde::Deserialize;
use std::sync::Arc;
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

pub struct Db {
    db: D1Database,
}

// --------------------------------------------------------- rows owned by D1 alone

#[derive(Deserialize)]
struct UserRow {
    user_id: String,
    email: String,
    display_name: String,
    role: String,
    is_active: i64,
}

#[derive(Deserialize)]
struct RevRow {
    rev: i64,
}

#[derive(Deserialize)]
struct CaseRow {
    case_id: String,
    mrn: String,
    form_id: String,
    assignee: Option<String>,
    rev: i64,
    updated_at: String,
}

#[derive(Deserialize)]
struct KindRow {
    kind: String,
    config: String,
}

#[derive(Deserialize)]
struct ColumnsRow {
    columns: i64,
}

// ------------------------------------------------------------------ helpers

fn s(v: &str) -> JsValue {
    JsValue::from_str(v)
}

fn n(v: i64) -> JsValue {
    JsValue::from_f64(v as f64)
}

fn storage(e: impl std::fmt::Display) -> LogicError {
    LogicError::Storage(format!("{e}"))
}

fn parse_role(raw: &str) -> Role {
    match raw {
        "admin" => Role::Admin,
        _ => Role::Abstractor,
    }
}

fn into_user(r: UserRow) -> Account {
    Account {
        is_active: r.is_active != 0,
        user: UserInfo {
            user_id: r.user_id,
            email: r.email,
            display_name: r.display_name,
            role: parse_role(&r.role),
        },
    }
}

fn into_summary(r: CaseRow) -> LogicResult<CaseSummary> {
    Ok(CaseSummary {
        case_id: CaseId::parse(&r.case_id).map_err(storage)?,
        mrn: r.mrn,
        form_id: FormId::parse(&r.form_id).map_err(storage)?,
        assignee: r.assignee,
        rev: CaseRev(r.rev),
        updated_at: r.updated_at,
    })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

const SELECT_CASE: &str = "SELECT case_id, mrn, form_id, assignee, rev, updated_at FROM case_index";

impl Db {
    pub fn new(db: D1Database) -> Self {
        Db { db }
    }

    fn stmt(&self, sql: &str, args: &[JsValue]) -> LogicResult<D1PreparedStatement> {
        self.db.prepare(sql).bind(args).map_err(storage)
    }

    async fn query<T: for<'de> Deserialize<'de>>(
        &self,
        sql: &str,
        args: &[JsValue],
    ) -> LogicResult<Vec<T>> {
        self.stmt(sql, args)?
            .all()
            .await
            .map_err(storage)?
            .results::<T>()
            .map_err(storage)
    }

    async fn run(&self, sql: &str, args: &[JsValue]) -> LogicResult<()> {
        self.stmt(sql, args)?
            .run()
            .await
            .map_err(storage)
            .map(|_| ())
    }

    /// Every config mutation runs in one D1 batch with the `config_version` bump, so a
    /// client can never observe a changed form under an unchanged `config_rev`.
    async fn batch(&self, statements: Vec<D1PreparedStatement>) -> LogicResult<()> {
        let mut all = statements;
        all.push(self.stmt("UPDATE config_version SET rev = rev + 1", &[])?);
        self.db.batch(all).await.map_err(storage).map(|_| ())
    }

    // ---------------------------------------------------------------- users

    pub async fn account_by_email(&self, email: &str) -> LogicResult<Option<Account>> {
        let rows: Vec<UserRow> = self
            .query(
                "SELECT user_id, email, display_name, role, is_active
                   FROM app_user WHERE email = ?",
                &[s(&crate::logic::auth::normalize_email(email))],
            )
            .await?;
        Ok(rows.into_iter().next().map(into_user))
    }

    pub async fn account_by_id(&self, user_id: &str) -> LogicResult<Option<Account>> {
        let rows: Vec<UserRow> = self
            .query(
                "SELECT user_id, email, display_name, role, is_active
                   FROM app_user WHERE user_id = ?",
                &[s(user_id)],
            )
            .await?;
        Ok(rows.into_iter().next().map(into_user))
    }

    // --------------------------------------------------------------- config

    pub async fn config_rev(&self) -> LogicResult<ConfigRev> {
        let rows: Vec<RevRow> = self
            .query("SELECT rev FROM config_version LIMIT 1", &[])
            .await?;
        Ok(ConfigRev(rows.first().map(|r| r.rev).unwrap_or(1)))
    }

    async fn form_rows(&self) -> LogicResult<Vec<FormRow>> {
        self.query("SELECT form_id, key, name, archived_at FROM form", &[])
            .await
    }

    async fn section_rows(&self) -> LogicResult<Vec<SectionRow>> {
        self.query(
            "SELECT section_id, form_id, name, ordinal, columns FROM section ORDER BY ordinal",
            &[],
        )
        .await
    }

    async fn field_rows(&self) -> LogicResult<Vec<FieldRow>> {
        self.query(
            "SELECT field_id, key, kind, config, archived_at FROM field",
            &[],
        )
        .await
    }

    async fn option_rows(&self) -> LogicResult<Vec<OptionRow>> {
        self.query(
            "SELECT field_id, code, label, ordinal FROM field_option ORDER BY ordinal",
            &[],
        )
        .await
    }

    async fn placement_rows(&self) -> LogicResult<Vec<PlacementRow>> {
        self.query(
            "SELECT section_id, field_id, ordinal, col_span, label, required
               FROM section_field ORDER BY ordinal",
            &[],
        )
        .await
    }

    /// The whole configuration. Config is a few thousand rows, so partial sync is not worth
    /// the complexity — the client re-fetches wholesale when `config_rev` moves.
    pub async fn load_config(&self) -> LogicResult<ConfigDelta> {
        assemble_config(
            self.config_rev().await?,
            &self.form_rows().await?,
            &self.section_rows().await?,
            &self.field_rows().await?,
            &self.option_rows().await?,
            &self.placement_rows().await?,
        )
    }

    /// Every field definition, for server-side re-validation on the write path.
    pub async fn field_defs(&self) -> LogicResult<Vec<FieldDef>> {
        field_defs(&self.field_rows().await?, &self.option_rows().await?)
    }

    /// The stored kind and raw `config` column for one field, for the immutability check.
    pub async fn field_kind(&self, field_id: FieldId) -> LogicResult<Option<(FieldKind, String)>> {
        let fid = field_id.to_string();
        let rows: Vec<KindRow> = self
            .query(
                "SELECT kind, config FROM field WHERE field_id = ?",
                &[s(&fid)],
            )
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let options: Vec<OptionRow> = self
            .query(
                "SELECT field_id, code, label, ordinal FROM field_option
                  WHERE field_id = ? ORDER BY ordinal",
                &[s(&fid)],
            )
            .await?;
        let opts: Arc<[FieldOption]> = Arc::from(
            options
                .into_iter()
                .map(|o| FieldOption {
                    code: medatat_core::ids::OptionCode::new(o.code),
                    label: o.label,
                    ordinal: o.ordinal as i32,
                })
                .collect::<Vec<_>>(),
        );
        Ok(Some((
            kind_from_row(&row.kind, &row.config, opts)?,
            row.config,
        )))
    }

    // ------------------------------------------------------- config mutation

    pub async fn create_form(&self, key: &str, name: &str) -> LogicResult<FormId> {
        let form_id = FormId::new();
        self.batch(vec![self.stmt(
            "INSERT INTO form (form_id, key, name) VALUES (?, ?, ?)",
            &[s(&form_id.to_string()), s(key), s(name)],
        )?])
        .await?;
        Ok(form_id)
    }

    pub async fn patch_form(
        &self,
        form_id: FormId,
        name: Option<&str>,
        archived: Option<bool>,
    ) -> LogicResult<()> {
        let fid = form_id.to_string();
        let mut statements = Vec::new();
        if let Some(name) = name {
            statements.push(self.stmt(
                "UPDATE form SET name = ? WHERE form_id = ?",
                &[s(name), s(&fid)],
            )?);
        }
        if let Some(archived) = archived {
            statements.push(if archived {
                self.stmt(
                    "UPDATE form SET archived_at = ? WHERE form_id = ?",
                    &[s(&now_rfc3339()), s(&fid)],
                )?
            } else {
                self.stmt(
                    "UPDATE form SET archived_at = NULL WHERE form_id = ?",
                    &[s(&fid)],
                )?
            });
        }
        self.batch(statements).await
    }

    pub async fn create_section(
        &self,
        form_id: FormId,
        name: &str,
        ordinal: i32,
        columns: u8,
    ) -> LogicResult<SectionId> {
        let section_id = SectionId::new();
        self.batch(vec![self.stmt(
            "INSERT INTO section (section_id, form_id, name, ordinal, columns)
             VALUES (?, ?, ?, ?, ?)",
            &[
                s(&section_id.to_string()),
                s(&form_id.to_string()),
                s(name),
                n(ordinal as i64),
                n(columns as i64),
            ],
        )?])
        .await?;
        Ok(section_id)
    }

    pub async fn section_columns(&self, section_id: SectionId) -> LogicResult<Option<u8>> {
        let rows: Vec<ColumnsRow> = self
            .query(
                "SELECT columns FROM section WHERE section_id = ?",
                &[s(&section_id.to_string())],
            )
            .await?;
        Ok(rows.first().map(|r| r.columns.clamp(1, 3) as u8))
    }

    /// Every placement in a section, whole. This is `placement_rows()` with a `WHERE`, so
    /// the R12 clamp and a single-placement patch read the same row through the same query
    /// and the same `PlacementRow` — there is no second projection to drift.
    pub async fn section_placements(
        &self,
        section_id: SectionId,
    ) -> LogicResult<Vec<PlacementRow>> {
        self.query(
            "SELECT section_id, field_id, ordinal, col_span, label, required
               FROM section_field WHERE section_id = ?",
            &[s(&section_id.to_string())],
        )
        .await
    }

    /// R12: reducing `columns` clamps every child `col_span` **in the same transaction**, so
    /// the section is never momentarily wider than its own children allow.
    pub async fn patch_section(
        &self,
        section_id: SectionId,
        name: Option<&str>,
        ordinal: Option<i32>,
        columns: Option<u8>,
        clamps: &[(FieldId, u8)],
    ) -> LogicResult<()> {
        let sid = section_id.to_string();
        let mut statements = Vec::new();
        if let Some(name) = name {
            statements.push(self.stmt(
                "UPDATE section SET name = ? WHERE section_id = ?",
                &[s(name), s(&sid)],
            )?);
        }
        if let Some(ordinal) = ordinal {
            statements.push(self.stmt(
                "UPDATE section SET ordinal = ? WHERE section_id = ?",
                &[n(ordinal as i64), s(&sid)],
            )?);
        }
        if let Some(columns) = columns {
            statements.push(self.stmt(
                "UPDATE section SET columns = ? WHERE section_id = ?",
                &[n(columns as i64), s(&sid)],
            )?);
            for (field_id, span) in clamps {
                statements.push(self.stmt(
                    "UPDATE section_field SET col_span = ?
                      WHERE section_id = ? AND field_id = ?",
                    &[n(*span as i64), s(&sid), s(&field_id.to_string())],
                )?);
            }
        }
        self.batch(statements).await
    }

    /// Cascades placements. **Never deletes values** — they live in the DO, keyed by
    /// `field_id`, and outlive every placement.
    pub async fn delete_section(&self, section_id: SectionId) -> LogicResult<()> {
        let sid = section_id.to_string();
        self.batch(vec![
            self.stmt("DELETE FROM section_field WHERE section_id = ?", &[s(&sid)])?,
            self.stmt("DELETE FROM section WHERE section_id = ?", &[s(&sid)])?,
        ])
        .await
    }

    pub async fn create_field(&self, key: &str, kind: &FieldKind) -> LogicResult<FieldId> {
        validate_field_kind(kind)?;
        let field_id = FieldId::new();
        let fid = field_id.to_string();
        let (tag, config) = kind_to_row(kind)?;
        let mut statements = vec![self.stmt(
            "INSERT INTO field (field_id, key, kind, config) VALUES (?, ?, ?, ?)",
            &[s(&fid), s(key), s(tag), s(&config)],
        )?];
        for o in kind.options().into_iter().flat_map(|o| o.iter()) {
            statements.push(self.stmt(
                "INSERT INTO field_option (field_id, code, label, ordinal) VALUES (?, ?, ?, ?)",
                &[
                    s(&fid),
                    s(o.code.as_str()),
                    s(&o.label),
                    n(o.ordinal as i64),
                ],
            )?);
        }
        self.batch(statements).await?;
        Ok(field_id)
    }

    /// `kind` is never written here — it is immutable, and `routes/config.rs` rejects any
    /// patch that names a different one before this is reached.
    pub async fn patch_field(&self, field_id: FieldId, kind: &FieldKind) -> LogicResult<()> {
        validate_field_kind(kind)?;
        let (_, config) = kind_to_row(kind)?;
        let fid = field_id.to_string();
        let mut statements = vec![self.stmt(
            "UPDATE field SET config = ? WHERE field_id = ?",
            &[s(&config), s(&fid)],
        )?];
        if let Some(options) = kind.options() {
            statements.push(self.stmt("DELETE FROM field_option WHERE field_id = ?", &[s(&fid)])?);
            for o in options.iter() {
                statements.push(self.stmt(
                    "INSERT INTO field_option (field_id, code, label, ordinal)
                     VALUES (?, ?, ?, ?)",
                    &[
                        s(&fid),
                        s(o.code.as_str()),
                        s(&o.label),
                        n(o.ordinal as i64),
                    ],
                )?);
            }
        }
        self.batch(statements).await
    }

    pub async fn place_field(
        &self,
        section_id: SectionId,
        field_id: FieldId,
        ordinal: i32,
        col_span: u8,
        label: &str,
        required: bool,
    ) -> LogicResult<()> {
        self.batch(vec![self.stmt(
            "INSERT INTO section_field (section_id, field_id, ordinal, col_span, label, required)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(section_id, field_id) DO UPDATE SET
               ordinal = excluded.ordinal, col_span = excluded.col_span,
               label = excluded.label, required = excluded.required",
            &[
                s(&section_id.to_string()),
                s(&field_id.to_string()),
                n(ordinal as i64),
                n(col_span as i64),
                s(label),
                n(required as i64),
            ],
        )?])
        .await
    }

    /// Removes the placement only. **Values survive.**
    pub async fn unplace_field(&self, section_id: SectionId, field_id: FieldId) -> LogicResult<()> {
        self.batch(vec![self.stmt(
            "DELETE FROM section_field WHERE section_id = ? AND field_id = ?",
            &[s(&section_id.to_string()), s(&field_id.to_string())],
        )?])
        .await
    }

    // ----------------------------------------------------------- case index

    pub async fn insert_case(
        &self,
        case_id: CaseId,
        mrn: &str,
        form_id: FormId,
        assignee: Option<&str>,
    ) -> LogicResult<()> {
        self.run(
            "INSERT INTO case_index (case_id, mrn, form_id, assignee, rev, updated_at)
             VALUES (?, ?, ?, ?, 0, ?)",
            &[
                s(&case_id.to_string()),
                s(mrn),
                s(&form_id.to_string()),
                assignee.map(s).unwrap_or(JsValue::NULL),
                s(&now_rfc3339()),
            ],
        )
        .await
    }

    /// Called by the DO after a successful value write. **May fail independently** — that
    /// is the eventual-consistency seam, repaired by `POST /admin/reindex`.
    pub async fn touch_case(&self, case_id: CaseId, rev: CaseRev) -> LogicResult<()> {
        self.run(
            "UPDATE case_index SET rev = ?, updated_at = ? WHERE case_id = ?",
            &[n(rev.0), s(&now_rfc3339()), s(&case_id.to_string())],
        )
        .await
    }

    /// Keyset paging on `updated_at`, and one row past the limit so `has_more` needs no
    /// `COUNT(*)` over an index that is only eventually consistent anyway.
    pub async fn list_cases(
        &self,
        assignee: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> LogicResult<CasePage> {
        let rows: Vec<CaseRow> = self
            .query(
                &format!(
                    "{SELECT_CASE}
                      WHERE (?1 IS NULL OR assignee = ?1)
                        AND (?2 IS NULL OR updated_at < ?2)
                      ORDER BY updated_at DESC
                      LIMIT ?3"
                ),
                &[
                    assignee.map(s).unwrap_or(JsValue::NULL),
                    cursor.map(s).unwrap_or(JsValue::NULL),
                    n(limit as i64 + 1),
                ],
            )
            .await?;
        let summaries = rows
            .into_iter()
            .map(into_summary)
            .collect::<LogicResult<Vec<_>>>()?;
        Ok(build_page(summaries, limit))
    }

    pub async fn export_page(
        &self,
        form_id: Option<FormId>,
        cursor: Option<&str>,
        limit: u32,
    ) -> LogicResult<CasePage> {
        let rows: Vec<CaseRow> = self
            .query(
                &format!(
                    "{SELECT_CASE}
                      WHERE (?1 IS NULL OR form_id = ?1)
                        AND (?2 IS NULL OR updated_at < ?2)
                      ORDER BY updated_at DESC
                      LIMIT ?3"
                ),
                &[
                    form_id.map(|f| s(&f.to_string())).unwrap_or(JsValue::NULL),
                    cursor.map(s).unwrap_or(JsValue::NULL),
                    n(limit as i64 + 1),
                ],
            )
            .await?;
        let summaries = rows
            .into_iter()
            .map(into_summary)
            .collect::<LogicResult<Vec<_>>>()?;
        Ok(build_page(summaries, limit))
    }
}
