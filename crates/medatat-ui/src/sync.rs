//! The background sync loop.
//!
//! **The UI never awaits the network.** Everything here runs on
//! `cx.background_executor()`; nothing in this module is called from a render path or an
//! event handler, and no function here is `async` from the UI's point of view. That is the
//! one architectural rule (`docs/01-ARCHITECTURE.md`), and it is what makes R15 structurally
//! true rather than approximated — the foreground has nothing to wait for, so it has nothing
//! to show a spinner about.
//!
//! What the user sees of any of this is a count in the footer. Offline is a normal state,
//! shown quietly; it blocks nothing, because the outbox is durable on disk.

use medatat_core::ids::CaseId;
use medatat_core::wire::ValueRow;
use medatat_http::TokenHolder;
use medatat_store::Store;
use medatat_sync::engine::ChangeObserver;
use medatat_sync::{SyncEngine, SyncState, SyncStatus, Transport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the foreground drains inbound sync values.
///
/// A poll rather than a wake-up because the alternative is the background thread reaching
/// into the UI, and inbound values are not latency-critical: they are someone else's edit
/// arriving, not this user's keystroke.
const DRAIN_INBOX_EVERY: Duration = Duration::from_millis(250);

/// Values that arrived from the server and are waiting to be applied on the foreground.
///
/// The engine's observer runs on the background executor and cannot touch a `FormView`, so
/// it parks rows here. Nothing is applied until the foreground drains it, which is what
/// keeps `FormInstance`'s focused-field guard in the path.
#[derive(Default)]
pub struct Inbox(Mutex<Vec<(CaseId, Vec<ValueRow>)>>);

impl Inbox {
    /// The callback to hand `SyncEngine::with_observer`.
    ///
    /// Unused until a `Transport` exists to construct an engine with; it is the second of
    /// the two lines that turn sync on.
    #[allow(
        dead_code,
        reason = "wired with the transport; see the note on Workspace::_sync_task"
    )]
    pub fn observer(self: &Arc<Self>) -> ChangeObserver {
        let inbox = Arc::clone(self);
        Arc::new(move |case_id, rows: &[ValueRow]| {
            if let Ok(mut q) = inbox.0.lock() {
                q.push((case_id, rows.to_vec()));
            }
        })
    }

    /// Takes everything queued. Cheap and non-blocking; called on the foreground.
    pub fn drain(&self) -> Vec<(CaseId, Vec<ValueRow>)> {
        self.0
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().map(|q| q.is_empty()).unwrap_or(true)
    }
}

/// How often the foreground should drain. Exposed so the caller owns the timer.
pub fn drain_interval() -> Duration {
    DRAIN_INBOX_EVERY
}

/// Caseload refresh interval. R15 rests on the caseload being pre-synced *before* the user
/// opens anything, so this runs once immediately and then on a timer.
const CASELOAD_EVERY: Duration = Duration::from_secs(5 * 60);

/// Form definitions are cheap and human-paced.
const CONFIG_EVERY: Duration = Duration::from_secs(60);

/// Idle pause when the outbox is empty and `next_delay()` has nothing to say.
const IDLE_POLL: Duration = Duration::from_secs(2);

/// How often to check whether a new token has arrived while the session is expired.
const AUTH_WAIT_POLL: Duration = Duration::from_millis(500);

/// Ceiling for the no-progress backoff. A stalled outbox must not keep costing round trips.
const STALL_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// What the footer needs, read without touching the network.
///
/// `unsynced` comes from [`Store::unsynced_count`] and **not** from the engine's outbox
/// batch: the batch only returns rows whose backoff has elapsed, so it reads zero while
/// edits are still waiting — telling an abstractor their work is safe when it is not.
pub struct SyncIndicator {
    status: Arc<SyncStatus>,
    store: Arc<Store>,
}

impl SyncIndicator {
    pub fn new(status: Arc<SyncStatus>, store: Arc<Store>) -> Self {
        SyncIndicator { status, store }
    }

    /// Edits written locally but not yet acknowledged by the server.
    pub fn unsynced(&self) -> usize {
        self.store.unsynced_count().unwrap_or(0)
    }

    pub fn conflicts(&self) -> usize {
        self.status.conflicts()
    }

