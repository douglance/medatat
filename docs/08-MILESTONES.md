# 08 — Milestones

Nine milestones. Each has a **demo** (what you can show) and **exit criteria** (what must be
objectively true). A milestone is not done until every exit criterion passes.

Milestones are ordered so that the two things most likely to invalidate the plan — GPUI
tri-platform viability and the latency claim — are settled in the first two weeks.

---

## M0 — Platform bring-up

**~1 week. Throwaway code is acceptable.**

A GPUI window that builds and runs on macOS, Windows, and Linux at a pinned rev, rendering
one 3-column form with one of every field kind — **including the 24hr time input**.

### Exit criteria
- [ ] `cargo build -p medatat-ui` succeeds on macOS, Windows, and Linux.
- [ ] The window opens and renders on all three, including under `lavapipe` on Linux.
- [ ] All seven widget kinds render and accept input (R5–R11).
- [ ] The time input round-trips `09:30` and rejects `24:00` (R8).
- [ ] A 3-column section lays out correctly, and one field spans 2 columns (R12).
- [ ] `gpui`, `gpui_platform`, and `gpui-component` revs are pinned in `Cargo.toml`;
      `Cargo.lock` is committed.

### Why first
If Linux Vulkan or the Windows backend fails, the toolkit choice is wrong and everything
downstream changes. Finding that in week one costs a week; finding it in month six costs
the project.

---

## M1 — Performance gate

**~1 week. Throwaway code is acceptable. This milestone can stop the project.**

Encrypted local SQLite with 1,000 seeded fields; a minimal Rust Worker and `CaseDO`; measure
Benches 1–4.

Also proves the three `workers-rs` unknowns, **in this order**, because each can force a
rework:

1. A real `send_email` call from Rust lands in an inbox.
2. Compressed WASM bundle with the real dependency set is under 10 MB, and startup under 1 s.
3. `getrandom`'s `js` backend works for token and code generation.

