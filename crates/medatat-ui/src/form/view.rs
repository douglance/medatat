//! The runtime form renderer.
//!
//! Two rules carry most of the weight here:
//!
//! 1. **A keystroke must never `cx.notify()` the parent.** Doing so rebuilds all 300
//!    fields per character and turns typing into an O(n) operation. The focused input
//!    re-renders itself; the parent re-renders only on structural change. Because
//!    conditional logic was cut (`docs/adr/0005`), there is essentially no mid-edit reason
//!    for the parent to re-render at all.
//! 2. **Presentation decisions live in `medatat_core::view`**, not here. This module maps a
//!    `WidgetSpec` onto widgets and does nothing else, which is what keeps the GUI test
//!    budget at three.
//!
//! Note what this file does *not* import: no `gpui-component` widget or event type appears
//! anywhere below. Widget events arrive already translated as `widgets::WidgetChange`.

use crate::form::palette::FieldPalette;
use crate::mode::{RenderMode, Selection};
use crate::widgets::{
    self, OnChoose, OnPick, OnSelectField, WidgetChange, WidgetState, time_input,
};
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::value::{parse_date, parse_decimal};
use medatat_core::{
    CaseId, CaseRev, FieldId, FieldIdx, FormDef, FormInstance, OptionCode, Value, WidgetKind,
    effective_columns, focus_order, widget_spec,
};
use medatat_store::Store;
use std::rc::Rc;
use std::sync::Arc;

/// Test-only counters for the anti-quadratic guard. They do not exist in a release build;
/// `subscriptions_fire_once_per_edit` reads them to prove a keystroke costs one subscription
/// and **zero** parent re-renders.
#[cfg(test)]
pub(crate) static CHANGE_EVENTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static PARENT_RENDERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub struct FormView {
    inst: FormInstance,
    widgets: Vec<WidgetState>,
    _subs: Vec<Subscription>,
    focus_order: Vec<FieldIdx>,
    collapsed: Vec<bool>,
    store: Arc<Store>,
    focus: FocusHandle,
    /// Edits arriving from sync while a field is focused. Applied on blur — rewriting text
    /// under someone's cursor is the fastest way to make a form feel hostile.
    deferred: Vec<(FieldId, Value)>,
    focused: Option<FieldIdx>,
    /// The section the user was last in. Collapse/expand targets this, so it still works
    /// after collapsing drops focus back to the form root.
    last_section: usize,
    dirty_since_flush: bool,
    /// The `Cmd/Ctrl-F` field palette, present only while it is open. A separate entity so
    /// typing a query re-renders the palette rather than all 300 fields — see `palette.rs`.
    palette: Option<Entity<FieldPalette>>,
    /// Scroll position of the section list, used for scroll-into-view on focus.
    scroll: ScrollHandle,
    /// Runtime or design. The element tree is the same either way — see `mode.rs`.
    mode: RenderMode,
    /// Called when a design-mode click selects a field, so the builder's inspector can
    /// follow along. `None` in runtime, where clicks focus instead of selecting.
    on_select: Option<OnSelectField>,
    /// Called by the design-mode column control on a section header.
    on_columns: Option<OnColumns>,
}

/// `(section index, new column count)`. The builder applies it through
/// `FormDef::finalize`, which is where R12's `col_span` clamp already lives.
pub type OnColumns = Rc<dyn Fn(usize, u8, &mut Window, &mut gpui::App)>;

impl FormView {
    pub fn new(
        def: Arc<FormDef>,
        case_id: CaseId,
        base_rev: CaseRev,
        values: Vec<(FieldId, Value)>,
        store: Arc<Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inst = FormInstance::new(Arc::clone(&def), case_id, base_rev, values);
        let collapsed: Vec<bool> = def.sections.iter().map(|s| s.default_collapsed).collect();

        let mut widgets = Vec::with_capacity(def.field_count());
        let mut subs = Vec::new();

        for sf in def.iter_fields() {
            let spec = widget_spec(sf, &inst, sf.col_span);
            let state = WidgetState::for_spec(&spec, window, cx);

            // One subscription per entity; the closure captures a u32 and a Copy enum.
            let idx = sf.idx;
            let kind = spec.kind;
            if let Some(sub) =
                widgets::subscribe(&state, kind, window, cx, move |this, change, window, cx| {
                    this.on_change(idx, kind, change, window, cx);
                })
            {
                subs.push(sub);
            }
            widgets.push(state);
        }

        let focus_order = focus_order(&def, &collapsed);

        // Take focus on open. Without this the root element is never focused, so key events
        // never route through it and Tab does nothing until the user clicks something —
        // which for a keyboard-only abstractor means the form appears dead on arrival.
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        FormView {
            inst,
            widgets,
            _subs: subs,
            focus_order,
            collapsed,
            store,
            focus,
            deferred: Vec::new(),
            focused: None,
            last_section: 0,
            dirty_since_flush: false,
            palette: None,
            scroll: ScrollHandle::new(),
            mode: RenderMode::Runtime,
            on_select: None,
            on_columns: None,
        }
    }

