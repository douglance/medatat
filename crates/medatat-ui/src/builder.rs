//! The form builder (M5, R4) — `docs/06-FORM-BUILDER.md`.
//!
//! Three panes: the section tree, the canvas, and the inspector.
//!
//! **The canvas is not a second renderer.** It is the same `FormView` the abstractor uses,
//! switched to `RenderMode::Design`. Every layout decision — columns, spans, responsive
//! degradation — is therefore made in exactly one place, and WYSIWYG costs nothing. See
//! `mode.rs` before adding anything here that emits form elements.
//!
//! Reordering ships as `Alt-Up`/`Alt-Down` and explicit ↑/↓ buttons. `gpui-component` has no
//! drag primitive and `docs/06-FORM-BUILDER.md` is explicit that drag is not an M5 gate.

use crate::form::FormView;
use crate::mode::{RenderMode, Selection};
use crate::widgets::OnSelectField;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, FontWeight, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::{CaseId, CaseRev, ConfigRev, FieldIdx, FormDef, WidgetKind};
use medatat_store::Store;
use std::rc::Rc;
use std::sync::Arc;

pub struct BuilderView {
    store: Arc<Store>,
    draft: Arc<FormDef>,
    /// The runtime renderer, in design mode. Rebuilt on a structural change because
    /// `FormDef::finalize` reassigns every `FieldIdx`.
    canvas: Entity<FormView>,
    selection: Option<Selection>,
    /// "Preview as abstractor" — flips the canvas to `Runtime` against a scratch case.
    preview: bool,
    focus: FocusHandle,
    tree_scroll: ScrollHandle,
    /// The throwaway case the canvas renders against. Never in the worklist.
    scratch: CaseId,
}

impl BuilderView {
    pub fn new(
        store: Arc<Store>,
        draft: Arc<FormDef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let scratch = CaseId::new();
        let canvas = Self::build_canvas(&store, &draft, scratch, window, cx);
        BuilderView {
            store,
            draft,
            canvas,
            selection: None,
            preview: false,
            focus: cx.focus_handle(),
            tree_scroll: ScrollHandle::new(),
            scratch,
        }
    }

    fn build_canvas(
        store: &Arc<Store>,
        draft: &Arc<FormDef>,
        scratch: CaseId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<FormView> {
        let on_select: OnSelectField = {
            let this = cx.entity().downgrade();
            Rc::new(move |idx, _, cx| {
                let _ = this.update(cx, |b: &mut BuilderView, cx| {
                    b.set_selection(Some(Selection::Field(idx)), cx);
                });
            })
        };

        let def = Arc::clone(draft);
        let store = Arc::clone(store);
        cx.new(|cx| {
            let mut v = FormView::new(def, scratch, CaseRev::ZERO, Vec::new(), store, window, cx);
            v.set_on_select(on_select);
            v.set_mode(RenderMode::Design { selected: None }, cx);
            v
        })
    }

    /// Selection is mirrored into the canvas so the outline and the inspector agree.
    fn set_selection(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        self.selection = selection;
        self.canvas.update(cx, |v, cx| v.select(selection, cx));
        cx.notify();
    }

    /// Flips the canvas between design and runtime without rebuilding anything: it is the
    /// same view, and that is the entire point of `RenderMode`.
    fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = !self.preview;
        let mode = if self.preview {
            RenderMode::Runtime
        } else {
            RenderMode::Design {
                selected: self.selection,
            }
        };
        self.canvas.update(cx, |v, cx| v.set_mode(mode, cx));
        cx.notify();
    }

    /// Applies a structural change: persist, then rebuild the canvas.
    ///
    /// The rebuild is not laziness. `FormDef::finalize` reassigns every `FieldIdx` densely in
    /// render order, so a `FormView` holding widgets indexed by the old values would be
    /// pointing at the wrong fields. Rebuilding is correct and costs one form's worth of
    /// entity construction, on an action a coordinator takes by hand.
    fn commit_structure(&mut self, next: FormDef, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.store.save_form(&next, ConfigRev(1)) {
            // A failed config write must be visible, not swallowed: the coordinator would
            // otherwise keep editing a form that is not being saved.
            tracing::error!("could not save form: {e}");
            return;
        }
        self.draft = Arc::new(next);
        self.canvas = Self::build_canvas(&self.store, &self.draft, self.scratch, window, cx);
        // Field indices moved, so a field selection no longer means what it did.
        let keep = match self.selection {
            Some(Selection::Section(i)) if i < self.draft.sections.len() => {
                Some(Selection::Section(i))
            }
            _ => None,
        };
        self.set_selection(keep, cx);
    }

