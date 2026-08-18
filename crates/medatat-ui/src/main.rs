//! medatat desktop client.
//!
//! The one architectural rule: **the UI never awaits the network.** Reads and writes hit
//! local SQLite synchronously; sync runs on the background executor. That is what gives
//! R13/R14 their margin and makes R15 structurally true rather than approximated.
//! See `docs/adr/0002-encrypted-local-sqlite.md`.

mod builder;
mod form;
#[cfg(test)]
mod gui_tests;
mod mode;
mod sync;
mod widgets;
mod worklist;

use anyhow::Result;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString, Styled as _, Window,
    WindowBounds, WindowOptions, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::{CaseId, ConfigRev, FormDef};
use medatat_store::Store;
use std::rc::Rc;
use std::sync::Arc;

use builder::BuilderView;
use form::FormView;
use worklist::WorklistView;

/// Where the Worker lives. Overridable so a developer can point at a local `wrangler dev`
/// without a rebuild.
const DEFAULT_API: &str = "http://localhost:8787";

/// Until there is a login screen, every case belongs to this assignee. The real value comes
/// from the session token; see `docs/05-UI-SPEC.md#login`.
const DEMO_ASSIGNEE: &str = "demo";

/// How many cases the worklist reads. The demo seeds 50, which is M6's demo target.
const WORKLIST_LIMIT: usize = 500;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "medatat=info,warn".into()),
        )
        .init();

    // Under `phi`, core dumps would otherwise be able to spill decrypted values to disk.
    #[cfg(all(feature = "phi", unix))]
    disable_core_dumps();

    let store = Arc::new(open_store()?);

    // `gpui_platform::application()` rather than `Application::new()`: current gpui splits
    // platform selection into its own crate and `Application` no longer has a `new`.
    let app = gpui_platform::application();
    app.run(move |cx: &mut App| {
        widgets::init(cx);
        cx.activate(true);

        let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("medatat".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = widgets::open_root_window(
            options,
            |window, cx| cx.new(|cx| Workspace::new(Arc::clone(&store), window, cx)),
            cx,
        );
        match result {
            Ok(()) => tracing::info!("window open"),
            Err(e) => {
                tracing::error!("could not open a window: {e}");
                cx.quit();
            }
        }
    });
    Ok(())
}

/// Opens the local store, falling back to memory only when encryption is requested but no
/// keychain exists. Writing an unencrypted database instead would silently defeat the
/// point — see `docs/12-PHI-READINESS.md`.
fn open_store() -> Result<Store> {
    let dir = data_dir();
    // SQLite will not create a missing parent, so on a first run every open fails and the
    // whole session silently becomes in-memory. Create the directory first.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not create {}: {e}", dir.display());
    }
    let path = dir.join("medatat.db");
    match Store::open(&path) {
        Ok(s) => Ok(s),
        Err(e) => {
            tracing::warn!("local store unavailable ({e}); running in memory for this session");
            Ok(Store::open_in_memory()?)
        }
    }
}

fn data_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    #[cfg(target_os = "macos")]
    let base = std::path::PathBuf::from(home).join("Library/Application Support");
    #[cfg(target_os = "windows")]
    let base = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or(home));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(home).join(".local/share"));
    base.join("medatat")
}

#[cfg(all(feature = "phi", unix))]
fn disable_core_dumps() {
    // SAFETY: setrlimit with a valid resource and a zeroed limit is always sound.
    unsafe {
        let lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &lim);
    }
}

