//! The three budgeted GUI tests (`docs/07-TESTING.md`).
//!
//! `#[gpui::test]` drives an app context headlessly — no window is shown and no display is
//! required — so these run in CI and on a machine whose screen is locked. Everything else
//! belongs below the GUI line; three is a budget, not a target.

/// The platform's "secondary" modifier, as a keystroke prefix.
///
/// Bindings use gpui's `secondary()`, which is **Cmd on macOS and Ctrl everywhere else**.
/// A test that hardcodes `"cmd-f"` therefore exercises the binding on exactly one platform
/// and silently fails on the other two — which is precisely what CI found: 64 tests passed
/// on Linux and Windows and the five `cmd-` ones did not, because the app was right and the
/// tests were macOS-shaped.
fn secondary(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("cmd-{key}")
    } else {
        format!("ctrl-{key}")
    }
}

use crate::form::FormView;
use crate::form::view::{CHANGE_EVENTS, PARENT_RENDERS};
use crate::widgets;
use gpui::TestAppContext;
use medatat_core::def::{FieldDef, FieldKind, SectionDef, SectionField};
use medatat_core::{CaseId, CaseRev, FieldId, FieldIdx, FormDef, FormId, SectionId};
use medatat_store::Store;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

fn field_of(n: usize, kind: FieldKind) -> SectionField {
    SectionField {
        idx: FieldIdx(0),
        field: Arc::new(FieldDef {
            field_id: FieldId::new(),
            key: format!("f{n}"),
            kind,
        }),
        label: format!("Field {n}"),
        ordinal: n as i32,
        col_span: 1,
        required: false,
    }
}

fn single_kind_form(kind: FieldKind) -> Arc<FormDef> {
    Arc::new(FormDef::new(
        FormId::new(),
        "One",
        vec![section("Only", vec![field_of(0, kind)], false)],
    ))
}

fn text_field(n: usize) -> SectionField {
    SectionField {
        idx: FieldIdx(0),
        field: Arc::new(FieldDef {
            field_id: FieldId::new(),
            key: format!("f{n}"),
            kind: FieldKind::Text { max_len: None },
        }),
        label: format!("Field {n}"),
        ordinal: n as i32,
        col_span: 1,
        required: false,
    }
}

fn section(title: &str, fields: Vec<SectionField>, collapsed: bool) -> SectionDef {
    SectionDef {
        section_id: SectionId::new(),
        title: title.into(),
        ordinal: 0,
        columns: 1,
        default_collapsed: collapsed,
        fields,
    }
}

/// A form of `n` text fields in one section.
fn wide_form(n: usize) -> Arc<FormDef> {
    let fields = (0..n).map(text_field).collect();
    Arc::new(FormDef::new(
        FormId::new(),
        "Load",
        vec![section("All", fields, false)],
    ))
}

