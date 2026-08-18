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
use crate::form::view::OnColumns;
use crate::mode::{RenderMode, Selection};
use crate::widgets::OnSelectField;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, FontWeight, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::builder::{
    ColumnChange, EditError, FieldEdit, classify, plan_column_change, unplaced_fields,
    validate_field, validate_form, validate_key, validate_placement,
};
use medatat_core::def::{FieldDef, FieldKind};
use medatat_core::{CaseId, CaseRev, ConfigRev, FieldId, FieldIdx, FormDef, WidgetKind};
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
    /// Fields that exist but sit in no section. Removing a placement puts a field here; its
    /// values are untouched and reappear if it is placed again.
    unplaced: Vec<Arc<FieldDef>>,
    /// A column reduction awaiting confirmation, with the list of spans it will narrow.
    /// Held rather than applied so the coordinator sees the consequence first.
    pending_columns: Option<(usize, ColumnChange)>,
    /// Everything `validate_form` found at the last save. All of it, not the first.
    errors: Vec<String>,
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
            unplaced: Vec::new(),
            pending_columns: None,
            errors: Vec::new(),
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

        let on_columns: OnColumns = {
            let this = cx.entity().downgrade();
            Rc::new(move |section, columns, window, cx| {
                let _ = this.update(cx, |b: &mut BuilderView, cx| {
                    b.set_columns(section, columns, window, cx);
                });
            })
        };

        let def = Arc::clone(draft);
        let store = Arc::clone(store);
        cx.new(|cx| {
            let mut v = FormView::new(def, scratch, CaseRev::ZERO, Vec::new(), store, window, cx);
            v.set_on_select(on_select);
            v.set_on_columns(on_columns);
            v.set_mode(RenderMode::Design { selected: None }, cx);
            v
        })
    }

    /// The 1 / 2 / 3 control on a section header (R12).
    ///
    /// Reducing the count must clamp every child `col_span`. That clamp is
    /// `FormDef::finalize`'s, not this function's — the UI deliberately does not pre-clamp,
    /// so it cannot disagree with the server's answer.
    fn set_columns(
        &mut self,
        section: usize,
        columns: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sec) = self.draft.sections.get(section) else {
            return;
        };
        match plan_column_change(sec, columns) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            // Narrowing loses span information, so show which fields it will narrow and
            // wait. Discovering it afterwards is the failure this avoids.
            Ok(plan) if !plan.clamped.is_empty() => {
                self.pending_columns = Some((section, plan));
                cx.notify();
            }
            Ok(_) => self.apply_columns(section, columns, window, cx),
        }
    }

    fn apply_columns(
        &mut self,
        section: usize,
        columns: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_columns = None;
        let Some(next) = with_columns(&self.draft, section, columns) else {
            return;
        };
        self.commit_structure(next, window, cx);
        self.set_selection(Some(Selection::Section(section)), cx);
    }

    /// Removes a placement. Values are never touched — the field lands in Unplaced.
    fn remove_placement(&mut self, idx: FieldIdx, window: &mut Window, cx: &mut Context<Self>) {
        let Some((next, removed)) = with_placement_removed(&self.draft, idx) else {
            return;
        };
        self.unplaced.push(removed);
        self.commit_structure(next, window, cx);
        self.set_selection(None, cx);
    }

    /// "Replace field": a new field of a different kind at the same position, with the
    /// original moved to Unplaced and its values left intact.
    fn replace_field(
        &mut self,
        idx: FieldIdx,
        kind: FieldKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match with_field_replaced(&self.draft, idx, kind) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok((next, displaced)) => {
                self.unplaced.push(displaced);
                self.commit_structure(next, window, cx);
            }
        }
    }

    fn set_span(&mut self, idx: FieldIdx, span: u8, window: &mut Window, cx: &mut Context<Self>) {
        match with_span(&self.draft, idx, span) {
            Err(e) => {
                // `EditError` already reads as a sentence written for a human.
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => self.commit_structure(next, window, cx),
        }
    }

    fn set_required(
        &mut self,
        idx: FieldIdx,
        required: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(next) = with_required(&self.draft, idx, required) {
            self.commit_structure(next, window, cx);
        }
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
        // Every problem, not the first — a coordinator should not have to play whack-a-mole
        // through six save attempts. The Worker runs the same function.
        self.errors = validate_form(&next)
            .iter()
            .map(|(_, e)| e.to_string())
            .collect();

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
    fn move_section(
        &mut self,
        section: usize,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((next, moved_to)) = with_section_moved(&self.draft, section, delta) else {
            return;
        };
        self.commit_structure(next, window, cx);
        self.set_selection(Some(Selection::Section(moved_to)), cx);
    }

    /// Moves a field within its section.
    fn move_field(
        &mut self,
        idx: FieldIdx,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((next, moved_id)) = with_field_moved(&self.draft, idx, delta) else {
            return;
        };
        self.commit_structure(next, window, cx);
        // `finalize` reassigned every index, so re-select the field by its stable id
        // rather than by the position it used to occupy.
        let moved = self.draft.idx_of(moved_id).map(Selection::Field);
        self.set_selection(moved, cx);
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

/// `(section index, position within section)` for a field.
fn locate(def: &FormDef, idx: FieldIdx) -> Option<(usize, usize)> {
    def.sections
        .iter()
        .enumerate()
        .find_map(|(s, sec)| sec.fields.iter().position(|f| f.idx == idx).map(|f| (s, f)))
}

/// Sets a section's column count (R12).
///
/// Returns `None` when nothing would change. The `col_span` clamp is deliberately **not**
/// applied here — `FormDef::finalize` owns it, so the UI cannot arrive at a different answer
/// than the server does.
pub fn with_columns(def: &FormDef, section: usize, columns: u8) -> Option<FormDef> {
    let mut next = def.clone();
    let s = next.sections.get_mut(section)?;
    if s.columns == columns {
        return None;
    }
    s.columns = columns;
    next.finalize();
    Some(next)
}

/// Moves a section one place. Returns the new definition and where it landed.
pub fn with_section_moved(def: &FormDef, section: usize, delta: isize) -> Option<(FormDef, usize)> {
    let target = usize::try_from(section as isize + delta).ok()?;
    if section >= def.sections.len() || target >= def.sections.len() {
        return None;
    }
    let mut next = def.clone();
    next.sections.swap(section, target);
    renumber(&mut next);
    next.finalize();
    Some((next, target))
}

/// Moves a field within its section. Returns the new definition and the moved field's id,
/// because `finalize` reassigns every `FieldIdx` and the old one no longer identifies it.
pub fn with_field_moved(
    def: &FormDef,
    idx: FieldIdx,
    delta: isize,
) -> Option<(FormDef, medatat_core::FieldId)> {
    let (s, f) = locate(def, idx)?;
    let target = usize::try_from(f as isize + delta).ok()?;
    if target >= def.sections[s].fields.len() {
        return None;
    }
    let mut next = def.clone();
    next.sections[s].fields.swap(f, target);
    let moved_id = next.sections[s].fields[target].field.field_id;
    renumber(&mut next);
    next.finalize();
    Some((next, moved_id))
}

/// Sets a placement's column span (R12).
///
/// `medatat_core::builder::validate_placement` decides whether it is legal; this function
/// does not second-guess it, so the client refuses exactly what the Worker refuses.
pub fn with_span(def: &FormDef, idx: FieldIdx, span: u8) -> Result<FormDef, EditError> {
    let (s, f) = locate(def, idx).ok_or(EditError::EmptyLabel)?;
    let columns = def.sections[s].columns;
    let label = def.sections[s].fields[f].label.clone();
    validate_placement(&label, span, columns)?;

    let mut next = def.clone();
    next.sections[s].fields[f].col_span = span;
    next.finalize();
    Ok(next)
}

/// Toggles a placement's required flag.
pub fn with_required(def: &FormDef, idx: FieldIdx, required: bool) -> Option<FormDef> {
    let (s, f) = locate(def, idx)?;
    let mut next = def.clone();
    next.sections[s].fields[f].required = required;
    next.finalize();
    Some(next)
}

/// Removes a *placement*, never values.
///
/// The `FieldDef` comes back so the caller can put it in the Unplaced drawer. Its stored
/// values are untouched and reappear if the field is ever placed again — that is the whole
/// reason removing a field from a form is safe enough to do without a confirmation.
pub fn with_placement_removed(def: &FormDef, idx: FieldIdx) -> Option<(FormDef, Arc<FieldDef>)> {
    let (s, f) = locate(def, idx)?;
    let mut next = def.clone();
    let removed = next.sections[s].fields.remove(f);
    renumber(&mut next);
    next.finalize();
    Some((next, removed.field))
}

/// The "Replace field" action, which is what `classify` returning `NeedsReplacement` means.
///
/// A `kind` change reinterprets every stored value, so it is never applied in place. This
/// creates a **new** field with a new id at the same position and hands back the original
/// for the Unplaced drawer, where its values remain viewable.
pub fn with_field_replaced(
    def: &FormDef,
    idx: FieldIdx,
    new_kind: FieldKind,
) -> Result<(FormDef, Arc<FieldDef>), EditError> {
    let (s, f) = locate(def, idx).ok_or(EditError::EmptyLabel)?;
    let old = Arc::clone(&def.sections[s].fields[f].field);

    let key = next_key(def, &old.key);
    validate_key(&key)?;
    let replacement = FieldDef {
        field_id: FieldId::new(),
        key,
        kind: new_kind,
    };
    validate_field(&replacement)?;
    // Belt and braces: the whole point is that this is *not* an in-place edit.
    debug_assert!(matches!(
        classify(&old, &replacement),
        FieldEdit::NeedsReplacement
    ));

    let mut next = def.clone();
    next.sections[s].fields[f].field = Arc::new(replacement);
    next.finalize();
    Ok((next, old))
}

/// A free key derived from an existing one: `dob` → `dob_2`, `dob_3`, …
fn next_key(def: &FormDef, base: &str) -> String {
    let stem = base
        .rsplit_once('_')
        .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty())
        .map(|(head, _)| head)
        .unwrap_or(base);
    let taken: std::collections::HashSet<&str> =
        def.iter_fields().map(|f| f.field.key.as_str()).collect();
    (2..)
        .map(|n| format!("{stem}_{n}"))
        .find(|k| !taken.contains(k.as_str()))
        // 64 chars is the key limit, so truncate the stem rather than emit an illegal key.
        .unwrap_or_else(|| stem.chars().take(60).collect::<String>() + "_2")
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
                    .child(self.nudge_button(
                        format!("sec-up-{s}"),
                        "↑",
                        Selection::Section(s),
                        -1,
                        cx,
                    ))
                    .child(self.nudge_button(
                        format!("sec-dn-{s}"),
                        "↓",
                        Selection::Section(s),
                        1,
                        cx,
                    ))
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
                        .child(
                            div()
                                .flex_1()
                                .child(SharedString::from(field.label.clone())),
                        )
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

    /// The inspector. Every control routes through `medatat_core::builder`, so the client
    /// refuses exactly what the Worker refuses.
    fn inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let body: Vec<AnyElement> = match self.selection {
            None => vec![text_row("Select a section or field.")],
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
                    let columns = self.draft.section_of(idx).map(|s| s.columns).unwrap_or(1);
                    vec![
                        row("Label", &sf.label),
                        row("Key", &sf.field.key),
                        // Locked. A kind is never re-typed in place — changing it creates a
                        // new field, which is the invariant that replaces form versioning.
                        row("Kind", &format!("{kind:?}  🔒")),
                        self.required_control(idx, sf.required, cx),
                        self.span_control(idx, sf.col_span, columns, cx),
                        self.replace_control(idx, &sf.field.kind, cx),
                        self.remove_control(idx, cx),
                    ]
                }
            },
        };

        v_flex()
            .w(px(280.))
            .h_full()
            .p_2()
            .gap_1()
            .child(SharedString::from("INSPECTOR"))
            .children(body)
            .children(self.pending_columns_notice(cx))
            .children(self.error_strip())
            .children(self.unplaced_drawer())
            .into_any_element()
    }

    fn required_control(
        &self,
        idx: FieldIdx,
        required: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .gap_2()
            .child(div().w(px(80.)).child(SharedString::from("Required")))
            .child(
                div()
                    .id("req-toggle")
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .child(SharedString::from(if required { "[x]" } else { "[ ]" }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_required(idx, !required, window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Col span 1 / 2 / 3, offering only what the section can hold (R12).
    fn span_control(
        &self,
        idx: FieldIdx,
        span: u8,
        columns: u8,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .gap_2()
            .child(div().w(px(80.)).child(SharedString::from("Col span")))
            .children((1..=columns).map(|n| {
                div()
                    .id(SharedString::from(format!("span-{}-{n}", idx.0)))
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(n == span, |d| d.font_weight(FontWeight::BOLD))
                    .child(SharedString::from(n.to_string()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_span(idx, n, window, cx);
                    }))
            }))
            .into_any_element()
    }

    /// "Replace field" (`classify` → `NeedsReplacement`). Not an error dialog: it is the
    /// supported way to change a field's type.
    fn replace_control(
        &self,
        idx: FieldIdx,
        current: &FieldKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current_tag = current.tag();
        v_flex()
            .w_full()
            .gap_1()
            .child(SharedString::from("Replace field with…"))
            .child(h_flex().w_full().gap_1().flex_wrap().children(
                replacement_kinds().into_iter().filter_map(|(tag, kind)| {
                    // Replacing a kind with itself is an in-place edit, not a
                    // replacement, so it is not offered.
                    (tag != current_tag).then(|| {
                        div()
                            .id(SharedString::from(format!("repl-{}-{tag}", idx.0)))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .child(SharedString::from(tag))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.replace_field(idx, kind.clone(), window, cx);
                            }))
                    })
                }),
            ))
            .into_any_element()
    }

    fn remove_control(&self, idx: FieldIdx, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("remove-placement")
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .child(SharedString::from("Remove from form"))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.remove_placement(idx, window, cx);
            }))
            .into_any_element()
    }

    /// The clamp preview from `plan_column_change`, shown before anything is applied.
    fn pending_columns_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (section, plan) = self.pending_columns.as_ref()?;
        let section = *section;
        let columns = plan.columns;
        let lines: Vec<AnyElement> = plan
            .clamped
            .iter()
            .map(|(id, old, new)| {
                let label = self
                    .draft
                    .idx_of(*id)
                    .and_then(|i| self.draft.field_at(i))
                    .map(|sf| sf.label.clone())
                    .unwrap_or_else(|| String::from("(field)"));
                text_row(&format!("{label}: span {old} → {new}"))
            })
            .collect();

        Some(
            v_flex()
                .w_full()
                .mt_2()
                .p_1()
                .gap_1()
                .rounded_sm()
                .border_1()
                .child(SharedString::from(format!(
                    "Narrowing to {columns} column{} will clamp:",
                    if columns == 1 { "" } else { "s" }
                )))
                .children(lines)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .id("cols-apply")
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .child(SharedString::from("Apply"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.apply_columns(section, columns, window, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("cols-cancel")
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .child(SharedString::from("Cancel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pending_columns = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Everything `validate_form` found, at once.
    fn error_strip(&self) -> Option<AnyElement> {
        if self.errors.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .mt_2()
                .p_1()
                .gap_1()
                .rounded_sm()
                .border_1()
                .child(SharedString::from(format!(
                    "{} problem(s)",
                    self.errors.len()
                )))
                // `EditError`'s Display is already written for a human; rendering it
                // verbatim is what keeps the client and the Worker saying the same thing.
                .children(self.errors.iter().map(|e| text_row(e)))
                .into_any_element(),
        )
    }

    /// Fields that exist but sit in no section. Their values are intact.
    fn unplaced_drawer(&self) -> Option<AnyElement> {
        let all: Vec<FieldDef> = self.unplaced.iter().map(|f| (**f).clone()).collect();
        let names: Vec<AnyElement> = unplaced_fields(&all, &self.draft)
            .map(|f| text_row(&format!("{}  ({})", f.key, f.kind.tag())))
            .collect();
        if names.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .mt_2()
                .p_1()
                .gap_1()
                .rounded_sm()
                .border_1()
                .child(SharedString::from("Unplaced fields"))
                .child(text_row("Values are kept and reappear if placed again."))
                .children(names)
                .into_any_element(),
        )
    }
}

/// The kinds "Replace field" can produce, with sensible defaults for their config.
fn replacement_kinds() -> Vec<(&'static str, FieldKind)> {
    vec![
        ("text", FieldKind::Text { max_len: None }),
        (
            "numeric",
            FieldKind::Numeric {
                min: None,
                max: None,
                scale: 0,
            },
        ),
        ("date", FieldKind::Date),
        ("time", FieldKind::Time),
        (
            "textarea",
            FieldKind::Textarea {
                rows: 4,
                max_len: None,
            },
        ),
    ]
}

fn text_row(value: &str) -> AnyElement {
    div()
        .w_full()
        .child(SharedString::from(value.to_string()))
        .into_any_element()
}

fn row(label: &str, value: &str) -> AnyElement {
    h_flex()
        .w_full()
        .gap_2()
        .child(
            div()
                .w(px(80.))
                .child(SharedString::from(label.to_string())),
        )
        .child(div().flex_1().child(SharedString::from(value.to_string())))
        .into_any_element()
}

impl Render for BuilderView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree(cx);
        let inspector = self.inspector(cx);
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

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::def::{FieldDef, FieldKind, SectionDef, SectionField};
    use medatat_core::{FieldId, FormId, SectionId};

    fn field(key: &str, col_span: u8) -> SectionField {
        SectionField {
            idx: FieldIdx(0),
            field: Arc::new(FieldDef {
                field_id: FieldId::new(),
                key: key.into(),
                kind: FieldKind::Text { max_len: None },
            }),
            label: key.into(),
            ordinal: 0,
            col_span,
            required: false,
        }
    }

    fn section(title: &str, columns: u8, fields: Vec<SectionField>) -> SectionDef {
        SectionDef {
            section_id: SectionId::new(),
            title: title.into(),
            ordinal: 0,
            columns,
            default_collapsed: false,
            fields,
        }
    }

    fn form() -> FormDef {
        let mut d = FormDef::new(
            FormId::new(),
            "Intake",
            vec![
                section("Demographics", 2, vec![field("a", 1), field("b", 1)]),
                section("Vitals", 3, vec![field("c", 3), field("d", 1)]),
            ],
        );
        renumber(&mut d);
        d.finalize();
        d
    }

    #[test]
    fn m5_reducing_columns_clamps_the_spanning_field() {
        // M5 acceptance 8: "Vitals" goes 3 → 2 and the 3-span field must clamp rather
        // than overflow. The clamp is `FormDef::finalize`'s, not the builder's.
        let before = form();
        assert_eq!(before.sections[1].fields[0].col_span, 3);

        let after = with_columns(&before, 1, 2).expect("columns changed");
        assert_eq!(after.sections[1].columns, 2);
        assert_eq!(after.sections[1].fields[0].col_span, 2, "must clamp to 2");
        assert_eq!(after.sections[1].fields[1].col_span, 1, "untouched");
    }

    #[test]
    fn m5_widening_columns_does_not_re_expand_a_clamped_span() {
        // Clamping is lossy on purpose: going back to 3 must not resurrect the old 3-span,
        // or a coordinator's deliberate narrowing would silently undo itself.
        let narrowed = with_columns(&form(), 1, 2).unwrap();
        let widened = with_columns(&narrowed, 1, 3).unwrap();
        assert_eq!(widened.sections[1].fields[0].col_span, 2);
    }

    #[test]
    fn setting_the_same_column_count_is_not_a_change() {
        assert!(with_columns(&form(), 0, 2).is_none());
    }

    #[test]
    fn sections_reorder_and_renumber() {
        let before = form();
        let (after, landed) = with_section_moved(&before, 0, 1).expect("moved");
        assert_eq!(landed, 1);
        assert_eq!(after.sections[0].title, "Vitals");
        assert_eq!(after.sections[1].title, "Demographics");
        // Ordinals follow the new order, so `finalize`'s sort is stable next time.
        assert_eq!(after.sections[0].ordinal, 0);
        assert_eq!(after.sections[1].ordinal, 1);
    }

    #[test]
    fn reordering_past_either_end_is_refused() {
        let d = form();
        assert!(with_section_moved(&d, 0, -1).is_none());
        assert!(with_section_moved(&d, 1, 1).is_none());
    }

    #[test]
    fn moving_a_field_keeps_its_identity_across_reindexing() {
        let before = form();
        let first = before.sections[0].fields[0].field.field_id;
        let idx = before.sections[0].fields[0].idx;

        let (after, moved_id) = with_field_moved(&before, idx, 1).expect("moved");
        assert_eq!(moved_id, first, "the id is what survives, not the index");
        assert_eq!(after.sections[0].fields[1].field.field_id, first);
        // R4's invariant: the id is stable, so it still resolves after reindexing.
        assert_eq!(after.idx_of(first), Some(after.sections[0].fields[1].idx));
    }

    #[test]
    fn m5_removing_a_placement_returns_the_field_and_keeps_the_rest() {
        // The spec's hard rule: removing a placement removes the *placement*. The field
        // definition comes back so it can go to Unplaced; nothing here touches values.
        let before = form();
        let idx = before.sections[0].fields[0].idx;
        let key = before.sections[0].fields[0].field.key.clone();

        let (after, removed) = with_placement_removed(&before, idx).expect("removed");
        assert_eq!(removed.key, key, "the field definition survives removal");
        assert_eq!(after.sections[0].fields.len(), 1);
        assert_eq!(after.field_count(), before.field_count() - 1);
        assert!(after.idx_of(removed.field_id).is_none(), "no longer placed");
    }

    #[test]
    fn m5_replace_field_makes_a_new_id_and_displaces_the_original() {
        // M5 acceptance 9. A kind change is never in place, so the replacement is a new
        // field at the same position and the original goes to Unplaced with its values.
        let before = form();
        let idx = before.sections[0].fields[0].idx;
        let original = Arc::clone(&before.sections[0].fields[0].field);

        let (after, displaced) = with_field_replaced(
            &before,
            idx,
            FieldKind::Numeric {
                min: None,
                max: None,
                scale: 2,
            },
        )
        .expect("replaced");

        assert_eq!(displaced.field_id, original.field_id);
        let placed = &after.sections[0].fields[0].field;
        assert_ne!(
            placed.field_id, original.field_id,
            "a new id, never a re-type"
        );
        assert_ne!(placed.key, original.key, "keys stay unique");
        assert!(matches!(placed.kind, FieldKind::Numeric { .. }));
        // Position is preserved, which is what makes Replace feel like an edit.
        assert_eq!(
            after.sections[0].fields[0].label,
            before.sections[0].fields[0].label
        );
        assert_eq!(after.field_count(), before.field_count());
    }

    #[test]
    fn m5_replacement_keys_do_not_collide_on_repeat() {
        let d = form();
        let idx = d.sections[0].fields[0].idx;
        let (once, _) = with_field_replaced(&d, idx, FieldKind::Date).unwrap();
        let idx2 = once.sections[0].fields[0].idx;
        let (twice, _) = with_field_replaced(&once, idx2, FieldKind::Time).unwrap();

        let keys: Vec<&str> = twice.iter_fields().map(|f| f.field.key.as_str()).collect();
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len(), "keys must stay unique: {keys:?}");
    }

    #[test]
    fn m5_span_beyond_the_section_is_refused_by_core() {
        // The refusal is `medatat_core::builder::validate_placement`'s, not ours, so the
        // client and the Worker cannot disagree about what is legal.
        let d = form();
        let idx = d.sections[0].fields[0].idx; // a 2-column section
        assert!(with_span(&d, idx, 3).is_err());
        assert_eq!(
            with_span(&d, idx, 2).unwrap().sections[0].fields[0].col_span,
            2
        );
    }

    #[test]
    fn m5_column_plan_reports_what_will_clamp_without_applying_it() {
        // `plan_column_change` returns rather than applies, so the coordinator sees the
        // consequence before committing to it.
        let d = form();
        let plan = plan_column_change(&d.sections[1], 2).expect("planned");
        assert_eq!(plan.clamped.len(), 1, "only the 3-span field narrows");
        assert_eq!(plan.clamped[0].1, 3);
        assert_eq!(plan.clamped[0].2, 2);
        // Nothing changed on the form itself.
        assert_eq!(d.sections[1].columns, 3);
        assert_eq!(d.sections[1].fields[0].col_span, 3);
    }

    #[test]
    fn a_field_does_not_move_out_of_its_section() {
        let d = form();
        let last_of_first = d.sections[0].fields[1].idx;
        assert!(
            with_field_moved(&d, last_of_first, 1).is_none(),
            "moving past the end of a section must not spill into the next one"
        );
    }
}