    /// Switches between the abstractor's view and the builder canvas. Same renderer.
    pub fn set_mode(&mut self, mode: RenderMode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }

    /// Installs the builder's selection callback.
    pub fn set_on_select(&mut self, on_select: OnSelectField) {
        self.on_select = Some(on_select);
    }

    /// Installs the builder's column-change callback (R12). Design mode only.
    pub fn set_on_columns(&mut self, on_columns: OnColumns) {
        self.on_columns = Some(on_columns);
    }

    /// Moves the design-mode selection without disturbing anything else.
    pub fn select(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        if self.mode.is_design() {
            self.mode = RenderMode::Design {
                selected: selection,
            };
            cx.notify();
        }
    }

    /// The single entry point for everything a widget reports.
    ///
    /// The `Text` arm is the hot path — one keystroke — and deliberately ends without a
    /// `cx.notify()`. Every other arm is a discrete user action (a pick, leaving a field),
    /// so one parent re-render there is O(1) in user actions, not O(characters).
    fn on_change(
        &mut self,
        idx: FieldIdx,
        kind: WidgetKind,
        change: WidgetChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        CHANGE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        match change {
            WidgetChange::Text(raw) => {
                self.on_edit(idx, kind, raw);
                // Deliberately no cx.notify(). See the module note.
            }
            WidgetChange::Option(code) => {
                self.pick_option(idx, code);
                self.flush();
                cx.notify();
            }
            WidgetChange::Focused => {
                self.focused = Some(idx);
            }
            WidgetChange::Committed => {
                // Only clear if focus has not already moved elsewhere. Tab focuses the next
                // field before this blur arrives, and clearing unconditionally would erase
                // the new field and break the very next Tab.
                if self.focused == Some(idx) {
                    self.focused = None;
                }
                self.commit_field(idx, window, cx);
            }
        }
    }

    /// Parses raw text into a `Value` and records it. Runs on every keystroke, so it must
    /// stay O(1): one parse, one validate, one field.
    fn on_edit(&mut self, idx: FieldIdx, kind: WidgetKind, raw: String) {
        let value = match kind {
            // Time is canonicalised on blur, not per keystroke — see time_input.
            WidgetKind::Time => return,
            WidgetKind::Numeric => match parse_decimal(&raw) {
                Ok(d) => Value::Num(d),
                Err(_) if raw.trim().is_empty() => Value::Null,
                Err(_) => return, // keep the raw text visible; commit will report it
            },
            WidgetKind::Date => match parse_date(&raw) {
                Ok(d) => Value::Date(d),
                Err(_) if raw.trim().is_empty() => Value::Null,
                Err(_) => return,
            },
            _ if raw.is_empty() => Value::Null,
            _ => Value::Text(raw),
        };
        self.inst.set(idx, value);
        self.dirty_since_flush = true;
    }

    /// Records a radio or select choice. The code, never the label — a label edit must not
    /// be able to retype stored data.
    fn pick_option(&mut self, idx: FieldIdx, code: Option<String>) {
        let value = match code {
            Some(c) if !c.is_empty() => Value::Opt(OptionCode::new(c)),
            _ => Value::Null,
        };
        self.inst.set(idx, value);
        self.dirty_since_flush = true;
    }