/// Opens a `FormView` in a headless test window.
///
/// A macro rather than a function because `add_window_view` hands back a **borrow** of the
/// test context, which a function cannot return alongside the view.
macro_rules! open_form {
    ($cx:ident, $def:expr, $store:expr) => {{
        $cx.update(|cx| widgets::init(cx));
        let (view, cx) = $cx.add_window_view(|window, cx| {
            FormView::new(
                $def,
                CaseId::new(),
                CaseRev::ZERO,
                Vec::new(),
                $store,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        (view, cx)
    }};
}

/// 1. **`subscriptions_fire_once_per_edit`** — the anti-quadratic guard.
///
/// 300 fields, 300 subscriptions, one typed character. Exactly one subscription may fire,
/// and — the part that actually costs O(n) when it regresses — the parent must not
/// re-render at all. This has already tried to break twice, through two different doors.
#[gpui::test]
fn subscriptions_fire_once_per_edit(cx: &mut TestAppContext) {
    let def = wide_form(300);
    let store = Arc::new(Store::open_in_memory().expect("store"));

    let (view, cx) = open_form!(cx, def, store);

    // Focus the first field so a keystroke has somewhere to land.
    cx.update(|window, cx| {
        view.update(cx, |v, cx| v.focus_step(1, window, cx));
    });
    cx.run_until_parked();

    CHANGE_EVENTS.store(0, Relaxed);
    PARENT_RENDERS.store(0, Relaxed);

    cx.simulate_input("a");
    cx.run_until_parked();

    assert_eq!(
        CHANGE_EVENTS.load(Relaxed),
        1,
        "one character must reach exactly one subscription, not 300"
    );
    assert_eq!(
        PARENT_RENDERS.load(Relaxed),
        0,
        "a keystroke must not re-render the parent — that is what makes typing O(n)"
    );

    // And the character actually landed: the guard must not pass by doing nothing.
    let dirty = cx.update(|_, cx| view.read(cx).instance().dirty_count());
    assert_eq!(dirty, 1, "exactly one field changed");
}

/// 2. **`tab_order_matches_focus_order`** — including that collapsed sections contribute
///    no stops, which is the difference between a keyboard form and a broken one.
#[gpui::test]
fn tab_order_matches_focus_order(cx: &mut TestAppContext) {
    let visible: Vec<SectionField> = (0..3).map(text_field).collect();
    let hidden: Vec<SectionField> = (3..6).map(text_field).collect();
    let mut def = FormDef::new(
        FormId::new(),
        "Two",
        vec![
            section("Visible", visible, false),
            // Collapsed on open: its fields must contribute no focus stops.
            section("Collapsed", hidden, true),
        ],
    );
    def.finalize();
    let def = Arc::new(def);
    let store = Arc::new(Store::open_in_memory().expect("store"));

    let (view, cx) = open_form!(cx, def, store);

    let order = cx.update(|_, cx| view.read(cx).focus_order().to_vec());
    assert_eq!(
        order,
        vec![FieldIdx(0), FieldIdx(1), FieldIdx(2)],
        "a collapsed section contributes no focus stops"
    );

    // Tab walks that order, and only that order.
    for expected in &order {
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        let focused = cx.update(|_, cx| view.read(cx).focused_field());
        assert_eq!(focused, Some(*expected), "tab must follow focus_order");
    }

    // Past the end it wraps to the first stop, never into the collapsed section.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    let focused = cx.update(|_, cx| view.read(cx).focused_field());
    assert_eq!(
        focused,
        Some(FieldIdx(0)),
        "wraps rather than entering a collapsed section"
    );
}

/// 3. **`closing_case_clears_inputs`** — PHI hygiene.
///
/// `Value` zeroizes on drop under `phi`, but the same text also lives inside
/// `gpui-component`'s editing state, which zeroize cannot reach. Closing a case has to
/// blank the widgets explicitly or the contents outlive the case in a live buffer.
#[cfg(feature = "phi")]
#[gpui::test]
fn closing_case_clears_inputs(cx: &mut TestAppContext) {
    let def = wide_form(3);
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, Arc::clone(&def), store);

    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.focus_step(1, window, cx);
        });
    });
    cx.run_until_parked();
    cx.simulate_input("secret");
    cx.run_until_parked();

    let before = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert!(!before.is_empty(), "the field holds text before closing");

    cx.update(|window, cx| view.update(cx, |v, cx| v.clear_inputs(window, cx)));
    cx.run_until_parked();

    let after = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert!(
        after.is_empty(),
        "closing a case must leave no field contents in a live input buffer, found {after:?}"
    );
}

/// **The time field is the only place the UI is the sole guard before a value is parsed.**
///
/// A rejected keystroke that silently lands puts bad data in a clinical record. The
/// rejection lives in `InputState::validate`, whose behaviour only exists once the widget is
/// wired — no pure function can observe whether the hook is actually installed.
#[gpui::test]
fn r8_time_field_rejects_illegal_keystrokes(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, single_kind_form(FieldKind::Time), store);

    cx.update(|window, cx| view.update(cx, |v, cx| v.focus_step(1, window, cx)));
    cx.run_until_parked();

    // A plausible mistake: 12-hour time with a meridiem suffix.
    cx.simulate_input("9:30pm");
    cx.run_until_parked();

    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert!(
        !text.contains('p') && !text.contains('m'),
        "letters must never land in a time field, found {text:?}"
    );
    assert_eq!(text, "9:30", "the legal characters land, unreformatted");
}

