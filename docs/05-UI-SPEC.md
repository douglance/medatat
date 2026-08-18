# 05 — UI Specification

Owned by `medatat-ui`. **The only crate that may `use gpui`.**

## Toolkit

`gpui` + `gpui-component` (longbridge), both Apache-2.0.

```toml
gpui           = { version = "0.2.2", git = "https://github.com/zed-industries/zed", rev = "<PIN>" }
gpui_platform  = { git = "https://github.com/zed-industries/zed", rev = "<PIN>",
                   features = ["font-kit", "x11", "wayland"] }
gpui-component = { git = "https://github.com/longbridge/gpui-component", rev = "<PIN>" }
```

**Pin `rev`, never `branch`.** The crates.io `gpui` 0.2.2 release is stale and structurally
incompatible with current `main` (which split out the unpublished `gpui_platform`), so a git
dependency is the only path. Commit `Cargo.lock`. A weekly CI job bumps the revs and builds,
keeping upgrades incremental instead of a cliff. Rationale: [ADR-0003](adr/0003-gpui-component.md).

**Wrap every `gpui-component` call** in `medatat_ui::widgets::*`. An upstream breaking change
must be a one-file fix, not a hundred-site edit.

## Widget mapping (R5–R11)

| Requirement | `FieldKind` | Widget | Notes |
|---|---|---|---|
| R5 | `Text` | `Input` | `max_len` enforced |
| R6 | `Numeric` | `NumberInput` | `Decimal`; min/max/scale validated |
| R7 | `Date` | `DatePicker` | ISO display, never locale |
| R8 | `Time` | **hand-built** (below) | gpui-component has no TimePicker |
| R9 | `Radio` | `Radio` group | arrow-key cycling |
| R10 | `Select` | `Select` / `Combobox` | `searchable` picks which |
| R11 | `Textarea` | `Textarea` | `rows` sets initial height |

## View model

```rust
pub struct FormView {
    inst:        FormInstance,          // from medatat-core
    widgets:     Vec<WidgetState>,      // indexed by FieldIdx
    _subs:       Vec<Subscription>,
    focus_order: Vec<FieldIdx>,
    collapsed:   FixedBitSet,           // by section index
    mode:        RenderMode,
    case_id:     CaseId,
}

enum WidgetState {
    Input(Entity<InputState>),          // Text, Numeric, Textarea, Time
    Date(Entity<DatePickerState>),
    Select(Entity<DropdownState>),
    Radio,                              // stateless — selection lives in FormInstance
}

pub enum RenderMode {
    Runtime,
    Design { selected: Option<Selection>, drop_target: Option<DropTarget> },
}
```

### Widget lifecycle: eager, with the section as the laziness boundary

Create all widgets when the form opens. 300 `Entity<InputState>` is roughly 300 slotmap
slots plus 300 small Ropes — under 1 MB, ~1–3 ms to build. That is well inside the R13
budget.

Lazy per-field creation is **wrong** here: Tab traversal, scroll-into-field, and Cmd-F all
need the entity to exist, and first-focus materialisation produces exactly the jank R15
exists to prevent. Radio groups and non-searchable selects need no entity at all, which cuts
the count materially.

If a form ever exceeds ~800 fields, materialize on first section expand. Sections are
already collapsible, so that boundary costs nothing to add later.

### Do not virtualize the form body

`uniform_list` requires identical item heights, which heterogeneous form fields do not have.
Beyond that, virtualizing breaks Tab order, focus retention, scroll-into-field, and Cmd-F —
every one of which is core to keyboard-driven abstraction. **Section collapsing is the
correct lever** for large forms.

### Event wiring — the performance rule

```rust
for (i, sf) in def.iter_fields().enumerate() {
    let idx = FieldIdx(i as u32);
    subs.push(cx.subscribe(&state, move |this, ent, ev: &InputEvent, cx| {
        if matches!(ev, InputEvent::Change) {
            let raw = ent.read(cx).value().to_string();
            this.on_field_edit(idx, raw, cx);   // parse → inst.set → store::apply_local
            // Deliberately NO cx.notify(). The focused InputState re-renders itself.
        }
    }));
}
```

**A keystroke must never `cx.notify()` the parent.** Doing so rebuilds all 300 fields per
character and turns typing into an O(n) operation. Because conditional logic is cut, the
parent has essentially no reason to re-render mid-edit: no visibility recompute, no layout
invalidation, no neighbour revalidation.

