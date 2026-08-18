//! The single validation engine.
//!
//! This function runs in the client's keystroke handler *and* in the Worker's write path.
//! There must never be a second implementation — that is the entire reason the Worker is
//! written in Rust. See `docs/adr/0004-workers-rs.md`.

use crate::def::{FieldDef, FieldKind};
use crate::error::ValidationError;
use crate::value::{Value, check_scale};

/// Validates one value against one field definition.
///
/// `required` lives on the *placement*, not the field, so it is checked by
/// [`validate_placement`] rather than here.
pub fn validate(def: &FieldDef, value: &Value) -> Result<(), ValidationError> {
    if value.is_null() {
        return Ok(());
    }
    match (&def.kind, value) {
        (FieldKind::Text { max_len }, Value::Text(s)) => check_len(s, *max_len),

        (FieldKind::Textarea { max_len, .. }, Value::Text(s)) => check_len(s, *max_len),

        (FieldKind::Numeric { min, max, scale }, Value::Num(d)) => {
            if let Some(min) = min
                && d < min
            {
                return Err(ValidationError::BelowMin {
                    min: min.to_string(),
                });
            }
            if let Some(max) = max
                && d > max
            {
                return Err(ValidationError::AboveMax {
                    max: max.to_string(),
                });
            }
            if *scale > 0 || d.normalize().scale() > 0 {
                check_scale(*d, *scale)
                    .map_err(|_| ValidationError::TooManyDecimals { max: *scale })?;
            }
            Ok(())
        }

        (FieldKind::Date, Value::Date(_)) => Ok(()),
        (FieldKind::Time, Value::Time(_)) => Ok(()),

        (FieldKind::Radio { options }, Value::Opt(code))
        | (FieldKind::Select { options, .. }, Value::Opt(code)) => {
            if options.iter().any(|o| &o.code == code) {
                Ok(())
            } else {
                Err(ValidationError::UnknownOption(code.0.clone()))
            }
        }

        _ => Err(ValidationError::WrongType),
    }
}

/// Validates a value against a field *and* its placement, which is where `required` lives.
pub fn validate_placement(
    def: &FieldDef,
    required: bool,
    value: &Value,
) -> Result<(), ValidationError> {
    if value.is_null() {
        return if required {
            Err(ValidationError::Required)
        } else {
            Ok(())
        };
    }
    if let Value::Text(s) = value
        && required
        && s.trim().is_empty()
    {
        return Err(ValidationError::Required);
    }
    validate(def, value)
}

fn check_len(s: &str, max_len: Option<u32>) -> Result<(), ValidationError> {
    if let Some(max) = max_len {
        let actual = s.chars().count() as u32;
        if actual > max {
            return Err(ValidationError::TooLong { max, actual });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::FieldOption;
    use crate::ids::{FieldId, OptionCode};
    use crate::value::{parse_date, parse_decimal, parse_time_24};
    use std::sync::Arc;

    fn def(kind: FieldKind) -> FieldDef {
        FieldDef {
            field_id: FieldId::new(),
            key: "k".into(),
            kind,
        }
    }

    fn opts() -> Arc<[FieldOption]> {
        Arc::from(vec![
            FieldOption {
                code: OptionCode::new("M"),
                label: "Male".into(),
                ordinal: 0,
            },
            FieldOption {
                code: OptionCode::new("F"),
                label: "Female".into(),
                ordinal: 1,
            },
        ])
    }

    #[test]
    fn null_is_always_allowed_without_required() {
        for kind in [
            FieldKind::Text { max_len: Some(1) },
            FieldKind::Numeric {
                min: None,
                max: None,
                scale: 0,
            },
            FieldKind::Date,
            FieldKind::Time,
        ] {
            assert!(validate(&def(kind), &Value::Null).is_ok());
        }
    }

    #[test]
    fn r5_text_length_enforced() {
        let d = def(FieldKind::Text { max_len: Some(5) });
        assert!(validate(&d, &Value::Text("hello".into())).is_ok());
        assert_eq!(
            validate(&d, &Value::Text("hello!".into())),
            Err(ValidationError::TooLong { max: 5, actual: 6 })
        );
    }

    #[test]
    fn r6_numeric_range_and_scale() {
        let d = def(FieldKind::Numeric {
            min: Some(parse_decimal("0").unwrap()),
            max: Some(parse_decimal("300").unwrap()),
            scale: 2,
        });
        assert!(validate(&d, &Value::Num(parse_decimal("12.50").unwrap())).is_ok());
        assert!(matches!(
            validate(&d, &Value::Num(parse_decimal("-1").unwrap())),
            Err(ValidationError::BelowMin { .. })
        ));
        assert!(matches!(
            validate(&d, &Value::Num(parse_decimal("301").unwrap())),
            Err(ValidationError::AboveMax { .. })
        ));
        assert!(matches!(
            validate(&d, &Value::Num(parse_decimal("1.555").unwrap())),
            Err(ValidationError::TooManyDecimals { max: 2 })
        ));
    }

    #[test]
    fn r8_time_value_is_always_in_range_by_construction() {
        // parse_time_24 is the gate; anything that reached a Value::Time is valid.
        let d = def(FieldKind::Time);
        assert!(validate(&d, &Value::Time(parse_time_24("23:59").unwrap())).is_ok());
        assert!(parse_time_24("24:00").is_err());
    }

    #[test]
    fn r9_r10_unknown_option_rejected() {
        for kind in [
            FieldKind::Radio { options: opts() },
            FieldKind::Select {
                options: opts(),
                searchable: false,
            },
        ] {
            let d = def(kind);
            assert!(validate(&d, &Value::Opt(OptionCode::new("M"))).is_ok());
            assert_eq!(
                validate(&d, &Value::Opt(OptionCode::new("X"))),
                Err(ValidationError::UnknownOption("X".into()))
            );
        }
    }

    #[test]
    fn wrong_type_is_rejected_for_every_kind() {
        let cases = [
            (
                FieldKind::Text { max_len: None },
                Value::Num(parse_decimal("1").unwrap()),
            ),
            (
                FieldKind::Numeric {
                    min: None,
                    max: None,
                    scale: 0,
                },
                Value::Text("x".into()),
            ),
            (
                FieldKind::Date,
                Value::Time(parse_time_24("09:00").unwrap()),
            ),
            (
                FieldKind::Time,
                Value::Date(parse_date("2026-01-01").unwrap()),
            ),
            (
                FieldKind::Radio { options: opts() },
                Value::Text("M".into()),
            ),
        ];
        for (kind, value) in cases {
            assert_eq!(
                validate(&def(kind), &value),
                Err(ValidationError::WrongType)
            );
        }
    }

    #[test]
    fn required_is_checked_at_the_placement() {
        let d = def(FieldKind::Text { max_len: None });
        assert_eq!(
            validate_placement(&d, true, &Value::Null),
            Err(ValidationError::Required)
        );
        assert!(validate_placement(&d, false, &Value::Null).is_ok());
        assert_eq!(
            validate_placement(&d, true, &Value::Text("   ".into())),
            Err(ValidationError::Required),
            "whitespace-only is not a value"
        );
    }
}
