//! The three budgeted GUI tests (`docs/07-TESTING.md`).
//!
//! `#[gpui::test]` drives an app context headlessly — no window is shown and no display is
//! required — so these run in CI and on a machine whose screen is locked. Everything else
//! belongs below the GUI line; three is a budget, not a target.

use crate::form::FormView;
use crate::form::view::{CHANGE_EVENTS, PARENT_RENDERS};
use crate::widgets;
use gpui::TestAppContext;
use medatat_core::def::{FieldDef, FieldKind, SectionDef, SectionField};
use medatat_core::{CaseId, CaseRev, FieldId, FieldIdx, FormDef, FormId, SectionId};
use medatat_store::Store;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

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
