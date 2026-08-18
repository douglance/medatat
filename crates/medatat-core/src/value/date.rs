//! Calendar date parsing (R7). ISO-8601 only — never OS locale formatting.

use chrono::NaiveDate;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DateParseError {
    #[error("date is empty")]
    Empty,
    #[error("date must be YYYY-MM-DD")]
    Malformed,
    #[error("not a real calendar date")]
    NotACalendarDate,
}

pub fn parse_date(s: &str) -> Result<NaiveDate, DateParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(DateParseError::Empty);
    }
    // chrono's %Y-%m-%d accepts unpadded components ("2026-8-17"), so shape is checked
    // first. Storage is fixed-width ISO-8601 and must stay lexically sortable.
    if !is_iso_shape(s) {
        return Err(DateParseError::Malformed);
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| DateParseError::NotACalendarDate)
}

/// Exactly `YYYY-MM-DD`: ten characters, digits everywhere but positions 4 and 7.
fn is_iso_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

pub fn format_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r7_round_trips() {
        for s in ["2026-08-17", "1900-01-01", "2000-02-29", "2099-12-31"] {
            assert_eq!(format_date(parse_date(s).unwrap()), s);
        }
    }

    #[test]
    fn r7_rejects_impossible_dates() {
        assert_eq!(
            parse_date("2026-02-30"),
            Err(DateParseError::NotACalendarDate)
        );
        assert_eq!(
            parse_date("2026-13-01"),
            Err(DateParseError::NotACalendarDate)
        );
        assert_eq!(
            parse_date("2025-02-29"),
            Err(DateParseError::NotACalendarDate)
        );
    }

    #[test]
    fn r7_rejects_non_iso() {
        for bad in ["", "17/08/2026", "Aug 17 2026", "2026-8-17", "abc"] {
            assert!(parse_date(bad).is_err(), "should reject {bad:?}");
        }
    }
}
