//! The storage seam for one case.
//!
//! `CaseStore` is deliberately synchronous and deliberately small: the real implementation
//! is `SqlStorage` inside a Durable Object, whose `exec` is synchronous, and the DO is
//! single-threaded so there is nothing to lock. Keeping the seam this narrow is what lets
//! every rule in `logic/values.rs` be exercised natively, with no workerd in the loop.

use crate::error::{LogicError, LogicResult};
use medatat_core::def::FieldDef;
use medatat_core::def::FormDef;
use medatat_core::ids::{ActorId, CaseRev, FieldId};
use medatat_core::value::Value;
use medatat_core::wire::{ValueChange, ValueRow};
use std::collections::HashMap;
use std::sync::Arc;

pub trait CaseStore {
    /// The case's current revision. `CaseRev::ZERO` for a case that has never been written.
    fn rev(&self) -> LogicResult<CaseRev>;

    /// Rows among `fields` whose `rev` is strictly greater than `since`.
    ///
    /// This is the whole of conflict detection: scoped to the fields actually being
    /// written, so two abstractors editing different fields of one case never collide.
    fn changed_since(&self, fields: &[FieldId], since: CaseRev) -> LogicResult<Vec<ValueRow>>;

    /// Every row with `rev > since`. `CaseRev::ZERO` reads the whole case.
    fn get_all(&self, since: CaseRev) -> LogicResult<Vec<ValueRow>>;

    /// Apply a batch atomically at `rev`, stamping `actor` on every touched row.
    fn put(&self, changes: &[ValueChange], rev: CaseRev, actor: &ActorId) -> LogicResult<()>;
}

/// Field definitions by id, for server-side re-validation.
///
/// The Worker resolves this from D1 and hands it to the write path; it is never taken from
/// the request body, for the same reason `actor_id` is not.
#[derive(Debug, Clone, Default)]
pub struct FieldLookup {
    by_id: HashMap<FieldId, Arc<FieldDef>>,
}

impl FieldLookup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_defs(defs: impl IntoIterator<Item = FieldDef>) -> Self {
        FieldLookup {
            by_id: defs
                .into_iter()
                .map(|d| (d.field_id, Arc::new(d)))
                .collect(),
        }
    }

    pub fn from_form(form: &FormDef) -> Self {
        FieldLookup {
            by_id: form
                .iter_fields()
                .map(|f| (f.field.field_id, Arc::clone(&f.field)))
                .collect(),
        }
    }

    pub fn insert(&mut self, def: FieldDef) {
        self.by_id.insert(def.field_id, Arc::new(def));
    }

    pub fn get(&self, id: FieldId) -> Option<&FieldDef> {
        self.by_id.get(&id).map(|d| d.as_ref())
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// In-memory `CaseStore` for tests. Mirrors the DO's semantics exactly: one row per field,
/// each row carrying the case rev at which that field last changed.
#[derive(Debug, Default)]
pub struct MemCaseStore {
    inner: std::cell::RefCell<MemInner>,
}

#[derive(Debug, Default)]
struct MemInner {
    rev: CaseRev,
    rows: HashMap<FieldId, Row>,
    /// Set to make every call fail, so callers can prove they propagate storage errors.
    fail: Option<String>,
}

#[derive(Debug, Clone)]
struct Row {
    value: Value,
    rev: CaseRev,
    updated_by: ActorId,
    updated_at: String,
}

impl MemCaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a row as if it had been written at `rev` by `actor`, without touching the case
    /// rev counter's monotonicity rules. Used to stage a conflict.
    pub fn seed(&self, field_id: FieldId, value: Value, rev: CaseRev, actor: &str) {
        let mut inner = self.inner.borrow_mut();
        inner.rows.insert(
            field_id,
            Row {
                value,
                rev,
                updated_by: ActorId::new(actor),
                updated_at: "2026-08-17T00:00:00Z".to_string(),
            },
        );
        if rev > inner.rev {
            inner.rev = rev;
        }
    }

    pub fn fail_with(&self, msg: &str) {
        self.inner.borrow_mut().fail = Some(msg.to_string());
    }

    pub fn value_of(&self, field_id: FieldId) -> Option<Value> {
        self.inner
            .borrow()
            .rows
            .get(&field_id)
            .map(|r| r.value.clone())
    }

    pub fn actor_of(&self, field_id: FieldId) -> Option<ActorId> {
        self.inner
            .borrow()
            .rows
            .get(&field_id)
            .map(|r| r.updated_by.clone())
    }

    fn guard(&self) -> LogicResult<()> {
        match &self.inner.borrow().fail {
            Some(m) => Err(LogicError::Storage(m.clone())),
            None => Ok(()),
        }
    }
}

fn row_to_wire(field_id: FieldId, r: &Row) -> ValueRow {
    ValueRow {
        field_id,
        value: r.value.clone(),
        rev: r.rev,
        updated_by: Some(r.updated_by.clone()),
        updated_at: Some(r.updated_at.clone()),
    }
}

impl CaseStore for MemCaseStore {
    fn rev(&self) -> LogicResult<CaseRev> {
        self.guard()?;
        Ok(self.inner.borrow().rev)
    }

    fn changed_since(&self, fields: &[FieldId], since: CaseRev) -> LogicResult<Vec<ValueRow>> {
        self.guard()?;
        let inner = self.inner.borrow();
        let mut out: Vec<ValueRow> = fields
            .iter()
            .filter_map(|id| inner.rows.get(id).map(|r| (id, r)))
            .filter(|(_, r)| r.rev > since)
            .map(|(id, r)| row_to_wire(*id, r))
            .collect();
        out.sort_by_key(|r| r.field_id);
        Ok(out)
    }

    fn get_all(&self, since: CaseRev) -> LogicResult<Vec<ValueRow>> {
        self.guard()?;
        let inner = self.inner.borrow();
        let mut out: Vec<ValueRow> = inner
            .rows
            .iter()
            .filter(|(_, r)| r.rev > since)
            .map(|(id, r)| row_to_wire(*id, r))
            .collect();
        out.sort_by_key(|r| r.field_id);
        Ok(out)
    }

    fn put(&self, changes: &[ValueChange], rev: CaseRev, actor: &ActorId) -> LogicResult<()> {
        self.guard()?;
        let mut inner = self.inner.borrow_mut();
        for c in changes {
            inner.rows.insert(
                c.field_id,
                Row {
                    value: c.value.clone(),
                    rev,
                    updated_by: actor.clone(),
                    updated_at: "2026-08-17T00:00:00Z".to_string(),
                },
            );
        }
        inner.rev = rev;
        Ok(())
    }
}
