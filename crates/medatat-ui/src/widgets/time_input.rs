//! The 24-hour time input (R8).
//!
//! `gpui-component` ships no TimePicker, so this is the one widget built from scratch.
//!
//! The design rule that matters: **never reformat while the user is typing.** Live masking
//! moves the caret out from under them, which is the number-one bug class in masked
//! inputs. Input is validated for styling only; canonicalisation happens on blur, Tab, or
//! Enter. All the parsing lives in `medatat_core::value::time`, so it is proptested
//! without a GUI and shared verbatim with the Worker.

//! The two halves of the filter reach `gpui-component` through different hooks, because
//! they are different kinds of operation:
//!
//! - Rejection is a pure predicate over the proposed text, so it goes to
//!   `InputState::validate`, which reverts to the previous text and returns without
//!   touching the caret. That is what "reject the keystroke" has to mean.
//! - Separator insertion rewrites the text, which `validate` cannot do, so it runs from the
//!   change subscription in `widgets::subscribe` — and only when the caret is at
//!   end-of-text, where there is nothing after it to displace.

use medatat_core::Value;
use medatat_core::value::{TimeParseError, step_minutes};
use medatat_core::{format_time_24, parse_time_24};

/// What a keystroke should do to the raw text, before any parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeEdit {
    /// Accept the new text as-is.
    Accept(String),
    /// Accept it and insert a separator, because the caret is at the end and this is the
    /// third digit. Safe precisely because there is no text after the caret to displace.
    AcceptWithColon(String),
    /// Reject the keystroke; the character is not legal in a time field.
    Reject,
}

/// Whether this text belongs in a time field at all.
///
/// Shaped for `InputState::validate`, which takes the whole proposed text and reverts the
/// edit on `false`. Deliberately says nothing about whether the time is *valid* — `24:00`
/// passes here and fails at [`commit`], because rejecting it mid-typing would make `2` and
/// `4` untypeable as the start of `04:00`.
pub fn accepts(proposed: &str) -> bool {
    proposed.len() <= 5
        && proposed.bytes().all(|b| b.is_ascii_digit() || b == b':')
        && proposed.bytes().filter(|&b| b == b':').count() <= 1
}

/// The separator to insert after the third digit, if this is that moment.
///
/// `None` unless the caret sits at end-of-text: inserting anywhere else would displace
/// what follows and jump the caret, which is the bug this whole module exists to avoid.
pub fn autocomplete_colon(text: &str, caret_at_end: bool) -> Option<String> {
    if !caret_at_end || text.len() != 3 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = text.split_at(2);
    Some(format!("{h}:{m}"))
}

/// The rejection filter. Blocks illegal characters; never rewrites what is already there.
///
/// The whole-keystroke view, used by the tests. At runtime the two halves are applied
/// through separate `gpui-component` hooks — see the module note.
pub fn filter_input(current: &str, proposed: &str, caret_at_end: bool) -> TimeEdit {
    if !accepts(proposed) {
        return TimeEdit::Reject;
    }
    let grew = proposed.len() > current.len();
    if grew && let Some(with_colon) = autocomplete_colon(proposed, caret_at_end) {
        return TimeEdit::AcceptWithColon(with_colon);
    }
    TimeEdit::Accept(proposed.to_string())
}

/// Applied on blur, Tab, or Enter.
///
/// On success the text is replaced with the canonical `HH:MM`. On failure the raw text is
/// **kept** and the field marked invalid — silently discarding what someone typed is worse
/// than showing them it was wrong.
pub fn commit(raw: &str) -> Result<(Value, String), (String, TimeParseError)> {
    if raw.trim().is_empty() {
        return Ok((Value::Null, String::new()));
    }
    match parse_time_24(raw) {
        Ok(t) => Ok((Value::Time(t), format_time_24(t))),
        Err(e) => Err((raw.to_string(), e)),
    }
}

