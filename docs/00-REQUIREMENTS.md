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
| R16 | ~100M field values across ~100k patient cases | **Not yet verified.** Per-case sizing (~100 KB of a 10 GB DO budget) is an extrapolation; the 100k corpus has never been built. See [10-LIMITATIONS](10-LIMITATIONS.md) |

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
| R16 | One Durable Object per case, ~100 KB of a 10 GB budget each | [01](01-ARCHITECTURE.md), [02](02-DATA-MODEL.md) | **unverified at scale** |

## Verification status — 2026-08-18

Read this before trusting a tick anywhere else.

**Verified by measurement:** R13 (195 µs against a 200 ms requirement), R14 (5.2 ms for 300
fields; 86 µs for the single-field case that actually happens while typing).

**Verified by execution:** R5–R12 through widget-spec snapshots and 14 headless
`#[gpui::test]` cases that dispatch real keystrokes; R2/R3 through runtime round-trips;
form-builder acceptance item 9 through a real SQLite store.

**Verified structurally:** R15 — a lint asserts no spinner exists, and the design makes one
unnecessary rather than merely discouraged.

**Not verified: R16, and it is now clear why.** Per-case sizing is an extrapolation; the
100k-case corpus has never been built. The two numbers that would settle it have since been
measured against the deployed Worker rather than the emulator, and they are what makes the
corpus infeasible rather than merely unbuilt:

- **Seeding throughput: 0.12 cases/s in production.** The local figure was 1.90 — 16×
  optimistic, an emulator artifact, and retracted. At the real rate a 100k corpus takes
  roughly 131 hours, and adding concurrency does not fix it: throughput plateaus at 1.75×
  and is server-bound, most likely on `case_index` contention in D1.
- **Durable Object cold wake: 0.4–1.0 s**, against 0.16 s warm. That is three to six times
  the entire 200 ms budget of R13 — which is not a problem but a vindication: it is exactly
  the reason [ADR-0002](adr/0002-encrypted-local-sqlite.md) keeps the network off the critical path.
  A design that awaited the network could not meet R13 on a cold DO, ever.

So R16 rests on per-case extrapolation from a 500k-row local corpus (Bench 5, where the R13
margin is flat) plus the production per-case cost. That is honest evidence for the shape of
the curve and no evidence at all for the endpoint.

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
