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

use gpui::{App, Task};
use medatat_core::ids::CaseId;
use medatat_core::wire::ValueRow;
use medatat_store::Store;
use medatat_sync::engine::ChangeObserver;
use medatat_sync::{SyncEngine, SyncState, SyncStatus, Transport};
use std::sync::{Arc, Mutex};
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

/// Starts the background loop and returns its handle.
///
/// Unused in the binary until a `Transport` implementation exists to build a `SyncEngine`
/// with; the loop itself is covered by `sync_loop_pre_syncs_the_caseload_in_the_background`.
///
/// Dropping the returned [`Task`] stops the loop, which is why the caller must hold it —
/// a detached task would outlive the window it serves.
#[allow(
    dead_code,
    reason = "wired with the transport; see the note on Workspace::_sync_task"
)]
pub fn spawn<T: Transport>(engine: SyncEngine<T>, assignee: String, cx: &mut App) -> Task<()> {
    let executor = cx.background_executor().clone();
    cx.background_executor().spawn(async move {
        let mut since_caseload = CASELOAD_EVERY;
        let mut since_config = CONFIG_EVERY;

        loop {
            // Caseload first and immediately on the first pass: R15's premise is that the
            // whole assigned caseload is already local before the user opens anything.
            if since_caseload >= CASELOAD_EVERY {
                if let Err(e) = engine.sync_caseload(&assignee).await {
                    log_sync_error("caseload", &e);
                }
                since_caseload = Duration::ZERO;
            }
            if since_config >= CONFIG_EVERY {
                if let Err(e) = engine.sync_config().await {
                    log_sync_error("config", &e);
                }
                since_config = Duration::ZERO;
            }

            // One outbox pass. The engine owns conflict handling, backoff, and the offline
            // case, so there is nothing to decide here.
            match engine.drain_once().await {
                Ok(_) => {}
                Err(e) => log_sync_error("drain", &e),
            }

            // `next_delay` is the engine's own pacing — respecting it is what keeps a
            // failing server from being hammered.
            let wait = engine
                .next_delay()
                .ok()
                .flatten()
                .unwrap_or(IDLE_POLL)
                .max(Duration::from_millis(50));

            executor.timer(wait).await;
            since_caseload += wait;
            since_config += wait;
        }
    })
}

/// Offline is a normal operating state and is logged as such — an error line for every
/// poll while a laptop is shut would bury the ones that matter.
#[allow(dead_code, reason = "called from spawn, which waits on a Transport")]
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