/// `Up`/`Down` = ±1 minute, `Shift`+ = ±1 hour, wrapping at midnight.
/// An empty field starts at 00:00 so the keys always do something.
pub fn nudge(raw: &str, minutes: i32) -> String {
    let base = parse_time_24(raw).unwrap_or_default();
    format_time_24(step_minutes(base, minutes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r8_rejects_illegal_characters_without_rewriting() {
        assert_eq!(filter_input("09", "09a", true), TimeEdit::Reject);
        assert_eq!(filter_input("09", "09:3:", true), TimeEdit::Reject);
        assert_eq!(filter_input("09:30", "09:300", true), TimeEdit::Reject);
        assert_eq!(filter_input("", "-", true), TimeEdit::Reject);
    }

    #[test]
    fn r8_inserts_the_colon_only_at_end_of_text() {
        assert_eq!(
            filter_input("09", "093", true),
            TimeEdit::AcceptWithColon("09:3".into())
        );
        // Caret mid-string: inserting would jump the caret, so don't.
        assert_eq!(
            filter_input("09", "093", false),
            TimeEdit::Accept("093".into())
        );
    }

    #[test]
    fn r8_deleting_back_through_three_digits_does_not_reinsert_the_colon() {
        // Runtime relies on this: the change subscription tracks the previous text so a
        // deletion that happens to land on three digits is not treated as a third keystroke.
        assert_eq!(
            filter_input("0935", "093", true),
            TimeEdit::Accept("093".into())
        );
    }

    #[test]
    fn r8_accepts_is_the_rejection_half_of_filter_input() {
        // `accepts` is what reaches `InputState::validate`, so it must agree with
        // `filter_input` on every rejection or the two hooks would disagree at runtime.
        for proposed in ["09a", "09:3:", "09:300", "-", "12:345"] {
            assert!(!accepts(proposed), "{proposed} should be rejected");
            assert_eq!(filter_input("", proposed, true), TimeEdit::Reject);
        }
        for proposed in ["", "9", "09", "09:", "09:3", "23:59"] {
            assert!(accepts(proposed), "{proposed} should be accepted");
            assert_ne!(filter_input("", proposed, true), TimeEdit::Reject);
        }
    }

    #[test]
    fn r8_autocomplete_colon_only_fires_at_end_of_text() {
        assert_eq!(autocomplete_colon("093", true), Some("09:3".into()));
        assert_eq!(autocomplete_colon("093", false), None, "caret mid-string");
        assert_eq!(autocomplete_colon("09", true), None, "only two digits");
        assert_eq!(autocomplete_colon("0930", true), None, "already past three");
        assert_eq!(autocomplete_colon("09:", true), None, "not all digits");
    }

    #[test]
    fn r8_never_reformats_mid_typing() {
        // A partial time stays exactly as typed.
        assert_eq!(filter_input("", "9", true), TimeEdit::Accept("9".into()));
        assert_eq!(filter_input("9", "9:", true), TimeEdit::Accept("9:".into()));
        assert_eq!(
            filter_input("9:", "9:3", true),
            TimeEdit::Accept("9:3".into())
        );
    }

    #[test]
    fn r8_commit_canonicalises_to_hhmm() {
        for (typed, shown) in [
            ("930", "09:30"),
            ("9", "09:00"),
            ("9:5", "09:05"),
            ("2359", "23:59"),
        ] {
            let (v, text) = commit(typed).unwrap();
            assert_eq!(text, shown, "typed {typed}");
            assert!(matches!(v, Value::Time(_)));
        }
    }

    #[test]
    fn r8_commit_keeps_bad_input_visible() {
        let (kept, _) = commit("24:00").unwrap_err();
        assert_eq!(kept, "24:00", "never silently discard what the user typed");
        assert!(commit("12:60").is_err());
    }

    #[test]
    fn empty_commits_to_null() {
        assert_eq!(commit("   ").unwrap(), (Value::Null, String::new()));
    }

    #[test]
    fn r8_nudge_wraps_at_midnight() {
        assert_eq!(nudge("23:59", 1), "00:00");
        assert_eq!(nudge("00:00", -1), "23:59");
        assert_eq!(nudge("09:30", 60), "10:30");
        assert_eq!(nudge("", 1), "00:01");
    }
}