/// R8: five characters is `HH:MM`, so a sixth cannot land.
#[gpui::test]
fn r8_time_field_is_length_capped(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, single_kind_form(FieldKind::Time), store);

    cx.update(|window, cx| view.update(cx, |v, cx| v.focus_step(1, window, cx)));
    cx.run_until_parked();

    cx.simulate_input("123456789");
    cx.run_until_parked();

    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert!(
        text.chars().count() <= 5,
        "HH:MM is five characters; found {text:?}"
    );
}

/// R8: the separator arrives with the third digit, at end of text.
#[gpui::test]
fn r8_time_field_inserts_the_separator_while_typing(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, single_kind_form(FieldKind::Time), store);

    cx.update(|window, cx| view.update(cx, |v, cx| v.focus_step(1, window, cx)));
    cx.run_until_parked();

    cx.simulate_input("093");
    cx.run_until_parked();

    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert_eq!(text, "09:3", "the third digit brings the separator with it");
}

/// R8: `Up`/`Down` step by a minute, `Shift` by an hour.
///
/// Wiring all the way down: `InputState` binds `up`/`down` to `MoveUp`/`MoveDown` in its own
/// key context, so the nudge only works because a capture-phase handler runs first. A
/// bubble-phase handler would never see the key.
#[gpui::test]
fn r8_time_field_nudges_on_arrows(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, single_kind_form(FieldKind::Time), store);

    cx.update(|window, cx| view.update(cx, |v, cx| v.focus_step(1, window, cx)));
    cx.run_until_parked();

    cx.simulate_input("09:30");
    cx.run_until_parked();

    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert_eq!(text, "09:31", "up is one minute");

    cx.simulate_keystrokes("down down");
    cx.run_until_parked();
    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert_eq!(text, "09:29", "down is one minute");

    cx.simulate_keystrokes("shift-up");
    cx.run_until_parked();
    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert_eq!(text, "10:29", "shift is one hour");
}

/// `Cmd/Ctrl-F` opens the field palette, and `Enter` takes the top match.
///
/// Wiring: the palette is a separate entity, so this also proves the query box takes focus
/// and that `Escape` gets back out without closing anything else.
#[gpui::test]
fn cmd_f_opens_the_field_palette(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, wide_form(5), store);

    assert!(!cx.update(|_, cx| view.read(cx).palette_is_open()));

    cx.simulate_keystrokes(&secondary("f"));
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| view.read(cx).palette_is_open()),
        "cmd-f must open the palette"
    );

    // Enter takes the top match and closes the palette, leaving focus on a field.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!cx.update(|_, cx| view.read(cx).palette_is_open()));
    assert!(
        cx.update(|_, cx| view.read(cx).focused_field()).is_some(),
        "choosing a match must land focus on a field"
    );
}

/// `Escape` closes the palette without leaving the case.
#[gpui::test]
fn escape_closes_the_field_palette(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, wide_form(3), store);

    cx.simulate_keystrokes(&secondary("f"));
    cx.run_until_parked();
    assert!(cx.update(|_, cx| view.read(cx).palette_is_open()));

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        !cx.update(|_, cx| view.read(cx).palette_is_open()),
        "escape must close the palette"
    );
}

/// `Alt-Up`/`Alt-Down` move between sections, including out of a focused text input.
#[gpui::test]
fn alt_up_down_navigates_between_sections(cx: &mut TestAppContext) {
    let (def, store) = two_sections();
    let (view, cx) = open_form!(cx, def, store);

    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    cx.simulate_keystrokes("alt-down");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).focused_field()),
        Some(FieldIdx(2)),
        "alt-down lands on the next section's first field"
    );

    cx.simulate_keystrokes("alt-up");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).focused_field()),
        Some(FieldIdx(0)),
        "alt-up goes back"
    );
}

