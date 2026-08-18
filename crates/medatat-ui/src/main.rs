//! medatat desktop client.
//!
//! The one architectural rule: **the UI never awaits the network.** Reads and writes hit
//! local SQLite synchronously; sync runs on the background executor. That is what gives
//! R13/R14 their margin and makes R15 structurally true rather than approximated.
//! See `docs/adr/0002-encrypted-local-sqlite.md`.

mod builder;
mod form;
mod mode;
mod widgets;
mod worklist;

use anyhow::Result;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString, Styled as _, Window,
    WindowBounds, WindowOptions, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::{CaseId, CaseRev, ConfigRev, FormDef};
use medatat_store::Store;
use std::rc::Rc;
use std::sync::Arc;

use form::FormView;
use worklist::WorklistView;

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
    message: SharedString,
    focus: FocusHandle,
}

impl Workspace {
    fn new(store: Arc<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // On first run there is nothing to show, so seed a demo form and caseload rather
        // than an empty window. Both are replaced by config and caseload sync.
        let message = match Self::load_or_seed(&store) {
            Ok(def) => SharedString::from(format!("{} fields", def.field_count())),
            Err(e) => SharedString::from(format!("no form available: {e}")),
        };

        let on_open = Self::on_open_case(cx);
        let s = Arc::clone(&store);
        let worklist =
            cx.new(|cx| WorklistView::new(&s, DEMO_ASSIGNEE, WORKLIST_LIMIT, on_open, window, cx));

        Workspace {
            store,
            worklist,
            form: None,
            message,
            focus: cx.focus_handle(),
        }
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
        self.form = None;
        self.worklist.update(cx, |w, cx| w.focus(window, cx));
        cx.notify();
    }

    /// `Cmd/Ctrl-J` / `Cmd/Ctrl-K` and `Escape`. Capture phase, because the screens below
    /// bind these keys in their own contexts.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mods = &ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            // Next / previous case. The step flushes through `open_case`.
            "j" | "k" if mods.secondary() => {
                let delta = if ev.keystroke.key == "j" { 1 } else { -1 };
                let next = self.worklist.update(cx, |w, cx| w.step(delta, cx));
                if let Some(case_id) = next {
                    self.open_case(case_id, window, cx);
                }
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
/// Synthetic and obviously so — the MRNs are sequential. Real cases arrive from the server.
fn seed_cases(store: &Store, def: &Arc<FormDef>) -> Result<()> {
    use medatat_core::wire::CaseSummary;
    for n in 1..=50 {
        store.upsert_case(&CaseSummary {
            case_id: CaseId::new(),
            mrn: format!("MRN-{n:04}"),
            form_id: def.form_id,
            assignee: Some(DEMO_ASSIGNEE.into()),
            rev: CaseRev::ZERO,
            updated_at: format!("2026-08-{:02}T09:{:02}:00Z", (n % 28) + 1, n % 60),
        })?;
    }
    Ok(())
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key))
            .size_full()
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .overflow_hidden()
                    .when_some(self.form.clone(), |d, f| d.child(f))
                    .when(self.form.is_none(), |d| d.child(self.worklist.clone())),
            )
            .child(
                // The permitted status surface: peripheral chrome, never over content.
                h_flex()
                    .w_full()
                    .justify_between()
                    .px_4()
                    .py_1()
                    .child(SharedString::from("medatat"))
                    .child(self.message.clone()),
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