/// The application shell. Owns the current screen and the persistent status chrome.
struct Workspace {
    store: Arc<Store>,
    worklist: Entity<WorklistView>,
    /// The open case editor, or `None` while the worklist is showing.
    form: Option<Entity<FormView>>,
    /// The form builder (R4). Takes the screen when open.
    builder: Option<Entity<BuilderView>>,
    def: Option<Arc<FormDef>>,
    message: SharedString,
    focus: FocusHandle,
    /// Peripheral sync chrome (R15). Reads the local outbox, so it is meaningful even
    /// before a `Transport` exists — an abstractor's own unsynced edits are counted from
    /// the moment they are written.
    sync: sync::SyncIndicator,
    /// The sync loop's handle. Dropping it stops the loop, so `Workspace` holds it.
    /// `None` only if the transport or its runtime thread could not be started.
    _sync_task: Option<sync::SyncHandle>,
    /// The session token, replaceable in place. Re-auth after an idle timeout must not
    /// tear down the engine, because that would take the abstractor's in-memory work with
    /// it — the failure this whole design exists to prevent.
    token: medatat_http::TokenHolder,
    /// The re-auth box, present only while the session is expired. Deliberately a footer
    /// row rather than a modal: blocking the form would cost the abstractor the work still
    /// sitting in memory, which is the exact thing `TokenHolder` exists to protect.
    reauth: Option<widgets::LineInput>,
    _reauth_sub: Option<gpui::Subscription>,
    /// Inbound values parked by the background loop, drained on the foreground so they go
    /// through `FormView`'s focused-field guard rather than round the side of it.
    inbox: Arc<sync::Inbox>,
    _drain_task: gpui::Task<()>,
}