/// `Cmd/Ctrl-[` and `]` collapse and expand, **from inside a focused text input** —
/// which is where a keyboard user actually is, since Tab lands them in a field.
///
/// The binding is `Cmd/Ctrl-[`/`]` rather than the `Alt-Left`/`Alt-Right` that reads more
/// naturally, and that is deliberate: macOS consumes option+arrow as "move by word" before
/// the event enters the element dispatch tree, so no handler can see it while a field has
/// focus. `InputState` *does* bind `cmd-[`/`cmd-]` to Outdent/Indent, and this test is the
/// evidence that capture phase beats that keymap binding. **Do not rebind this to
/// `Alt-Left`/`Alt-Right`** — it will appear to work until someone presses Tab first.
#[gpui::test]
fn cmd_brackets_collapse_and_expand_a_section(cx: &mut TestAppContext) {
    let (def, store) = two_sections();
    let (view, cx) = open_form!(cx, def, store);

    // Put focus inside a text input first: this is the case `Alt-Left` could never serve.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).focused_field()),
        Some(FieldIdx(0)),
        "focus is inside a field, not on the form root"
    );

    cx.simulate_keystrokes(&secondary("["));
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| view.read(cx).is_collapsed(0)),
        "cmd-[ collapses the current section from inside a field"
    );
    assert_eq!(
        cx.update(|_, cx| view.read(cx).focus_order().to_vec()),
        vec![FieldIdx(2), FieldIdx(3)],
        "a collapsed section contributes no stops"
    );

    cx.simulate_keystrokes(&secondary("]"));
    cx.run_until_parked();
    assert!(!cx.update(|_, cx| view.read(cx).is_collapsed(0)));
    assert_eq!(
        cx.update(|_, cx| view.read(cx).focus_order().len()),
        4,
        "and the stops come back"
    );
}

/// `Alt-Up`/`Alt-Down` reorder in the builder — the shipped alternative to drag.
#[gpui::test]
fn alt_arrows_reorder_in_the_builder(cx: &mut TestAppContext) {
    use crate::builder::BuilderView;
    use crate::mode::Selection;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let a: Vec<SectionField> = (0..1).map(text_field).collect();
    let b: Vec<SectionField> = (1..2).map(text_field).collect();
    let mut def = FormDef::new(
        FormId::new(),
        "Reorder",
        vec![section("First", a, false), section("Second", b, false)],
    );
    def.finalize();
    let def = Arc::new(def);
    store
        .save_form(&def, medatat_core::ConfigRev(1))
        .expect("saved");

    cx.update(widgets::init);
    let (builder, cx) = cx.add_window_view(|window, cx| {
        BuilderView::new(Arc::clone(&store), Arc::clone(&def), window, cx)
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        builder.update(cx, |b, cx| {
            b.set_selection_for_test(Some(Selection::Section(0)), cx)
        })
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("alt-down");
    cx.run_until_parked();

    let titles: Vec<String> = cx.update(|_, cx| {
        builder
            .read(cx)
            .draft()
            .sections
            .iter()
            .map(|s| s.title.clone())
            .collect()
    });
    assert_eq!(
        titles,
        vec!["Second".to_string(), "First".to_string()],
        "alt-down moves the selected section one place"
    );
    assert_eq!(
        cx.update(|_, cx| builder.read(cx).selection()),
        Some(Selection::Section(1)),
        "selection follows the section it moved"
    );
}

/// Two two-field sections, and a store to hold them.
fn two_sections() -> (Arc<FormDef>, Arc<Store>) {
    let a: Vec<SectionField> = (0..2).map(text_field).collect();
    let b: Vec<SectionField> = (2..4).map(text_field).collect();
    let mut def = FormDef::new(
        FormId::new(),
        "Two",
        vec![section("A", a, false), section("B", b, false)],
    );
    def.finalize();
    (
        Arc::new(def),
        Arc::new(Store::open_in_memory().expect("store")),
    )
}

/// `Cmd/Ctrl-J` opens the next case from the worklist, and `Escape` goes back.
///
/// Shell-level wiring: the shortcut is handled on `Workspace`, above both screens, and has
/// to beat whatever the open case editor binds.
#[gpui::test]
fn cmd_j_opens_the_next_case_from_the_worklist(cx: &mut TestAppContext) {
    use medatat_core::wire::CaseSummary;
    use medatat_core::{CaseId as Cid, ConfigRev};

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let def = wide_form(2);
    let fields: Vec<_> = def.iter_fields().map(|f| (*f.field).clone()).collect();
    store.save_fields(&fields).expect("fields");
    store.save_form(&def, ConfigRev(1)).expect("form");
    for n in 0..3u64 {
        store
            .upsert_case(&CaseSummary {
                case_id: Cid::new(),
                mrn: format!("SYN-MRN-{n:04}"),
                form_id: def.form_id,
                assignee: Some(crate::DEMO_ASSIGNEE.into()),
                rev: CaseRev::ZERO,
                updated_at: format!("2026-08-1{n}T09:00:00Z"),
            })
            .expect("case");
    }

    cx.update(widgets::init);
    let s = Arc::clone(&store);
    let (workspace, cx) = cx.add_window_view(|window, cx| crate::Workspace::new(s, window, cx));
    cx.run_until_parked();

    assert!(
        cx.update(|_, cx| workspace.read(cx).form.is_none()),
        "the worklist shows first, with no case open"
    );

    cx.simulate_keystrokes(&secondary("j"));
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| workspace.read(cx).form.is_some()),
        "cmd-j opens a case"
    );

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| workspace.read(cx).form.is_none()),
        "escape returns to the worklist"
    );
}