    /// Canonicalises a time field and persists. Called on blur, Tab, and Enter.
    pub fn commit_field(&mut self, idx: FieldIdx, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sf) = self.inst.def().field_at(idx) else {
            return;
        };
        if matches!(WidgetKind::of(&sf.field.kind), WidgetKind::Time)
            && let Some(entity) = self.widgets[idx.as_usize()].as_input()
        {
            let entity = entity.clone();
            let raw = self.widgets[idx.as_usize()].text(cx);
            match time_input::commit(&raw) {
                Ok((value, canonical)) => {
                    entity.update(cx, |s, cx| s.set_value(canonical, window, cx));
                    self.inst.set(idx, value);
                }
                Err(_) => {
                    // Keep what they typed and mark it invalid, rather than discarding it.
                    self.inst.set(idx, Value::Null);
                }
            }
            self.dirty_since_flush = true;
        }
        self.flush();
        self.apply_deferred();
        cx.notify();
    }

    /// Writes pending edits to local SQLite. Synchronous and microseconds — this *is* the
    /// save (R14). Sync to the server happens later, in the background.
    pub fn flush(&mut self) {
        if !self.dirty_since_flush {
            return;
        }
        let changes: Vec<(FieldId, Value)> =
            self.inst.pending().map(|(id, v)| (id, v.clone())).collect();
        if changes.is_empty() {
            self.dirty_since_flush = false;
            return;
        }
        match self
            .store
            .apply_local(self.inst.case_id(), &changes, self.inst.base_rev())
        {
            Ok(()) => self.dirty_since_flush = false,
            // A failed local write is serious: report it rather than losing it silently.
            Err(e) => tracing::error!("local save failed: {e}"),
        }
    }

    /// Applies a batch of inbound sync values, one row at a time through the focus guard.
    ///
    /// The batch shape matters: the engine hands back what it applied to the store, so the
    /// view never re-reads and diffs. Rewriting the field under the user's cursor is what
    /// `receive_remote` refuses to do.
    pub fn receive_remote_rows(
        &mut self,
        rows: &[medatat_core::wire::ValueRow],
        cx: &mut Context<Self>,
    ) {
        for row in rows {
            self.receive_remote(row.field_id, row.value.clone(), cx);
        }
    }

    /// The case this view is editing, so the shell can route only that case's rows here.
    pub fn case_id(&self) -> CaseId {
        self.inst.case_id()
    }

    /// Queues an inbound sync value, or applies it if the field is not being edited.
    pub fn receive_remote(&mut self, id: FieldId, value: Value, cx: &mut Context<Self>) {
        let idx = self.inst.def().idx_of(id);
        if idx.is_some() && idx == self.focused {
            self.deferred.push((id, value));
            return;
        }
        if self.inst.apply_remote(id, value) {
            cx.notify();
        }
    }

    fn apply_deferred(&mut self) {
        for (id, v) in std::mem::take(&mut self.deferred) {
            self.inst.apply_remote(id, v);
        }
    }

    pub fn toggle_section(&mut self, section: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.collapsed.get_mut(section) else {
            return;
        };
        *c = !*c;
        self.focus_order = focus_order(self.inst.def(), &self.collapsed);

        // Collapsing the section you are standing in destroys the element holding focus,
        // and focus does not fall back on its own — key events then route nowhere and the
        // keyboard goes dead, so you cannot even expand the section again. Take focus back
        // to the form root, which is always rendered.
        if self
            .focused
            .is_some_and(|idx| !self.focus_order.contains(&idx))
        {
            self.focused = None;
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    #[allow(
        dead_code,
        reason = "read by the three budgeted GUI tests; see docs/07-TESTING.md"
    )]
    pub fn focus_order(&self) -> &[FieldIdx] {
        &self.focus_order
    }

    /// The text currently in one widget. Used by `closing_case_clears_inputs`.
    #[cfg(test)]
    pub(crate) fn widget_text(&self, idx: FieldIdx, cx: &gpui::App) -> String {
        self.widgets
            .get(idx.as_usize())
            .map(|w| w.text(cx))
            .unwrap_or_default()
    }

    /// Whether a section is collapsed. Read by the `Alt-Left`/`Alt-Right` test.
    #[cfg(test)]
    pub(crate) fn is_collapsed(&self, section: usize) -> bool {
        self.collapsed.get(section).copied().unwrap_or(false)
    }

    /// Which field the view believes has focus. Read by `tab_order_matches_focus_order`.
    #[cfg(test)]
    pub(crate) fn focused_field(&self) -> Option<FieldIdx> {
        self.focused
    }

    /// Blanks every editable widget. PHI hygiene: a closed case must leave no field
    /// contents behind in a live input buffer, which `Value`'s zeroize cannot reach because
    /// the text also lives inside `gpui-component`'s own editing state.
    ///
    /// Gated on `phi` because that is the only caller — it exists where it is used.
    #[cfg(feature = "phi")]
    pub fn clear_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for w in &self.widgets {
            w.clear(window, cx);
        }
        cx.notify();
    }

    /// The keyboard model (`docs/05-UI-SPEC.md#keyboard-model`).
    ///
    /// All of it runs in the capture phase. `InputState` binds `tab` to `IndentInline` and
    /// `up`/`down` to `MoveUp`/`MoveDown` in its own key context, so a bubble-phase handler
    /// would never see those keys at all.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Design mode has no caret to move: the widgets are non-interactive, so focus
        // traversal and field editing keys would be meaningless. The builder owns the
        // keyboard there (reordering, selection) and handles it above this view.
        if self.mode.is_design() {
            return;
        }

        let mods = &ev.keystroke.modifiers;
        let key = ev.keystroke.key.as_str();

        // The palette owns the keyboard while it is open.
        if self.palette.is_some() {
            match key {
                "escape" => {
                    self.close_palette(cx);
                    cx.stop_propagation();
                }
                "enter" => {
                    let chosen = self.palette.as_ref().and_then(|p| p.read(cx).first_match());
                    if let Some(idx) = chosen {
                        self.close_palette(cx);
                        self.reveal_field(idx, window, cx);
                    }
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        // `secondary` is Cmd on macOS and Ctrl elsewhere, which is why we ask gpui rather
        // than hard-coding either.
        if key == "f" && mods.secondary() {
            self.open_palette(window, cx);
            cx.stop_propagation();
            return;
        }

        // Collapse and expand (`docs/05-UI-SPEC.md`).
        //
        // `Cmd/Ctrl-[` and `]`, **not** `Alt-Left`/`Alt-Right`. macOS claims option+arrow as
        // "move by word" and consumes it before the event enters the element dispatch tree,
        // so no handler — capture phase included — can ever see it while a text input has
        // focus. Since Tab lands you in a field, that made the binding unreachable exactly
        // when a keyboard user would reach for it. Do not "fix" this back.
        //
        // `InputState` does bind `cmd-[`/`cmd-]` to Outdent/Indent, but that is a *keymap*
        // binding, and capture runs before action dispatch — the same reason the time
        // field's Up/Down nudge beats `MoveUp`/`MoveDown`.
        if mods.secondary() && matches!(key, "[" | "]") {
            self.set_collapsed(key == "[", window, cx);
            cx.stop_propagation();
            return;
        }

        if mods.alt {
            match key {
                "up" => self.step_section(-1, window, cx),
                "down" => self.step_section(1, window, cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }

        match key {
            "tab" => {
                self.focus_step(if mods.shift { -1 } else { 1 }, window, cx);
                cx.stop_propagation();
            }
            // Enter commits and advances, except where Enter means something else in the
            // widget itself: a textarea takes a newline, and a select or date picker is
            // confirming its own popup.
            "enter" => match self.focused_kind() {
                Some(WidgetKind::Textarea) if !mods.control => {}
                Some(WidgetKind::Textarea) => {
                    self.commit_and_advance(window, cx);
                    cx.stop_propagation();
                }
                Some(WidgetKind::Select) | Some(WidgetKind::Date) | None => {}
                Some(_) => {
                    self.commit_and_advance(window, cx);
                    cx.stop_propagation();
                }
            },
            // Type-ahead on a select: jump to the next option whose label starts with the
            // letter, repeating the letter to cycle among them. No accumulation timer —
            // cycling gives the same reach without a hidden deadline the user cannot see.
            k if self.focused_kind() == Some(WidgetKind::Select)
                && !mods.secondary()
                && !mods.alt
                && !mods.control
                && k.chars().count() == 1
                && k.chars().all(char::is_alphanumeric) =>
            {
                if let Some(c) = k.chars().next() {
                    self.typeahead_select(c, window, cx);
                }
                cx.stop_propagation();
            }
            // Arrows cycle a radio group. Time fields handle their own ±1 minute nudge in
            // `widgets::render_time`, closer to the input that owns the text.
            "up" | "left" | "down" | "right" if self.focused_kind() == Some(WidgetKind::Radio) => {
                let delta = if matches!(key, "up" | "left") { -1 } else { 1 };
                self.cycle_radio(delta, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// Commits the focused field and moves to the next stop — the single-line Enter
    /// behaviour, and what `Ctrl-Enter` does out of a textarea.
    fn commit_and_advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(idx) = self.focused {
            self.commit_field(idx, window, cx);
        }
        self.focus_step(1, window, cx);
    }

    /// The widget kind under focus, if any.
    fn focused_kind(&self) -> Option<WidgetKind> {
        let idx = self.focused?;
        self.inst
            .def()
            .field_at(idx)
            .map(|sf| WidgetKind::of(&sf.field.kind))
    }

    /// Which section holds the focused field. Falls back to the first section so the
    /// section keys still do something before anything has been focused.
    fn current_section(&self) -> usize {
        let Some(idx) = self.focused else {
            return self.last_section;
        };
        self.inst
            .def()
            .sections
            .iter()
            .position(|s| s.fields.iter().any(|f| f.idx == idx))
            .unwrap_or(self.last_section)
    }

    /// `Alt-Up` / `Alt-Down`: focus the first field of the previous or next section that
    /// actually has focusable content. Collapsed sections are skipped, not entered.
    fn step_section(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.inst.def().sections.len();
        if count == 0 {
            return;
        }
        let mut i = self.current_section() as isize;
        for _ in 0..count {
            i = (i + delta).rem_euclid(count as isize);
            let s = i as usize;
            if self.collapsed.get(s).copied().unwrap_or(false) {
                continue;
            }
            if let Some(first) = self.inst.def().sections[s].fields.first().map(|f| f.idx) {
                self.reveal_field(first, window, cx);
                return;
            }
        }
    }

    /// `Alt-Left` / `Alt-Right` on the current section.
    fn set_collapsed(&mut self, collapsed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let section = self.current_section();
        if self.collapsed.get(section).copied() == Some(collapsed) {
            return;
        }
        self.toggle_section(section, window, cx);
    }

    /// Select type-ahead: the next option after the current one whose label starts with
    /// `c`, wrapping, so pressing the same letter cycles.
    fn typeahead_select(&mut self, c: char, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.focused else { return };
        let Some(sf) = self.inst.def().field_at(idx) else {
            return;
        };
        let Some(options) = sf.field.kind.options() else {
            return;
        };
        let current = match self.inst.get(idx) {
            Value::Opt(code) => options.iter().position(|o| o.code == *code),
            _ => None,
        };
        let n = options.len();
        let start = current.map(|i| i + 1).unwrap_or(0);
        let hit = (0..n).map(|k| (start + k) % n).find(|&i| {
            options[i]
                .label
                .chars()
                .next()
                .is_some_and(|f| f.eq_ignore_ascii_case(&c))
        });
        let Some(i) = hit else { return };

        self.pick_option(idx, Some(options[i].code.0.clone()));
        self.widgets[idx.as_usize()].show_selected(i, window, cx);
        self.flush();
        cx.notify();
    }

    /// Arrow cycling inside a radio group, wrapping at both ends.
    fn cycle_radio(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(idx) = self.focused else { return };
        let Some(sf) = self.inst.def().field_at(idx) else {
            return;
        };
        let Some(options) = sf.field.kind.options() else {
            return;
        };
        if options.is_empty() {
            return;
        }
        let current = match self.inst.get(idx) {
            Value::Opt(c) => options.iter().position(|o| o.code == *c),
            _ => None,
        };
        let next = match current {
            Some(i) => (i as isize + delta).rem_euclid(options.len() as isize) as usize,
            None if delta >= 0 => 0,
            None => options.len() - 1,
        };
        let code = options[next].code.0.clone();
        self.pick_option(idx, Some(code));
        self.flush();
        cx.notify();
    }

    /// Focus a field and bring it on screen: expand its section first, because focusing
    /// something inside a collapsed section would put the caret where nothing is drawn.
    pub fn reveal_field(&mut self, idx: FieldIdx, window: &mut Window, cx: &mut Context<Self>) {
        let section = self
            .inst
            .def()
            .sections
            .iter()
            .position(|s| s.fields.iter().any(|f| f.idx == idx));
        if let Some(s) = section {
            if self.collapsed.get(s).copied().unwrap_or(false) {
                self.toggle_section(s, window, cx);
            }
            // No animation: in a data-entry tool a moving viewport reads as latency.
            self.scroll.scroll_to_item(s);
        }
        self.focus_field(idx, window, cx);
    }

    /// Opens the `Cmd/Ctrl-F` field palette and puts the caret in its query box.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_none() {
            let def = Arc::clone(self.inst.def());
            let on_choose = self.on_choose(cx);
            self.palette = Some(cx.new(|cx| FieldPalette::new(def, on_choose, window, cx)));
        }
        if let Some(p) = &self.palette {
            p.update(cx, |palette, cx| palette.focus(window, cx));
        }
        // One notify, for the structural change of the palette appearing.
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette = None;
        cx.notify();
    }

    /// The callback the palette calls when a row is clicked.
    fn on_choose(&self, cx: &mut Context<Self>) -> OnChoose {
        let this = cx.entity().downgrade();
        Rc::new(move |idx, window, cx| {
            let _ = this.update(cx, |view: &mut FormView, cx| {
                view.close_palette(cx);
                view.reveal_field(idx, window, cx);
            });
        })
    }

    /// Moves focus one stop along `medatat_core::view::focus_order`, wrapping at both ends.
    ///
    /// The order comes from core rather than from gpui's own tab ring precisely because a
    /// collapsed section must contribute no stops: `focus_order` already omits them, so
    /// focus can never land on something invisible.
    pub fn focus_step(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.focus_order.len();
        if len == 0 {
            return;
        }
        let current = self
            .focused
            .and_then(|idx| self.focus_order.iter().position(|&f| f == idx));
        let next = match current {
            Some(i) => (i as isize + delta).rem_euclid(len as isize) as usize,
            // Entering from nowhere: Tab starts at the top, Shift-Tab at the bottom.
            None if delta >= 0 => 0,
            None => len - 1,
        };
        self.reveal_field(self.focus_order[next], window, cx);
    }

    /// Puts focus on one field, whatever kind backs it.
    ///
    /// Commits the field being left rather than waiting for a blur event. Moving focus
    /// programmatically does **not** produce `InputEvent::Blur`, so relying on it meant Tab
    /// never canonicalised a time field, never flushed to SQLite, and never released a
    /// deferred inbound value. Leaving a field is a deliberate act; the view knows which
    /// field it is leaving, so it commits it itself.
    pub fn focus_field(&mut self, idx: FieldIdx, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(leaving) = self.focused.filter(|&f| f != idx) {
            self.commit_field(leaving, window, cx);
        }
        let Some(state) = self.widgets.get(idx.as_usize()) else {
            return;
        };
        state.focus_handle(cx).focus(window, cx);
        // Set after `focus`, so a synchronously delivered blur for the field we just left
        // cannot clear what we are setting.
        self.focused = Some(idx);
        self.last_section = self
            .inst
            .def()
            .sections
            .iter()
            .position(|s| s.fields.iter().any(|f| f.idx == idx))
            .unwrap_or(self.last_section);
        cx.notify();
    }

    #[allow(
        dead_code,
        reason = "read by the three budgeted GUI tests; see docs/07-TESTING.md"
    )]
    /// Whether the field palette has the keyboard. The shell asks before acting on
    /// `Escape`, so closing the palette never also closes the case.
    pub fn palette_is_open(&self) -> bool {
        self.palette.is_some()
    }

    #[allow(
        dead_code,
        reason = "read by the three budgeted GUI tests; see docs/07-TESTING.md"
    )]
    pub fn instance(&self) -> &FormInstance {
        &self.inst
    }

    #[allow(
        dead_code,
        reason = "subscriptions_fire_once_per_edit reads this; see docs/07-TESTING.md"
    )]
    pub fn subscription_count(&self) -> usize {
        self._subs.len()
    }

    /// The callback a radio group calls when an option is clicked. Built from a weak
    /// handle so a click that outlives the view is a no-op rather than a panic.
    fn on_pick(&self, cx: &mut Context<Self>) -> OnPick {
        let this = cx.entity().downgrade();
        Rc::new(move |idx, code, _window, cx| {
            let _ = this.update(cx, |view: &mut FormView, cx| {
                view.pick_option(idx, code);
                view.flush();
                cx.notify();
            });
        })
    }
}

