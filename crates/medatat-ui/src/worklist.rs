//! The worklist screen (M6): the abstractor's caseload, straight out of local SQLite.
//!
//! Sorting and filtering are client-side and in memory. That is not an optimisation, it is
//! the requirement — R13/R14 exist so that neither ever waits on anything, and a filter that
//! round-trips to SQLite on each keystroke would reintroduce exactly the latency R15 forbids
//! showing a spinner for. The rows are read once on open and re-derived from that snapshot.
//!
//! As with the form, a keystroke in the filter box must not re-render the world: the filter
//! input is its own entity, so typing re-renders the row list and nothing above it.

use crate::widgets::{self, LineInput};
use gpui::{
    AnyElement, App, Context, FocusHandle, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{h_flex, v_flex};
use medatat_core::view::fuzzy_score;
use medatat_core::{CaseId, FormInstance};
use medatat_store::{CaseRow, Store};
use std::rc::Rc;
use std::sync::Arc;

/// What the worklist calls when a row is opened. Case-oriented rather than field-oriented,
/// so it is deliberately not the form palette's `OnChoose`.
pub type OnOpenCase = Rc<dyn Fn(CaseId, &mut Window, &mut App)>;

/// One row, with the completion count already computed. Derived once when the worklist is
/// loaded so that sorting and filtering never touch the database again.
#[derive(Clone)]
pub struct WorklistRow {
    pub case_id: CaseId,
    pub mrn: SharedString,
    pub form: SharedString,
    pub updated: SharedString,
    pub filled: usize,
    pub total: usize,
    /// Local edits not yet acknowledged by the server. Peripheral chrome only (R15).
    pub unsynced: bool,
}

/// How the table is ordered. Client-side, so switching is instant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Most recently touched first — the default, and what `Store::worklist` already returns.
    Updated,
    Mrn,
    /// Least complete first: the ones with work left on them.
    Completion,
}

pub struct WorklistView {
    all: Vec<WorklistRow>,
    /// Indices into `all`, after filter and sort.
    visible: Vec<usize>,
    selected: usize,
    sort: Sort,
    filter: LineInput,
    _filter_sub: Subscription,
    query: String,
    focus: FocusHandle,
    scroll: ScrollHandle,
    on_open: OnOpenCase,
}

