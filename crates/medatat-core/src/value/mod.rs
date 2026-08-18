//! The stored value of a single field.
//!
//! Seven field kinds map onto four storage columns — see `docs/02-DATA-MODEL.md`.
//!
//! PHI protections (redacting `Debug`, zeroize on `Drop`) are behind the `phi` feature,
//! which is off by default so values are printable during development. See
//! `docs/12-PHI-READINESS.md`.

pub mod date;
pub mod numeric;
pub mod time;

use crate::ids::OptionCode;
use chrono::{NaiveDate, NaiveTime};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

pub use date::{DateParseError, format_date, parse_date};
pub use numeric::{
    NumericParseError, check_scale, format_decimal, format_decimal_scaled, parse_decimal,
};
pub use time::{TimeParseError, format_time_24, parse_time_24, step_minutes};

/// The storage column a kind writes to. Exactly one column is non-NULL per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueColumn {
    Text,
    Numeric,
    Date,
    Time,
}

impl ValueColumn {
    pub fn column_name(self) -> &'static str {
        match self {
            ValueColumn::Text => "value_text",
            ValueColumn::Numeric => "value_numeric",
            ValueColumn::Date => "value_date",
            ValueColumn::Time => "value_time",
        }
    }
}

/// A field's value.
///
/// `Null` currently conflates "not reached yet", "chart does not say", and "documented as
/// absent". That is a known limitation — see `docs/10-LIMITATIONS.md`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Text(String),
    /// Serialised as a decimal *string*, never a float.
    #[serde(with = "decimal_str")]
    Num(Decimal),
    Date(NaiveDate),
    /// Serialised as `HH:MM`, not chrono's default `HH:MM:SS`.
    ///
    /// The field kind has no second precision, and storage, display, and the wire must all
    /// agree on one encoding — otherwise `docs/03-API.md` documents `"09:30"` while the
    /// client actually sends `"09:30:00"`, and the two halves of the system are written
    /// against different contracts.
    #[serde(with = "time_hhmm")]
    Time(NaiveTime),
    Opt(OptionCode),
}

#[allow(clippy::derivable_impls)] // Value has a Drop impl under `phi`; keep this explicit.
impl Default for Value {
    fn default() -> Self {
        Value::Null
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn column(&self) -> Option<ValueColumn> {
        Some(match self {
            Value::Null => return None,
            Value::Text(_) | Value::Opt(_) => ValueColumn::Text,
            Value::Num(_) => ValueColumn::Numeric,
            Value::Date(_) => ValueColumn::Date,
            Value::Time(_) => ValueColumn::Time,
        })
    }

    /// The canonical storage encoding: exactly what goes into the SQLite column.
    pub fn to_storage_string(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Text(s) => Some(s.clone()),
            Value::Opt(c) => Some(c.0.clone()),
            Value::Num(d) => Some(d.to_string()),
            Value::Date(d) => Some(format_date(*d)),
            Value::Time(t) => Some(format_time_24(*t)),
        }
    }

    /// What the user sees in an input. Empty string for `Null`.
    pub fn to_display_string(&self) -> String {
        self.to_storage_string().unwrap_or_default()
    }
}

/// Redacting `Debug` under `phi`, so no field value can reach a log line, panic message,
/// or tracing span.
#[cfg(feature = "phi")]
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "Null"),
            Value::Text(s) => write!(f, "Text(<redacted, {} chars>)", s.len()),
            Value::Num(_) => write!(f, "Num(<redacted>)"),
            Value::Date(_) => write!(f, "Date(<redacted>)"),
            Value::Time(_) => write!(f, "Time(<redacted>)"),
            Value::Opt(_) => write!(f, "Opt(<redacted>)"),
        }
    }
}