/// `Cmd/Ctrl-B` opens the builder and `Escape` leaves it.
#[gpui::test]
fn cmd_b_opens_the_builder(cx: &mut TestAppContext) {
    use medatat_core::ConfigRev;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let def = wide_form(2);
    let fields: Vec<_> = def.iter_fields().map(|f| (*f.field).clone()).collect();
    store.save_fields(&fields).expect("fields");
    store.save_form(&def, ConfigRev(1)).expect("form");

    cx.update(widgets::init);
    let s = Arc::clone(&store);
    let (workspace, cx) = cx.add_window_view(|window, cx| crate::Workspace::new(s, window, cx));
    cx.run_until_parked();

    cx.simulate_keystrokes(&secondary("b"));
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| workspace.read(cx).builder.is_some()),
        "cmd-b opens the builder"
    );

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| workspace.read(cx).builder.is_none()),
        "escape leaves the builder"
    );
}

/// `medatat-testkit`'s `MockTransport` does not implement `Transport` — testkit avoids
/// depending on `medatat-sync` on purpose, so each caller writes this adapter.
mod mock_transport {
    use async_trait::async_trait;
    use medatat_core::ids::{CaseId, CaseRev, ConfigRev};
    use medatat_core::wire::{
        CasePage, CaseQuery, ConfigDelta, PutValuesReq, PutValuesResp, ValuePage,
    };
    use medatat_sync::{Transport, TransportError};
    use medatat_testkit::{MockError, MockTransport};
    use std::sync::Arc;

    pub struct Adapter(pub Arc<MockTransport>);

    fn map(e: MockError) -> TransportError {
        match e {
            MockError::Offline => TransportError::Offline,
            other => TransportError::Server {
                status: 500,
                message: other.to_string(),
            },
        }
    }

    #[async_trait]
    impl Transport for Adapter {
        async fn config(&self, since: ConfigRev) -> Result<Option<ConfigDelta>, TransportError> {
            self.0.config(since).map_err(map)
        }
        async fn list_cases(&self, q: CaseQuery) -> Result<CasePage, TransportError> {
            self.0.list_cases(q).map_err(map)
        }
        async fn get_values(&self, c: CaseId, since: CaseRev) -> Result<ValuePage, TransportError> {
            self.0.get_values(c, since).map_err(map)
        }
        async fn put_values(
            &self,
            c: CaseId,
            r: PutValuesReq,
        ) -> Result<PutValuesResp, TransportError> {
            self.0.put_values(c, r).map_err(map)
        }
    }
}

