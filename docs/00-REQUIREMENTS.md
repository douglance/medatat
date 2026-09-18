# 00 — Requirements

The authoritative statement of what medatat must do. Every other document derives from
this one. If a design decision conflicts with this file, this file wins.

## Verbatim requirements

> Data abstraction tool for medical data. Must support arbitrary, data driven fields
> within a configurable set of forms. Forms and fields are configured via a UI, and can
> support text fields, numeric fields, dates, time entry in 24hr format, radio lists,
> select dropdowns, and text areas. Should be able to control the number of columns for
> each section of the form. (e.g. 1-3 column wide) Loading and saving of forms must be
> under 200ms with hundreds of fields on the form. (No loading spinners. Ever.) Must work
> at the scale of ~100 million saved field values across a hundred thousand patient cases.

## Decomposed requirements

Each requirement has a stable ID. Code comments, tests, and commit messages reference
these IDs.

| ID  | Requirement | Acceptance criterion |
|-----|-------------|----------------------|
| R1  | Data abstraction tool for medical data | Case-centric model. Synthetic data only today; PHI protections implemented behind the `phi` feature, off by default |
| R2  | Arbitrary, data-driven fields | A new field can be created at runtime through the UI and immediately hold values, with no code change, no deploy, and no schema migration |
| R3  | Fields live in a configurable set of forms | Forms, sections, and field placements are rows, editable at runtime |
| R4  | Forms and fields configured via a UI | A non-developer can create a form with sections and fields, set column counts, and publish it for abstractors |
| R5  | Text fields | Renders a single-line text input; value round-trips |
| R6  | Numeric fields | Renders a numeric input; exact decimal semantics; min/max/scale enforced |
| R7  | Date fields | Renders a date picker; value round-trips as a calendar date |
| R8  | Time entry in 24hr format | Renders a 24-hour time input. Never AM/PM, never locale-dependent. `24:00` and `12:60` are rejected |
| R9  | Radio lists | Renders a radio group from a configured option list; keyboard-cyclable |
| R10 | Select dropdowns | Renders a dropdown from a configured option list; type-ahead works |
| R11 | Text areas | Renders a multi-line text input; value round-trips |
| R12 | 1–3 columns per section | Each section stores a column count of 1, 2, or 3; fields may span up to that count |
| R13 | Form load < 200ms with hundreds of fields | Bench 1, gated at 5 ms. **Measured 195 µs** |
| R14 | Form save < 200ms with hundreds of fields | Bench 2, gated at 10 ms. **Measured 5.2 ms** for 300 fields, 86 µs for one |
| R15 | No loading spinners. Ever. | A lint asserting no spinner/progress/skeleton exists in the UI crate. Bench 3 measures open-to-paint but **asserts nothing** — two of its four spans live in `medatat-ui` |
| R16 | ~100M field values across ~100k patient cases | **Measured 2026-09-17.** Bench 5 at 100,000 cases x 1,000 fields = 100,000,000 values. Against a one-case control interleaved in the same process: **0.97x per load, 1.07x per save** (tolerance 1.5x) |

## Compliance matrix

