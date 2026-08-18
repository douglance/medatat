//! A live, editable form.
//!
//! `values` is a dense `Vec` indexed by `FieldIdx`, never a `HashMap<FieldId, _>` — this is
//! a hot path touched on every keystroke.
//!
//! With conditional logic cut (`docs/adr/0005`), `set` validates exactly one field and
//! returns one error. There is no dependency graph and no visibility recompute, so an edit
//! is strictly O(1) regardless of form size.

use crate::def::FormDef;
use crate::error::ValidationError;
use crate::ids::{CaseId, CaseRev, FieldId, FieldIdx};
use crate::validate::validate_placement;
use crate::value::Value;
use fixedbitset::FixedBitSet;
use std::sync::{Arc, LazyLock};

/// Under the `phi` feature `Value` implements `Drop`, which disables const promotion of
/// `&Value::Null`. A static keeps out-of-range reads allocation-free in both builds.
static NULL: LazyLock<Value> = LazyLock::new(|| Value::Null);

pub struct FormInstance {
    def: Arc<FormDef>,
    case_id: CaseId,
    base_rev: CaseRev,
    values: Vec<Value>,
    /// Edited locally and not yet confirmed by the server.
    dirty: FixedBitSet,
    /// Sent to the server, awaiting a response.
    inflight: FixedBitSet,
    errors: Vec<Option<ValidationError>>,
}

impl FormInstance {
    pub fn new(
        def: Arc<FormDef>,
        case_id: CaseId,
        base_rev: CaseRev,
        values: impl IntoIterator<Item = (FieldId, Value)>,
    ) -> Self {
        let n = def.field_count();
        let mut inst = FormInstance {
            values: vec![Value::Null; n],
            dirty: FixedBitSet::with_capacity(n),
            inflight: FixedBitSet::with_capacity(n),
            errors: vec![None; n],
            def,
            case_id,
            base_rev,
        };
        for (id, v) in values {
            if let Some(idx) = inst.def.idx_of(id) {
                inst.values[idx.as_usize()] = v;
            }
            // Values for fields no longer placed in this form are intentionally dropped
            // from the view. They remain stored server-side and reappear if re-placed.
        }
        inst.revalidate_all();
        inst
    }

    pub fn def(&self) -> &Arc<FormDef> {
        &self.def
    }
    pub fn case_id(&self) -> CaseId {
        self.case_id
    }
    pub fn base_rev(&self) -> CaseRev {
        self.base_rev
    }

    pub fn get(&self, idx: FieldIdx) -> &Value {
        self.values.get(idx.as_usize()).unwrap_or(&NULL)
    }