The parent re-renders only on: section collapse/expand, conflict arrival, inbound sync
values, and window resize crossing a column breakpoint.

Each closure captures only a `u32`. Capturing `Arc<FieldDef>` in 300 closures is a
measurable memory regression and is not necessary.

Under `--features phi`, `FormView::drop` explicitly clears every `InputState` — those Ropes
would hold PHI, so closing a case is not merely dropping a view. Tested by
`ui::tests::closing_case_clears_inputs`, which runs in the `phi` CI job. Default builds skip
it. See [12-PHI-READINESS.md](12-PHI-READINESS.md).

## Column layout (R12)

```rust
fn render_section(&self, s: &SectionDef, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
    let cols = effective_columns(s.columns, window.viewport_size().width) as usize;
    v_form()
        .columns(cols)
        .label_width(px(160.))
        .gap(px(12.))
        .children(s.fields.iter().map(|sf| {
            field()
                .label(sf.label.clone())
                .required(sf.required)
                .col_span((sf.col_span as usize).min(cols))   // defensive clamp
                .when_some(self.inst.error(sf.idx), |f, e| f.invalid(e.to_string()))
                .child(self.render_widget(sf, window, cx))
        }))
}
```

```rust
// medatat-core — pure, unit-tested, no gpui types
pub fn effective_columns(declared: u8, width_px: f32) -> u8 {
    debug_assert!((1..=3).contains(&declared));
    match width_px {
        w if w < 720.0  => 1,
        w if w < 1080.0 => declared.min(2),
        _               => declared,
    }
}
```

