# ADR-0003 — GPUI with `gpui-component` for the desktop UI

**Status:** Accepted · **Date:** 2026-08-17 · **Drives:** R4–R12

## Context

The client is a Rust GPUI desktop app on macOS, Windows, and Linux. GPUI itself ships **no
widgets** — only `Div`, `Img`, `Svg`, `Canvas`, `List`, `UniformList`, `StyledText`,
`InteractiveText`, `Deferred`, `Surface`, plus IME plumbing. Forms need text, numeric, date,
24hr time, radio, select, and textarea inputs (R5–R11) with 1–3 column layout (R12).

## Options surveyed

| Library | crates.io | License | Stars | Activity | Form widgets |
|---|---|---|---|---|---|
| **gpui-component** (longbridge) | 0.5.2 | Apache-2.0 | 12.9k | commits daily | Input, NumberInput, Textarea, Select, Combobox, Radio, Checkbox, DatePicker, Calendar, Table, Form with `.columns()`/`.col_span()`. **No TimePicker** |
| adabraka-ui | 0.3.9 | MIT | 455 | ~6 months stale | All of the above **+ TimePicker** — but depends on a **personal fork of GPUI** |
| gpui-ui-kit | 0.5.10 | ISC | 47 | active | Input, NumberInput, Select, Checkbox. No date/time/radio/form layout |
| fluix | 0.1.25 | MIT | 22 | abandoned | Minimal |
| Zed's own `ui` crate | **unpublished** | GPL-3.0 | — | active | Maintainers: "intended specifically for use with Zed" |

## Decision

**`gpui-component`, and build the 24hr time input ourselves.**

## Rationale

This is close to a monoculture finding, which is itself the useful result: `gpui-component`
is the only GPUI form library that is actively maintained, permissively licensed,
tri-platform, and tracking upstream GPUI.

**Adabraka disqualifies itself on the one thing that looked attractive.** It has the
TimePicker we lack, but its `Cargo.toml` reads
`gpui = { package = "adabraka-gpui", version = "0.5" }` — a single author's personal fork of
GPUI. Adopting it means inheriting that fork's rebase lag and platform bugs with no path
back upstream, and its last commit is ~6 months old. Taking on a forked GUI framework to
avoid building one widget is a bad trade.

**The gap is genuinely small.** A 24hr time entry is a text input with `HH:MM` validation.
Our design ([05-UI-SPEC.md](../05-UI-SPEC.md#the-24-hour-time-field-r8)) deliberately avoids
live masking — which is the correct interaction design anyway, since live reformatting moves
the caret — so it needs no masking API from any library. Under a day of work.

`gpui-component`'s Form provides `.columns(n)`, `.col_span(n)`, `.col_start(n)`, and
`.label_width(px)`, which maps directly onto R12.

## Verified

The M0 probe built this dependency set on macOS: **894 packages, 7m13s cold, links to a
working binary.** Pins:

- `zed` @ `aa3718614b3ade75524be6f8b2e101bd1166e02c`
- `gpui-component` @ `972a3ebfd01afca7da6d8b6f31c9a51288ea5565`

Two things the research did not predict, both confirmed by reading the resolved checkout:

- **`gpui-component` does not pin a `gpui` rev** — it tracks Zed's branch HEAD. We must pin
  both ourselves or the build is not reproducible. It also pulls `gpui_macros`, `gpui_web`,
  and `reqwest_client` from the Zed repo.
- It requires **one `[patch.crates-io]` entry**: `psm = { git = ".../stacker", branch = "master" }`.
  Do *not* add a `[patch."https://github.com/zed-industries/zed"]` section — patching a git
  source with itself is a hard cargo error.

Also confirmed against the source: `crates/ui/src/time/` contains **only** `Calendar` and
`DatePicker`. There is no TimePicker, so R8's hand-built widget stands. And
`gpui-component` ships a `decimal` feature enabling `rust_decimal`, which R6 uses.

Windows and Linux remain untested.

## Consequences

**Dependency shape.** The crates.io `gpui` 0.2.2 release is ~10 months stale and
structurally incompatible with current `main`, which split platform code into an unpublished
`gpui_platform` crate. Git dependencies are the only viable path:

```toml
gpui           = { version = "0.2.2", git = "…/zed", rev = "<PIN>" }
gpui_platform  = { git = "…/zed", rev = "<PIN>", features = ["font-kit","x11","wayland"] }
gpui-component = { git = "…/gpui-component", rev = "<PIN>" }
```

**Pinning: not what the plan assumed.** The intended rule was "pin `rev`, never `branch`".
That turns out to be **unimplementable**, and discovering why cost about half the UI's
initial compile errors.

`gpui-component` declares its Zed dependencies *unpinned*. Cargo treats `git+URL` and
`git+URL?rev=X` as two **distinct sources**, so pinning our side put two copies of `gpui`
in the graph — and every `gpui` type mismatched across the medatat-ui/gpui-component
boundary. The symptoms looked like API errors (`Root: Render` unsatisfied,
`gpui_component::init` rejecting `&mut App`) and were not.

`[patch."https://github.com/zed-industries/zed"]` does not fix it either: Cargo rejects a
patch whose source URL equals the one being patched.

The only working configuration is to **match gpui-component's source spec exactly** — no
`rev =` on the `gpui`/`gpui_platform` workspace entries — and let the **committed
`Cargo.lock`** carry the commit. Reproducibility is preserved (the lockfile holds exactly
one `gpui` at `aa3718614b3ade75524be6f8b2e101bd1166e02c`), but it now depends on the
lockfile rather than the manifest. **`Cargo.lock` must never be gitignored.**

`gpui-component` itself is still pinned by `rev`, because nothing else depends on it. A weekly CI job bumps revs and builds so
upgrades stay incremental rather than becoming a cliff. Cargo clones ~1 GB of Zed history —
set `CARGO_NET_GIT_FETCH_WITH_CLI=true` and cache `~/.cargo/git`.

**Wrap every call** in `medatat_ui::widgets::*`, so an upstream break is a one-file fix.

**Accepted risks:** GPUI is self-declared pre-1.0 with no stability contract; `gpui-component`
is one company's library and the only thing making forms viable (Apache-2.0, so vendoring a
fork is the contingency); GPUI has no screen-reader support, which is acceptable only
because accessibility is confirmed out of scope; there is no drag-and-drop primitive, which
is why the form builder ships keyboard and button reordering first
([06-FORM-BUILDER.md](../06-FORM-BUILDER.md)).

**M0 exists to test this decision** on all three platforms in week one.