impl Workspace {
    fn new(store: Arc<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // On first run there is nothing to show, so seed a demo form and caseload rather
        // than an empty window. Both are replaced by config and caseload sync.
        let (def, message) = match Self::load_or_seed(&store) {
            Ok(def) => {
                let msg = SharedString::from(format!("{} fields · ⌘B builder", def.field_count()));
                (Some(def), msg)
            }
            Err(e) => (None, SharedString::from(format!("no form available: {e}"))),
        };

        let store_for_sync = Arc::clone(&store);

        // Inbound sync values land here and are applied on the foreground. A poll, not a
        // wake-up: the background executor must never reach into a view.
        let inbox = Arc::new(sync::Inbox::default());
        let mut status = Arc::new(medatat_sync::SyncStatus::default());
        let drain_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(sync::drain_interval()).await;
                if this
                    .update(cx, |w: &mut Workspace, cx| w.drain_inbox(cx))
                    .is_err()
                {
                    return; // the workspace is gone
                }
            }
        });

        // Sync runs on the background executor from here on. Nothing below this line is
        // awaited by the UI; the foreground only ever reads the store.
        let token = medatat_http::TokenHolder::new(std::env::var("MEDATAT_TOKEN").ok());
        let api = std::env::var("MEDATAT_API").unwrap_or_else(|_| DEFAULT_API.to_string());
        let sync_task = match medatat_http::HttpTransport::new(&api, token.clone()) {
            Ok(transport) => {
                let engine = medatat_sync::SyncEngine::new(Arc::clone(&store), transport)
                    .with_observer(inbox.observer());
                status = engine.status();
                match sync::spawn(engine, DEMO_ASSIGNEE.to_string(), token.clone()) {
                    Ok(handle) => {
                        tracing::info!("syncing against {api}");
                        Some(handle)
                    }
                    Err(e) => {
                        tracing::error!("could not start the sync thread ({e}); local-only");
                        None
                    }
                }
            }
            Err(e) => {
                // Local editing still works; only the network half is missing.
                tracing::error!("no transport ({e}); running local-only");
                None
            }
        };

        let on_open = Self::on_open_case(cx);
        let s = Arc::clone(&store);
        let worklist =
            cx.new(|cx| WorklistView::new(&s, DEMO_ASSIGNEE, WORKLIST_LIMIT, on_open, window, cx));

        // Take focus on open, for the same reason `FormView` does: without it the shell's
        // own element is never focused, so `Cmd-J`/`Cmd-K`/`Cmd-B` reach nothing until the
        // user happens to click. A keyboard-driven app that ignores the keyboard until it
        // is clicked is not keyboard-driven.
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        Workspace {
            store,
            worklist,
            form: None,
            builder: None,
            def,
            message,
            focus,
            sync: sync::SyncIndicator::new(status, Arc::clone(&store_for_sync)),
            // No `Transport` implementation exists yet, so there is no engine to run. The
            // loop itself is written and tested (`sync::spawn`); this is the one line that
            // starts it once a transport lands.
            _sync_task: sync_task,
            token,
            reauth: None,
            _reauth_sub: None,
            inbox,
            _drain_task: drain_task,
        }
    }

    /// Shows or hides the re-auth box to match the sync state.
    ///
    /// Nothing is torn down when the session expires: the engine keeps running, the open
    /// case keeps its in-memory state, and a new token simply replaces the old one in place.
    fn sync_reauth_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let needed = self.sync.state() == medatat_sync::SyncState::NeedsAuth;
        if needed == self.reauth.is_some() {
            return;
        }
        if !needed {
            self.reauth = None;
            self._reauth_sub = None;
            cx.notify();
            return;
        }
        let input = widgets::LineInput::new("Paste session token", window, cx);
        let token = self.token.clone();
        self._reauth_sub =
            Some(
                input.subscribe_commit(window, cx, move |this: &mut Workspace, text, _, cx| {
                    let t = text.trim();
                    if t.is_empty() {
                        return;
                    }
                    // The engine picks this up on its next request; nothing restarts.
                    token.set(Some(t.to_string()));
                    this.reauth = None;
                    this._reauth_sub = None;
                    cx.notify();
                }),
            );
        self.reauth = Some(input);
        cx.notify();
    }

    /// Applies whatever the background loop parked, to the open case only.
    ///
    /// Every row goes through `FormView::receive_remote`, which defers anything for the
    /// field the user is currently in until they leave it (`docs/04-SYNC.md`).
    fn drain_inbox(&mut self, cx: &mut Context<Self>) {
        if self.inbox.is_empty() {
            return;
        }
        let batches = self.inbox.drain();
        let Some(form) = self.form.clone() else {
            // No case open: the values are already in SQLite and will be read on open.
            return;
        };
        let open_case = form.read(cx).case_id();
        for (case_id, rows) in batches {
            if case_id != open_case {
                continue;
            }
            form.update(cx, |v, cx| v.receive_remote_rows(&rows, cx));
        }
    }

    /// Opens the form builder on the current definition.
    ///
    /// The spec gates the builder on `role = admin` and says the UI is not the enforcement
    /// point — the API returns 403 regardless. There is no session yet, so there is no role
    /// to check; this opens for anyone until login exists.
    fn open_builder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(def) = self.def.clone() else { return };
        self.flush_open_case(cx);
        let store = Arc::clone(&self.store);
        self.builder = Some(cx.new(|cx| BuilderView::new(store, def, window, cx)));
        cx.notify();
    }

    /// The callback the worklist calls when a row is clicked.
    fn on_open_case(cx: &mut Context<Self>) -> worklist::OnOpenCase {
        let this = cx.entity().downgrade();
        Rc::new(move |case_id, window, cx| {
            let _ = this.update(cx, |w: &mut Workspace, cx| {
                w.open_case(case_id, window, cx);
            });
        })
    }

    /// Opens a case, flushing whatever was open first.
    ///
    /// The flush is not a nicety: navigating away is exactly the moment an abstractor
    /// expects their work to be safe, and the local write costs microseconds (R14).
    fn open_case(&mut self, case_id: CaseId, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_open_case(cx);

        let row = match self.store.case(case_id) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("case {case_id} unavailable: {e}");
                return;
            }
        };
        let def = match self.store.load_form(row.form_id) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("form for case {case_id} unavailable: {e}");
                return;
            }
        };
        let values = self.store.load_case_values(case_id).unwrap_or_default();
        let store = Arc::clone(&self.store);
        self.form =
            Some(cx.new(|cx| FormView::new(def, case_id, row.rev, values, store, window, cx)));
        cx.notify();
    }

    fn flush_open_case(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = &self.form {
            f.update(cx, |view, _| view.flush());
        }
    }

    /// Back to the worklist, flushing on the way out.
    fn close_case(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_open_case(cx);
        // PHI hygiene: `Value` zeroizes on drop, but the same text also lives in
        // `gpui-component`'s editing state, which zeroize cannot reach. Blank the widgets
        // before dropping the view or the contents outlive the case in a live buffer.
        #[cfg(feature = "phi")]
        if let Some(f) = &self.form {
            f.update(cx, |v, cx| v.clear_inputs(window, cx));
        }
        self.form = None;
        self.worklist.update(cx, |w, cx| w.focus(window, cx));
        cx.notify();
    }

    /// `Cmd/Ctrl-J` / `Cmd/Ctrl-K` and `Escape`. Capture phase, because the screens below
    /// bind these keys in their own contexts.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mods = &ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "b" if mods.secondary() => {
                if self.builder.is_some() {
                    self.builder = None;
                } else {
                    self.open_builder(window, cx);
                }
                cx.notify();
                cx.stop_propagation();
            }
            // Next / previous case. The step flushes through `open_case`.
            "j" | "k" if mods.secondary() => {
                let delta = if ev.keystroke.key == "j" { 1 } else { -1 };
                let next = self.worklist.update(cx, |w, cx| w.step(delta, cx));
                if let Some(case_id) = next {
                    self.open_case(case_id, window, cx);
                }
                cx.stop_propagation();
            }
            "escape" if self.builder.is_some() => {
                self.builder = None;
                cx.notify();
                cx.stop_propagation();
            }
            // Escape leaves the case — unless the field palette has the keyboard, in which
            // case it is the palette's to close.
            "escape" if self.form.is_some() => {
                let palette = self
                    .form
                    .as_ref()
                    .is_some_and(|f| f.read(cx).palette_is_open());
                if !palette {
                    self.close_case(window, cx);
                    cx.stop_propagation();
                }
            }
            _ => {}
        }
    }

    fn load_or_seed(store: &Store) -> Result<Arc<FormDef>> {
        let def = match store.load_all_forms()?.into_iter().next() {
            Some(first) => first,
            None => {
                let def = demo_form();
                // Fields before the form, and always both: the `field` table is what
                // outlives placement, so a form saved without it leaves the builder's
                // Unplaced drawer with nothing to find.
                let fields: Vec<_> = def.iter_fields().map(|f| (*f.field).clone()).collect();
                store.save_fields(&fields)?;
                store.save_form(&def, ConfigRev(1))?;
                Arc::new(def)
            }
        };
        if store.worklist(DEMO_ASSIGNEE, 1)?.is_empty() {
            seed_cases(store, &def)?;
        }
        Ok(def)
    }
}

