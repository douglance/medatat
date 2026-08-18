//! 24-hour time parsing and formatting (R8).
//!
//! The most heavily tested module in the codebase. Deliberately locale-free: there is no
//! AM/PM handling and no call into OS locale formatting, because the requirement is
//! "time entry in 24hr format", not "time entry in the user's preferred format".
//!
//! The UI never live-reformats while typing — see `docs/05-UI-SPEC.md`. This module is
//! called on blur, on Tab, and on Enter.

use chrono::{NaiveTime, Timelike};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TimeParseError {
    #[error("time is empty")]
    Empty,
    #[error("time contains a non-digit character")]
    NotDigits,
    #[error("too many digits for a time")]
    TooLong,
    #[error("hour {0} is out of range (0-23)")]
    HourOutOfRange(u32),
    #[error("minute {0} is out of range (0-59)")]
    MinuteOutOfRange(u32),
    #[error("malformed time")]
    Malformed,
}

/// Parses a 24-hour time from the shorthand forms an abstractor actually types.
///
/// | input     | result | | input   | result |
/// |-----------|--------|-|---------|--------|
/// | `"9"`     | 09:00  | | `"930"` | 09:30  |
/// | `"09"`    | 09:00  | | `"0930"`| 09:30  |
/// | `"9:30"`  | 09:30  | | `"9:5"` | 09:05  |
/// | `"2359"`  | 23:59  | | `"0000"`| 00:00  |
///
/// Rejects `"24:00"`, `"1260"`, `"9:30pm"`, and anything over four digits.
pub fn parse_time_24(s: &str) -> Result<NaiveTime, TimeParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(TimeParseError::Empty);
    }

    let (h, m) = match s.split_once(':') {
        Some((hs, ms)) => {
            let hs = hs.trim();
            let ms = ms.trim();
            if hs.is_empty() || ms.is_empty() || hs.len() > 2 || ms.len() > 2 {
                return Err(TimeParseError::Malformed);
            }
            (digits(hs)?, digits(ms)?)
        }
        None => {
            let d = digits(s)?;
            let _ = d; // parsed for validation; recomputed per length below
            match s.len() {
                1 | 2 => (digits(s)?, 0),
                3 => (digits(&s[..1])?, digits(&s[1..])?),
                4 => (digits(&s[..2])?, digits(&s[2..])?),
                _ => return Err(TimeParseError::TooLong),
            }
        }
    };

    if h > 23 {
        return Err(TimeParseError::HourOutOfRange(h));
    }
    if m > 59 {
        return Err(TimeParseError::MinuteOutOfRange(m));
    }
    NaiveTime::from_hms_opt(h, m, 0).ok_or(TimeParseError::Malformed)
}

fn digits(s: &str) -> Result<u32, TimeParseError> {
    if s.is_empty() {
        return Err(TimeParseError::Empty);
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimeParseError::NotDigits);
    }
    s.parse::<u32>().map_err(|_| TimeParseError::TooLong)
}

/// Always `HH:MM`, zero-padded, 24-hour. Seconds are discarded — the field kind has no
/// second precision.
pub fn format_time_24(t: NaiveTime) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

/// Steps a time by whole minutes, wrapping at midnight in both directions.
/// Backs the Up/Down and Shift+Up/Down bindings.
pub fn step_minutes(t: NaiveTime, delta: i32) -> NaiveTime {
    let total = t.hour() as i32 * 60 + t.minute() as i32;
    let wrapped = (total + delta).rem_euclid(24 * 60);
    NaiveTime::from_hms_opt((wrapped / 60) as u32, (wrapped % 60) as u32, 0)
        .expect("wrapped into 0..1440")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn r8_accepts_documented_shorthand() {
        for (input, expect) in [
            ("9", t(9, 0)),
            ("09", t(9, 0)),
            ("930", t(9, 30)),
            ("0930", t(9, 30)),
            ("9:30", t(9, 30)),
            ("09:30", t(9, 30)),
            ("9:5", t(9, 5)),
            ("2359", t(23, 59)),
            ("0000", t(0, 0)),
            ("00:00", t(0, 0)),
            ("23:59", t(23, 59)),
            ("0", t(0, 0)),
            (" 930 ", t(9, 30)),
        ] {
            assert_eq!(parse_time_24(input), Ok(expect), "input {input:?}");
        }
    }

    #[test]
    fn r8_rejects_2400() {
        assert_eq!(
            parse_time_24("24:00"),
            Err(TimeParseError::HourOutOfRange(24))
        );
        assert_eq!(
            parse_time_24("2400"),
            Err(TimeParseError::HourOutOfRange(24))
        );
    }

    #[test]
    fn r8_rejects_minute_60() {
        assert_eq!(
            parse_time_24("12:60"),
            Err(TimeParseError::MinuteOutOfRange(60))
        );
        assert_eq!(
            parse_time_24("1260"),
            Err(TimeParseError::MinuteOutOfRange(60))
        );
    }

    #[test]
    fn r8_rejects_malformed() {
        for bad in [
            "", "   ", "abc", "-1", "9:30pm", "999999", "12345", "9:", ":30", "9:300", "1:2:3",
        ] {
            assert!(parse_time_24(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn r8_never_produces_am_pm_or_locale() {
        assert_eq!(format_time_24(t(0, 0)), "00:00");
        assert_eq!(format_time_24(t(9, 5)), "09:05");
        assert_eq!(format_time_24(t(13, 0)), "13:00");
        assert_eq!(format_time_24(t(23, 59)), "23:59");
    }

    #[test]
    fn step_wraps_at_midnight() {
        assert_eq!(step_minutes(t(23, 59), 1), t(0, 0));
        assert_eq!(step_minutes(t(0, 0), -1), t(23, 59));
        assert_eq!(step_minutes(t(9, 30), 60), t(10, 30));
        assert_eq!(step_minutes(t(0, 30), -60), t(23, 30));
    }

    proptest! {
        #[test]
        fn r8_round_trips(h in 0u32..24, m in 0u32..60) {
            let time = t(h, m);
            prop_assert_eq!(parse_time_24(&format_time_24(time)), Ok(time));
        }

        #[test]
        fn r8_never_panics(s in ".*") {
            let _ = parse_time_24(&s);
        }

        #[test]
        fn step_always_in_range(h in 0u32..24, m in 0u32..60, d in -5000i32..5000) {
            let r = step_minutes(t(h, m), d);
            prop_assert!(r.hour() < 24 && r.minute() < 60);
        }
    }
}