impl WorklistView {
    pub fn new(
        store: &Arc<Store>,
        assignee: &str,
        limit: usize,
        on_open: OnOpenCase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let all = load_rows(store, assignee, limit);
        let filter = LineInput::new("Filter cases…", window, cx);
        let sub = filter.subscribe(window, cx, |this: &mut Self, text, _, cx| {
            this.query = text;
            this.rebuild();
            // Notifies this entity only — the shell above is untouched.
            cx.notify();
        });

        let mut this = WorklistView {
            all,
            visible: Vec::new(),
            selected: 0,
            sort: Sort::Updated,
            filter,
            _filter_sub: sub,
            query: String::new(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            on_open,
        };
        this.rebuild();
        this
    }

    /// Re-derives `visible` from the in-memory snapshot. Never touches SQLite.
    fn rebuild(&mut self) {
        let keep = self.selected_case();
        self.visible = arrange(&self.all, &self.query, self.sort);
        // Keep the cursor on the same case across a filter change where possible.
        self.selected = keep
            .and_then(|id| self.visible.iter().position(|&i| self.all[i].case_id == id))
            .unwrap_or(0);
    }

    pub fn set_sort(&mut self, sort: Sort, cx: &mut Context<Self>) {
        self.sort = sort;
        self.rebuild();
        cx.notify();
    }

    /// The case the cursor is on.
    pub fn selected_case(&self) -> Option<CaseId> {
        self.visible
            .get(self.selected)
            .map(|&i| self.all[i].case_id)
    }

    /// `Cmd/Ctrl-J` / `Cmd/Ctrl-K`. Clamps rather than wraps: on a worklist, wrapping from
    /// the last case back to the first silently loses your place in the queue.
    pub fn step(&mut self, delta: isize, cx: &mut Context<Self>) -> Option<CaseId> {
        if self.visible.is_empty() {
            return None;
        }
        let last = self.visible.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
        self.selected_case()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// A sortable column heading. Marks the active sort so the order is never a mystery.
    fn heading(
        &self,
        label: &'static str,
        sort: Sort,
        width: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // `AnyElement`, not `impl IntoElement`: under Rust 2024 the opaque type would
        // capture `cx`'s lifetime and keep the borrow alive across the next heading.
        let active = self.sort == sort;
        div()
            .id(SharedString::from(label))
            .w(width)
            .cursor_pointer()
            .when(active, |d| d.font_weight(FontWeight::BOLD))
            .child(SharedString::from(if active {
                format!("{label} ↓")
            } else {
                label.to_string()
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.set_sort(sort, cx)))
            .into_any_element()
    }
}

/// Filters and sorts, returning indices into `all`.
///
/// A free function over a slice on purpose: this is the whole of M6's "sort and filter are
/// client-side and instant" claim, and as a free function it is testable without a window,
/// which the GUI test budget of three would otherwise not stretch to.
pub fn arrange(all: &[WorklistRow], query: &str, sort: Sort) -> Vec<usize> {
    let mut hits: Vec<(u32, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            if query.is_empty() {
                return Some((0, i));
            }
            // Same fuzzy ranking as the field palette, over MRN and form name, so the two
            // search boxes in this app behave identically.
            match (fuzzy_score(query, &r.mrn), fuzzy_score(query, &r.form)) {
                (Some(a), Some(b)) => Some((a.min(b), i)),
                (Some(a), None) | (None, Some(a)) => Some((a, i)),
                (None, None) => None,
            }
        })
        .collect();

    match sort {
        // Query ranking first, then the store's own `updated_at DESC` ordering.
        Sort::Updated => hits.sort_by_key(|&(score, i)| (score, i)),
        Sort::Mrn => hits.sort_by(|a, b| all[a.1].mrn.cmp(&all[b.1].mrn)),
        // Least complete first: those are the ones with work left on them.
        Sort::Completion => hits.sort_by_key(|&(_, i)| {
            let r = &all[i];
            r.filled * 100 / r.total.max(1)
        }),
    }

    hits.into_iter().map(|(_, i)| i).collect()
}

/// Reads the caseload and computes `filled / total` per case.
///
/// One range scan per case over the clustered value rows — microseconds each, and it is why
/// the completion column can exist at all without a background job. A case whose form or
/// values fail to load is still listed, with an unknown count, rather than vanishing.
fn load_rows(store: &Arc<Store>, assignee: &str, limit: usize) -> Vec<WorklistRow> {
    let cases: Vec<CaseRow> = match store.worklist(assignee, limit) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("worklist read failed: {e}");
            return Vec::new();
        }
    };

    cases
        .into_iter()
        .map(|c| {
            let def = store.load_form(c.form_id).ok();
            let (form, total, filled) = match def {
                Some(def) => {
                    let values = store.load_case_values(c.case_id).unwrap_or_default();
                    let total = def.field_count();
                    let name = def.name.clone();
                    // `filled_count` is core's definition of "filled", not a second one.
                    let inst = FormInstance::new(Arc::clone(&def), c.case_id, c.rev, values);
                    (name, total, inst.filled_count())
                }
                None => (String::from("(unknown form)"), 0, 0),
            };
            WorklistRow {
                case_id: c.case_id,
                mrn: SharedString::from(c.mrn.unwrap_or_else(|| "—".into())),
                form: SharedString::from(form),
                updated: SharedString::from(short_date(&c.updated_at)),
                filled,
                total,
                unsynced: c.rev > c.synced_rev,
            }
        })
        .collect()
}

/// `2026-08-17T14:03:11Z` reads as `2026-08-17 14:03`. Seconds are noise in a queue.
fn short_date(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.split_once('T') {
        Some((date, time)) => {
            let hhmm: String = time.chars().take(5).collect();
            format!("{date} {hhmm}")
        }
        None => trimmed.to_string(),
    }
}

