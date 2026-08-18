//! A scripted transport double.
//!
//! `medatat-sync` owns the async `Transport` trait; this type deliberately does not
//! implement it, because the testkit must not depend on the crate it is used to test. The
//! methods here mirror that trait one-for-one and are synchronous, so `medatat-sync` wraps
//! them in a three-line `#[async_trait] impl Transport for MockTransport` and needs no
//! runtime to drive the double itself.
//!
//! Everything is behind `&self` with interior mutability: a `SyncEngine` holds the
//! transport by shared reference, so a test has to be able to knock it offline mid-run.

use medatat_core::ids::{CaseId, CaseRev, ConfigRev, FieldId};
use medatat_core::wire::{
    CasePage, CaseQuery, CaseSummary, ConfigDelta, PutValuesReq, PutValuesResp, ValuePage, ValueRow,
};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use thiserror::Error;

/// Failures a `Transport` implementation can surface. `Offline` is a normal state, not an
/// error — see `docs/04-SYNC.md`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MockError {
    #[error("transport is offline")]
    Offline,
    #[error("injected transient failure")]
    Injected,
    #[error("no case {0}")]
    NotFound(CaseId),
}

#[derive(Debug, Default)]
struct State {
    online: bool,
    fail_next: usize,
    calls: Vec<String>,
    values: HashMap<CaseId, Vec<ValueRow>>,
    revs: HashMap<CaseId, CaseRev>,
    cases: Vec<CaseSummary>,
    config: Option<ConfigDelta>,
}

/// A `Transport` test double: scripted responses, a call log, and failure injection.
#[derive(Debug)]
pub struct MockTransport {
    state: Mutex<State>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    pub fn new() -> Self {
        MockTransport {
            state: Mutex::new(State {
                online: true,
                ..State::default()
            }),
        }
    }

    // ------------------------------------------------------------------ scripting

    /// Seeds the server-side values for one case, at the rev carried by the rows.
    pub fn with_values(self, case_id: CaseId, rows: Vec<ValueRow>) -> Self {
        {
            let mut s = self.lock();
            let rev = rows.iter().map(|r| r.rev).max().unwrap_or(CaseRev::ZERO);
            s.values.insert(case_id, rows);
            s.revs.insert(case_id, rev);
        }
        self
    }

    /// Seeds the worklist returned by [`MockTransport::list_cases`].
    pub fn with_cases(self, cases: Vec<CaseSummary>) -> Self {
        self.lock().cases = cases;
        self
    }

    /// Seeds the delta returned by [`MockTransport::config`].
    pub fn with_config(self, delta: ConfigDelta) -> Self {
        self.lock().config = Some(delta);
        self
    }

    // ------------------------------------------------------------------ control

    /// Fails the next `n` calls with [`MockError::Injected`], then behaves normally.
    /// Drives the backoff tests without any real waiting.
    pub fn fail_next(&self, n: usize) {
        self.lock().fail_next = n;
    }

    pub fn go_offline(&self) {
        self.lock().online = false;
    }

    pub fn go_online(&self) {
        self.lock().online = true;
    }

    pub fn is_online(&self) -> bool {
        self.lock().online
    }

