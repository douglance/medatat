//! Pure logic for editing forms (R4).
//!
//! Lives here rather than in `medatat-ui` for the same reason [`crate::view`] does: the
//! builder makes real decisions — what may change, what must be rejected, what a column
//! reduction does to its children — and those need to be testable without a window. The UI
//! renders these outcomes; it does not compute them.
//!
//! The Worker validates with the same functions, so the builder cannot offer an edit the
//! server will refuse.

use crate::def::{FieldDef, FieldKind, FieldOption, FormDef, SectionDef};
use crate::ids::{FieldId, SectionId};
use crate::layout::clamp_col_span;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Why a proposed form edit cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum EditError {
    #[error("a field's kind cannot be changed; create a replacement field instead")]
    KindIsImmutable,
    #[error("key must match ^[a-z][a-z0-9_]{{0,63}}$")]
    BadKey,
    #[error("key \"{0}\" is already used")]
    DuplicateKey(String),
    #[error("a section must have 1, 2, or 3 columns")]
    BadColumnCount,
    #[error("a field cannot span {span} columns in a {columns}-column section")]
    SpanExceedsColumns { span: u8, columns: u8 },
    #[error("label must not be empty")]
    EmptyLabel,
    #[error("minimum {min} is greater than maximum {max}")]
    MinExceedsMax { min: String, max: String },
    #[error("at most 10 decimal places")]
    ScaleTooLarge,
    #[error("a {0} field needs at least two options")]
    TooFewOptions(&'static str),
    #[error("option code \"{0}\" appears more than once")]
    DuplicateOption(String),
    #[error("option code must not be empty")]
    EmptyOptionCode,
}

/// Whether an edit to a field definition can be applied in place.
///
/// The distinction exists because changing a field's `kind` would reinterpret every value
/// already stored against it — a text value read back as a number is not a migration, it is
/// data corruption. So `kind` is immutable and a change creates a *new* field, leaving the
/// old values intact and viewable. That single rule is what lets the form-version lifecycle
/// stay cut; see `docs/adr/0005-cut-audit-versioning-rules.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldEdit {
    /// Safe to apply to the existing field.
    InPlace,
    /// Requires a replacement field with a new `FieldId`.
    NeedsReplacement,
}

pub fn classify(old: &FieldDef, new: &FieldDef) -> FieldEdit {
    if old.kind.tag() != new.kind.tag() {
        FieldEdit::NeedsReplacement
    } else {
        FieldEdit::InPlace
    }
}

/// Field keys are stable machine identifiers and appear in exports, so they are
/// deliberately narrow: lowercase, starts with a letter, no spaces or punctuation.
pub fn validate_key(key: &str) -> Result<(), EditError> {
    let mut bytes = key.bytes();
    let ok = key.len() <= 64
        && matches!(bytes.next(), Some(b) if b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok { Ok(()) } else { Err(EditError::BadKey) }
}

/// Validates a field definition on its own terms, before it is placed anywhere.
pub fn validate_field(def: &FieldDef) -> Result<(), EditError> {
    validate_key(&def.key)?;
    match &def.kind {
        FieldKind::Numeric { min, max, scale } => {
            if *scale > 10 {
                return Err(EditError::ScaleTooLarge);
            }
            if let (Some(min), Some(max)) = (min, max)
                && min > max
            {
                return Err(EditError::MinExceedsMax {
                    min: min.to_string(),
                    max: max.to_string(),
                });
            }
            Ok(())
        }
        FieldKind::Radio { options } => validate_options(options, "radio"),
        FieldKind::Select { options, .. } => validate_options(options, "select"),
        _ => Ok(()),
    }
}

fn validate_options(options: &[FieldOption], kind: &'static str) -> Result<(), EditError> {
    if options.len() < 2 {
        return Err(EditError::TooFewOptions(kind));
    }
    let mut seen = std::collections::HashSet::new();
    for o in options {
        if o.code.0.trim().is_empty() {
            return Err(EditError::EmptyOptionCode);
        }
        if !seen.insert(&o.code.0) {
            return Err(EditError::DuplicateOption(o.code.0.clone()));
        }
    }
    Ok(())
}

/// Validates a placement against the section that will hold it (R12).
pub fn validate_placement(label: &str, col_span: u8, columns: u8) -> Result<(), EditError> {
    if label.trim().is_empty() {
        return Err(EditError::EmptyLabel);
    }
    if !(1..=3).contains(&columns) {
        return Err(EditError::BadColumnCount);
    }
    if !(1..=columns).contains(&col_span) {
        return Err(EditError::SpanExceedsColumns {
            span: col_span,
            columns,
        });
    }
    Ok(())
}

/// What reducing a section's column count does to the fields inside it.
///
/// Returned rather than applied so the builder can warn before committing: a coordinator
/// dropping a 3-column section to 2 should see which fields will be narrowed, not discover
/// it afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnChange {
    pub columns: u8,
    /// `(field, old span, new span)` for every field that will be clamped.
    pub clamped: Vec<(FieldId, u8, u8)>,
}