| ID | Design element | Document | Verified by |
|----|----------------|----------|-------------|
| R1 | Synthetic corpus; `phi`-gated SQLCipher, redaction, and zeroize | [12](12-PHI-READINESS.md) | `--features phi` CI job; `store::tests::wrong_key_fails`, `core::tests::debug_redacts` |
| R2 | EAV rows keyed by a globally stable `field_id`; no DDL to add a field | [02](02-DATA-MODEL.md) | `sync::tests::runtime_field_round_trip` |
| R3 | `form` / `section` / `section_field` tables | [02](02-DATA-MODEL.md) | `worker::tests::form_crud` |
| R4 | Three-pane builder; canvas is the runtime renderer in design mode | [06](06-FORM-BUILDER.md) | M5 acceptance demo |
| R5–R11 | `FieldKind` variants mapped to gpui-component widgets | [05](05-UI-SPEC.md) | `widget_spec` snapshot tests, one per kind |
| R8 | Hand-built masked input; `parse_time_24` / `format_time_24` | [05 §Time field](05-UI-SPEC.md#the-24-hour-time-field-r8) | `core::value::time` proptests |
| R12 | `section.columns` CHECK 1..3; `col_span` clamped at render | [02](02-DATA-MODEL.md), [05](05-UI-SPEC.md) | `effective_columns` unit tests |
| R13 | Local encrypted SQLite is the UI's read path | [01](01-ARCHITECTURE.md) | Bench 1 ✅ 195 µs |
| R14 | Local synchronous write + background outbox drain | [04](04-SYNC.md) | Bench 2 ✅ 5.2 ms |
| R15 | Whole assigned caseload pre-synced before the user opens anything | [04 §Caseload pre-sync](04-SYNC.md#caseload-pre-sync) | spinner lint ✅; caseload pre-sync itself **not yet wired to the UI** |
| R16 | `WITHOUT ROWID` on `(case_id, field_id)`; one Durable Object per case, ~128 KB of a 10 GB budget each | [01](01-ARCHITECTURE.md), [02](02-DATA-MODEL.md) | Bench 5 at 100M values ✅ 0.97x read, 1.07x write |

## Verification status — 2026-09-17

Read this before trusting a tick anywhere else.

**Verified by measurement:** R13 (195 µs against a 200 ms requirement), R14 (5.2 ms for 300
fields; 86 µs for the single-field case that actually happens while typing), and **R16**, at
the full 100,000,000 values.

**Verified by execution:** R5–R12 through widget-spec snapshots and 14 headless
`#[gpui::test]` cases that dispatch real keystrokes; R2/R3 through runtime round-trips;
form-builder acceptance item 9 through a real SQLite store.

**Verified structurally:** R15 — a lint asserts no spinner exists, and the design makes one
unnecessary rather than merely discouraged.

**R16 is verified, and the corpus was built after all.** Bench 5 seeded
**100,000 cases x 1,000 fields = 100,000,000 values** in 407 seconds, then measured a
one-case store and the full corpus *alternately in the same process*, so host load falls on
both equally rather than being compared across runs:

| measurement | 1 case | 100,000 cases | ratio |
|---|---|---|---|
| load 1000 values p50 | 260 µs | 253 µs | **0.97x** |
| load p99 | 2.148 ms | 2.124 ms | 0.99x |
| save 300 fields p50 | 2.685 ms | 2.757 ms | 1.03x |
| save mean | 2.807 ms | 3.004 ms | **1.07x** |

A hundred thousand times the data costs nothing measurable. Absolute figures on that host,
under a load average of 15: **917 µs to open a case, 2.87 ms to save 300 fields**, against a
200 ms requirement — and `0/200` saves exceeded the 10 ms gate, with the WAL flat across the
slowest five, so the tail is not checkpoint stalling.

**What this does not settle.** The earlier claim that the corpus was *infeasible* confused
two different limits, and only one of them was ever real:

- **Server seeding throughput is still 0.12 cases/s in production**, still plateauing at
  1.75x under concurrency, still most likely bound on `case_index` contention in D1. A 100k
  corpus through the Worker write path is still ~131 hours. That is a **seeding** limit, not
  a storage one, and R16 does not ask about it.
- **Durable Object cold wake: 0.4–1.0 s**, against 0.16 s warm. Three to six times the whole
  200 ms budget of R13 — not a problem but a vindication, and exactly why
  [ADR-0002](adr/0002-encrypted-local-sqlite.md) keeps the network off the critical path. A
  design that awaited the network could not meet R13 on a cold DO, ever.

So: the **client storage layer** is verified at 100M values by direct measurement. Seeding a
production corpus at that size through the Worker remains slow and unattempted. Anyone who
needs the latter should first establish whether the `case_index` update can be debounced.

**Platform coverage.** All three platforms now build and pass the full test suite in CI,
including the headless GPUI suite. That is narrower than it sounds: CI never opens a window,
and **no human has typed into this application on anything but macOS**. The keyboard tests
were themselves macOS-shaped until 2026-08-18 — they hardcoded `cmd-` and could only ever
have passed there — which is precisely the kind of assumption a green cross-platform job
does not catch. See [10-LIMITATIONS §10](10-LIMITATIONS.md#10-keyboard-evidence-was-macos-only--and-the-tests-not-the-app-were-the-problem).

## Explicitly out of scope

These were considered and cut. Each is additive later; none is required.

- **Audit trail / change history.** Not stated in the requirements. The expensive one to
  retrofit, because it wants to be written in the same transaction as the value.
- **Form version lifecycle** (draft/published/retired, per-version pinning). Replaced by a
  single invariant: a field's `kind` is never re-typed in place; changing it creates a new
  field and leaves old values intact.
- **Conditional logic** (show-if, required-when, cross-field validation). Type-level
  validation stays, because "numeric field" and "24hr time field" are meaningless without it.
- **Cross-case reporting and analytics.** See [10-LIMITATIONS.md](10-LIMITATIONS.md#1-cross-case-reporting-is-not-built).
- **Accessibility / screen-reader support.** Confirmed not required. Keyboard navigation is
  still a first-class product feature — see [05 §Keyboard model](05-UI-SPEC.md#keyboard-model).
- **Offline-only deployment.** The app works offline, but sync requires network eventually.

## Assumptions of record

1. Abstractors work an assigned caseload of hundreds of cases, not all 100k. This is what
   makes caseload pre-sync (and therefore R15) tractable. If a user must be able to open
   any of 100k cases at random with no warning, revisit [04-SYNC.md](04-SYNC.md).
2. Two abstractors editing the *same field* of the *same case* simultaneously is rare.
   Handled by per-field conflict detection with a user-resolved strip, not by merge.
3. Option lists per field are small (tens, not tens of thousands). See
   [10-LIMITATIONS.md](10-LIMITATIONS.md#3-large-coded-option-lists-are-not-supported).