/// The background loop actually runs, on the background executor, and pre-syncs the
/// caseload before anything else.
///
/// R15 rests on the caseload being local before the user opens anything, so "it syncs
/// eventually" is not the property — "it has already synced" is.
#[test]
fn sync_loop_pre_syncs_the_caseload_in_the_background() {
    use medatat_sync::SyncEngine;
    use medatat_testkit::MockTransport;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let mock = Arc::new(MockTransport::new());
    let engine = SyncEngine::new(
        Arc::clone(&store),
        mock_transport::Adapter(Arc::clone(&mock)),
    );

    let _handle = crate::sync::spawn(engine, "demo".into(), medatat_http::TokenHolder::default())
        .expect("sync thread");
    // Wait for the condition actually being asserted, not a weaker proxy for it.
    //
    // This waited for `!calls.is_empty()` — the FIRST call — and then asserted that TWO
    // specific calls had happened. On a fast machine the second usually lands in the same
    // instant; on a slower CI runner it does not, and the test fails for a reason that has
    // nothing to do with the behaviour. It passed on macOS and Windows and failed on Linux
    // for exactly that reason.
    let seen_both = |c: &[String]| {
        c.iter().any(|x| x.starts_with("list_cases")) && c.iter().any(|x| x.starts_with("config"))
    };
    wait_for(|| seen_both(&mock.calls()));

    let calls = mock.calls();
    assert!(
        calls.iter().any(|c| c.starts_with("list_cases")),
        "the caseload must be pulled without anyone asking, got {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.starts_with("config")),
        "form definitions too, got {calls:?}"
    );
}

/// Offline is a normal state: the loop keeps running and nothing blocks.
#[test]
fn sync_loop_survives_being_offline() {
    use medatat_sync::SyncEngine;
    use medatat_testkit::MockTransport;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let mock = Arc::new(MockTransport::new());
    mock.go_offline();
    let engine = SyncEngine::new(
        Arc::clone(&store),
        mock_transport::Adapter(Arc::clone(&mock)),
    );

    let _handle = crate::sync::spawn(engine, "demo".into(), medatat_http::TokenHolder::default())
        .expect("sync thread");

    // It tried, it failed, it did not panic and did not stop.
    assert!(
        wait_for(|| !mock.calls().is_empty()),
        "the loop still attempts while offline"
    );

    mock.go_online();
    mock.clear_calls();
    assert!(
        wait_for(|| !mock.calls().is_empty()),
        "coming back online resumes without a restart"
    );
}

/// **Inbound sync never overwrites the field the user is in** (`docs/04-SYNC.md`).
///
/// Until the engine gained a change observer this criterion was unfalsifiable: the guard
/// existed in `FormView::receive_remote` and nothing could ever call it. This is the test
/// that makes it a claim rather than an intention.
#[gpui::test]
fn inbound_sync_defers_the_focused_field_until_the_user_leaves(cx: &mut TestAppContext) {
    use medatat_core::Value;
    use medatat_core::wire::ValueRow;

    let def = wide_form(3);
    let ids: Vec<_> = def.iter_fields().map(|f| f.field.field_id).collect();
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, Arc::clone(&def), store);

    // Stand in the first field.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).focused_field()),
        Some(FieldIdx(0))
    );

    let row = |i: usize, text: &str| ValueRow {
        field_id: ids[i],
        value: Value::Text(text.into()),
        rev: CaseRev(1),
        updated_by: None,
        updated_at: None,
    };

    // Someone else's edits arrive for the focused field and an idle one.
    cx.update(|_, cx| {
        view.update(cx, |v, cx| {
            v.receive_remote_rows(&[row(0, "theirs"), row(1, "also theirs")], cx)
        })
    });
    cx.run_until_parked();

    let focused_value = cx.update(|_, cx| view.read(cx).instance().get(FieldIdx(0)).clone());
    assert_eq!(
        focused_value,
        Value::Null,
        "the field under the cursor must not be rewritten"
    );
    let idle_value = cx.update(|_, cx| view.read(cx).instance().get(FieldIdx(1)).clone());
    assert_eq!(
        idle_value,
        Value::Text("also theirs".into()),
        "a field nobody is in takes the update immediately"
    );

    // Leaving the field is what releases the deferred value.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    let after = cx.update(|_, cx| view.read(cx).instance().get(FieldIdx(0)).clone());
    assert_eq!(
        after,
        Value::Text("theirs".into()),
        "the deferred value lands once the user has moved on"
    );
}

