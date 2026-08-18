//! Form definitions: what fields exist, how they are configured, and where they sit.
//!
//! `FieldId` is global and stable; `SectionField` is the placement. Moving, relabelling,
//! or removing a placement never touches stored values, and a field's `kind` is never
//! re-typed in place. That single invariant replaces an entire form-version lifecycle —
//! see `docs/adr/0005-cut-audit-versioning-rules.md`.

use crate::ids::{FieldId, FieldIdx, FormId, OptionCode, SectionId};
use crate::value::ValueColumn;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// The seven field kinds required by R5–R11.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FieldKind {
    /// R5
    Text { max_len: Option<u32> },
    /// R6 — exact decimals, never `f64`.
    Numeric {
        #[serde(default, with = "opt_decimal_str")]
        min: Option<Decimal>,
        #[serde(default, with = "opt_decimal_str")]
        max: Option<Decimal>,
        #[serde(default)]
        scale: u8,
    },
    /// R7
    Date,
    /// R8 — always 24-hour.
    Time,
    /// R9
    Radio { options: Arc<[FieldOption]> },
    /// R10
    Select {
        options: Arc<[FieldOption]>,
        #[serde(default)]
        searchable: bool,
    },
    /// R11
    Textarea {
        #[serde(default = "default_rows")]
        rows: u16,
        max_len: Option<u32>,
    },
}

fn default_rows() -> u16 {
    4
}

impl FieldKind {
    /// The storage column this kind writes to. One function, used by every query builder.
    pub fn value_column(&self) -> ValueColumn {
        match self {
            FieldKind::Text { .. } | FieldKind::Textarea { .. } => ValueColumn::Text,
            FieldKind::Radio { .. } | FieldKind::Select { .. } => ValueColumn::Text,
            FieldKind::Numeric { .. } => ValueColumn::Numeric,
            FieldKind::Date => ValueColumn::Date,
            FieldKind::Time => ValueColumn::Time,
        }
    }

