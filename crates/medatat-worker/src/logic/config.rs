//! Form-builder rules (R3, R4, R12), as pure functions.
//!
//! Two of these are the whole reason the form-version lifecycle could be cut:
//!
//! - **`kind` is immutable.** Changing it means a new `field_id`, so old values stay intact
//!   and viewable rather than being reinterpreted under a new type.
//! - **`col_span <= section.columns`,** enforced on write because SQLite cannot express a
//!   cross-table CHECK, and re-clamped whenever `columns` is reduced.

use crate::error::{LogicError, LogicResult};
use medatat_core::def::{FieldKind, is_kind_change};
use medatat_core::ids::FieldId;
use medatat_core::layout::{clamp_col_span, col_span_is_valid};

pub const KIND_IMMUTABLE_MESSAGE: &str = "field kind is immutable; create a replacement field";

/// R12: a section has 1, 2, or 3 columns. Anything else is a client error, not a clamp —
/// the builder clamps for feel, the API rejects for truth.
pub fn validate_columns(columns: u8) -> LogicResult<()> {
    if (1..=3).contains(&columns) {
        Ok(())
    } else {
        Err(LogicError::Validation(format!(
            "columns must be 1, 2, or 3 (got {columns})"
        )))
    }
}

/// R12: a field may never span more columns than its section has.
pub fn validate_placement(col_span: u8, columns: u8) -> LogicResult<()> {
    validate_columns(columns)?;
    if col_span_is_valid(col_span, columns) {
        Ok(())
    } else {
        Err(LogicError::Validation(format!(
            "col_span {col_span} exceeds section columns {columns}"
        )))
    }
}

/// The `col_span` a placement patch lands on, validated.
///
/// The validation is unconditional *by construction*: the caller passes the row's current
/// span rather than deciding for itself whether the body touched `col_span`. A patch that
/// only carries a label must still be checked, because the stored row may predate a
/// `columns` reduction — and "the body did not touch it, so skip the check" is exactly the
/// shortcut that would let R12 rot.
pub fn resolve_col_span(requested: Option<u8>, current: u8, columns: u8) -> LogicResult<u8> {
    let span = requested.unwrap_or(current);
    validate_placement(span, columns)?;
    Ok(span)
}

/// The `col_span` updates that reducing a section to `columns` requires. Applied in the
/// same transaction as the `columns` change, so the section is never briefly inconsistent.
pub fn clamps_for_columns(children: &[(FieldId, u8)], columns: u8) -> Vec<(FieldId, u8)> {
    children
        .iter()
        .filter_map(|&(id, span)| {
            let clamped = clamp_col_span(span, columns);
            (clamped != span).then_some((id, clamped))
        })
        .collect()
}

/// Rejects a `kind` change on an existing field.
pub fn check_kind_immutable(existing: &FieldKind, incoming: &FieldKind) -> LogicResult<()> {
    if is_kind_change(existing, incoming) {
        Err(LogicError::Validation(KIND_IMMUTABLE_MESSAGE.into()))
    } else {
        Ok(())
    }
}

/// `PATCH /config/fields/{id}` carries `{config?, options?}` and nothing else. A body that
/// names a different `kind` is rejected rather than ignored: silently dropping it would let
/// a builder believe it had re-typed a field.
pub fn check_kind_patch(existing: &FieldKind, patch: &serde_json::Value) -> LogicResult<()> {
    match patch.get("kind") {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::String(tag)) if tag == existing.tag() => Ok(()),
        Some(_) => Err(LogicError::Validation(KIND_IMMUTABLE_MESSAGE.into())),
    }
}

