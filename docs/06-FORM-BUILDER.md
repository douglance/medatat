# 06 — Form Builder (R4)

The UI through which a non-developer defines forms, sections, fields, and column layout.
This is the largest single piece of UI in the project — plan accordingly.

## The invariant that replaces versioning

> `field.id` is global and stable. A field's `kind` is never re-typed in place.

Changing a text field into a numeric field creates a **new** field with a new id. The old
field keeps its values, which remain viewable. This one rule is why there is no draft /
published / retired lifecycle, no per-version pinning, and no migration machinery — all of
which were cut. See [00-REQUIREMENTS.md](00-REQUIREMENTS.md#explicitly-out-of-scope).

Consequences, all deliberate:

- Edits go live immediately. There is no publish step.
- Removing a field from a section removes the **placement**, never the values.
- Re-adding that field later shows the old values again.
- `PATCH /config/fields/{id}` rejects a `kind` change with `422`; the UI surfaces this as
  **Replace field**, which creates the replacement, places it at the same position, and
  leaves the original in an "Unplaced fields" drawer.

## Layout

Three panes, plus a header.

```
┌────────────────────────────────────────────────────────────────────┐
│  Intake  ·  [Preview as abstractor]              config rev 41     │
├───────────────┬──────────────────────────────┬─────────────────────┤
│ SECTIONS      │  CANVAS                      │  INSPECTOR          │
│               │                              │                     │
│ ▾ Demographics│  ┌── Demographics ── [1|2|3]│  Field               │
│     Name      │  │ Name        │ DOB       ││  ─────────────────  │
│     DOB       │  │ [__________]│[________] ││  Label  [Date of…]  │
│     Sex       │  │ Sex                      ││  Key    dob         │
│ ▾ Vitals      │  │ ( ) M ( ) F ( ) Other    ││  Kind   date  🔒    │
│     Height    │  └──────────────────────────┘│  Required  [x]      │
│     Weight    │                              │  Col span  [1 ▾]    │
│               │  ┌── Vitals ─────── [1|2|3]│                     │
│ + Section     │  │ Height  │ Weight │ BMI  ││  [Replace field…]   │
│ + Field       │  └──────────────────────────┘│  [Remove from form] │
└───────────────┴──────────────────────────────┴─────────────────────┘
```

### The canvas is the runtime renderer

```rust
enum RenderMode {
    Runtime,
    Design { selected: Option<Selection>, drop_target: Option<DropTarget> },
}
```

`FormView` renders the same element tree in both modes. Design mode changes only:

- **Hit-testing** — a click selects the field rather than focusing its input.
- **Chrome** — selection outline, section header controls, drop indicators.
- **Input state** — widgets render but are non-interactive.

This is the single most valuable decision in the builder: WYSIWYG is free, and a layout bug
can never appear in one mode but not the other. **Do not build a second renderer.**

"Preview as abstractor" flips `mode` to `Runtime` against a scratch case, so a coordinator
can tab through their own form.

## Left pane — section tree

Sections with their fields nested, in ordinal order. Supports:

- Select (drives the inspector).
- Reorder sections and fields.
- Add section, add field, remove placement.
- An **Unplaced fields** group listing fields that exist but sit in no section — the
  destination for replaced fields and removed placements.

**Reordering: build the buttons first.** `gpui-component` has no drag-and-drop primitive,
and drag-reorder is a genuine unknown on this toolkit. Ship `Alt-Up` / `Alt-Down` and
explicit ↑/↓ buttons in M5; add drag afterwards if it proves cheap. **Reordering by drag is
not a requirement** — do not let it block the milestone.

## Centre pane — canvas and column control (R12)

Each section header carries a **1 / 2 / 3 segmented control** bound directly to
`section.columns`. Changing it issues `PATCH /config/sections/{id}` and re-renders
immediately.

**Reducing the column count clamps every child `col_span` in the same transaction** — a
field spanning 3 in a section reduced to 2 becomes a 2-span. The API guarantees this
atomically; the UI must not attempt its own clamp beforehand or the two will disagree.

Field selection is a click. Multi-select is out of scope.

## Right pane — inspector

Contextual on selection.

### Section selected

| Control | Binds to |
|---|---|
| Name | `section.name` |
| Columns | `section.columns` (1–3) |
| Ordinal | `section.ordinal` |
| Delete section | cascades placements, **never values** |

### Field selected

| Control | Binds to | Notes |
|---|---|---|
| Label | `section_field.label` | Free text; per-placement |
| Key | `field.key` | Immutable after creation |
| Kind | `field.kind` | **Read-only, lock icon.** Offers *Replace field* |
| Required | `section_field.required` | |
| Col span | `section_field.col_span` | Clamped to `section.columns` |
| Kind-specific config | `field.config` | See below |

### Kind-specific config (R5–R11)

| Kind | Controls |
|---|---|
| `text` | Max length |
| `textarea` | Rows, max length |
| `numeric` | Min, max, decimal places (`scale`). Min/max entered as text, parsed as `Decimal` |
| `date` | none |
| `time` | none — always 24hr (R8) |
| `radio` | Option list editor |
| `select` | Option list editor, searchable toggle |

### Option list editor (R9, R10)

Rows of `code` + `label`, reorderable, with add and remove. `code` is what gets stored in
`field_value`; `label` is display only.

**Removing an option that is in use does not delete stored values.** Those values render as
`code (unknown option)` in a warning style. The editor warns before removing an option, but
does not prevent it — a coordinator correcting a mistake must be able to.

**The display half of that rule is as load-bearing as the non-deletion half**, and is easier
to get wrong because nothing fails. An implementation that preserves the value but renders
an unmatched code as *nothing* — no radio selection, an empty select — is worse than the
removal that caused it: the data is intact in `field_value` and still exports, but the
abstractor sees a blank field and has no way to know a value is there. Render the raw code
rather than nothing.

Option lists here are for small vocabularies. See
[10-LIMITATIONS.md](10-LIMITATIONS.md#3-large-coded-option-lists-are-not-supported).

## Validation before write

Client-side, mirroring the server checks so the coordinator gets immediate feedback:

- `col_span <= section.columns`
- `columns` ∈ 1..=3
- `key` unique and matching `^[a-z][a-z0-9_]{0,63}$`
- numeric `min <= max`, `scale <= 10`
- radio/select have at least two options, with unique codes
- label non-empty

Server-side validation is authoritative and uses the same `medatat_core` functions.

## Permissions

Builder screens require `role = admin`. Non-admins never see the entry point, and the API
returns `403` regardless — the UI is not the enforcement point.

## Config propagation

Every mutation bumps `config_version.rev`. Clients poll `GET /config?since_rev=<n>` every
60 s and on focus. On change, the client replaces local `form` rows and rebuilds its
`FormRegistry`.

**Forms already open in an editor are not hot-swapped.** The new definition applies on next
open. Swapping a form's structure under an abstractor mid-entry would be hostile, and with
stable field ids there is no correctness reason to do it.

## Acceptance (M5)

A coordinator, using only the UI and with no deploy:

1. Creates a form named "Intake".
2. Adds a section "Demographics" set to 2 columns, and a section "Vitals" set to 3.
3. Adds at least one field of **every** kind: text, numeric, date, time, radio, select,
   textarea (R5–R11).
4. Sets one field to span 2 columns.
5. Marks two fields required.
6. Uses "Preview as abstractor" and tabs through the whole form.
7. An abstractor on another machine opens a new case on that form within 60 s and fills it,
   including a 24hr time value.
8. The coordinator reduces "Vitals" from 3 columns to 2 and confirms the spanning field
   clamps rather than overflowing.
9. The coordinator replaces a text field with a numeric one and confirms the original values
   remain viewable in the Unplaced fields drawer.