/// A local edit wins over an inbound value for the same field: it is unsynced work, and
/// silently replacing it would lose the abstractor's typing.
#[gpui::test]
fn inbound_sync_never_clobbers_an_unsynced_local_edit(cx: &mut TestAppContext) {
    use medatat_core::Value;
    use medatat_core::wire::ValueRow;

    let def = wide_form(2);
    let ids: Vec<_> = def.iter_fields().map(|f| f.field.field_id).collect();
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let (view, cx) = open_form!(cx, Arc::clone(&def), store);

    // Type into the first field, then move away so it is dirty but not focused.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    cx.simulate_input("mine");
    cx.run_until_parked();
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();

    cx.update(|_, cx| {
        view.update(cx, |v, cx| {
            v.receive_remote_rows(
                &[ValueRow {
                    field_id: ids[0],
                    value: Value::Text("theirs".into()),
                    rev: CaseRev(1),
                    updated_by: None,
                    updated_at: None,
                }],
                cx,
            )
        })
    });
    cx.run_until_parked();

    let value = cx.update(|_, cx| view.read(cx).instance().get(FieldIdx(0)).clone());
    assert_eq!(
        value,
        Value::Text("mine".into()),
        "an unsynced local edit is not overwritten by an inbound value"
    );
}

/// **R14: tabbing away persists.** The local write *is* the save, so leaving a field must
/// reach SQLite — not wait for a blur event that programmatic focus never produces.
#[gpui::test]
fn r14_tabbing_away_from_a_field_flushes_it_to_sqlite(cx: &mut TestAppContext) {
    use medatat_core::ConfigRev;
    use medatat_core::wire::CaseSummary;

    let def = wide_form(3);
    let store = Arc::new(Store::open_in_memory().expect("store"));
    store.save_form(&def, ConfigRev(1)).expect("form");

    // `apply_local` writes against a real case row, so the case has to exist — the same
    // precondition `Workspace::open_case` satisfies by reading one out of the worklist.
    let case = CaseId::new();
    store
        .upsert_case(&CaseSummary {
            case_id: case,
            mrn: "SYN-MRN-0001".into(),
            form_id: def.form_id,
            assignee: Some(crate::DEMO_ASSIGNEE.into()),
            rev: CaseRev::ZERO,
            updated_at: "2026-08-18T09:00:00Z".into(),
        })
        .expect("case");

    cx.update(widgets::init);
    let s = Arc::clone(&store);
    let d = Arc::clone(&def);
    let (view, cx) = cx.add_window_view(|window, cx| {
        FormView::new(d, case, CaseRev::ZERO, Vec::new(), s, window, cx)
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    cx.simulate_input("typed");
    cx.run_until_parked();

    cx.simulate_keystrokes("tab");
    cx.run_until_parked();

    assert_eq!(cx.update(|_, cx| view.read(cx).case_id()), case);
    let stored = store.load_case_values(case).expect("values");
    assert!(
        stored
            .iter()
            .any(|(_, v)| matches!(v, medatat_core::Value::Text(t) if t == "typed")),
        "leaving a field must write it to the local store, found {stored:?}"
    );
}

/// R8: a time field is canonicalised when you tab out of it, not only on Enter.
#[gpui::test]
fn r8_tabbing_out_of_a_time_field_canonicalises_it(cx: &mut TestAppContext) {
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let mut def = FormDef::new(
        FormId::new(),
        "Times",
        vec![section(
            "S",
            vec![field_of(0, FieldKind::Time), text_field(1)],
            false,
        )],
    );
    def.finalize();
    let (view, cx) = open_form!(cx, Arc::new(def), store);

    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    cx.simulate_input("9");
    cx.run_until_parked();

    cx.simulate_keystrokes("tab");
    cx.run_until_parked();

    let text = cx.update(|_, cx| view.read(cx).widget_text(FieldIdx(0), cx));
    assert_eq!(
        text, "09:00",
        "tabbing out canonicalises, as blur and Enter do"
    );
}

/// Polls a condition for up to two seconds. The sync loop runs on a real thread, so its
/// tests wait on wall-clock rather than gpui's test executor.
/// Polls until `done`, for up to 10 s.
///
/// The budget is generous on purpose: this waits on a real background thread with its own
/// runtime, and a shared CI runner is far slower than a developer machine. A tight timeout
/// here does not catch bugs, it manufactures flakes — and a flaky test teaches people to
/// re-run rather than to read.
fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}