/// Seeds a demo caseload so the worklist is not empty before caseload sync exists.
///
/// Every value comes from `medatat-testkit` — rule 10 of AGENTS.md says synthetic data has
/// exactly one source, and the moment demo data has a second one the two drift. Behind the
/// `demo` feature so the scaffolding is one flag away from gone.
#[cfg(feature = "demo")]
fn seed_cases(store: &Store, def: &Arc<FormDef>) -> Result<()> {
    use medatat_core::CaseRev;
    use medatat_core::wire::{CaseSummary, ValueRow};
    use medatat_testkit::{synthetic_case, synthetic_mrn};

    for n in 1..=50u64 {
        let case_id = CaseId::new();
        store.upsert_case(&CaseSummary {
            case_id,
            // `SYN-MRN-…`, deliberately unmistakable for a real institution's format.
            mrn: synthetic_mrn(n),
            form_id: def.form_id,
            assignee: Some(DEMO_ASSIGNEE.into()),
            rev: CaseRev::ZERO,
            updated_at: format!("2026-08-{:02}T09:{:02}:00Z", (n % 28) + 1, n % 60),
        })?;
        // Values too, so the worklist's `filled / total` column shows a real spread rather
        // than 50 identical zeroes.
        //
        // Seeded as **server** values, not local edits. `apply_local` would queue an outbox
        // row per value, and demo cases do not exist on any server — so against a real
        // Worker the loop would spend forever pushing rows that can only ever 404.
        if n % 3 != 0 {
            let rows: Vec<ValueRow> = synthetic_case(def, n)
                .into_iter()
                .take(if n % 2 == 0 { def.field_count() } else { 2 })
                .map(|(field_id, value)| ValueRow {
                    field_id,
                    value,
                    rev: CaseRev::ZERO,
                    updated_by: None,
                    updated_at: None,
                })
                .collect();
            store.apply_server_values(case_id, &rows, CaseRev::ZERO)?;
        }
    }
    Ok(())
}

