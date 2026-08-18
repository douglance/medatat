//! The `Cmd/Ctrl-F` field palette (`medatat_core::view::search_fields`).
//!
//! This is a separate gpui entity for one reason, and it is the same reason the whole
//! keystroke path is shaped the way it is: **a keystroke must never re-render the form.**
//! If the palette lived on `FormView` as plain state, every character typed into the query
//! box would notify `FormView` and rebuild all 300 fields — the exact quadratic hazard the
//! module note in `view.rs` exists to prevent, arriving through a different door. As its own
//! entity, a query keystroke re-renders the result list and nothing else.

use crate::widgets::{self, OnChoose, PaletteInput};
use gpui::{Context, IntoElement, Render, Subscription, Window};
use medatat_core::{FieldIdx, FormDef, search_fields};
use std::sync::Arc;

pub struct FieldPalette {
    def: Arc<FormDef>,
    query: PaletteInput,
    /// Best match first, straight from core's ranking.
    matches: Vec<FieldIdx>,
    on_choose: OnChoose,
    _sub: Subscription,
}

impl FieldPalette {
    pub fn new(
        def: Arc<FormDef>,
        on_choose: OnChoose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = PaletteInput::new(window, cx);
        let sub = query.subscribe(window, cx, |this: &mut Self, text, _, cx| {
            this.matches = search_fields(&this.def, &text);
            // Notifies this entity only. `FormView` is untouched.
            cx.notify();
        });
        // An empty query matches everything, so the palette opens as a jump list.
        let matches = search_fields(&def, "");
        FieldPalette {
            def,
            query,
            matches,
            on_choose,
            _sub: sub,
        }
    }

    /// Puts the caret in the query box.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.focus_handle(cx).focus(window, cx);
    }

    /// What Enter accepts.
    pub fn first_match(&self) -> Option<FieldIdx> {
        self.matches.first().copied()
    }
}

impl Render for FieldPalette {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<(FieldIdx, String)> = self
            .matches
            .iter()
            .filter_map(|&idx| self.def.field_at(idx).map(|sf| (idx, sf.label.clone())))
            .collect();
        widgets::render_palette(&self.query, &rows, &self.on_choose)
    }
}