    /// Moves a section up or down one place.
    fn move_section(&mut self, section: usize, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let mut next = (*self.draft).clone();
        let target = section as isize + delta;
        if target < 0 || target as usize >= next.sections.len() {
            return;
        }
        next.sections.swap(section, target as usize);
        renumber(&mut next);
        // `finalize` re-sorts by ordinal and re-derives indices; it is also where R12's
        // col_span clamp lives, so structural edits get it for free.
        next.finalize();
        self.selection = Some(Selection::Section(target as usize));
        self.commit_structure(next, window, cx);
        self.set_selection(Some(Selection::Section(target as usize)), cx);
    }

    /// Moves a field within its section.
    fn move_field(&mut self, idx: FieldIdx, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((s, f)) = self.locate(idx) else { return };
        let mut next = (*self.draft).clone();
        let fields = &mut next.sections[s].fields;
        let target = f as isize + delta;
        if target < 0 || target as usize >= fields.len() {
            return;
        }
        fields.swap(f, target as usize);
        renumber(&mut next);
        next.finalize();
        self.commit_structure(next, window, cx);

        // The moved field kept its identity, so re-select it by id at its new index.
        let id = self.draft.sections[s].fields[target as usize].field.field_id;
        let moved = self.draft.idx_of(id).map(Selection::Field);
        self.set_selection(moved, cx);
    }

    /// `(section index, position within section)` for a field.
    fn locate(&self, idx: FieldIdx) -> Option<(usize, usize)> {
        self.draft.sections.iter().enumerate().find_map(|(s, sec)| {
            sec.fields
                .iter()
                .position(|f| f.idx == idx)
                .map(|f| (s, f))
        })
    }

    /// `Alt-Up` / `Alt-Down` reorder whatever is selected. Same operation as the buttons,
    /// because the buttons are the requirement and the keys are the fast path.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !ev.keystroke.modifiers.alt {
            return;
        }
        let delta = match ev.keystroke.key.as_str() {
            "up" => -1,
            "down" => 1,
            _ => return,
        };
        match self.selection {
            Some(Selection::Section(i)) => self.move_section(i, delta, window, cx),
            Some(Selection::Field(idx)) => self.move_field(idx, delta, window, cx),
            None => return,
        }
        cx.stop_propagation();
    }
}

/// Rewrites ordinals to match current vector order, so `finalize`'s sort is a no-op rather
/// than a surprise.
fn renumber(def: &mut FormDef) {
    for (s, section) in def.sections.iter_mut().enumerate() {
        section.ordinal = s as i32;
        for (f, field) in section.fields.iter_mut().enumerate() {
            field.ordinal = f as i32;
        }
    }
}

impl BuilderView {
    fn tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let selection = self.selection;
        let mut rows: Vec<AnyElement> = Vec::new();

        for (s, section) in self.draft.sections.iter().enumerate() {
            let selected = selection == Some(Selection::Section(s));
            rows.push(
                h_flex()
                    .id(SharedString::from(format!("sec-row-{s}")))
                    .w_full()
                    .px_1()
                    .py_1()
                    .gap_1()
                    .cursor_pointer()
                    .when(selected, |d| d.font_weight(FontWeight::BOLD))
                    .child(
                        div()
                            .flex_1()
                            .child(SharedString::from(format!("▾ {}", section.title))),
                    )
                    .child(self.nudge_button(format!("sec-up-{s}"), "↑", Selection::Section(s), -1, cx))
                    .child(self.nudge_button(format!("sec-dn-{s}"), "↓", Selection::Section(s), 1, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_selection(Some(Selection::Section(s)), cx);
                    }))
                    .into_any_element(),
            );

