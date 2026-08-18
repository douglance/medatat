//! Worklist paging and case-creation rules.
//!
//! `GET /cases` reads the D1 `case_index`, which is **eventually consistent** with the
//! Durable Objects that own the values (`docs/10-LIMITATIONS.md` #2). Paging is keyset, not
//! offset: the cursor is the last row's `updated_at`, so a case that changes mid-scan
//! cannot make another one skip past the reader.

use crate::error::{LogicError, LogicResult};
use medatat_core::wire::{CasePage, CaseSummary};

pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 200;

/// `POST /bulk/cases` creates at most this many cases per request. Each one is a separate
/// Durable Object round trip, and a Worker request has a wall-clock budget.
pub const BULK_MAX_CASES: usize = 100;

/// Clamp rather than reject: a client asking for more than the cap gets the cap, which is
/// the behaviour a paging loop wants.
pub fn effective_limit(requested: Option<u32>) -> u32 {
    match requested {
        None | Some(0) => DEFAULT_LIMIT,
        Some(n) => n.min(MAX_LIMIT),
    }
}

/// `assignee=me` resolves against the bearer token, never against a body field. Any other
/// value is taken literally so an admin can read another abstractor's worklist.
pub fn resolve_assignee(param: Option<&str>, me: &str) -> Option<String> {
    match param.map(str::trim) {
        None | Some("") | Some("*") | Some("all") => None,
        Some("me") => Some(me.to_string()),
        Some(other) => Some(other.to_string()),
    }
}

/// Turn `limit + 1` rows into a page. The extra row is how `has_more` is known without a
/// second `COUNT(*)` over an index that is only eventually consistent anyway.
pub fn build_page(mut rows: Vec<CaseSummary>, limit: u32) -> CasePage {
    let limit = limit as usize;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let cursor = has_more
        .then(|| rows.last().map(|c| c.updated_at.clone()))
        .flatten();
    CasePage {
        cases: rows,
        cursor,
        has_more,
    }
}

pub fn check_bulk_size(n: usize) -> LogicResult<()> {
    if n == 0 {
        return Err(LogicError::Validation("no cases in the request".into()));
    }
    if n > BULK_MAX_CASES {
        return Err(LogicError::Validation(format!(
            "at most {BULK_MAX_CASES} cases per request (got {n})"
        )));
    }
    Ok(())
}

/// An MRN identifies a chart. It is not validated for shape — sites format them
/// differently — but it must be present, because a case with no MRN cannot be found again.
pub fn check_mrn(mrn: &str) -> LogicResult<()> {
    if mrn.trim().is_empty() {
        Err(LogicError::Validation("mrn must not be empty".into()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::ids::{CaseId, CaseRev, FormId};

    fn case(n: u8) -> CaseSummary {
        CaseSummary {
            case_id: CaseId::new(),
            mrn: format!("MRN-{n:05}"),
            form_id: FormId::nil(),
            assignee: Some("u-1".into()),
            rev: CaseRev(n as i64),
            updated_at: format!("2026-08-17T00:00:{n:02}Z"),
        }
    }

    #[test]
    fn limit_defaults_and_caps() {
        assert_eq!(effective_limit(None), DEFAULT_LIMIT);
        assert_eq!(effective_limit(Some(0)), DEFAULT_LIMIT);
        assert_eq!(effective_limit(Some(10)), 10);
        assert_eq!(effective_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn assignee_me_resolves_from_the_session_not_the_query() {
        assert_eq!(resolve_assignee(Some("me"), "u-7").as_deref(), Some("u-7"));
        assert_eq!(resolve_assignee(None, "u-7"), None);
        assert_eq!(resolve_assignee(Some("u-9"), "u-7").as_deref(), Some("u-9"));
    }

    #[test]
    fn a_full_page_reports_more_and_carries_a_cursor() {
        let rows: Vec<CaseSummary> = (0..4).map(case).collect();
        let page = build_page(rows, 3);
        assert_eq!(page.cases.len(), 3);
        assert!(page.has_more);
        assert_eq!(page.cursor.as_deref(), Some("2026-08-17T00:00:02Z"));
    }

    #[test]
    fn a_short_page_is_the_end_of_the_list() {
        let page = build_page(vec![case(0), case(1)], 3);
        assert_eq!(page.cases.len(), 2);
        assert!(!page.has_more);
        assert_eq!(page.cursor, None, "no cursor when there is nothing after");
    }

    #[test]
    fn an_empty_worklist_is_a_page_not_an_error() {
        let page = build_page(vec![], 50);
        assert!(page.cases.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn bulk_requests_are_bounded_in_both_directions() {
        assert!(check_bulk_size(0).is_err());
        assert!(check_bulk_size(1).is_ok());
        assert!(check_bulk_size(BULK_MAX_CASES).is_ok());
        assert!(check_bulk_size(BULK_MAX_CASES + 1).is_err());
    }

    #[test]
    fn a_case_without_an_mrn_is_rejected() {
        assert!(check_mrn("  ").is_err());
        assert!(check_mrn("MRN-00042").is_ok());
    }
}