#[cfg(not(feature = "demo"))]
fn seed_cases(_: &Store, _: &Arc<FormDef>) -> Result<()> {
    Ok(())
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The prompt follows the sync state rather than being pushed by it, so a session
        // that expires while the window is idle still surfaces.
        self.sync_reauth_prompt(window, cx);

        v_flex()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key))
            .size_full()
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .overflow_hidden()
                    // Builder, then open case, then the worklist.
                    .when_some(self.builder.clone(), |d, b| d.child(b))
                    .when(self.builder.is_none(), |d| {
                        d.when_some(self.form.clone(), |d, f| d.child(f))
                            .when(self.form.is_none(), |d| d.child(self.worklist.clone()))
                    }),
            )
            .children(self.reauth.as_ref().map(|input| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_4()
                    .py_1()
                    .child(SharedString::from("Session expired — sync is paused:"))
                    .child(div().w(px(320.)).child(widgets::render_filter(input)))
            }))
            .child(
                // The permitted status surface: peripheral chrome, never over content.
                h_flex()
                    .w_full()
                    .justify_between()
                    .px_4()
                    .py_1()
                    .child(SharedString::from("medatat"))
                    .child(
                        h_flex()
                            .gap_4()
                            // Sync status is a count in the footer and nothing else. Never
                            // a spinner, never over content (R15).
                            .children(self.sync.line().map(SharedString::from))
                            .child(self.message.clone()),
                    ),
            )
    }
}

/// A demo form covering all seven field kinds (R5–R11) across 1-, 2-, and 3-column
/// sections (R12). Replaced by real configuration once config sync runs.
fn demo_form() -> FormDef {
    use medatat_core::def::{FieldDef, FieldKind, FieldOption, SectionDef, SectionField};
    use medatat_core::{FieldId, FieldIdx, FormId, OptionCode, SectionId};

    let sex: Arc<[FieldOption]> = Arc::from(vec![
        FieldOption {
            code: OptionCode::new("M"),
            label: "Male".into(),
            ordinal: 0,
        },
        FieldOption {
            code: OptionCode::new("F"),
            label: "Female".into(),
            ordinal: 1,
        },
        FieldOption {
            code: OptionCode::new("O"),
            label: "Other".into(),
            ordinal: 2,
        },
    ]);
    let site: Arc<[FieldOption]> = Arc::from(vec![
        FieldOption {
            code: OptionCode::new("ED"),
            label: "Emergency".into(),
            ordinal: 0,
        },
        FieldOption {
            code: OptionCode::new("IP"),
            label: "Inpatient".into(),
            ordinal: 1,
        },
    ]);

    let mut n = 0;
    let mut mk = |label: &str, kind: FieldKind, col_span: u8, required: bool| {
        n += 1;
        SectionField {
            idx: FieldIdx(0),
            field: Arc::new(FieldDef {
                field_id: FieldId::new(),
                key: format!("f{n}"),
                kind,
            }),
            label: label.into(),
            ordinal: n,
            col_span,
            required,
        }
    };

    FormDef::new(
        FormId::new(),
        "Demo intake",
        vec![
            SectionDef {
                section_id: SectionId::new(),
                title: "Demographics".into(),
                ordinal: 0,
                columns: 2,
                default_collapsed: false,
                fields: vec![
                    mk(
                        "Family name",
                        FieldKind::Text { max_len: Some(64) },
                        1,
                        true,
                    ),
                    mk(
                        "Given name",
                        FieldKind::Text { max_len: Some(64) },
                        1,
                        false,
                    ),
                    mk("Date of birth", FieldKind::Date, 1, true),
                    mk("Sex", FieldKind::Radio { options: sex }, 1, false),
                ],
            },
            SectionDef {
                section_id: SectionId::new(),
                title: "Encounter".into(),
                ordinal: 1,
                columns: 3,
                default_collapsed: false,
                fields: vec![
                    mk("Arrival time", FieldKind::Time, 1, true),
                    mk("Triage time", FieldKind::Time, 1, false),
                    mk(
                        "Site",
                        FieldKind::Select {
                            options: site,
                            searchable: false,
                        },
                        1,
                        false,
                    ),
                    mk(
                        "Weight (kg)",
                        FieldKind::Numeric {
                            min: None,
                            max: None,
                            scale: 2,
                        },
                        1,
                        false,
                    ),
                    mk(
                        "Height (cm)",
                        FieldKind::Numeric {
                            min: None,
                            max: None,
                            scale: 1,
                        },
                        1,
                        false,
                    ),
                ],
            },
            SectionDef {
                section_id: SectionId::new(),
                title: "Narrative".into(),
                ordinal: 2,
                columns: 1,
                default_collapsed: false,
                fields: vec![mk(
                    "Presenting complaint",
                    FieldKind::Textarea {
                        rows: 6,
                        max_len: Some(4000),
                    },
                    1,
                    false,
                )],
            },
        ],
    )
}