            for field in &section.fields {
                let idx = field.idx;
                let selected = selection == Some(Selection::Field(idx));
                rows.push(
                    h_flex()
                        .id(SharedString::from(format!("fld-row-{}", idx.0)))
                        .w_full()
                        .pl_4()
                        .pr_1()
                        .py_1()
                        .gap_1()
                        .cursor_pointer()
                        .when(selected, |d| d.font_weight(FontWeight::BOLD))
                        .child(div().flex_1().child(SharedString::from(field.label.clone())))
                        .child(self.nudge_button(
                            format!("fld-up-{}", idx.0),
                            "↑",
                            Selection::Field(idx),
                            -1,
                            cx,
                        ))
                        .child(self.nudge_button(
                            format!("fld-dn-{}", idx.0),
                            "↓",
                            Selection::Field(idx),
                            1,
                            cx,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_selection(Some(Selection::Field(idx)), cx);
                        }))
                        .into_any_element(),
                );
            }
        }

        v_flex()
            .id("builder-tree")
            .w(px(240.))
            .h_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .track_scroll(&self.tree_scroll)
            .child(SharedString::from("SECTIONS"))
            .children(rows)
            .into_any_element()
    }

    /// One ↑ or ↓ button. The buttons are the shipped reorder mechanism, not a fallback.
    fn nudge_button(
        &self,
        id: String,
        glyph: &'static str,
        target: Selection,
        delta: isize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .child(SharedString::from(glyph))
            .on_click(cx.listener(move |this, _, window, cx| {
                match target {
                    Selection::Section(i) => this.move_section(i, delta, window, cx),
                    Selection::Field(idx) => this.move_field(idx, delta, window, cx),
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    /// The inspector. Reads the draft; editing arrives with the `medatat-core` draft
    /// validation the lead is preparing, so that the client and the Worker check identically.
    fn inspector(&self) -> AnyElement {
        let body: Vec<AnyElement> = match self.selection {
            None => vec![
                div()
                    .child(SharedString::from("Select a section or field."))
                    .into_any_element(),
            ],
            Some(Selection::Section(i)) => match self.draft.sections.get(i) {
                None => Vec::new(),
                Some(s) => vec![
                    row("Name", &s.title),
                    row("Columns", &s.columns.to_string()),
                    row("Ordinal", &s.ordinal.to_string()),
                    row("Fields", &s.fields.len().to_string()),
                ],
            },
            Some(Selection::Field(idx)) => match self.draft.field_at(idx) {
                None => Vec::new(),
                Some(sf) => {
                    let kind = WidgetKind::of(&sf.field.kind);
                    vec![
                        row("Label", &sf.label),
                        row("Key", &sf.field.key),
                        // Locked: a kind is never re-typed in place. Changing it creates a
                        // new field — the invariant that replaces form versioning.
                        row("Kind", &format!("{kind:?}  🔒")),
                        row("Required", if sf.required { "yes" } else { "no" }),
                        row("Col span", &sf.col_span.to_string()),
                    ]
                }
            },
        };

        v_flex()
            .w(px(260.))
            .h_full()
            .p_2()
            .gap_1()
            .child(SharedString::from("INSPECTOR"))
            .children(body)
            .into_any_element()
    }
}

fn row(label: &str, value: &str) -> AnyElement {
    h_flex()
        .w_full()
        .gap_2()
        .child(div().w(px(80.)).child(SharedString::from(label.to_string())))
        .child(
            div()
                .flex_1()
                .child(SharedString::from(value.to_string())),
        )
        .into_any_element()
}

impl Render for BuilderView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree(cx);
        let inspector = self.inspector();
        let preview = self.preview;

        v_flex()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key))
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .child(SharedString::from(self.draft.name.clone()))
                    .child(
                        div()
                            .id("preview-toggle")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(preview, |d| d.font_weight(FontWeight::BOLD))
                            .child(SharedString::from(if preview {
                                "Back to design"
                            } else {
                                "Preview as abstractor"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_preview(cx))),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .overflow_hidden()
                    .child(tree)
                    .child(div().flex_1().h_full().child(self.canvas.clone()))
                    .child(inspector),
            )
    }
}
