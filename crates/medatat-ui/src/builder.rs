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
use crate::widgets::{self, LineInput, OnSelectField};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, FontWeight, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::builder::{
    ColumnChange, EditError, FieldEdit, classify, plan_column_change, unplaced_fields,
    validate_field, validate_form, validate_key, validate_placement,
};
use medatat_core::def::{FieldDef, FieldKind, FieldOption, SectionDef, SectionField};
use medatat_core::parse_decimal;
use medatat_core::{
    CaseId, CaseRev, ConfigRev, FieldId, FieldIdx, FormDef, OptionCode, SectionId, WidgetKind,
};
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
    /// Every field the store knows about, placed or not. Refreshed on each commit; the
    /// Unplaced drawer is derived from it rather than tracked by hand, so it survives a
    /// restart and cannot drift out of step with what was actually saved.
    all_fields: Vec<FieldDef>,
    /// A column reduction awaiting confirmation, with the list of spans it will narrow.
    /// Held rather than applied so the coordinator sees the consequence first.
    pending_columns: Option<(usize, ColumnChange)>,
    /// Everything `validate_form` found at the last save. All of it, not the first.
    errors: Vec<String>,
    /// Rename box for the current selection, rebuilt whenever the selection changes so it
    /// always shows the selected thing's own text.
    rename: Option<LineInput>,
    rename_sub: Option<Subscription>,
    /// New-field name box, and the kind the next Add will create.
    new_name: LineInput,
    new_name_sub: Option<Subscription>,
    new_kind: FieldKind,
    pending_new_name: String,
    /// Kind-specific config boxes for the selected field, rebuilt with the selection.
    /// `values` mirrors them so a commit can rebuild the whole kind, not just one field.
    config: Vec<(&'static str, LineInput)>,
    config_values: Vec<String>,
    config_subs: Vec<Subscription>,
    /// `(code, label)` boxes for a radio or select, plus their mirrored text.
    options: Vec<(LineInput, LineInput)>,
    option_values: Vec<(String, String)>,
    option_subs: Vec<Subscription>,
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
        let mut this = BuilderView {
            store,
            draft,
            canvas,
            selection: None,
            preview: false,
            focus: cx.focus_handle(),
            tree_scroll: ScrollHandle::new(),
            scratch,
            all_fields: Vec::new(),
            pending_columns: None,
            errors: Vec::new(),
            rename: None,
            rename_sub: None,
            new_name: LineInput::new("New section or field name…", window, cx),
            new_name_sub: None,
            new_kind: FieldKind::Text { max_len: None },
            pending_new_name: String::new(),
            config: Vec::new(),
            config_values: Vec::new(),
            config_subs: Vec::new(),
            options: Vec::new(),
            option_values: Vec::new(),
            option_subs: Vec::new(),
        };
        this.wire_new_name(window, cx);
        this.all_fields = this.store.all_fields().unwrap_or_default();
        this
    }

    /// Subscribes to the new-name box. Kept as plain state rather than read on demand so
    /// the Add buttons do not need a `&App` at click time.
    fn wire_new_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_name_sub = Some(self.new_name.subscribe(
            window,
            cx,
            |this: &mut Self, text, _, _| {
                // No notify: this is a keystroke, and nothing on screen depends on it
                // until a button is pressed.
                this.pending_new_name = text;
            },
        ));
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
        let Some((next, _removed)) = with_placement_removed(&self.draft, idx) else {
            return;
        };
        // No bookkeeping: `commit_structure` persists the field, and the drawer is derived
        // from what the store holds.
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
            Ok((next, _displaced)) => self.commit_structure(next, window, cx),
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
        self.rename = None;
        self.rename_sub = None;
        self.config.clear();
        self.config_values.clear();
        self.config_subs.clear();
        self.options.clear();
        self.option_values.clear();
        self.option_subs.clear();
        cx.notify();
    }

    /// Builds the rename box for the current selection, on demand.
    ///
    /// It commits on blur or Enter rather than per keystroke, so a half-typed name is never
    /// validated at the user and never written to the store.
    fn ensure_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.is_some() {
            return;
        }
        let current = match self.selection {
            Some(Selection::Section(i)) => self.draft.sections.get(i).map(|s| s.title.clone()),
            Some(Selection::Field(idx)) => self.draft.field_at(idx).map(|sf| sf.label.clone()),
            None => None,
        };
        let Some(current) = current else { return };
        let input = LineInput::with_value("Name", &current, window, cx);
        let selection = self.selection;
        self.rename_sub =
            Some(
                input.subscribe_commit(window, cx, move |this: &mut Self, text, window, cx| {
                    this.apply_rename(selection, &text, window, cx);
                }),
            );
        self.rename = Some(input);
    }

    fn apply_rename(
        &mut self,
        selection: Option<Selection>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = match selection {
            Some(Selection::Section(i)) => with_section_title(&self.draft, i, text),
            Some(Selection::Field(idx)) => with_label(&self.draft, idx, text),
            None => return,
        };
        match result {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => {
                self.commit_structure(next, window, cx);
                self.set_selection(selection, cx);
            }
        }
    }

    /// Builds the kind-specific config boxes for the selected field.
    ///
    /// Each commits on blur or Enter and then rebuilds the *whole* kind from the mirrored
    /// values, because `FieldKind` is an enum: there is no way to set one part of it.
    fn ensure_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.config.is_empty() || !self.options.is_empty() {
            return;
        }
        let Some(Selection::Field(idx)) = self.selection else {
            return;
        };
        let Some(sf) = self.draft.field_at(idx) else {
            return;
        };

        let kind = sf.field.kind.clone();
        let specs: Vec<(&'static str, String)> = match &kind {
            FieldKind::Text { max_len } => vec![("Max length", opt_num(*max_len))],
            FieldKind::Textarea { rows, max_len } => {
                vec![
                    ("Rows", rows.to_string()),
                    ("Max length", opt_num(*max_len)),
                ]
            }
            FieldKind::Numeric { min, max, scale } => vec![
                ("Min", min.map(|d| d.to_string()).unwrap_or_default()),
                ("Max", max.map(|d| d.to_string()).unwrap_or_default()),
                ("Decimals", scale.to_string()),
            ],
            FieldKind::Radio { options } | FieldKind::Select { options, .. } => {
                // Cloned so the borrow of `self.draft` ends before the rows are built.
                let options = Arc::clone(options);
                self.build_option_rows(idx, &options, window, cx);
                return;
            }
            // Date and time carry no configuration: time is always 24-hour (R8).
            FieldKind::Date | FieldKind::Time => Vec::new(),
        };

        for (i, (label, value)) in specs.into_iter().enumerate() {
            let input = LineInput::with_value(label, &value, window, cx);
            self.config_subs.push(input.subscribe_commit(
                window,
                cx,
                move |this: &mut Self, text, window, cx| {
                    this.commit_config(idx, i, text, window, cx);
                },
            ));
            self.config_values.push(value);
            self.config.push((label, input));
        }
    }

    fn build_option_rows(
        &mut self,
        idx: FieldIdx,
        options: &[FieldOption],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One spare row at the end, so adding an option needs no separate button.
        let mut values: Vec<(String, String)> = options
            .iter()
            .map(|o| (o.code.0.clone(), o.label.clone()))
            .collect();
        values.push((String::new(), String::new()));

        for (i, (code, label)) in values.iter().enumerate() {
            let c = LineInput::with_value("code", code, window, cx);
            let l = LineInput::with_value("label", label, window, cx);
            self.option_subs.push(c.subscribe_commit(
                window,
                cx,
                move |this: &mut Self, text, window, cx| {
                    this.commit_option(idx, i, Some(text), None, window, cx);
                },
            ));
            self.option_subs.push(l.subscribe_commit(
                window,
                cx,
                move |this: &mut Self, text, window, cx| {
                    this.commit_option(idx, i, None, Some(text), window, cx);
                },
            ));
            self.options.push((c, l));
        }
        self.option_values = values;
    }

    /// Rebuilds the field's kind from every config box and applies it.
    fn commit_config(
        &mut self,
        idx: FieldIdx,
        which: usize,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(slot) = self.config_values.get_mut(which) {
            if *slot == text {
                return;
            }
            *slot = text;
        }
        let Some(sf) = self.draft.field_at(idx) else {
            return;
        };
        let v = |i: usize| self.config_values.get(i).cloned().unwrap_or_default();

        let kind = match &sf.field.kind {
            FieldKind::Text { .. } => FieldKind::Text {
                max_len: parse_opt_num(&v(0)),
            },
            FieldKind::Textarea { .. } => FieldKind::Textarea {
                rows: v(0).trim().parse().unwrap_or(1),
                max_len: parse_opt_num(&v(1)),
            },
            FieldKind::Numeric { .. } => FieldKind::Numeric {
                min: parse_decimal(v(0).trim()).ok(),
                max: parse_decimal(v(1).trim()).ok(),
                scale: v(2).trim().parse().unwrap_or(0),
            },
            _ => return,
        };
        self.apply_kind(idx, kind, window, cx);
    }

    fn commit_option(
        &mut self,
        idx: FieldIdx,
        row: usize,
        code: Option<String>,
        label: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.option_values.get_mut(row) else {
            return;
        };
        if let Some(c) = code {
            if slot.0 == c {
                return;
            }
            slot.0 = c;
        }
        if let Some(l) = label {
            if slot.1 == l {
                return;
            }
            slot.1 = l;
        }
        let pairs = self.option_values.clone();
        match with_options(&self.draft, idx, &pairs) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => {
                self.commit_structure(next, window, cx);
                self.set_selection(Some(Selection::Field(idx)), cx);
            }
        }
    }

    fn apply_kind(
        &mut self,
        idx: FieldIdx,
        kind: FieldKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match with_kind_config(&self.draft, idx, kind) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => self.commit_structure(next, window, cx),
        }
    }

    fn toggle_searchable(&mut self, idx: FieldIdx, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sf) = self.draft.field_at(idx) else {
            return;
        };
        let FieldKind::Select { searchable, .. } = sf.field.kind else {
            return;
        };
        match with_searchable(&self.draft, idx, !searchable) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => self.commit_structure(next, window, cx),
        }
    }

    /// Adds a section named from the new-name box.
    fn add_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.pending_new_name.trim().to_string();
        match with_section_added(&self.draft, &name, 1) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => {
                let landed = next.sections.len() - 1;
                self.commit_structure(next, window, cx);
                self.set_selection(Some(Selection::Section(landed)), cx);
            }
        }
    }

    /// Adds a field of `new_kind` to the selected section.
    fn add_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let section = self.current_section();
        let label = self.pending_new_name.trim().to_string();
        let key = key_from_label(&self.draft, &label);
        match with_field_added(&self.draft, section, &label, &key, self.new_kind.clone()) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            Ok(next) => self.commit_structure(next, window, cx),
        }
    }

    /// Puts an unplaced field back on the form, values and all.
    fn place_field(&mut self, which: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = unplaced_fields(&self.all_fields, &self.draft)
            .nth(which)
            .cloned()
            .map(Arc::new)
        else {
            return;
        };
        let section = self.current_section();
        let label = field.key.clone();
        match with_field_placed(&self.draft, section, field, &label) {
            Err(e) => {
                self.errors = vec![e.to_string()];
                cx.notify();
            }
            // No removal bookkeeping: the field is now placed, so `unplaced_fields`
            // stops returning it on its own.
            Ok(next) => self.commit_structure(next, window, cx),
        }
    }

    fn remove_section(&mut self, section: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((next, _orphans)) = with_section_removed(&self.draft, section) else {
            return;
        };
        // Cascades placements, never values: the fields stay in the store and surface in
        // the Unplaced drawer.
        self.commit_structure(next, window, cx);
        self.set_selection(None, cx);
    }

    /// Which section a new field lands in: the selected one, or the selected field's.
    fn current_section(&self) -> usize {
        match self.selection {
            Some(Selection::Section(i)) => i,
            Some(Selection::Field(idx)) => locate(&self.draft, idx).map(|(s, _)| s).unwrap_or(0),
            None => 0,
        }
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

/// Replaces a field's kind *configuration* — max length, rows, numeric bounds, options —
/// while leaving the kind itself alone.
///
/// `classify` is the guard, not a comment: if the new kind has a different tag this is a
/// replacement, not a config edit, and it is refused here rather than silently
/// reinterpreting every stored value.
pub fn with_kind_config(
    def: &FormDef,
    idx: FieldIdx,
    kind: FieldKind,
) -> Result<FormDef, EditError> {
    let (s, f) = locate(def, idx).ok_or(EditError::EmptyLabel)?;
    let old = &def.sections[s].fields[f].field;
    let candidate = FieldDef {
        field_id: old.field_id,
        key: old.key.clone(),
        kind,
    };
    if matches!(classify(old, &candidate), FieldEdit::NeedsReplacement) {
        return Err(EditError::KindIsImmutable);
    }
    validate_field(&candidate)?;

    let mut next = def.clone();
    next.sections[s].fields[f].field = Arc::new(candidate);
    next.finalize();
    Ok(next)
}

/// Rewrites a radio or select's option list (R9, R10).
///
/// Removing an option that is in use does **not** delete stored values — they stay in
/// `field_value` under their code and render as unknown. A coordinator correcting a mistake
/// must be able to, so core warns through `validate_field` and nothing here blocks it.
pub fn with_options(
    def: &FormDef,
    idx: FieldIdx,
    pairs: &[(String, String)],
) -> Result<FormDef, EditError> {
    let sf = def.field_at(idx).ok_or(EditError::EmptyLabel)?;
    let options: Arc<[FieldOption]> = pairs
        .iter()
        .enumerate()
        .filter(|(_, (code, label))| !(code.trim().is_empty() && label.trim().is_empty()))
        .map(|(i, (code, label))| FieldOption {
            code: OptionCode::new(code.trim()),
            label: label.trim().to_string(),
            ordinal: i as i32,
        })
        .collect::<Vec<_>>()
        .into();

    let kind = match &sf.field.kind {
        FieldKind::Radio { .. } => FieldKind::Radio { options },
        FieldKind::Select { searchable, .. } => FieldKind::Select {
            options,
            searchable: *searchable,
        },
        // Not an option-bearing kind; nothing to write.
        _ => return Err(EditError::KindIsImmutable),
    };
    with_kind_config(def, idx, kind)
}

/// Toggles a select's searchable flag (R10).
pub fn with_searchable(
    def: &FormDef,
    idx: FieldIdx,
    searchable: bool,
) -> Result<FormDef, EditError> {
    let sf = def.field_at(idx).ok_or(EditError::EmptyLabel)?;
    let FieldKind::Select { options, .. } = &sf.field.kind else {
        return Err(EditError::KindIsImmutable);
    };
    let kind = FieldKind::Select {
        options: Arc::clone(options),
        searchable,
    };
    with_kind_config(def, idx, kind)
}

/// `Option<u32>` as a box's text: empty means "no limit".
fn opt_num(v: Option<u32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn parse_opt_num(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() { None } else { t.parse().ok() }
}

/// Derives a field key from a human label: "Date of birth" → `date_of_birth`.
///
/// Keys are immutable after creation and must match `^[a-z][a-z0-9_]{0,63}$`, so a
/// coordinator should never have to think about them. `validate_key` in core is still the
/// judge — this only tries to produce something it will accept.
fn key_from_label(def: &FormDef, label: &str) -> String {
    let mut key: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Must start with a letter, and underscores must not pile up at either end.
    key = key.trim_matches('_').to_string();
    while key.contains("__") {
        key = key.replace("__", "_");
    }
    if !key.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
        key.insert(0, 'f');
    }
    key.truncate(64);
    if def.iter_fields().any(|f| f.field.key == key) {
        next_key(def, &key)
    } else {
        key
    }
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

/// Renames a placement's label. Per-placement, not per-field: the same field can carry a
/// different label in a different form.
pub fn with_label(def: &FormDef, idx: FieldIdx, label: &str) -> Result<FormDef, EditError> {
    let (s, f) = locate(def, idx).ok_or(EditError::EmptyLabel)?;
    let columns = def.sections[s].columns;
    let span = def.sections[s].fields[f].col_span;
    validate_placement(label, span, columns)?;

    let mut next = def.clone();
    next.sections[s].fields[f].label = label.trim().to_string();
    next.finalize();
    Ok(next)
}

/// Renames a section.
pub fn with_section_title(
    def: &FormDef,
    section: usize,
    title: &str,
) -> Result<FormDef, EditError> {
    if title.trim().is_empty() {
        return Err(EditError::EmptyLabel);
    }
    let mut next = def.clone();
    next.sections
        .get_mut(section)
        .ok_or(EditError::EmptyLabel)?
        .title = title.trim().to_string();
    next.finalize();
    Ok(next)
}

/// Appends an empty section.
pub fn with_section_added(def: &FormDef, title: &str, columns: u8) -> Result<FormDef, EditError> {
    if title.trim().is_empty() {
        return Err(EditError::EmptyLabel);
    }
    if !(1..=3).contains(&columns) {
        return Err(EditError::BadColumnCount);
    }
    let mut next = def.clone();
    next.sections.push(SectionDef {
        section_id: SectionId::new(),
        title: title.trim().to_string(),
        ordinal: next.sections.len() as i32,
        columns,
        default_collapsed: false,
        fields: Vec::new(),
    });
    renumber(&mut next);
    next.finalize();
    Ok(next)
}

/// Removes a section, returning the fields it held so they land in Unplaced.
///
/// Cascades placements and **never** values, which is why this needs no confirmation: a
/// section deleted by mistake costs the coordinator some re-placing, never data.
pub fn with_section_removed(
    def: &FormDef,
    section: usize,
) -> Option<(FormDef, Vec<Arc<FieldDef>>)> {
    if section >= def.sections.len() {
        return None;
    }
    let mut next = def.clone();
    let removed = next.sections.remove(section);
    let orphans = removed.fields.into_iter().map(|f| f.field).collect();
    renumber(&mut next);
    next.finalize();
    Some((next, orphans))
}

/// Adds a field to a section. The label and key are validated by core before anything moves.
pub fn with_field_added(
    def: &FormDef,
    section: usize,
    label: &str,
    key: &str,
    kind: FieldKind,
) -> Result<FormDef, EditError> {
    let columns = def
        .sections
        .get(section)
        .ok_or(EditError::BadColumnCount)?
        .columns;
    validate_placement(label, 1, columns)?;
    validate_key(key)?;
    if def.iter_fields().any(|f| f.field.key == key) {
        return Err(EditError::DuplicateKey(key.to_string()));
    }
    let field = FieldDef {
        field_id: FieldId::new(),
        key: key.to_string(),
        kind,
    };
    validate_field(&field)?;

    let mut next = def.clone();
    let ordinal = next.sections[section].fields.len() as i32;
    next.sections[section].fields.push(SectionField {
        idx: FieldIdx(0),
        field: Arc::new(field),
        label: label.trim().to_string(),
        ordinal,
        col_span: 1,
        required: false,
    });
    renumber(&mut next);
    next.finalize();
    Ok(next)
}

/// Places an unplaced field back into a section. Its stored values reappear with it — that
/// is the whole point of never deleting them on unplace.
pub fn with_field_placed(
    def: &FormDef,
    section: usize,
    field: Arc<FieldDef>,
    label: &str,
) -> Result<FormDef, EditError> {
    let columns = def
        .sections
        .get(section)
        .ok_or(EditError::BadColumnCount)?
        .columns;
    validate_placement(label, 1, columns)?;

    let mut next = def.clone();
    let ordinal = next.sections[section].fields.len() as i32;
    next.sections[section].fields.push(SectionField {
        idx: FieldIdx(0),
        field,
        label: label.trim().to_string(),
        ordinal,
        col_span: 1,
        required: false,
    });
    renumber(&mut next);
    next.finalize();
    Ok(next)
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
                    self.rename_row("Name"),
                    row("Columns", &s.columns.to_string()),
                    row("Ordinal", &s.ordinal.to_string()),
                    row("Fields", &s.fields.len().to_string()),
                    button(
                        "delete-section",
                        "Delete section",
                        cx,
                        move |this, window, cx| {
                            this.remove_section(i, window, cx);
                        },
                    ),
                    text_row("Deleting cascades placements, never values."),
                ],
            },
            Some(Selection::Field(idx)) => match self.draft.field_at(idx) {
                None => Vec::new(),
                Some(sf) => {
                    let kind = WidgetKind::of(&sf.field.kind);
                    let columns = self.draft.section_of(idx).map(|s| s.columns).unwrap_or(1);
                    vec![
                        self.rename_row("Label"),
                        // Immutable after creation, so it is shown and never offered.
                        row("Key", &sf.field.key),
                        // Locked. A kind is never re-typed in place — changing it creates a
                        // new field, which is the invariant that replaces form versioning.
                        row("Kind", &format!("{kind:?}  🔒")),
                        self.required_control(idx, sf.required, cx),
                        self.span_control(idx, sf.col_span, columns, cx),
                        self.config_rows(),
                        self.option_editor(idx, &sf.field.kind, cx),
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
            .child(self.add_controls(cx))
            .children(self.error_strip())
            .children(self.unplaced_drawer(cx))
            .into_any_element()
    }

    /// The rename box for whatever is selected, or a plain read-only row until it is built.
    fn rename_row(&self, label: &'static str) -> AnyElement {
        match &self.rename {
            Some(input) => h_flex()
                .w_full()
                .gap_2()
                .child(div().w(px(80.)).child(SharedString::from(label)))
                .child(div().flex_1().child(widgets::render_filter(input)))
                .into_any_element(),
            None => {
                let current = match self.selection {
                    Some(Selection::Section(i)) => {
                        self.draft.sections.get(i).map(|s| s.title.clone())
                    }
                    Some(Selection::Field(idx)) => {
                        self.draft.field_at(idx).map(|sf| sf.label.clone())
                    }
                    None => None,
                };
                row(label, current.as_deref().unwrap_or(""))
            }
        }
    }

    /// Kind-specific config: max length, rows, numeric bounds (R5–R8).
    fn config_rows(&self) -> AnyElement {
        if self.config.is_empty() {
            return div().into_any_element();
        }
        v_flex()
            .w_full()
            .gap_1()
            .children(self.config.iter().map(|(label, input)| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(div().w(px(80.)).child(SharedString::from(*label)))
                    .child(div().flex_1().child(widgets::render_filter(input)))
            }))
            .into_any_element()
    }

    /// The option list editor (R9, R10): `code` + `label` per row, plus a spare row.
    ///
    /// `code` is what lands in `field_value`; `label` is display only. Removing an option
    /// that is in use leaves its stored values alone — they simply no longer match an
    /// option, which is recoverable, unlike deleting them.
    fn option_editor(&self, idx: FieldIdx, kind: &FieldKind, cx: &mut Context<Self>) -> AnyElement {
        if self.options.is_empty() {
            return div().into_any_element();
        }
        let searchable = matches!(
            kind,
            FieldKind::Select {
                searchable: true,
                ..
            }
        );
        let is_select = matches!(kind, FieldKind::Select { .. });

        v_flex()
            .w_full()
            .gap_1()
            .child(SharedString::from("Options  (code · label)"))
            .children(self.options.iter().map(|(code, label)| {
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(div().w(px(80.)).child(widgets::render_filter(code)))
                    .child(div().flex_1().child(widgets::render_filter(label)))
            }))
            .child(text_row(
                "Clear a code to remove it. Stored values are kept.",
            ))
            .when(is_select, |d| {
                d.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().w(px(80.)).child(SharedString::from("Searchable")))
                        .child(
                            div()
                                .id("searchable-toggle")
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .child(SharedString::from(if searchable { "[x]" } else { "[ ]" }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_searchable(idx, window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    /// The add controls: one name box, and buttons for section and each field kind.
    fn add_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let kinds = replacement_kinds();
        let current_tag = self.new_kind.tag();
        v_flex()
            .w_full()
            .mt_2()
            .gap_1()
            .child(SharedString::from("ADD"))
            .child(widgets::render_filter(&self.new_name))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .flex_wrap()
                    .children(kinds.into_iter().map(|(tag, kind)| {
                        div()
                            .id(SharedString::from(format!("kind-{tag}")))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(tag == current_tag, |d| d.font_weight(FontWeight::BOLD))
                            .child(SharedString::from(tag))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.new_kind = kind.clone();
                                cx.notify();
                            }))
                    })),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(button(
                        "add-section",
                        "+ Section",
                        cx,
                        |this, window, cx| {
                            this.add_section(window, cx);
                        },
                    ))
                    .child(button("add-field", "+ Field", cx, |this, window, cx| {
                        this.add_field(window, cx);
                    })),
            )
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
    fn unplaced_drawer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Derived from the store, not tracked by hand: `unplaced_fields` is the authority,
        // so a field placed again drops out on its own and the drawer survives a restart.
        let names: Vec<AnyElement> = unplaced_fields(&self.all_fields, &self.draft)
            .enumerate()
            .map(|(i, f)| {
                let text = format!("{}  ({})", f.key, f.kind.tag());
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(div().flex_1().child(SharedString::from(text)))
                    .child(button(
                        Box::leak(format!("place-{i}").into_boxed_str()),
                        "Place",
                        cx,
                        move |this, window, cx| this.place_field(i, window, cx),
                    ))
                    .into_any_element()
            })
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
        // Radio and select ship with two placeholder options because `validate_field`
        // requires at least two — an empty option list is not a valid field, so offering
        // one would only produce an error the coordinator cannot act on yet.
        (
            "radio",
            FieldKind::Radio {
                options: starter_options(),
            },
        ),
        (
            "select",
            FieldKind::Select {
                options: starter_options(),
                searchable: false,
            },
        ),
    ]
}

/// The two options a new radio or select starts with, renamed in the option editor.
fn starter_options() -> Arc<[FieldOption]> {
    Arc::from(vec![
        FieldOption {
            code: OptionCode::new("a"),
            label: "Option A".into(),
            ordinal: 0,
        },
        FieldOption {
            code: OptionCode::new("b"),
            label: "Option B".into(),
            ordinal: 1,
        },
    ])
}

/// A clickable label. Plain `div` rather than a `gpui-component` button: the builder's
/// chrome stays in this file, and rule 3 keeps upstream widgets inside `widgets/`.
fn button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<BuilderView>,
    on_click: impl Fn(&mut BuilderView, &mut Window, &mut Context<BuilderView>) + 'static,
) -> AnyElement {
    div()
        .id(SharedString::from(id))
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .child(SharedString::from(label))
        .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        .into_any_element()
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // These belong to the current selection, so they are built here rather than held
        // across a selection change where they would show the previous field's text.
        self.ensure_rename(window, cx);
        self.ensure_config(window, cx);
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
    fn m5_add_section_and_field_of_every_kind() {
        // M5 acceptance 1–3, headless: a form gains sections and one field of each kind.
        let mut d = with_section_added(&form(), "Vitals 2", 2).expect("section added");
        assert_eq!(d.sections.len(), 3);
        let s = d.sections.len() - 1;

        for (n, (tag, kind)) in replacement_kinds().into_iter().enumerate() {
            d = with_field_added(&d, s, &format!("Field {n}"), &format!("f_{tag}"), kind)
                .unwrap_or_else(|e| panic!("adding {tag}: {e}"));
        }
        assert_eq!(d.sections[s].fields.len(), replacement_kinds().len());
        // Nothing the builder can add may leave the form invalid.
        assert!(validate_form(&d).is_empty(), "{:?}", validate_form(&d));
    }

    #[test]
    fn m5_duplicate_keys_are_refused_by_core() {
        let d = form();
        let existing = d.sections[0].fields[0].field.key.clone();
        let err = with_field_added(&d, 0, "Another", &existing, FieldKind::Date).unwrap_err();
        assert!(matches!(err, EditError::DuplicateKey(_)));
    }

    #[test]
    fn m5_key_from_label_produces_a_key_core_accepts() {
        let d = form();
        for label in ["Date of birth", "  Weight (kg) ", "3rd reading", "Sex"] {
            let key = key_from_label(&d, label);
            assert!(validate_key(&key).is_ok(), "{label:?} produced {key:?}");
        }
        assert_eq!(key_from_label(&d, "Date of birth"), "date_of_birth");
        // A label that collides with an existing key gets a free one instead.
        let taken = &d.sections[0].fields[0].field.key;
        assert_ne!(&key_from_label(&d, taken), taken);
    }

    #[test]
    fn m5_deleting_a_section_orphans_its_fields_rather_than_dropping_them() {
        let before = form();
        let held: Vec<_> = before.sections[1]
            .fields
            .iter()
            .map(|f| f.field.field_id)
            .collect();

        let (after, orphans) = with_section_removed(&before, 1).expect("removed");
        assert_eq!(after.sections.len(), 1);
        let returned: Vec<_> = orphans.iter().map(|f| f.field_id).collect();
        assert_eq!(returned, held, "every field comes back for the drawer");
    }

    #[test]
    fn m5_an_unplaced_field_can_be_placed_again() {
        // The round trip that makes "removing a placement never deletes values" useful.
        let before = form();
        let idx = before.sections[0].fields[0].idx;
        let (without, removed) = with_placement_removed(&before, idx).expect("removed");
        let id = removed.field_id;

        let after = with_field_placed(&without, 1, removed, "Back again").expect("placed");
        assert_eq!(
            after.idx_of(id).is_some(),
            true,
            "the same field id is placed again"
        );
        assert_eq!(after.field_count(), before.field_count());
    }

    #[test]
    fn m5_renaming_refuses_an_empty_label() {
        let d = form();
        let idx = d.sections[0].fields[0].idx;
        assert!(matches!(
            with_label(&d, idx, "   "),
            Err(EditError::EmptyLabel)
        ));
        assert!(matches!(
            with_section_title(&d, 0, ""),
            Err(EditError::EmptyLabel)
        ));
        // A good rename trims.
        assert_eq!(
            with_label(&d, idx, "  Given name  ").unwrap().sections[0].fields[0].label,
            "Given name"
        );
    }

    fn opt_form() -> FormDef {
        let mut f = field("sex", 1);
        Arc::make_mut(&mut f.field).kind = FieldKind::Select {
            options: starter_options(),
            searchable: false,
        };
        let mut d = FormDef::new(FormId::new(), "F", vec![section("S", 1, vec![f])]);
        renumber(&mut d);
        d.finalize();
        d
    }

    #[test]
    fn m5_kind_config_edits_are_in_place_and_keep_the_field_id() {
        // Changing max_len is a config edit, not a re-type: same tag, so same field.
        let before = form();
        let idx = before.sections[0].fields[0].idx;
        let id = before.sections[0].fields[0].field.field_id;

        let after = with_kind_config(&before, idx, FieldKind::Text { max_len: Some(64) })
            .expect("in-place");
        assert_eq!(after.sections[0].fields[0].field.field_id, id, "same field");
        assert!(matches!(
            after.sections[0].fields[0].field.kind,
            FieldKind::Text { max_len: Some(64) }
        ));
    }

    #[test]
    fn m5_kind_config_refuses_a_change_of_kind() {
        // The guard that stops a config edit quietly becoming a re-type.
        let d = form();
        let idx = d.sections[0].fields[0].idx;
        assert!(matches!(
            with_kind_config(&d, idx, FieldKind::Date),
            Err(EditError::KindIsImmutable)
        ));
    }

    #[test]
    fn m5_numeric_bounds_are_validated_by_core() {
        let mut d = form();
        let idx = d.sections[0].fields[0].idx;
        d = with_field_replaced(
            &d,
            idx,
            FieldKind::Numeric {
                min: None,
                max: None,
                scale: 0,
            },
        )
        .unwrap()
        .0;
        let idx = d.sections[0].fields[0].idx;

        let bad = FieldKind::Numeric {
            min: parse_decimal("10").ok(),
            max: parse_decimal("1").ok(),
            scale: 0,
        };
        assert!(matches!(
            with_kind_config(&d, idx, bad),
            Err(EditError::MinExceedsMax { .. })
        ));

        let too_precise = FieldKind::Numeric {
            min: None,
            max: None,
            scale: 11,
        };
        assert!(matches!(
            with_kind_config(&d, idx, too_precise),
            Err(EditError::ScaleTooLarge)
        ));
    }

    #[test]
    fn m5_option_editor_writes_codes_and_drops_blank_rows() {
        // The editor always carries a spare blank row; it must not become an option.
        let d = opt_form();
        let idx = d.sections[0].fields[0].idx;
        let pairs = vec![
            ("m".to_string(), "Male".to_string()),
            ("f".to_string(), "Female".to_string()),
            (String::new(), String::new()),
        ];
        let after = with_options(&d, idx, &pairs).expect("options written");
        let FieldKind::Select { options, .. } = &after.sections[0].fields[0].field.kind else {
            panic!("still a select");
        };
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].code.as_str(), "m");
        assert_eq!(options[1].label, "Female");
    }

    #[test]
    fn m5_option_list_below_two_is_refused_by_core() {
        // `validate_field` requires at least two; the editor does not second-guess it.
        let d = opt_form();
        let idx = d.sections[0].fields[0].idx;
        let one = vec![("m".to_string(), "Male".to_string())];
        assert!(matches!(
            with_options(&d, idx, &one),
            Err(EditError::TooFewOptions(_))
        ));
    }

    #[test]
    fn m5_duplicate_option_codes_are_refused() {
        let d = opt_form();
        let idx = d.sections[0].fields[0].idx;
        let dupes = vec![
            ("m".to_string(), "Male".to_string()),
            ("m".to_string(), "Man".to_string()),
        ];
        assert!(matches!(
            with_options(&d, idx, &dupes),
            Err(EditError::DuplicateOption(_))
        ));
    }

    #[test]
    fn m5_searchable_toggles_without_disturbing_the_options() {
        let d = opt_form();
        let idx = d.sections[0].fields[0].idx;
        let after = with_searchable(&d, idx, true).expect("toggled");
        let FieldKind::Select {
            options,
            searchable,
        } = &after.sections[0].fields[0].field.kind
        else {
            panic!("still a select");
        };
        assert!(searchable);
        assert_eq!(options.len(), 2, "options survive the toggle");
        // And it is still the same field, not a replacement.
        assert_eq!(
            after.sections[0].fields[0].field.field_id,
            d.sections[0].fields[0].field.field_id
        );
    }

    #[test]
    fn m5_every_addable_kind_is_valid_the_moment_it_is_created() {
        // The kind picker must never offer something that fails validation on arrival —
        // radio and select in particular, which need two options to be legal at all.
        for (tag, kind) in replacement_kinds() {
            let def = FieldDef {
                field_id: FieldId::new(),
                key: "k".into(),
                kind,
            };
            assert!(
                validate_field(&def).is_ok(),
                "{tag} was not valid when created"
            );
        }
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

/// Store-backed tests for the Unplaced drawer, which is the one part of M5 whose
/// correctness spans `medatat-core`, `medatat-store`, and this crate.
#[cfg(test)]
mod persistence_tests {
    use super::*;
    use medatat_core::def::{FieldDef, FieldKind, SectionDef, SectionField};
    use medatat_core::{FieldId, FormId, SectionId};
    use medatat_store::Store;

    fn form_with(keys: &[&str]) -> FormDef {
        let fields = keys
            .iter()
            .map(|k| SectionField {
                idx: FieldIdx(0),
                field: Arc::new(FieldDef {
                    field_id: FieldId::new(),
                    key: (*k).into(),
                    kind: FieldKind::Text { max_len: None },
                }),
                label: (*k).into(),
                ordinal: 0,
                col_span: 1,
                required: false,
            })
            .collect();
        FormDef::new(
            FormId::new(),
            "Intake",
            vec![SectionDef {
                section_id: SectionId::new(),
                title: "S".into(),
                ordinal: 0,
                columns: 1,
                default_collapsed: false,
                fields,
            }],
        )
    }

    /// Saves a definition the way `commit_structure` does: fields first, from both drafts.
    fn persist(store: &Store, previous: Option<&FormDef>, next: &FormDef) {
        let mut fields: Vec<FieldDef> = previous
            .into_iter()
            .flat_map(|d| d.iter_fields())
            .chain(next.iter_fields())
            .map(|f| (*f.field).clone())
            .collect();
        fields.sort_by(|a, b| a.field_id.cmp(&b.field_id));
        fields.dedup_by(|a, b| a.field_id == b.field_id);
        store.save_fields(&fields).expect("fields saved");
        store.save_form(next, ConfigRev(1)).expect("form saved");
    }

    #[test]
    fn m5_an_unplaced_field_survives_a_reopen() {
        // M5 acceptance 9, end to end: remove a placement, reopen from the store, and the
        // field is still findable — which is the route back to its values.
        let store = Store::open_in_memory().expect("store");
        let before = form_with(&["family_name", "given_name"]);
        persist(&store, None, &before);

        let idx = before.sections[0].fields[0].idx;
        let removed_id = before.sections[0].fields[0].field.field_id;
        let (after, _) = with_placement_removed(&before, idx).expect("removed");
        persist(&store, Some(&before), &after);

        // Reopen: exactly what `BuilderView::new` does on a cold start.
        let reopened = store.load_form(after.form_id).expect("form reloaded");
        let all = store.all_fields().expect("fields reloaded");
        let unplaced: Vec<_> = unplaced_fields(&all, &reopened).collect();

        assert_eq!(unplaced.len(), 1, "the removed field is still known");
        assert_eq!(unplaced[0].field_id, removed_id);
        assert_eq!(unplaced[0].key, "family_name");
    }

    #[test]
    fn m5_placing_a_field_again_empties_the_drawer() {
        let store = Store::open_in_memory().expect("store");
        let before = form_with(&["a", "b"]);
        persist(&store, None, &before);

        let idx = before.sections[0].fields[0].idx;
        let (without, removed) = with_placement_removed(&before, idx).expect("removed");
        persist(&store, Some(&before), &without);

        let again = with_field_placed(&without, 0, removed, "Back").expect("placed");
        persist(&store, Some(&without), &again);

        let all = store.all_fields().expect("fields");
        let reopened = store.load_form(again.form_id).expect("form");
        assert_eq!(unplaced_fields(&all, &reopened).count(), 0);
    }

    #[test]
    fn m5_a_replaced_field_is_findable_after_a_reopen() {
        // Acceptance 9's other half: Replace leaves the original in the drawer, and its
        // values are still keyed by that id in `field_value`.
        let store = Store::open_in_memory().expect("store");
        let before = form_with(&["dob"]);
        persist(&store, None, &before);

        let idx = before.sections[0].fields[0].idx;
        let original = before.sections[0].fields[0].field.field_id;
        let (after, _) = with_field_replaced(&before, idx, FieldKind::Date).expect("replaced");
        persist(&store, Some(&before), &after);

        let all = store.all_fields().expect("fields");
        let reopened = store.load_form(after.form_id).expect("form");
        let unplaced: Vec<_> = unplaced_fields(&all, &reopened).collect();
        assert_eq!(unplaced.len(), 1);
        assert_eq!(
            unplaced[0].field_id, original,
            "the original, not the replacement"
        );
    }
}