    /// Every call attempted, in order, including ones that failed. A test asserting
    /// "reconnect drained the outbox once" reads this.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
    }

    /// Replaces the scripted config after construction, for tests that need to change it
    /// mid-run rather than at build time.
    pub fn set_config(&self, delta: ConfigDelta) {
        self.lock().config = Some(delta);
    }

    /// Reflects a case's stored revision into the listing the client sees.
    ///
    /// `put_values` advances the per-case rev but leaves `cases` alone, because those are
    /// two different server-side writes in reality — the CaseDO write and the D1 index
    /// update. Tests that exercise the pull path call this to model the index catching up.
    pub fn bump_case_rev(&self, case_id: CaseId) {
        let mut s = self.lock();
        let rev = s.revs.get(&case_id).copied().unwrap_or(CaseRev::ZERO);
        if let Some(c) = s.cases.iter_mut().find(|c| c.case_id == case_id) {
            c.rev = rev;
        }
    }

    pub fn clear_calls(&self) {
        self.lock().calls.clear();
    }

    /// The current server rev for a case — what the next successful write will exceed.
    pub fn rev(&self, case_id: CaseId) -> CaseRev {
        self.lock()
            .revs
            .get(&case_id)
            .copied()
            .unwrap_or(CaseRev::ZERO)
    }

    /// The server's current view of a case, for asserting what a write actually stored.
    pub fn stored_values(&self, case_id: CaseId) -> Vec<ValueRow> {
        self.lock()
            .values
            .get(&case_id)
            .cloned()
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------ transport surface

    pub fn config(&self, since: ConfigRev) -> Result<Option<ConfigDelta>, MockError> {
        let mut s = self.gate(format!("config(since={})", since.0))?;
        Ok(s.config.take().filter(|d| d.config_rev > since))
    }

    pub fn list_cases(&self, q: CaseQuery) -> Result<CasePage, MockError> {
        let s = self.gate(format!("list_cases(assignee={:?})", q.assignee))?;
        let cases: Vec<CaseSummary> = s
            .cases
            .iter()
            .filter(|c| match (&q.assignee, &c.assignee) {
                (Some(want), Some(have)) => want == have,
                (Some(_), None) => false,
                (None, _) => true,
            })
            .take(q.limit.unwrap_or(u32::MAX) as usize)
            .cloned()
            .collect();
        Ok(CasePage {
            cases,
            cursor: None,
            has_more: false,
        })
    }

    pub fn get_values(&self, case_id: CaseId, since_rev: CaseRev) -> Result<ValuePage, MockError> {
        let s = self.gate(format!("get_values({case_id}, since={since_rev})"))?;
        let rows = s.values.get(&case_id).ok_or(MockError::NotFound(case_id))?;
        Ok(ValuePage {
            case_id,
            rev: s.revs.get(&case_id).copied().unwrap_or(CaseRev::ZERO),
            values: rows.iter().filter(|r| r.rev > since_rev).cloned().collect(),
        })
    }

    /// Applies a write with the same per-field conflict rule the Worker uses: only a
    /// genuine same-field race rejects, so two abstractors in different sections both
    /// succeed. See `docs/04-SYNC.md`.
    pub fn put_values(
        &self,
        case_id: CaseId,
        req: PutValuesReq,
    ) -> Result<PutValuesResp, MockError> {
        let mut s = self.gate(format!(
            "put_values({case_id}, base_rev={}, n={})",
            req.base_rev,
            req.changes.len()
        ))?;

        let server_rev = s.revs.get(&case_id).copied().unwrap_or(CaseRev::ZERO);
        let rows = s.values.entry(case_id).or_default();

        let touched: Vec<FieldId> = req.changes.iter().map(|c| c.field_id).collect();
        let conflicts: Vec<ValueRow> = rows
            .iter()
            .filter(|r| r.rev > req.base_rev && touched.contains(&r.field_id))
            .cloned()
            .collect();

        if !conflicts.is_empty() {
            return Ok(PutValuesResp::Conflict {
                server_rev,
                conflicts,
            });
        }

        let new_rev = server_rev.next();
        for change in &req.changes {
            let row = ValueRow {
                field_id: change.field_id,
                value: change.value.clone(),
                rev: new_rev,
                updated_by: None,
                updated_at: None,
            };
            match rows.iter_mut().find(|r| r.field_id == change.field_id) {
                Some(existing) => *existing = row,
                None => rows.push(row),
            }
        }
        s.revs.insert(case_id, new_rev);

        Ok(PutValuesResp::Applied {
            rev: new_rev,
            applied: touched,
        })
    }

    // ------------------------------------------------------------------ internals

    /// Logs the call, then applies offline state and injected failures in that order —
    /// a failed call still counts as an attempt, which is what backoff tests assert on.
    fn gate(&self, call: String) -> Result<MutexGuard<'_, State>, MockError> {
        let mut s = self.lock();
        s.calls.push(call);
        if !s.online {
            return Err(MockError::Offline);
        }
        if s.fail_next > 0 {
            s.fail_next -= 1;
            return Err(MockError::Injected);
        }
        Ok(s)
    }

    /// A poisoned mutex means an earlier test assertion panicked; recovering keeps the
    /// original failure visible instead of burying it under a poison panic.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cases::synthetic_value_rows;
    use crate::forms::synthetic_form;
    use medatat_core::value::Value;
    use medatat_core::wire::ValueChange;

    fn seeded() -> (CaseId, MockTransport, Vec<FieldId>) {
        let form = synthetic_form(20);
        let case_id = CaseId::new();
        let rows = synthetic_value_rows(&form, 1, CaseRev(1));
        let ids = rows.iter().map(|r| r.field_id).collect();
        (
            case_id,
            MockTransport::new().with_values(case_id, rows),
            ids,
        )
    }

    #[test]
    fn scripted_values_come_back() {
        let (case_id, t, _) = seeded();
        let page = t.get_values(case_id, CaseRev::ZERO).unwrap();
        assert_eq!(page.case_id, case_id);
        assert_eq!(page.rev, CaseRev(1));
        assert_eq!(page.values.len(), 20);
    }

    #[test]
    fn since_rev_filters_out_unchanged_rows() {
        let (case_id, t, _) = seeded();
        let page = t.get_values(case_id, CaseRev(1)).unwrap();
        assert!(page.values.is_empty(), "nothing changed after rev 1");
    }

    #[test]
    fn offline_is_reported_and_recovers() {
        let (case_id, t, _) = seeded();
        t.go_offline();
        assert!(matches!(
            t.get_values(case_id, CaseRev::ZERO),
            Err(MockError::Offline)
        ));
        t.go_online();
        assert!(t.get_values(case_id, CaseRev::ZERO).is_ok());
    }

    #[test]
    fn injected_failures_are_consumed_one_per_call() {
        let (case_id, t, _) = seeded();
        t.fail_next(2);
        for attempt in 0..2 {
            assert!(
                matches!(
                    t.get_values(case_id, CaseRev::ZERO),
                    Err(MockError::Injected)
                ),
                "attempt {attempt} should have been injected"
            );
        }
        assert!(t.get_values(case_id, CaseRev::ZERO).is_ok());
    }

    #[test]
    fn failed_calls_still_appear_in_the_log() {
        let (case_id, t, _) = seeded();
        t.fail_next(1);
        let _ = t.get_values(case_id, CaseRev::ZERO);
        assert_eq!(t.call_count(), 1);
        assert!(t.calls()[0].starts_with("get_values("));
    }

    #[test]
    fn a_write_bumps_the_rev_and_stores_the_value() {
        let (case_id, t, ids) = seeded();
        let resp = t
            .put_values(
                case_id,
                PutValuesReq {
                    base_rev: CaseRev(1),
                    changes: vec![ValueChange {
                        field_id: ids[0],
                        value: Value::Text("written".into()),
                    }],
                },
            )
            .unwrap();

        match resp {
            PutValuesResp::Applied { rev, applied } => {
                assert_eq!(rev, CaseRev(2));
                assert_eq!(applied, vec![ids[0]]);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(t.rev(case_id), CaseRev(2));
        let stored = t.stored_values(case_id);
        let row = stored.iter().find(|r| r.field_id == ids[0]).unwrap();
        assert_eq!(row.value, Value::Text("written".into()));
    }

    #[test]
    fn a_stale_base_rev_conflicts_on_the_same_field_only() {
        let (case_id, t, ids) = seeded();
        // Someone else writes field 0, taking the case to rev 2.
        t.put_values(
            case_id,
            PutValuesReq {
                base_rev: CaseRev(1),
                changes: vec![ValueChange {
                    field_id: ids[0],
                    value: Value::Text("theirs".into()),
                }],
            },
        )
        .unwrap();

        // A stale write to the same field conflicts.
        let same = t
            .put_values(
                case_id,
                PutValuesReq {
                    base_rev: CaseRev(1),
                    changes: vec![ValueChange {
                        field_id: ids[0],
                        value: Value::Text("mine".into()),
                    }],
                },
            )
            .unwrap();
        assert!(matches!(same, PutValuesResp::Conflict { .. }));

        // An equally stale write to a *different* field still applies.
        let other = t
            .put_values(
                case_id,
                PutValuesReq {
                    base_rev: CaseRev(1),
                    changes: vec![ValueChange {
                        field_id: ids[1],
                        value: Value::Text("mine".into()),
                    }],
                },
            )
            .unwrap();
        assert!(matches!(other, PutValuesResp::Applied { .. }));
    }

    #[test]
    fn an_unknown_case_is_not_found() {
        let t = MockTransport::new();
        let missing = CaseId::new();
        assert!(matches!(
            t.get_values(missing, CaseRev::ZERO),
            Err(MockError::NotFound(id)) if id == missing
        ));
    }
}