/// Plain `Debug` when PHI mode is off — values are printable during development, which is
/// the entire point of the feature flag.
#[cfg(not(feature = "phi"))]
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "Null"),
            Value::Text(s) => write!(f, "Text({s:?})"),
            Value::Num(d) => write!(f, "Num({d})"),
            Value::Date(d) => write!(f, "Date({d})"),
            Value::Time(t) => write!(f, "Time({})", format_time_24(*t)),
            Value::Opt(c) => write!(f, "Opt({})", c.0),
        }
    }
}

#[cfg(feature = "phi")]
impl Drop for Value {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        match self {
            Value::Text(s) => s.zeroize(),
            Value::Opt(c) => c.0.zeroize(),
            _ => {}
        }
    }
}

/// Times cross the wire as `HH:MM`, matching the storage encoding and R8 exactly.
/// Deserialisation accepts `HH:MM:SS` too, so a payload written by an older build still
/// parses.
mod time_hhmm {
    use super::{format_time_24, parse_time_24};
    use chrono::NaiveTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &NaiveTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_time_24(*t))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveTime, D::Error> {
        let s = String::deserialize(d)?;
        if let Ok(t) = parse_time_24(&s) {
            return Ok(t);
        }
        NaiveTime::parse_from_str(&s, "%H:%M:%S").map_err(serde::de::Error::custom)
    }
}

/// Decimals cross the wire as strings. A JSON number would be parsed as a float by most
/// consumers, which is exactly the corruption this avoids.
mod decimal_str {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::str::FromStr;

    pub fn serialize<S: Serializer>(d: &Decimal, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&d.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
        let s = String::deserialize(d)?;
        Decimal::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_encoding_is_canonical() {
        assert_eq!(Value::Null.to_storage_string(), None);
        assert_eq!(
            Value::Time(parse_time_24("9:5").unwrap()).to_storage_string(),
            Some("09:05".into())
        );
        assert_eq!(
            Value::Num(parse_decimal("12.50").unwrap()).to_storage_string(),
            Some("12.50".into())
        );
    }

    #[test]
    fn decimal_serialises_as_a_string_not_a_float() {
        let v = Value::Num(parse_decimal("0.1").unwrap());
        let j = serde_json::to_string(&v).unwrap();
        assert!(
            j.contains("\"0.1\""),
            "decimal must be a JSON string, got {j}"
        );
        assert_eq!(serde_json::from_str::<Value>(&j).unwrap(), v);
    }

    #[test]
    fn r8_time_serialises_as_hhmm_not_hhmmss() {
        // docs/03-API.md documents {"Time": "09:30"}. Storage uses HH:MM. The wire must
        // match both, or client and server are written against different contracts.
        let v = Value::Time(parse_time_24("09:30").unwrap());
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"Time":"09:30"}"#);
    }

    #[test]
    fn r8_time_still_accepts_the_older_hhmmss_form() {
        let v: Value = serde_json::from_str(r#"{"Time":"09:30:00"}"#).unwrap();
        assert_eq!(v, Value::Time(parse_time_24("09:30").unwrap()));
    }

    #[test]
    fn all_variants_round_trip_json() {
        for v in [
            Value::Null,
            Value::Text("hello".into()),
            Value::Num(parse_decimal("-3.25").unwrap()),
            Value::Date(parse_date("2026-08-17").unwrap()),
            Value::Time(parse_time_24("23:59").unwrap()),
            Value::Opt(OptionCode::new("M")),
        ] {
            let j = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&j).unwrap(), v, "json {j}");
        }
    }

    #[cfg(feature = "phi")]
    #[test]
    fn debug_redacts() {
        let v = Value::Text("Jane Patient".into());
        let s = format!("{v:?}");
        assert!(!s.contains("Jane"), "PHI leaked into Debug: {s}");
        assert!(s.contains("redacted"));
    }

    #[cfg(not(feature = "phi"))]
    #[test]
    fn debug_prints_when_phi_is_off() {
        assert!(format!("{:?}", Value::Text("visible".into())).contains("visible"));
    }
}