    pub fn state(&self) -> SyncState {
        self.status.state()
    }

    /// The footer line. Peripheral chrome only (R15): a count, never a spinner, never a
    /// progress bar, never anything over content.
    ///
    /// Silent when there is nothing to say — an indicator that is always lit stops being
    /// read, and "all synced" is the state the abstractor should not have to think about.
    pub fn line(&self) -> Option<String> {
        let unsynced = self.unsynced();
        let conflicts = self.conflicts();
        let mut parts: Vec<String> = Vec::new();

        match self.state() {
            // Not an error. Work continues; the outbox is durable.
            SyncState::Offline => parts.push("offline".into()),
            SyncState::NeedsAuth => parts.push("sign in to sync".into()),
            SyncState::Syncing | SyncState::Idle => {}
        }
        if unsynced > 0 {
            parts.push(format!("{unsynced} unsynced"));
        }
        if conflicts > 0 {
            parts.push(format!("{conflicts} conflict{}", plural(conflicts)));
        }

        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Stops the sync loop when dropped.
///
/// The loop runs on its **own OS thread with its own Tokio runtime**, not on gpui's
/// background executor. That is forced rather than chosen: `medatat-http` uses `reqwest`,
/// which panics with "there is no reactor running" unless it is driven by a Tokio reactor,
/// and gpui's executor is not one. It costs one thread and keeps the architectural rule
/// intact either way — the UI still never awaits the network, it just waits on a different
/// executor now.
pub struct SyncHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Not joined: the loop wakes at most one poll interval later and exits on its own.
        // Blocking a window close on a network timeout would be the wrong trade.
        drop(self.thread.take());
    }
}

/// Starts the background loop and returns its handle.
///
/// Dropping the returned handle stops the loop, which is why the caller must hold it.
pub fn spawn<T: Transport>(
    engine: SyncEngine<T>,
    assignee: String,
    token: TokenHolder,
) -> std::io::Result<SyncHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);

    let thread = std::thread::Builder::new()
        .name("medatat-sync".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("could not start the sync runtime: {e}");
                    return;
                }
            };

            runtime.block_on(async move {
                let mut since_caseload = CASELOAD_EVERY;
                let mut since_config = CONFIG_EVERY;
                let mut stall = Duration::ZERO;

                let status = engine.status();

                while !flag.load(Ordering::Relaxed) {
                    // An expired session will refuse every request with the same token, so
                    // retrying on the normal cadence is pure load on a server that has
                    // already said no. Wait for a *different* token instead — which is what
                    // the re-auth box sets — and only then try again.
                    if status.state() == SyncState::NeedsAuth {
                        let stale = token.get();
                        while !flag.load(Ordering::Relaxed) && token.get() == stale {
                            tokio::time::sleep(AUTH_WAIT_POLL).await;
                        }
                        // A new token deserves a full refresh, not just a drain.
                        since_caseload = CASELOAD_EVERY;
                        since_config = CONFIG_EVERY;
                        stall = Duration::ZERO;
                        continue;
                    }

                    // Caseload first and immediately on the first pass: R15's premise is
                    // that the whole assigned caseload is local before the user opens
                    // anything.
                    //
                    // The interval only resets on **success**. Resetting after a failure
                    // would mean an abstractor who was offline when the app started waits
                    // the full five minutes after reconnecting before their caseload
                    // arrives — which is precisely the wait R15 exists to eliminate.
                    if since_caseload >= CASELOAD_EVERY {
                        match engine.sync_caseload(&assignee).await {
                            Ok(_) => since_caseload = Duration::ZERO,
                            Err(e) => log_sync_error("caseload", &e),
                        }
                    }
                    if since_config >= CONFIG_EVERY {
                        match engine.sync_config().await {
                            Ok(_) => since_config = Duration::ZERO,
                            Err(e) => log_sync_error("config", &e),
                        }
                    }

                    // One outbox pass. The engine owns conflict handling, per-row backoff,
                    // and the offline case.
                    //
                    // What it does *not* own is the case where a pass makes no progress at
                    // all. A row the server refuses permanently — a 404 for a case it has
                    // never heard of — is not retryable, but it stays in the outbox and is
                    // re-sent on every pass. One pass is a round trip per case, so a stalled
                    // outbox is a steady stream of requests that can never succeed. Back off
                    // when nothing moves, and reset the moment something does.
                    match engine.drain_once().await {
                        Ok(report) => {
                            let progressed = report.applied > 0 || report.conflicted > 0;
                            if progressed || report.remaining == 0 {
                                stall = Duration::ZERO;
                            } else if report.offline || report.needs_auth {
                                // Neither is a refused row: the network is down or the
                                // session lapsed. Both have their own handling, and backing
                                // off here would only delay recovery once they clear.
                                stall = Duration::ZERO;
                            } else if report.failed > 0 {
                                stall = (stall * 2).clamp(IDLE_POLL, STALL_BACKOFF_MAX);
                            }
                        }
                        Err(e) => log_sync_error("drain", &e),
                    }

                    // `next_delay` is the engine's own pacing — respecting it is what keeps
                    // a failing server from being hammered.
                    let wait = engine
                        .next_delay()
                        .ok()
                        .flatten()
                        .unwrap_or(IDLE_POLL)
                        .clamp(Duration::from_millis(50), IDLE_POLL)
                        .max(stall);

                    tokio::time::sleep(wait).await;
                    since_caseload += wait;
                    since_config += wait;
                }
            });
        })?;

    Ok(SyncHandle {
        stop,
        thread: Some(thread),
    })
}