### Exit criteria
- [ ] **Bench 1** — 500-field load from SQLCipher: **p99 < 5 ms**.
- [ ] **Bench 2** — 300-field write, one transaction: **p99 < 10 ms**.
- [ ] **Bench 3** — open-to-first-paint over 200 cases: **p99 < 50 ms**.
- [ ] **Bench 4** — cold-DO full-case sync measured and recorded (no threshold).
- [ ] Seeding throughput measured on **1,000 cases**, with a full-corpus extrapolation.
      **Harness built and proven end to end; throughput NOT credibly measured.** The local
      emulator's rate degrades with store size (1.90 → 1.19 cases/s as `.wrangler/state`
      grew), so any figure from it — and any extrapolation off it — is an artifact. Needs
      one run against a deployed Worker. See
      [07-TESTING.md](07-TESTING.md#throughput-is-not-credibly-measurable-on-the-local-emulator).
- [ ] **Bench 4 needs a deployment, not a bigger local corpus.** "Cold" is a property of
      time and eviction, not corpus size — but the local emulator **never evicts at all**,
      which its own OOM proves (memory grew monotonically with objects touched). The only
      cold obtainable locally is a fresh `workerd` process re-opening SQLite, which is a
      real *storage* floor and not the hibernation wake path. See
      [07-TESTING.md](07-TESTING.md#the-oom-proves-the-emulator-never-evicts--so-cold-cannot-be-made-here). Run it with
      `medatat push --cases 1000 --fields 1000`, which writes through the ordinary
      `POST /cases` + `POST /cases/{id}/values` path and prints the projection. Not
      `/bulk/cases`: that carries no values, and a whole-case bulk write would stamp every
      row with the same `rev`, which is the spread Bench 4 and delta sync are measured
      against.
- [ ] `send_email` from `noreply@cetify.email` delivers to a real inbox.
- [ ] **`worker-build --profile release-wasm` succeeds.** A green `cargo build --target
      wasm32-unknown-unknown` is necessary but **not sufficient** — the bundle step runs
      wasm-bindgen, which can fail on its own (observed: `externref table required for
      catch wrappers`, caused by `strip` in the release profile). Treat the bundle build as
      the gate; `--release` is the wrong profile for it.
      **Measured 2026-08-17 on macOS: 1.01 MB raw, 0.38 MB gzipped** against the 10 MB
      limit — 26x margin. Startup still unmeasured; needs a real deploy.
- [ ] `wrangler deploy --dry-run` reports bundle size and startup within limits.
- [ ] `EXPLAIN QUERY PLAN` confirms the case-load query is a PK range scan.

**If Benches 1 or 2 miss, stop and revisit [01-ARCHITECTURE.md](01-ARCHITECTURE.md) before
building anything else.**

### Prerequisite
`cetify.email` must be registered, on Cloudflare DNS, NS-delegated, and onboarded to Email
Sending. See [09-SETUP.md](09-SETUP.md). It currently returns NXDOMAIN.

---

## M2 — Core and store

`medatat-core` complete; `medatat-store` complete. Headless, no GUI, no network.

### Exit criteria
- [ ] All types from [02-DATA-MODEL.md](02-DATA-MODEL.md) implemented.
- [ ] `validate(&FieldDef, &Value)` covers all seven kinds (R5–R11).
- [ ] `parse_time_24` / `format_time_24` pass the proptest and the full table test (R8).
- [ ] `effective_columns` and `col_span` clamping tested at every tier (R12).
- [ ] `Value` redacts in `Debug` and zeroizes on `Drop` under `--features phi`;
      `debug_redacts` passes. Default builds print values normally.
- [ ] Schema applies on plain SQLite; and under `--features phi`, SQLCipher applies and a
      wrong key fails cleanly.
- [ ] Outbox coalescing verified: 40 edits to one field → one row.
- [ ] `apply_local` atomicity verified.
- [ ] `medatat-core` has **no** I/O dependency (enforced by `cargo tree`).
- [ ] `cargo test -p medatat-core -p medatat-store` runs in under 5 s.

---

## M3 — Worker and sync

D1 schema, `CaseDO`, magic-code auth, `Transport`, delta sync loop, conflict handling.

### Exit criteria
- [ ] D1 migrations apply; `wrangler d1 migrations apply` is idempotent.
- [ ] Every endpoint in [03-API.md](03-API.md) implemented and returning the documented envelope.
- [ ] Magic-code login works end to end via `medatat-cli`.
- [ ] `POST /auth/request` returns 204 for unknown emails (no enumeration).
- [ ] Codes are single-use; 5 failed attempts invalidate.
- [ ] `CaseDO` creates its schema **lazily on first write**, not in the constructor.
- [ ] Per-field conflict detection: two clients on **different** fields both apply;
      **same** field yields exactly one conflict.
- [ ] Server-side validation rejects a bad value with 422 using the **same**
      `medatat_core::validate` as the client.
- [ ] `kind` patch rejected with 422; `col_span > columns` rejected with 422.
- [ ] `scripts/smoke.sh` passes against `wrangler dev`.
- [ ] `case_index` updates after a value write.

---

## M4 — Runtime renderer

`medatat-ui` renders a real `FormDef` from local SQLite; edits write through to local SQLite
and the outbox.

### Demo
Fill a 300-field form **with the network disconnected**, quit, relaunch — everything is
there. Reconnect and watch the outbox drain.

### Exit criteria
- [ ] All seven kinds render from a runtime-defined form (R5–R11).
- [ ] 1-, 2-, and 3-column sections render; `col_span` honoured and clamped (R12).
- [ ] `subscriptions_fire_once_per_edit` passes — the anti-quadratic guard.
- [ ] `tab_order_matches_focus_order` passes.
- [ ] `closing_case_clears_inputs` passes.
- [ ] `widget_spec` snapshots exist for all seven kinds.
- [ ] `cargo xtask lint-no-spinner` passes (R15).
- [ ] Bench 3 re-measured against the real renderer and still under 50 ms.
- [ ] Element-tree frame cost measured and recorded.
- [ ] Conflict strips render inline and survive restart.

---

## M5 — Form builder

Three-pane builder; the canvas is the runtime renderer in design mode.

### Exit criteria
The full acceptance script in
[06-FORM-BUILDER.md](06-FORM-BUILDER.md#acceptance-m5) passes, plus:

- [ ] Only one renderer exists — design mode is a `RenderMode`, not a second code path.
- [ ] `kind` is read-only in the inspector; **Replace field** works and preserves old values.
- [ ] Reducing a section's columns clamps child `col_span` atomically.
- [ ] Removing a field placement does **not** delete values; re-adding shows them again.
- [ ] Option list editing works for radio and select (R9, R10).
- [ ] Builder screens are admin-only, enforced by the **API** (403), not just the UI.
- [ ] Config changes reach a second client within 60 s.
- [ ] Reorder works via keyboard and buttons. Drag is optional and explicitly not a gate.

---

### M5 status — 2026-08-18

Acceptance items **1–6, 8 and 9 are implemented**, covered by 45 headless tests in
`medatat-ui` plus the pure logic in `medatat_core::builder`.

**Item 9 is verified by execution, not construction.** Three of those tests go through a
real SQLite store: save a form, remove a placement, reopen from disk, and assert the field
is still findable by id and key; place it again and watch the drawer empty itself; after a
Replace, confirm the drawer holds the *original* rather than the replacement. That is the
Unplaced drawer surviving a restart, actually run.

**Item 7 is not closable here.** "An abstractor on another machine opens the form within
60 s" needs two machines and live sync.

Two deliberate omissions, both to be resolved by login and the server config write path:
`⌘B` opens the builder with no `role = admin` gate (the API enforces it regardless, and the
spec's position is that the UI is not the enforcement point), and every save writes
`ConfigRev(1)` because nothing allocates config revisions client-side.

**The standing caveat still applies to the rest of it: no button in the builder has ever
been clicked.** The screen on the build machine is locked. Items 1–6 and 8 are verified by
construction and headless tests; only item 9 has been executed against real storage.

## M6 — Worklist, caseload pre-sync, keyboard model

### Demo
`Cmd-J` / `Cmd-K` through 50 cases with zero loading states, online and offline.

### Exit criteria
- [ ] Worklist renders from local SQLite; sort and filter are client-side and instant.
- [ ] Caseload pre-sync runs on login and every 5 minutes, in worklist order.
- [ ] Sync progress appears only as a footer status line — never a spinner or overlay (R15).
- [~] Every keybinding in [05](05-UI-SPEC.md#keyboard-model) is **implemented**, but none
      has been verified by execution — see the note below.
- [ ] `Cmd/Ctrl-F` field search expands, scrolls, and focuses.
- [ ] Focus never lands on a collapsed section's fields.
- [ ] Inbound sync never overwrites the focused field (deferred merge on blur).
- [ ] A full keyboard-only pass through a 300-field form, on all three OSes.

---

### A standing caveat on M6

Everything keyboard in M6 is implemented and reasoned through, and **none of it has ever
been pressed.** The machine running the build has a locked screen, so `Tab`, `Cmd-F`, and
time-field keystrokes are verified by construction plus pure-function tests, not at runtime.

Three things specifically remain unproven: that capture-phase interception actually beats
`InputState`'s own key context (it binds `tab` → `IndentInline` and `up`/`down` →
`MoveUp`/`MoveDown`, so bubble-phase handlers never see them); that
`InputState::validate` rejects a keystroke the way its source reads; and that focus lands
where `focus_step` sends it.

The cross-platform CI job will not catch these either — it builds, it does not drive a UI.
**Do not mark M6 done on construction evidence alone.** The last exit criterion — a full
keyboard-only pass through a 300-field form on all three OSes — is exactly the thing that
would close the gap, and it needs a human at an unlocked screen.

## M7 — Scale run

### Exit criteria
- [ ] 100,000 synthetic cases / ~100M values seeded (R16).
- [ ] Bench 4 run against the full corpus; results recorded.
- [ ] Actual seeding time and Cloudflare cost recorded against the M1 extrapolation.
- [ ] Benches 1 and 2 re-run against a client holding a realistic caseload; still inside targets.
- [ ] `POST /admin/reindex` implemented and verified to rebuild `case_index` from the DOs.
- [ ] A deliberately corrupted `case_index` row is detected and repaired by reindex.

---

## M8 — Packaging and hardening

### Exit criteria
- [ ] Signed and notarized macOS `.app`.
- [ ] Windows MSI or NSIS installer.
- [ ] Linux AppImage and `.deb`.
- [ ] Auto-update on all three.
- [ ] `cargo build --workspace --features phi` clean on all three platforms.
- [ ] The [PHI readiness checklist](12-PHI-READINESS.md) is reviewed and its code items
      (4–9) pass. Legal items (1–3) are not a blocker for shipping to synthetic-data users.
- [ ] `cargo audit` clean.
- [ ] All docs in `docs/` reconciled against the shipped implementation.

---

## Crate build order

Which crate to build at each milestone, with module layouts and target signatures:
[11-CRATE-GUIDE.md](11-CRATE-GUIDE.md#build-order).

## Dependency graph

```
M0 ─┐
    ├─► M2 ─► M3 ─► M4 ─► M5 ─► M6 ─► M7 ─► M8
M1 ─┘
```

M0 and M1 are independent and can run in parallel. Everything else is a chain — M4 needs a
renderer to measure, M5 needs the renderer to reuse in design mode, M6 needs forms and cases
to navigate, M7 needs the full write path.

## Standing rules

1. **A milestone is not done until its exit criteria pass.** No partial credit.
2. **No milestone may weaken an earlier exit criterion.** If M5 breaks the Bench 3 gate, M5
   is not done.
3. **Requirement IDs appear in tests and commits.** `test_r8_rejects_2400`, not `test_time_3`.
4. **Bad news travels immediately.** A missed benchmark or a failing platform is reported
   when found, not absorbed. M1 exists precisely to surface that early.