pub fn plan_column_change(
    section: &SectionDef,
    new_columns: u8,
) -> Result<ColumnChange, EditError> {
    if !(1..=3).contains(&new_columns) {
        return Err(EditError::BadColumnCount);
    }
    let clamped = section
        .fields
        .iter()
        .filter_map(|f| {
            let new = clamp_col_span(f.col_span, new_columns);
            (new != f.col_span).then_some((f.field.field_id, f.col_span, new))
        })
        .collect();
    Ok(ColumnChange {
        columns: new_columns,
        clamped,
    })
}

/// Whole-form validation, for the builder's save path.
///
/// Collects *every* problem rather than stopping at the first: a coordinator fixing a form
/// should see all of it at once, not play whack-a-mole through six save attempts.
pub fn validate_form(def: &FormDef) -> Vec<(Option<SectionId>, EditError)> {
    let mut errors = Vec::new();
    let mut keys = std::collections::HashSet::new();

    for section in &def.sections {
        let sid = Some(section.section_id);
        if !(1..=3).contains(&section.columns) {
            errors.push((sid, EditError::BadColumnCount));
        }
        if section.title.trim().is_empty() {
            errors.push((sid, EditError::EmptyLabel));
        }
        for f in &section.fields {
            if let Err(e) = validate_field(&f.field) {
                errors.push((sid, e));
            }
            if !keys.insert(f.field.key.clone()) {
                errors.push((sid, EditError::DuplicateKey(f.field.key.clone())));
            }
            if let Err(e) = validate_placement(&f.label, f.col_span, section.columns) {
                errors.push((sid, e));
            }
        }
    }
    errors
}