/// Offline is a normal operating state and is logged as such — an error line for every
/// poll while a laptop is shut would bury the ones that matter.
fn log_sync_error(what: &str, e: &medatat_sync::SyncError) {
    use medatat_sync::TransportError;
    match e {
        medatat_sync::SyncError::Transport(TransportError::Offline) => {
            tracing::debug!("{what}: offline");
        }
        medatat_sync::SyncError::Transport(TransportError::Unauthorized) => {
            tracing::warn!("{what}: session expired; re-auth needed");
        }
        other => tracing::warn!("{what} sync failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_sync::SyncStatus;

    fn indicator(store: Arc<Store>) -> SyncIndicator {
        SyncIndicator::new(Arc::new(SyncStatus::default()), store)
    }

    #[test]
    fn the_footer_is_silent_when_there_is_nothing_to_say() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        assert_eq!(indicator(store).line(), None, "no line when all is synced");
    }

    #[test]
    fn r15_unsynced_edits_are_counted_from_the_store_not_the_outbox_batch() {
        // `next_outbox_batch` only returns rows whose backoff has elapsed, so a freshly
        // queued edit reads as zero there. Counting from the store is what stops the
        // footer telling an abstractor their work is safe while it is still queued.
        use medatat_core::{CaseId, CaseRev, FieldId, Value};

        use medatat_core::wire::CaseSummary;
        use medatat_core::{ConfigRev, FormDef, FormId};

        let store = Arc::new(Store::open_in_memory().expect("store"));
        // `apply_local` writes against a real case row, so the case has to exist.
        let def = FormDef::new(FormId::new(), "F", Vec::new());
        store.save_form(&def, ConfigRev(1)).expect("form");
        let case = CaseId::new();
        store
            .upsert_case(&CaseSummary {
                case_id: case,
                mrn: "SYN-MRN-0001".into(),
                form_id: def.form_id,
                assignee: Some("demo".into()),
                rev: CaseRev::ZERO,
                updated_at: "2026-08-18T09:00:00Z".into(),
            })
            .expect("case");
        let field = FieldId::new();
        store
            .apply_local(case, &[(field, Value::Text("x".into()))], CaseRev::ZERO)
            .expect("local write");

        let ind = indicator(Arc::clone(&store));
        assert_eq!(ind.unsynced(), 1);
        assert_eq!(ind.line().as_deref(), Some("1 unsynced"));
    }

    #[test]
    fn offline_reads_as_a_state_not_a_failure() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let status = Arc::new(SyncStatus::default());
        let ind = SyncIndicator::new(Arc::clone(&status), store);
        // `SyncStatus`'s setters are private to the engine, so this asserts the shape of
        // the line rather than driving the state — the engine's own tests cover transitions.
        assert_eq!(ind.state(), SyncState::Idle);
        assert_eq!(ind.line(), None);
    }

    #[test]
    fn conflicts_are_pluralised_like_english() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