/// Rejects internally contradictory field configuration at creation time, so no value can
/// ever be written against a definition that no value could satisfy.
pub fn validate_field_kind(kind: &FieldKind) -> LogicResult<()> {
    match kind {
        FieldKind::Numeric { min, max, scale } => {
            if let (Some(min), Some(max)) = (min, max)
                && min > max
            {
                return Err(LogicError::Validation(format!(
                    "numeric min {min} is above max {max}"
                )));
            }
            if *scale > 28 {
                return Err(LogicError::Validation(format!(
                    "numeric scale {scale} exceeds the 28 digits a Decimal can hold"
                )));
            }
            Ok(())
        }
        FieldKind::Text { max_len } | FieldKind::Textarea { max_len, .. } => {
            if *max_len == Some(0) {
                return Err(LogicError::Validation("max_len must be at least 1".into()));
            }
            Ok(())
        }
        FieldKind::Radio { options } | FieldKind::Select { options, .. } => {
            if options.is_empty() {
                return Err(LogicError::Validation(
                    "a radio or select field needs at least one option".into(),
                ));
            }
            let mut codes: Vec<&str> = options.iter().map(|o| o.code.as_str()).collect();
            codes.sort_unstable();
            let before = codes.len();
            codes.dedup();
            if codes.len() != before {
                return Err(LogicError::Validation("duplicate option code".into()));
            }
            Ok(())
        }
        FieldKind::Date | FieldKind::Time => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::def::FieldOption;
    use medatat_core::ids::OptionCode;
    use medatat_core::value::parse_decimal;
    use std::sync::Arc;

    fn options(codes: &[&str]) -> Arc<[FieldOption]> {
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

    #[test]
    fn r12_col_span_above_section_columns_is_rejected() {
        let err = validate_placement(3, 2).unwrap_err();
        assert_eq!(err.http_status(), 422);
        assert!(err.to_string().contains("col_span 3"), "{err}");

        assert!(validate_placement(2, 2).is_ok());
        assert!(validate_placement(1, 3).is_ok());
        assert!(
            validate_placement(0, 2).is_err(),
            "col_span 0 is meaningless"
        );
    }

    #[test]
    fn r12_columns_outside_one_to_three_is_rejected() {
        for bad in [0u8, 4, 9] {
            assert_eq!(validate_columns(bad).unwrap_err().http_status(), 422);
        }
        for good in 1..=3u8 {
            assert!(validate_columns(good).is_ok());
        }
    }

    #[test]
    fn r12_a_patch_that_does_not_touch_col_span_is_still_validated() {
        // A row placed at span 3 while the section had 3 columns, in a section since
        // reduced to 2. Patching only its label must not smuggle the stale span through.
        assert!(resolve_col_span(None, 3, 2).is_err());
        assert_eq!(resolve_col_span(None, 3, 2).unwrap_err().http_status(), 422);

        // The same row, explicitly narrowed by the patch, is fine.
        assert_eq!(resolve_col_span(Some(2), 3, 2).unwrap(), 2);
        // An untouched span that still fits is left alone.
        assert_eq!(resolve_col_span(None, 2, 2).unwrap(), 2);
        // A patch that widens past the section is rejected.
        assert!(resolve_col_span(Some(3), 1, 2).is_err());
    }

    #[test]
    fn r12_reducing_columns_clamps_every_child_that_overflows() {
        let (a, b, c) = (FieldId::new(), FieldId::new(), FieldId::new());
        let children = [(a, 1u8), (b, 2), (c, 3)];

        let updates = clamps_for_columns(&children, 2);
        assert_eq!(updates, vec![(c, 2)], "only the overflowing child changes");

        let updates = clamps_for_columns(&children, 1);
        assert_eq!(updates, vec![(b, 1), (c, 1)]);

        assert!(
            clamps_for_columns(&children, 3).is_empty(),
            "widening a section clamps nothing"
        );
    }

    #[test]
    fn r2_kind_change_is_rejected() {
        let err = check_kind_immutable(
            &FieldKind::Text { max_len: None },
            &FieldKind::Numeric {
                min: None,
                max: None,
                scale: 0,
            },
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 422);
        assert_eq!(err.to_string(), KIND_IMMUTABLE_MESSAGE);
    }

    #[test]
    fn r2_same_kind_with_different_config_is_allowed() {
        assert!(
            check_kind_immutable(
                &FieldKind::Text { max_len: None },
                &FieldKind::Text { max_len: Some(255) }
            )
            .is_ok()
        );
    }

    #[test]
    fn r2_patching_a_field_to_a_new_kind_is_rejected() {
        let existing = FieldKind::Date;

        let err = check_kind_patch(&existing, &serde_json::json!({ "kind": "time" })).unwrap_err();
        assert_eq!(err.http_status(), 422);
        assert_eq!(err.to_string(), KIND_IMMUTABLE_MESSAGE);

        // A patch that merely restates the existing kind is harmless.
        assert!(check_kind_patch(&existing, &serde_json::json!({ "kind": "date" })).is_ok());
        // The documented body carries no `kind` at all.
        assert!(check_kind_patch(&existing, &serde_json::json!({ "config": {} })).is_ok());
        // A non-string `kind` is still a re-type attempt, not a no-op.
        assert!(check_kind_patch(&existing, &serde_json::json!({ "kind": 7 })).is_err());
    }

    #[test]
    fn r6_contradictory_numeric_bounds_are_rejected() {
        let err = validate_field_kind(&FieldKind::Numeric {
            min: Some(parse_decimal("300").unwrap()),
            max: Some(parse_decimal("0").unwrap()),
            scale: 0,
        })
        .unwrap_err();
        assert_eq!(err.http_status(), 422);

        assert!(
            validate_field_kind(&FieldKind::Numeric {
                min: Some(parse_decimal("0").unwrap()),
                max: Some(parse_decimal("300").unwrap()),
                scale: 2,
            })
            .is_ok()
        );
    }

    #[test]
    fn r9_r10_option_sets_must_be_usable() {
        assert!(
            validate_field_kind(&FieldKind::Radio {
                options: options(&[])
            })
            .is_err()
        );
        assert!(
            validate_field_kind(&FieldKind::Select {
                options: options(&["M", "M"]),
                searchable: false,
            })
            .is_err()
        );
        assert!(
            validate_field_kind(&FieldKind::Radio {
                options: options(&["M", "F"])
            })
            .is_ok()
        );
    }

    #[test]
    fn r5_zero_length_text_is_rejected() {
        assert!(validate_field_kind(&FieldKind::Text { max_len: Some(0) }).is_err());
        assert!(validate_field_kind(&FieldKind::Text { max_len: Some(1) }).is_ok());
        assert!(validate_field_kind(&FieldKind::Text { max_len: None }).is_ok());
    }
}