impl Render for WorklistView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.on_open.clone();
        let selected = self.selected;
        // Headings first: each needs `&mut Context`, and the row iterator borrows `self`.
        let h_mrn = self.heading("MRN", Sort::Mrn, px(140.), cx);
        let h_updated = self.heading("Updated", Sort::Updated, px(150.), cx);
        let h_filled = self.heading("Filled", Sort::Completion, px(90.), cx);
        let empty_message = if self.all.is_empty() {
            "No cases assigned."
        } else {
            "No cases match that filter."
        };
        let is_empty = self.visible.is_empty();

        let rows: Vec<_> = self
            .visible
            .iter()
            .enumerate()
            .map(|(pos, &i)| {
                let r = self.all[i].clone();
                let open = open.clone();
                let case_id = r.case_id;
                h_flex()
                    .id(SharedString::from(format!("case-{case_id}")))
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(pos == selected, |d| d.font_weight(FontWeight::BOLD))
                    .child(div().w(px(140.)).child(r.mrn.clone()))
                    .child(div().flex_1().child(r.form.clone()))
                    .child(div().w(px(150.)).child(r.updated.clone()))
                    .child(div().w(px(90.)).child(SharedString::from(format!(
                        "{} / {}{}",
                        r.filled,
                        r.total,
                        if r.unsynced { " ·" } else { "" }
                    ))))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = pos;
                        cx.notify();
                        (open)(case_id, window, cx);
                    }))
            })
            .collect();

        v_flex()
            .track_focus(&self.focus)
            .size_full()
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(SharedString::from("Worklist"))
                    .child(div().flex_1().child(widgets::render_filter(&self.filter))),
            )
            .child(
                // Column headings, matching the row layout above. Clicking one sorts —
                // in memory, over the snapshot, so it is instant by construction.
                h_flex()
                    .w_full()
                    .px_2()
                    .gap_2()
                    .child(h_mrn)
                    .child(div().flex_1().child(SharedString::from("Form")))
                    .child(h_updated)
                    .child(h_filled),
            )
            .child(
                div()
                    .id("worklist-scroll")
                    .w_full()
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows)
                    // Not a loading state: the read already happened and returned nothing.
                    .when(is_empty, |d| {
                        d.child(div().px_2().py_1().child(SharedString::from(empty_message)))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(mrn: &str, form: &str, filled: usize, total: usize) -> WorklistRow {
        WorklistRow {
            case_id: CaseId::new(),
            mrn: SharedString::from(mrn.to_string()),
            form: SharedString::from(form.to_string()),
            updated: SharedString::from("2026-08-17 09:00"),
            filled,
            total,
            unsynced: false,
        }
    }

    fn sample() -> Vec<WorklistRow> {
        vec![
            row("MRN-0003", "Trauma intake", 10, 10),
            row("MRN-0001", "Demo intake", 1, 10),
            row("MRN-0002", "Demo intake", 5, 10),
        ]
    }

    #[test]
    fn m6_empty_query_keeps_the_store_ordering() {
        // `Store::worklist` already returns `updated_at DESC`; an empty filter must not
        // reshuffle it.
        assert_eq!(arrange(&sample(), "", Sort::Updated), vec![0, 1, 2]);
    }

    #[test]
    fn m6_sort_by_mrn_is_lexicographic() {
        let rows = sample();
        let by_mrn: Vec<&str> = arrange(&rows, "", Sort::Mrn)
            .into_iter()
            .map(|i| rows[i].mrn.as_ref())
            .collect();
        assert_eq!(by_mrn, vec!["MRN-0001", "MRN-0002", "MRN-0003"]);
    }

    #[test]
    fn m6_sort_by_completion_puts_unfinished_work_first() {
        let rows = sample();
        let by_done: Vec<usize> = arrange(&rows, "", Sort::Completion)
            .into_iter()
            .map(|i| rows[i].filled)
            .collect();
        assert_eq!(by_done, vec![1, 5, 10]);
    }

    #[test]
    fn m6_filter_matches_mrn_and_form_name() {
        let rows = sample();
        // Matches the form name, so both "Demo intake" rows survive.
        assert_eq!(arrange(&rows, "demo", Sort::Updated).len(), 2);
        // Matches one MRN only.
        assert_eq!(arrange(&rows, "0003", Sort::Updated).len(), 1);
        assert!(arrange(&rows, "zzzz", Sort::Updated).is_empty());
    }

    #[test]
    fn completion_sort_survives_a_form_with_no_fields() {
        // `total` of zero must not divide by zero.
        let rows = vec![row("MRN-0001", "Empty", 0, 0)];
        assert_eq!(arrange(&rows, "", Sort::Completion), vec![0]);
    }

    #[test]
    fn updated_at_renders_without_seconds() {
        assert_eq!(short_date("2026-08-17T14:03:11Z"), "2026-08-17 14:03");
        // A value that is not ISO is passed through rather than mangled.
        assert_eq!(short_date("yesterday"), "yesterday");
    }
}