/// Fields that exist but sit in no section — the destination for a removed placement or a
/// replaced field. Their values are still stored and reappear if the field is placed again.
pub fn unplaced_fields<'a>(
    all: &'a [FieldDef],
    def: &FormDef,
) -> impl Iterator<Item = &'a FieldDef> + 'a {
    let placed: std::collections::HashSet<FieldId> =
        def.iter_fields().map(|f| f.field.field_id).collect();
    all.iter().filter(move |f| !placed.contains(&f.field_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::SectionField;
    use crate::ids::{FieldIdx, FormId, OptionCode};
    use crate::value::parse_decimal;
    use std::sync::Arc;

    fn f(key: &str, kind: FieldKind) -> FieldDef {
        FieldDef {
            field_id: FieldId::new(),
            key: key.into(),
            kind,
        }
    }

    fn opts(codes: &[&str]) -> Arc<[FieldOption]> {
        Arc::from(
            codes
                .iter()
                .enumerate()
                .map(|(i, c)| FieldOption {
                    code: OptionCode::new(*c),
                    label: c.to_string(),
                    ordinal: i as i32,
                })
                .collect::<Vec<_>>(),
        )
    }

    fn section(columns: u8, spans: &[u8]) -> SectionDef {
        SectionDef {
            section_id: SectionId::new(),
            title: "S".into(),
            ordinal: 0,
            columns,
            default_collapsed: false,
            fields: spans
                .iter()
                .enumerate()
                .map(|(i, &sp)| SectionField {
                    idx: FieldIdx(i as u32),
                    field: Arc::new(f(&format!("k{i}"), FieldKind::Text { max_len: None })),
                    label: format!("L{i}"),
                    ordinal: i as i32,
                    col_span: sp,
                    required: false,
                })
                .collect(),
        }
    }

    #[test]
    fn kind_change_requires_a_replacement_field() {
        let a = f("k", FieldKind::Text { max_len: None });
        let b = f(
            "k",
            FieldKind::Numeric {
                min: None,
                max: None,
                scale: 0,
            },
        );
        assert_eq!(classify(&a, &b), FieldEdit::NeedsReplacement);
    }

    #[test]
    fn config_changes_within_a_kind_apply_in_place() {
        let a = f("k", FieldKind::Text { max_len: None });
        let b = f("k", FieldKind::Text { max_len: Some(20) });
        assert_eq!(classify(&a, &b), FieldEdit::InPlace);
    }

    #[test]
    fn keys_are_narrow_by_design() {
        for good in ["a", "dob", "date_of_birth", "f1", "a".repeat(64).as_str()] {
            assert!(validate_key(good).is_ok(), "should accept {good:?}");
        }
        for bad in [
            "",
            "1a",
            "A",
            "date of birth",
            "date-of-birth",
            "dob!",
            &"a".repeat(65),
        ] {
            assert_eq!(
                validate_key(bad),
                Err(EditError::BadKey),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn numeric_bounds_must_be_ordered() {
        let d = f(
            "n",
            FieldKind::Numeric {
                min: Some(parse_decimal("10").unwrap()),
                max: Some(parse_decimal("1").unwrap()),
                scale: 2,
            },
        );
        assert!(matches!(
            validate_field(&d),
            Err(EditError::MinExceedsMax { .. })
        ));
    }

    #[test]
    fn r9_r10_option_lists_are_checked() {
        let one = f(
            "r",
            FieldKind::Radio {
                options: opts(&["M"]),
            },
        );
        assert_eq!(validate_field(&one), Err(EditError::TooFewOptions("radio")));

        let dup = f(
            "s",
            FieldKind::Select {
                options: opts(&["M", "M"]),
                searchable: false,
            },
        );
        assert_eq!(
            validate_field(&dup),
            Err(EditError::DuplicateOption("M".into()))
        );

        let ok = f(
            "r",
            FieldKind::Radio {
                options: opts(&["M", "F"]),
            },
        );
        assert!(validate_field(&ok).is_ok());
    }

    #[test]
    fn r12_placement_rejects_overflowing_spans() {
        assert!(validate_placement("L", 2, 3).is_ok());
        assert_eq!(
            validate_placement("L", 3, 2),
            Err(EditError::SpanExceedsColumns {
                span: 3,
                columns: 2
            })
        );
        assert_eq!(validate_placement("  ", 1, 1), Err(EditError::EmptyLabel));
        assert_eq!(
            validate_placement("L", 1, 4),
            Err(EditError::BadColumnCount)
        );
    }

    #[test]
    fn r12_column_reduction_reports_what_it_will_clamp() {
        let s = section(3, &[1, 2, 3]);
        let plan = plan_column_change(&s, 2).unwrap();
        assert_eq!(plan.columns, 2);
        assert_eq!(plan.clamped.len(), 1, "only the 3-span field is affected");
        assert_eq!(plan.clamped[0].1, 3);
        assert_eq!(plan.clamped[0].2, 2);
    }

    #[test]
    fn r12_widening_columns_clamps_nothing() {
        let s = section(2, &[1, 2]);
        assert!(plan_column_change(&s, 3).unwrap().clamped.is_empty());
    }

    #[test]
    fn form_validation_reports_every_problem_at_once() {
        let mut s = section(2, &[1, 3]); // one overflowing span
        s.title = "  ".into(); // and an empty title
        let def = FormDef::new(FormId::new(), "f", vec![s]);
        // FormDef::new clamps spans, so validate against the pre-clamp intent instead.
        let errors = validate_form(&def);
        assert!(
            errors
                .iter()
                .any(|(_, e)| matches!(e, EditError::EmptyLabel)),
            "expected the empty title to be reported: {errors:?}"
        );
    }

    #[test]
    fn duplicate_keys_are_caught_across_sections() {
        let mut a = section(1, &[1]);
        let mut b = section(1, &[1]);
        let shared = Arc::new(f("same", FieldKind::Date));
        a.fields[0].field = Arc::clone(&shared);
        b.fields[0].field = shared;
        b.ordinal = 1;
        let def = FormDef::new(FormId::new(), "f", vec![a, b]);
        let errors = validate_form(&def);
        assert!(
            errors
                .iter()
                .any(|(_, e)| matches!(e, EditError::DuplicateKey(_)))
        );
    }

    #[test]
    fn unplaced_fields_are_those_in_no_section() {
        let placed = f("placed", FieldKind::Date);
        let orphan = f("orphan", FieldKind::Date);
        let mut s = section(1, &[1]);
        s.fields[0].field = Arc::new(placed.clone());
        let def = FormDef::new(FormId::new(), "f", vec![s]);

        let all = vec![placed, orphan.clone()];
        let un: Vec<_> = unplaced_fields(&all, &def).collect();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0].key, "orphan");
    }
}
