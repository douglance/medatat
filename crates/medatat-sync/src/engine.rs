//! The drain loop.
//!
//! Runs entirely on a background executor. The UI never awaits it, never blocks on its
//! failure, and never renders its state as anything more than peripheral chrome.

use crate::backoff::delay_for;
use crate::transport::{Transport, TransportError};
use medatat_core::ids::{CaseId, CaseRev, ConfigRev, FieldId};
use medatat_core::wire::{CaseQuery, PutValuesReq, PutValuesResp, ValueChange, ValueRow};
use medatat_store::{OutboxRow, Store, StoreError};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// What the status indicator shows. Never a spinner — see R15.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Idle,
    Syncing,
    /// Network is unavailable. Work continues; the outbox is durable on disk.
    Offline,
    /// Session expired; the UI must prompt for re-auth without losing form state.
    NeedsAuth,
}

#[derive(Debug, Default)]
pub struct SyncStatus {
    unsynced: AtomicUsize,
    conflicts: AtomicUsize,
    state: AtomicU64,
}

impl SyncStatus {
    pub fn unsynced(&self) -> usize {
        self.unsynced.load(Ordering::Relaxed)
    }
    pub fn conflicts(&self) -> usize {
        self.conflicts.load(Ordering::Relaxed)
    }
    pub fn state(&self) -> SyncState {
        match self.state.load(Ordering::Relaxed) {
            1 => SyncState::Syncing,
            2 => SyncState::Offline,
            3 => SyncState::NeedsAuth,
            _ => SyncState::Idle,
        }
    }
    fn set_state(&self, s: SyncState) {
        self.state.store(
            match s {
                SyncState::Idle => 0,
                SyncState::Syncing => 1,
                SyncState::Offline => 2,
                SyncState::NeedsAuth => 3,
            },
            Ordering::Relaxed,
        );
    }
    fn set_unsynced(&self, n: usize) {
        self.unsynced.store(n, Ordering::Relaxed);
    }
    fn set_conflicts(&self, n: usize) {
        self.conflicts.store(n, Ordering::Relaxed);
    }
}

/// Outcome of one drain pass, so callers can pace themselves and tests can assert.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub applied: usize,
    pub conflicted: usize,
    pub failed: usize,
    /// Permanently refused and parked out of the drain batch — needs a human, not a retry.
    pub rejected: usize,
    pub remaining: usize,
    pub needs_auth: bool,
    pub offline: bool,
}

/// Notified whenever inbound values are applied to the local store.
///
/// The engine writes straight to SQLite, so an open `FormView` has no way to learn what
/// changed — and without that it cannot honour the rule that an inbound value must never
/// overwrite the field the user is focused in (`docs/04-SYNC.md`). `FormInstance` has the
/// guard; this is what gives it a caller.
///
/// The alternative — re-reading the case and diffing it against memory on every tick —
/// would be a second implementation of "what changed", and the two would eventually
/// disagree.
pub type ChangeObserver = Arc<dyn Fn(CaseId, &[ValueRow]) + Send + Sync>;

pub struct SyncEngine<T: Transport> {
    store: Arc<Store>,
    transport: T,
    status: Arc<SyncStatus>,
    batch_limit: usize,
    observer: Option<ChangeObserver>,
}

impl<T: Transport> SyncEngine<T> {
    pub fn new(store: Arc<Store>, transport: T) -> Self {
        SyncEngine {
            store,
            transport,
            status: Arc::new(SyncStatus::default()),
            batch_limit: 64,
            observer: None,
        }
    }

