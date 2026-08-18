//! Exact decimal handling (R6).
//!
//! Clinical values are never `f64`. SQLite has no exact numeric type and IEEE-754 would
//! silently corrupt doses, weights, and lab results, so values move as decimal strings and
//! live in memory as `rust_decimal::Decimal`.

use rust_decimal::Decimal;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NumericParseError {
    #[error("value is empty")]
    Empty,
    #[error("not a valid decimal number")]
    NotANumber,
    #[error("too many decimal places: {found} (max {max})")]
    TooManyPlaces { found: u32, max: u8 },
}

pub fn parse_decimal(s: &str) -> Result<Decimal, NumericParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(NumericParseError::Empty);
    }
    Decimal::from_str(s).map_err(|_| NumericParseError::NotANumber)
}

/// Formats without exponent notation, preserving the value's own scale.
pub fn format_decimal(d: Decimal) -> String {
    d.normalize().to_string()
}

/// Formats at a fixed number of decimal places, as configured on the field.
pub fn format_decimal_scaled(d: Decimal, scale: u8) -> String {
    format!("{:.*}", scale as usize, d)
}

/// Rejects values with more decimal places than the field allows. Rounding silently would
/// change a clinical value, so this is an error rather than a coercion.
pub fn check_scale(d: Decimal, max: u8) -> Result<(), NumericParseError> {
    let found = d.normalize().scale();
    if found > max as u32 {
        return Err(NumericParseError::TooManyPlaces { found, max });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r6_round_trips_exactly() {
        for s in ["0", "1", "-1", "12.50", "0.001", "999999.999", "-0.5"] {
            let d = parse_decimal(s).unwrap();
            assert_eq!(parse_decimal(&d.to_string()).unwrap(), d, "input {s}");
        }
    }

    #[test]
    fn r6_preserves_precision_f64_would_lose() {
        // 0.1 + 0.2 != 0.3 in IEEE-754. It must here.
        let a = parse_decimal("0.1").unwrap();
        let b = parse_decimal("0.2").unwrap();
        assert_eq!(a + b, parse_decimal("0.3").unwrap());
    }

    #[test]
    fn r6_rejects_non_numbers() {
        for bad in ["", "  ", "abc", "1.2.3", "1,000", "NaN", "1e999999"] {
            assert!(parse_decimal(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn r6_enforces_scale() {
        assert!(check_scale(parse_decimal("1.5").unwrap(), 2).is_ok());
        assert!(check_scale(parse_decimal("1.50").unwrap(), 2).is_ok());
        assert!(check_scale(parse_decimal("1.555").unwrap(), 2).is_err());
    }

    #[test]
    fn scaled_formatting_pads() {
        assert_eq!(
            format_decimal_scaled(parse_decimal("1.5").unwrap(), 2),
            "1.50"
        );
        assert_eq!(format_decimal_scaled(parse_decimal("2").unwrap(), 1), "2.0");
    }
}