impl Render for FormView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `Pixels`' inner f32 is private; `From` is the supported accessor.
        let width = f32::from(window.viewport_size().width);
        let def = Arc::clone(self.inst.def());
        let on_pick = self.on_pick(cx);
        let mode = self.mode;
        let ring = widgets::ring_color(cx);
        let on_select = self
            .on_select
            .clone()
            .unwrap_or_else(|| Rc::new(|_, _, _| {}));
        let on_columns: OnColumns = self
            .on_columns
            .clone()
            .unwrap_or_else(|| Rc::new(|_, _, _, _| {}));

        let sections = def.sections.iter().enumerate().map(|(i, section)| {
            let cols = effective_columns(section.columns, width);
            let collapsed = self.collapsed.get(i).copied().unwrap_or(false);

            let declared = section.columns;
            let header = h_flex()
                .id(SharedString::from(format!("sec-{i}")))
                .w_full()
                .justify_between()
                .px_2()
                .py_1()
                .cursor_pointer()
                .rounded_sm()
                .border_2()
                .border_color(if mode.selects_section(i) {
                    ring
                } else {
                    gpui::transparent_black()
                })
                // In design mode a header click selects the section for the inspector; in
                // runtime it collapses. Same header, different hit-testing — the whole of
                // what design mode is allowed to change.
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.mode.is_design() {
                        this.select(Some(Selection::Section(i)), cx);
                    } else {
                        this.toggle_section(i, window, cx);
                    }
                }))
                .child(SharedString::from(section.title.clone()))
                .child(if mode.is_design() {
                    // The 1 / 2 / 3 segmented control (R12). Applying it goes through
                    // `FormDef::finalize`, which clamps every child `col_span` — the UI
                    // never does its own clamp, so it cannot disagree with the server.
                    h_flex()
                        .gap_1()
                        .children((1u8..=3).map(|n| {
                            let cb = on_columns.clone();
                            div()
                                .id(SharedString::from(format!("cols-{i}-{n}")))
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .when(n == declared, |d| d.font_weight(gpui::FontWeight::BOLD))
                                .child(SharedString::from(n.to_string()))
                                .on_click(move |_, window, cx| cb(i, n, window, cx))
                        }))
                        .into_any_element()
                } else {
                    SharedString::from(format!(
                        "{} {} column{}",
                        if collapsed { "▸" } else { "▾" },
                        cols,
                        if cols == 1 { "" } else { "s" }
                    ))
                    .into_any_element()
                });

            let body = (!collapsed).then(|| {
                let fields: Vec<_> = section
                    .fields
                    .iter()
                    .map(|sf| {
                        let spec = widget_spec(sf, &self.inst, cols);
                        let selected = match self.inst.get(sf.idx) {
                            Value::Opt(c) => Some(c.0.clone()),
                            _ => None,
                        };
                        widgets::render_field(
                            &spec,
                            &self.widgets[sf.idx.as_usize()],
                            selected.as_deref(),
                            &on_pick,
                            mode,
                            &on_select,
                            cx,
                        )
                    })
                    .collect();
                widgets::form_grid(cols, fields)
            });

            v_flex()
                .w_full()
                .gap_2()
                .mb_4()
                .child(header)
                .children(body)
        });

        v_flex()
            .track_focus(&self.focus)
            // Capture phase, not bubble: the inputs bind `tab` to `IndentInline` in their
            // own key context, so a bubble handler would never see it. Capture runs from
            // the root down, before the input's action dispatch.
            .capture_key_down(cx.listener(Self::on_key))
            .size_full()
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .pb_2()
                    .child(SharedString::from(def.name.clone()))
                    // Status belongs in peripheral chrome, never where content goes.
                    .child(SharedString::from(format!(
                        "{} / {} filled{}",
                        self.inst.filled_count(),
                        def.field_count(),
                        if self.inst.is_dirty() {
                            " · unsaved"
                        } else {
                            ""
                        }
                    ))),
            )
            .child(
                // The form scrolls; the header and the palette do not. Sections are the
                // *direct* children here because `ScrollHandle::scroll_to_item` indexes
                // direct children — that is what makes scroll-into-view addressable.
                div()
                    .id("form-scroll")
                    .w_full()
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(sections)
                    .child(div().h(px(24.))),
            )
            .children(self.palette.clone())
    }
}