    /// Registers a callback fired after inbound values land, so an open view can apply the
    /// focused-field guard. Without one the engine still works; the guard simply has no
    /// caller, which is the state this method exists to end.
    pub fn with_observer(mut self, observer: ChangeObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    fn notify(&self, case_id: CaseId, rows: &[ValueRow]) {
        if let Some(o) = &self.observer
            && !rows.is_empty()
        {
            o(case_id, rows);
        }
    }

    pub fn status(&self) -> Arc<SyncStatus> {
        Arc::clone(&self.status)
    }

    /// One pass over the outbox. Grouped per case so a batch is one round trip and the
    /// server's per-field conflict check sees the whole set at once.
    pub async fn drain_once(&self) -> Result<DrainReport, SyncError> {
        let batch = self.store.next_outbox_batch(self.batch_limit)?;
        let mut report = DrainReport::default();
        if batch.is_empty() {
            let pending = self.store.unsynced_count()?;
            report.remaining = pending;
            self.status.set_unsynced(pending);
            self.status.set_state(if pending == 0 {
                SyncState::Idle
            } else {
                SyncState::Syncing
            });
            return Ok(report);
        }

        self.status.set_state(SyncState::Syncing);
        for (case_id, rows) in group_by_case(batch) {
            match self.push_case(case_id, &rows).await {
                Ok(PushOutcome::Applied(n)) => report.applied += n,
                Ok(PushOutcome::Conflicted(n)) => report.conflicted += n,
                Err(e) => {
                    report.failed += rows.len();
                    match e {
                        TransportError::Offline => {
                            report.offline = true;
                            self.status.set_state(SyncState::Offline);
                        }
                        TransportError::Unauthorized => {
                            report.needs_auth = true;
                            self.status.set_state(SyncState::NeedsAuth);
                        }
                        // A refusal the server will repeat forever is not a transient
                        // failure. Bumping attempts on a 404 grows the backoff but never
                        // removes the row, so a case the server will never accept becomes a
                        // permanent cost and an unsynced count that can never reach zero.
                        //
                        // `is_retryable()` already knows the difference; nothing consulted
                        // it here. Parked rather than dropped: dropping loses the value,
                        // which is the one thing this design refuses.
                        other if !other.is_retryable() => {
                            report.rejected += rows.len();
                            for r in &rows {
                                self.store.reject_outbox(
                                    case_id,
                                    r.field_id,
                                    &other.to_string(),
                                )?;
                            }
                        }
                        other => {
                            for r in &rows {
                                self.store.bump_attempts(
                                    case_id,
                                    r.field_id,
                                    &other.to_string(),
                                )?;
                            }
                        }
                    }
                    if report.offline || report.needs_auth {
                        break; // no point hammering the rest of the batch
                    }
                }
            }
        }

        // Counts rows still waiting out a backoff too — see Store::unsynced_count.
        report.remaining = self.store.unsynced_count()?;
        self.status.set_unsynced(report.remaining);
        if !report.offline && !report.needs_auth {
            self.status.set_state(if report.remaining == 0 {
                SyncState::Idle
            } else {
                SyncState::Syncing
            });
        }
        Ok(report)
    }

    async fn push_case(
        &self,
        case_id: CaseId,
        rows: &[OutboxRow],
    ) -> Result<PushOutcome, TransportError> {
        // All rows for a case share a base_rev in practice; the oldest is the safe choice,
        // because a stale base can only cause a (correct) conflict, never a lost update.
        let base_rev = rows
            .iter()
            .map(|r| r.base_rev)
            .min()
            .unwrap_or(CaseRev::ZERO);
        let changes: Vec<ValueChange> = rows
            .iter()
            .map(|r| ValueChange {
                field_id: r.field_id,
                value: r.value.clone(),
            })
            .collect();

        match self
            .transport
            .put_values(case_id, PutValuesReq { base_rev, changes })
            .await?
        {
            PutValuesResp::Applied { rev, applied } => {
                let n = applied.len();
                // Confirm against the sequence that was actually sent, not the field alone.
                // If the abstractor edited one of these fields while it was in flight, its
                // outbox row now holds a newer value; confirming by field id would delete
                // that row and clear `pending`, leaving the newer value in `field_value`
                // with nothing left to send it. Silent lost update — the one failure class
                // this design exists to rule out.
                let sent: Vec<(FieldId, i64)> = applied
                    .iter()
                    .filter_map(|f| rows.iter().find(|r| r.field_id == *f).map(|r| (*f, r.seq)))
                    .collect();
                self.store
                    .confirm(case_id, &sent, rev)
                    .map_err(|e| TransportError::Malformed(e.to_string()))?;
                Ok(PushOutcome::Applied(n))
            }
            PutValuesResp::Conflict {
                server_rev,
                conflicts,
            } => {
                let n = conflicts.len();
                // Record for the inline strip, take the server's value locally, and drop
                // our outbox rows for those fields so the loop cannot spin forever.
                self.notify(case_id, &conflicts);
                self.store
                    .record_conflicts(case_id, &conflicts)
                    .and_then(|_| {
                        self.store
                            .apply_server_values(case_id, &conflicts, server_rev)
                    })
                    .and_then(|_| {
                        let ids: Vec<FieldId> = conflicts.iter().map(|c| c.field_id).collect();
                        self.store.drop_outbox(case_id, &ids)
                    })
                    .map_err(|e| TransportError::Malformed(e.to_string()))?;
                self.status.set_conflicts(n);
                Ok(PushOutcome::Conflicted(n))
            }
        }
    }

    /// Pulls every case assigned to the user.
    ///
    /// **This is the mechanism behind R15.** The user only ever opens cases that are
    /// already local, so a cache miss is not part of normal operation.
    pub async fn sync_caseload(&self, assignee: &str) -> Result<CaseloadStats, SyncError> {
        let mut stats = CaseloadStats::default();
        let mut cursor = None;

        loop {
            let page = self
                .transport
                .list_cases(CaseQuery {
                    assignee: Some(assignee.to_string()),
                    since: cursor.clone(),
                    limit: Some(200),
                })
                .await?;

            for summary in &page.cases {
                // Read the local revision BEFORE upserting: upsert_case writes the
                // server's rev into the row, so doing it first would make every case look
                // up to date and nothing would ever be pulled.
                let local = self
                    .store
                    .synced_rev(summary.case_id)?
                    .unwrap_or(CaseRev::ZERO);
                self.store.upsert_case(summary)?;
                if summary.rev <= local {
                    stats.up_to_date += 1;
                    continue;
                }
                let vp = self.transport.get_values(summary.case_id, local).await?;
                self.store
                    .apply_server_values(summary.case_id, &vp.values, vp.rev)?;
                // Tell any open view what arrived, so it can defer anything landing on the
                // field the user is focused in rather than rewriting text under the caret.
                self.notify(summary.case_id, &vp.values);
                stats.fetched += 1;
                stats.values += vp.values.len();
            }

            stats.cases += page.cases.len();
            if !page.has_more {
                break;
            }
            cursor = page.cursor;
            if cursor.is_none() {
                break; // defensive: a server claiming has_more without a cursor
            }
        }
        Ok(stats)
    }

    /// Refreshes form definitions. Cheap and human-paced, so it polls wholesale.
    pub async fn sync_config(&self) -> Result<Option<ConfigRev>, SyncError> {
        let since = self
            .store
            .sync_state("config_rev")?
            .and_then(|s| s.parse::<i64>().ok())
            .map(ConfigRev)
            .unwrap_or(ConfigRev(0));

        let Some(delta) = self.transport.config(since).await? else {
            return Ok(None);
        };
        // Fields before forms. A form references fields, and `delta.fields` is the only
        // record of one that sits in no form — the builder's Unplaced drawer reads it, and
        // without it a coordinator who unplaces a field loses the route back to its stored
        // values on the next restart. The values themselves are never at risk; they stay in
        // `field_value` keyed by `field_id`.
        if !delta.fields.is_empty() {
            self.store.save_fields(&delta.fields)?;
        }
        for form in &delta.forms {
            self.store.save_form(form, delta.config_rev)?;
        }
        self.store
            .set_sync_state("config_rev", &delta.config_rev.0.to_string())?;
        Ok(Some(delta.config_rev))
    }

    /// How long to wait before retrying, given the most-failed row in the outbox.
    pub fn next_delay(&self) -> Result<Option<Duration>, SyncError> {
        let batch = self.store.next_outbox_batch(self.batch_limit)?;
        Ok(batch
            .iter()
            .map(|r| {
                delay_for(
                    r.attempts.clamp(0, u32::MAX as i64) as u32,
                    r.field_id.0.as_u128() as u64,
                )
            })
            .max())
    }
}

enum PushOutcome {
    Applied(usize),
    Conflicted(usize),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CaseloadStats {
    pub cases: usize,
    pub fetched: usize,
    pub up_to_date: usize,
    pub values: usize,
}

/// Groups in worklist order so the cases at the top of the user's screen land first.
fn group_by_case(rows: Vec<OutboxRow>) -> Vec<(CaseId, Vec<OutboxRow>)> {
    let mut map: BTreeMap<String, (CaseId, Vec<OutboxRow>)> = BTreeMap::new();
    for r in rows {
        map.entry(r.case_id.to_string())
            .or_insert_with(|| (r.case_id, Vec::new()))
            .1
            .push(r);
    }
    map.into_values().collect()
}