    /// Stable wire/DB discriminant. Must match the D1 CHECK constraint.
    pub fn tag(&self) -> &'static str {
        match self {
            FieldKind::Text { .. } => "text",
            FieldKind::Numeric { .. } => "numeric",
            FieldKind::Date => "date",
            FieldKind::Time => "time",
            FieldKind::Radio { .. } => "radio",
            FieldKind::Select { .. } => "select",
            FieldKind::Textarea { .. } => "textarea",
        }
    }

    pub fn options(&self) -> Option<&Arc<[FieldOption]>> {
        match self {
            FieldKind::Radio { options } | FieldKind::Select { options, .. } => Some(options),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldOption {
    pub code: OptionCode,
    pub label: String,
    pub ordinal: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    pub field_id: FieldId,
    pub key: String,
    pub kind: FieldKind,
}

/// A field's placement within a section. Label, span, and requiredness are per-placement,
/// so the same field can appear differently in different forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionField {
    #[serde(skip)]
    pub idx: FieldIdx,
    pub field: Arc<FieldDef>,
    pub label: String,
    pub ordinal: i32,
    /// 1..=3, clamped to the owning section's `columns` (R12).
    pub col_span: u8,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionDef {
    pub section_id: SectionId,
    pub title: String,
    pub ordinal: i32,
    /// 1, 2, or 3 (R12).
    pub columns: u8,
    #[serde(default)]
    pub default_collapsed: bool,
    pub fields: Vec<SectionField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormDef {
    pub form_id: FormId,
    pub name: String,
    pub sections: Vec<SectionDef>,
    /// Built by `finalize`; not serialised.
    #[serde(skip)]
    by_id: HashMap<FieldId, FieldIdx>,
    #[serde(skip)]
    field_count: usize,
}

impl FormDef {
    pub fn new(form_id: FormId, name: impl Into<String>, sections: Vec<SectionDef>) -> Self {
        let mut d = FormDef {
            form_id,
            name: name.into(),
            sections,
            by_id: HashMap::new(),
            field_count: 0,
        };
        d.finalize();
        d
    }

    /// Assigns dense `FieldIdx` values in render order and builds the id lookup.
    /// Must be called after any structural change or after deserialisation.
    pub fn finalize(&mut self) {
        self.sections.sort_by_key(|s| s.ordinal);
        let mut n = 0u32;
        let mut by_id = HashMap::new();
        for section in &mut self.sections {
            section.columns = section.columns.clamp(1, 3);
            section.fields.sort_by_key(|f| f.ordinal);
            for f in &mut section.fields {
                // R12: a field can never span more columns than its section has.
                // Calls `layout::clamp_col_span` rather than inlining the arithmetic, so
                // there is exactly one definition of the rule. Two would eventually
                // disagree, and the UI and the API would then clamp differently — which is
                // the specific failure `docs/06-FORM-BUILDER.md` warns about.
                f.col_span = crate::layout::clamp_col_span(f.col_span, section.columns);
                f.idx = FieldIdx(n);
                by_id.insert(f.field.field_id, FieldIdx(n));
                n += 1;
            }
        }
        self.field_count = n as usize;
        self.by_id = by_id;
    }

    pub fn field_count(&self) -> usize {
        self.field_count
    }

    pub fn idx_of(&self, id: FieldId) -> Option<FieldIdx> {
        self.by_id.get(&id).copied()
    }

    pub fn iter_fields(&self) -> impl Iterator<Item = &SectionField> + '_ {
        self.sections.iter().flat_map(|s| s.fields.iter())
    }

    pub fn field_at(&self, idx: FieldIdx) -> Option<&SectionField> {
        self.iter_fields().find(|f| f.idx == idx)
    }

    pub fn section_of(&self, idx: FieldIdx) -> Option<&SectionDef> {
        self.sections
            .iter()
            .find(|s| s.fields.iter().any(|f| f.idx == idx))
    }
}

/// A form's `kind` may never change in place. Callers use this to decide between an
/// in-place edit and creating a replacement field.
pub fn is_kind_change(old: &FieldKind, new: &FieldKind) -> bool {
    old.tag() != new.tag()
}

mod opt_decimal_str {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::str::FromStr;

    pub fn serialize<S: Serializer>(d: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(v) => s.serialize_str(&v.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
        let o = Option::<String>::deserialize(d)?;
        o.map(|s| Decimal::from_str(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: &str, kind: FieldKind) -> Arc<FieldDef> {
        Arc::new(FieldDef {
            field_id: FieldId::new(),
            key: key.into(),
            kind,
        })
    }

    fn section(title: &str, columns: u8, spans: &[u8]) -> SectionDef {
        SectionDef {
            section_id: SectionId::new(),
            title: title.into(),
            ordinal: 0,
            columns,
            default_collapsed: false,
            fields: spans
                .iter()
                .enumerate()
                .map(|(i, &sp)| SectionField {
                    idx: FieldIdx(0),
                    field: field(&format!("f{i}"), FieldKind::Text { max_len: None }),
                    label: format!("Field {i}"),
                    ordinal: i as i32,
                    col_span: sp,
                    required: false,
                })
                .collect(),
        }
    }

    #[test]
    fn r12_clamps_col_span_to_section_columns() {
        let d = FormDef::new(FormId::new(), "f", vec![section("s", 2, &[1, 2, 3])]);
        let spans: Vec<u8> = d.iter_fields().map(|f| f.col_span).collect();
        assert_eq!(
            spans,
            vec![1, 2, 2],
            "a 3-span in a 2-column section must clamp"
        );
    }

    #[test]
    fn r12_clamps_columns_into_range() {
        let d = FormDef::new(FormId::new(), "f", vec![section("s", 9, &[1])]);
        assert_eq!(d.sections[0].columns, 3);
        let d = FormDef::new(FormId::new(), "f", vec![section("s", 0, &[1])]);
        assert_eq!(d.sections[0].columns, 1);
    }

    #[test]
    fn indices_are_dense_and_in_render_order() {
        let d = FormDef::new(
            FormId::new(),
            "f",
            vec![section("a", 1, &[1, 1]), section("b", 1, &[1])],
        );
        let idxs: Vec<u32> = d.iter_fields().map(|f| f.idx.0).collect();
        assert_eq!(idxs, vec![0, 1, 2]);
        assert_eq!(d.field_count(), 3);
    }

    #[test]
    fn idx_lookup_round_trips() {
        let d = FormDef::new(FormId::new(), "f", vec![section("a", 1, &[1, 1])]);
        for f in d.iter_fields() {
            assert_eq!(d.idx_of(f.field.field_id), Some(f.idx));
        }
    }

    #[test]
    fn every_kind_maps_to_a_column() {
        use crate::value::ValueColumn as C;
        for (k, c) in [
            (FieldKind::Text { max_len: None }, C::Text),
            (
                FieldKind::Textarea {
                    rows: 3,
                    max_len: None,
                },
                C::Text,
            ),
            (
                FieldKind::Numeric {
                    min: None,
                    max: None,
                    scale: 2,
                },
                C::Numeric,
            ),
            (FieldKind::Date, C::Date),
            (FieldKind::Time, C::Time),
            (
                FieldKind::Radio {
                    options: Arc::from(vec![]),
                },
                C::Text,
            ),
            (
                FieldKind::Select {
                    options: Arc::from(vec![]),
                    searchable: false,
                },
                C::Text,
            ),
        ] {
            assert_eq!(k.value_column(), c, "kind {}", k.tag());
        }
    }

    #[test]
    fn finalize_clamps_via_the_one_shared_rule() {
        // finalize and layout::clamp_col_span must never drift apart. If they do, the
        // builder previews one layout and the server stores another.
        for columns in 1..=3u8 {
            for span in 0..=5u8 {
                let mut sec = section("s", columns, &[span]);
                sec.fields[0].col_span = span;
                let d = FormDef::new(FormId::new(), "f", vec![sec]);
                assert_eq!(
                    d.iter_fields().next().unwrap().col_span,
                    crate::layout::clamp_col_span(span, columns),
                    "columns={columns} span={span}"
                );
            }
        }
    }

    #[test]
    fn kind_change_is_detected() {
        assert!(is_kind_change(&FieldKind::Date, &FieldKind::Time));
        assert!(!is_kind_change(
            &FieldKind::Text { max_len: None },
            &FieldKind::Text { max_len: Some(10) }
        ));
    }
}