    pub fn error(&self, idx: FieldIdx) -> Option<&ValidationError> {
        self.errors.get(idx.as_usize()).and_then(|e| e.as_ref())
    }

    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_clear()
    }
    pub fn dirty_count(&self) -> usize {
        self.dirty.count_ones(..)
    }

    /// Sets one field. O(1): validates that field only.
    pub fn set(&mut self, idx: FieldIdx, value: Value) -> Option<ValidationError> {
        let i = idx.as_usize();
        if i >= self.values.len() {
            return None;
        }
        if self.values[i] == value {
            return self.errors[i].clone();
        }
        self.values[i] = value;
        self.dirty.insert(i);
        // The value that was sent is now stale, so the pending server response must not be
        // allowed to clear this field's dirty bit. Without this, an edit made while a write
        // is in flight is silently discarded on confirmation.
        self.inflight.set(i, false);
        let err = self.validate_one(idx);
        self.errors[i] = err.clone();
        err
    }

    /// Locally-changed values awaiting sync. O(dirty), never O(field_count).
    pub fn pending(&self) -> impl Iterator<Item = (FieldId, &Value)> + '_ {
        self.dirty.ones().filter_map(move |i| {
            let sf = self.def.field_at(FieldIdx(i as u32))?;
            Some((sf.field.field_id, &self.values[i]))
        })
    }

    /// Marks the pending set as in flight, so later edits are not lost when the response
    /// lands. Returns what was sent.
    pub fn take_pending(&mut self) -> Vec<(FieldId, Value)> {
        let out: Vec<_> = self
            .dirty
            .ones()
            .filter_map(|i| {
                let sf = self.def.field_at(FieldIdx(i as u32))?;
                Some((sf.field.field_id, self.values[i].clone()))
            })
            .collect();
        for i in self.dirty.ones().collect::<Vec<_>>() {
            self.inflight.insert(i);
        }
        out
    }

    /// The server accepted these fields. Clears dirty only where no newer edit landed.
    pub fn confirm(&mut self, fields: &[FieldId], new_rev: CaseRev) {
        for id in fields {
            if let Some(idx) = self.def.idx_of(*id) {
                let i = idx.as_usize();
                if self.inflight.contains(i) {
                    self.inflight.set(i, false);
                    self.dirty.set(i, false);
                }
            }
        }
        self.base_rev = new_rev;
    }

    /// Applies an inbound server value.
    ///
    /// Never overwrites a locally-dirty field — that is an unsynced edit, and clobbering it
    /// silently would lose the abstractor's work. The caller (the UI) is additionally
    /// responsible for not applying to the focused field; see `docs/04-SYNC.md`.
    pub fn apply_remote(&mut self, id: FieldId, value: Value) -> bool {
        let Some(idx) = self.def.idx_of(id) else {
            return false;
        };
        let i = idx.as_usize();
        if self.dirty.contains(i) || self.inflight.contains(i) {
            return false;
        }
        self.values[i] = value;
        self.errors[i] = self.validate_one(idx);
        true
    }

    pub fn revalidate_all(&mut self) {
        for i in 0..self.values.len() {
            self.errors[i] = self.validate_one(FieldIdx(i as u32));
        }
    }

    /// Fields that are required and still empty. Drives a completeness indicator.
    pub fn missing_required(&self) -> impl Iterator<Item = FieldIdx> + '_ {
        self.def
            .iter_fields()
            .filter(|sf| sf.required && self.get(sf.idx).is_null())
            .map(|sf| sf.idx)
    }

    pub fn filled_count(&self) -> usize {
        self.values.iter().filter(|v| !v.is_null()).count()
    }

    fn validate_one(&self, idx: FieldIdx) -> Option<ValidationError> {
        let sf = self.def.field_at(idx)?;
        validate_placement(&sf.field, sf.required, &self.values[idx.as_usize()]).err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{FieldDef, FieldKind, SectionDef, SectionField};
    use crate::ids::{FormId, SectionId};

    fn form(n: usize) -> Arc<FormDef> {
        let fields = (0..n)
            .map(|i| SectionField {
                idx: FieldIdx(0),
                field: Arc::new(FieldDef {
                    field_id: FieldId::new(),
                    key: format!("f{i}"),
                    kind: FieldKind::Text { max_len: Some(10) },
                }),
                label: format!("F{i}"),
                ordinal: i as i32,
                col_span: 1,
                required: false,
            })
            .collect();
        Arc::new(FormDef::new(
            FormId::new(),
            "t",
            vec![SectionDef {
                section_id: SectionId::new(),
                title: "s".into(),
                ordinal: 0,
                columns: 2,
                default_collapsed: false,
                fields,
            }],
        ))
    }

    #[test]
    fn pending_is_proportional_to_dirty_not_total() {
        let d = form(1000);
        let mut inst = FormInstance::new(d, CaseId::new(), CaseRev::ZERO, []);
        inst.set(FieldIdx(3), Value::Text("x".into()));
        assert_eq!(inst.pending().count(), 1, "must be O(dirty), not O(1000)");
        assert_eq!(inst.dirty_count(), 1);
    }

    #[test]
    fn set_returns_validation_error_for_that_field_only() {
        let d = form(3);
        let mut inst = FormInstance::new(d, CaseId::new(), CaseRev::ZERO, []);
        let err = inst.set(FieldIdx(1), Value::Text("this is far too long".into()));
        assert!(matches!(err, Some(ValidationError::TooLong { .. })));
        assert!(inst.error(FieldIdx(0)).is_none());
        assert!(inst.error(FieldIdx(2)).is_none());
    }

    #[test]
    fn setting_the_same_value_does_not_dirty() {
        let d = form(2);
        let mut inst = FormInstance::new(d, CaseId::new(), CaseRev::ZERO, []);
        inst.set(FieldIdx(0), Value::Null);
        assert_eq!(inst.dirty_count(), 0);
    }

    #[test]
    fn apply_remote_never_clobbers_a_local_edit() {
        let d = form(2);
        let ids: Vec<FieldId> = d.iter_fields().map(|f| f.field.field_id).collect();
        let mut inst = FormInstance::new(d, CaseId::new(), CaseRev::ZERO, []);

        inst.set(FieldIdx(0), Value::Text("mine".into()));
        assert!(!inst.apply_remote(ids[0], Value::Text("theirs".into())));
        assert_eq!(inst.get(FieldIdx(0)), &Value::Text("mine".into()));

        assert!(inst.apply_remote(ids[1], Value::Text("theirs".into())));
        assert_eq!(inst.get(FieldIdx(1)), &Value::Text("theirs".into()));
    }

    #[test]
    fn confirm_keeps_edits_made_while_in_flight() {
        let d = form(1);
        let ids: Vec<FieldId> = d.iter_fields().map(|f| f.field.field_id).collect();
        let mut inst = FormInstance::new(d, CaseId::new(), CaseRev::ZERO, []);

        inst.set(FieldIdx(0), Value::Text("a".into()));
        let _sent = inst.take_pending();
        inst.set(FieldIdx(0), Value::Text("b".into())); // edited again mid-flight
        inst.confirm(&ids, CaseRev(1));

        assert_eq!(
            inst.dirty_count(),
            1,
            "the newer edit must survive confirmation"
        );
        assert_eq!(inst.get(FieldIdx(0)), &Value::Text("b".into()));
    }

    #[test]
    fn values_for_unplaced_fields_are_ignored_not_panicked_on() {
        let d = form(1);
        let inst = FormInstance::new(
            d,
            CaseId::new(),
            CaseRev::ZERO,
            [(FieldId::new(), Value::Text("orphan".into()))],
        );
        assert_eq!(inst.filled_count(), 0);
    }
}