`col_span <= columns` is enforced at write time in the Worker
([03-API.md](03-API.md#form-section-field-mutations)) and clamped again at render. Reducing
a section's `columns` clamps every child `col_span` in the same transaction.

**Fallback:** if `gpui-component`'s `Form` mishandles spans or gaps, hand-roll the grid with
`div().flex().flex_wrap()` and computed fractional widths. Keep it behind
`medatat_ui::widgets::form_grid()` so the swap is one file.

## The 24-hour time field (R8)

`gpui-component` ships no TimePicker. This is the one widget we build. **Do not use a live
mask** — reformatting while typing moves the caret and is the number-one masked-input bug.

```rust
// medatat-core::value::time — the most heavily tested function in the codebase
pub fn parse_time_24(s: &str) -> Result<NaiveTime, TimeParseError>;
pub fn format_time_24(t: NaiveTime) -> String;   // always "HH:MM", zero-padded
```

### Accepted input

| Input | Result | | Input | Result |
|---|---|---|---|---|
| `"9"` | 09:00 | | `"0930"` | 09:30 |
| `"09"` | 09:00 | | `"9:30"` | 09:30 |
| `"930"` | 09:30 | | `"09:30"` | 09:30 |
| `"9:5"` | 09:05 | | `"2359"` | 23:59 |
| `"0000"` | 00:00 | | `"00:00"` | 00:00 |

### Rejected

`"24:00"`, `"2400"`, `"1260"`, `"12:60"`, `"-1"`, `"9:30pm"`, `"abc"`, `""`, `"999999"`

24-hour always. **No AM/PM. No locale. Never call OS locale formatting.**

### Interaction

- `InputState` with `max_len(5)` and a **rejection** pattern `^[0-9:]{0,5}$`. It blocks
  disallowed characters; it never rewrites what the user typed.
- **While typing:** validate for styling only. No reformatting, no caret movement. Insert
  `:` automatically only when the caret is at end-of-text and the third digit is entered —
  safe because there is no text after the caret to displace.
- **On blur / Tab / Enter:** run `parse_time_24`. On `Ok`, replace with
  `format_time_24(t)`. On `Err`, keep the raw text, show a red border and message, and mark
  the field invalid. Never silently discard input.
- **`Up` / `Down`:** ±1 minute. **`Shift+Up` / `Shift+Down`:** ±1 hour. Both wrap at
  midnight. Bound on a wrapper div gated on focus.

### Tests

```rust
proptest! {
    #[test]
    fn round_trips(h in 0u32..24, m in 0u32..60) {
        let t = NaiveTime::from_hms_opt(h, m, 0).unwrap();
        prop_assert_eq!(parse_time_24(&format_time_24(t)).unwrap(), t);
    }
}
```

Plus an explicit table test over every accepted and rejected string above.

## No-spinner rule (R15)

**No spinner, progress bar, skeleton, or "Loading…" text may exist anywhere in
`medatat-ui`.** This is enforced by a CI lint, not by review discipline:

```bash
# xtask/src/lint_no_spinner.rs, run in CI
grep -rniE 'spinner|loading\.\.\.|progressbar|skeleton|shimmer' crates/medatat-ui/src/ \
  && { echo "R15 violation"; exit 1; } || exit 0
```

The rule is satisfiable because it is structurally true, not because it is worked around:
reads are local and take microseconds, and the caseload is synced before the user opens
anything ([04-SYNC.md](04-SYNC.md#caseload-pre-sync)).

**Permitted status indicators**, none of which block or overlay content:

- A persistent "N unsynced" count in the window chrome.
- A one-line caseload-sync progress note in the worklist footer during initial login sync.
- Per-field conflict strips.

The distinction: an indicator reports background state in peripheral chrome. A spinner
occupies the place where content belongs and tells the user to wait. The second is banned.

## Keyboard model

Not an accessibility obligation — abstractors are keyboard-heavy power users, so this is a
product feature.

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | Move through `focus_order`; collapsed sections contribute no stops |
| `Enter` | Commit and advance (single-line inputs) |
| `Ctrl-Enter` | Advance out of a textarea |
| `Cmd/Ctrl-F` | Field-search palette: fuzzy-match labels, expand section, scroll, focus |
| `Alt-Up` / `Alt-Down` | Previous / next section |
| `Alt-Left` / `Alt-Right` | Collapse / expand section |
| `Cmd/Ctrl-J` / `Cmd/Ctrl-K` | Previous / next case in the worklist |
| `Up` / `Down` | Radio group cycling; time field ±1 minute |
| type-ahead | Select dropdowns jump by first letters |

- **Scroll-into-view on focus, with no animation.** Animation reads as latency in a
  data-entry tool.

  **Partially implemented, deliberately.** Scrolling works at *section* granularity and
  there is **no 120 px margin**. gpui's `ScrollHandle::scroll_to_item(ix)` indexes direct
  children of the scrolled container, and the direct children are sections — fields are
  nested inside `Form` grids so that `Form` owns `col_span` (R12). Per-field targeting with
  a margin needs either per-field bounds tracking or flattening the grids, and flattening
  would cost R12. A hand-computed offset via `bounds_for_item` + `set_offset` is possible
  but was not taken: the sign convention could not be verified on a locked screen, and a
  wrong guess scrolls the wrong way undetectably. Revisit when someone can see the result.
- **Always-visible high-contrast focus ring.** A keyboard user must never guess where they are.
- **Every focus move expands its containing section first.** Tab, `Alt-Up`/`Down`, and a
  palette pick all route through one `reveal_field`. Focusing into a collapsed section would
  otherwise put the caret where nothing is drawn.
- `focus_order` is derived from section and field ordinals, filtered by collapsed state.
  Tested by `ui::tests::tab_order_matches_focus_order`.

## Screens

### Worklist

Table of the user's assigned cases: MRN, form, updated, completion (`filled / total`).
`Cmd-J`/`K` navigates. Sorting and filtering are client-side over local SQLite, so both are
instantaneous. Footer carries the sync indicator.

### Case editor

Header: MRN, form name, unsynced count. Body: sections in ordinal order, each a collapsible
group rendering its column grid. Conflict strips render inline above the affected field.

### Form builder

See [06-FORM-BUILDER.md](06-FORM-BUILDER.md).

### Login

Two states: email entry, then 6-digit code entry. On success, caseload pre-sync begins and
the worklist appears immediately, populating as it syncs.

## Testing

Three `#[gpui::test]` cases only. Everything else is tested one level below pixels.

1. `subscriptions_fire_once_per_edit` — 300 subscriptions, one edit, assert a counter equals
   1. **This is the anti-quadratic guard and it must fail CI when broken.**
2. `tab_order_matches_focus_order` — including that collapsed sections contribute no stops.
3. `closing_case_clears_inputs` — PHI hygiene.

For everything else, `render_widget` derives from a pure function:

```rust
pub fn widget_spec(sf: &SectionField, inst: &FormInstance) -> WidgetSpec;
```

`WidgetSpec` is a plain data description (kind, value, error, span, enabled, options).
Snapshot-testing it gives full R5–R12 regression coverage with no GUI, no window, and no
GPU.
