# 10 — Limitations and Risks

Named rather than hidden. Each of these is a decision, not an oversight. If one of them
turns out to matter more than assumed, the fix is a design change — so they are written
down where a design change can find them.

---

## Gates

### G1 — `cetify.email` does not resolve

`dig cetify.email` returns **NXDOMAIN**; whois falls through to the `.email` TLD registry,
meaning no NS delegation exists. Magic-code login cannot work until the domain is
registered, added to Cloudflare DNS, delegated, and onboarded to Email Sending. Steps in
[09-SETUP.md](09-SETUP.md#domain--cetifyemail).

**This is the only gate that blocks development.**

### G2 — PHI is not in scope today

This system handles **synthetic data only**, and the protections for real patient data are
implemented behind the `phi` cargo feature, off by default. Nothing here blocks building or
shipping to synthetic-data users.

When real patient data becomes a goal, the full checklist is
[12-PHI-READINESS.md](12-PHI-READINESS.md). The long-lead item is an **Enterprise BAA with
Cloudflare** — they sign BAAs only with Enterprise customers, and this runs on a personal
Workers Paid account. Workers, D1, Durable Objects, R2, and KV are in-scope *products*, so
no re-architecture is implied; the contract simply has to exist first.

**Worth stating plainly either way:** the server half is Cloudflare-specific. If that
platform ever has to change, `medatat-worker` and the DO topology are a rewrite, not a port.
The client half — `medatat-core`, `medatat-store`, `medatat-sync`, `medatat-ui`, which is
most of the work — is portable to any backend serving [03-API.md](03-API.md). That is not an
accident; it is why `Transport` is a trait.

---

## Accepted limitations

### 1. Cross-case reporting is not built

DO-per-case makes cross-case SQL **impossible**. Each Durable Object is an island; there is
no query that spans them. The D1 `case_index` covers worklists only — case id, MRN, form,
assignee, rev, updated_at — and deliberately holds no field values.

The minimum viable answer is `GET /bulk/export`, which fans out over DOs and streams NDJSON
for offline analysis in DuckDB or similar. That is an offline batch operation, not an
interactive query.

**This is the sharpest trade in the design.** A tool that abstracts medical data into
structured fields exists so somebody can analyse those fields. The requirements do not ask
for reporting, so it is not built — but if analysing data across cases is actually a goal,
this is the limitation to raise first, and it would justify revisiting the storage topology
before M3 rather than after M7. Alternatives if so: mirror values into a purpose-built
analytics store on write, or reconsider whether ~100M rows can be partitioned across a small
number of D1 databases by form rather than by case.

### 2. The D1 case index is eventually consistent

A `CaseDO` write can succeed while its subsequent `case_index` update fails. The value is
safe; the worklist silently diverges — wrong `updated_at`, stale `rev`, or a missing row.

Mitigation is `POST /admin/reindex` (M7), which walks the DOs and rebuilds the index. There
is **no online self-healing**. At 100k cases a full reindex is a long-running job.

A user-visible symptom is a case that does not appear in a worklist despite existing. The
worklist is therefore never the authority on whether a case exists — `GET /cases/{id}/values`
is.

### 3. Large coded option lists are not supported

`field_option` rows replicate to every client along with form definitions. Clinical
vocabularies — ICD-10, SNOMED CT, RxNorm — have tens of thousands to hundreds of thousands
of entries. Replicating those to every workstation, per field, breaks the model: the local
form cache stops being small, and load stops being instant.

Such fields need a server-side searchable lookup with a different widget (typeahead against
an API, storing only the selected code). That is a distinct feature, not a config change.

**Assumption of record:** option lists are tens of entries, not tens of thousands. If a
coordinator needs ICD-10, this limitation is the blocker.

### 4. `Value::Null` conflates three different things

In chart abstraction, these are genuinely distinct:

- The abstractor has not reached this field yet.
- The abstractor looked and the chart does not say.
- The chart explicitly documents the finding as absent.

`Value::Null` represents all three identically. This is not in the requirements, so it is
not modelled. Retrofitting is a new `Value` variant plus a data migration — cheap while the
corpus is synthetic, expensive once real data lands. **If this matters, decide before real
patient data arrives** ([12-PHI-READINESS.md](12-PHI-READINESS.md) item 15).

### 5. No audit trail

Cut deliberately ([00-REQUIREMENTS.md](00-REQUIREMENTS.md#explicitly-out-of-scope)). Nothing
records who changed a value from what to what, or when.

**This is the most expensive cut to reverse.** An audit trail wants to be written in the
same transaction as the value, in both the client store and the `CaseDO`. Adding it later
means touching every write path and accepting that history begins on the day it ships.
Clinical registries and 21 CFR Part 11 environments typically require it. It was cut because
the requirements do not mention it — but it is the one omission most likely to be a surprise.

### 6. No form versioning

Also cut. Form edits go live immediately; there is no draft state and no publish step. A
coordinator mid-edit is visible to abstractors within 60 s.

Partially mitigated by the stable-field-id invariant: structural changes never orphan
values, and a `kind` change creates a new field rather than corrupting old data. What is
lost is the ability to answer "what did this form look like when this case was collected?"

### 7. No conditional logic

No show-if, no required-when, no cross-field validation. Type-level validation (numeric
range, valid 24hr time, max length) is present because the field kinds are meaningless
without it.

This is the cheapest cut to reverse — it reintroduces a rule graph and a visibility bitset
in `FormInstance`, and the per-keystroke cost stops being strictly O(1).

### 8. No offline form editing

Abstractors work offline. **Coordinators cannot** — the builder writes directly to D1.

### 9. Sync is per-caseload, not per-corpus

R15 holds because the user's assigned caseload is pre-synced. A user who must open any of
100k cases at random would hit a genuine cold fetch, and the no-spinner guarantee would need
renegotiating. See [04-SYNC.md](04-SYNC.md#caseload-pre-sync).

---

### 10. Keyboard evidence is macOS-only

The 14 `#[gpui::test]` cases dispatch real keystrokes and have caught three genuine focus
bugs, but they prove one dispatch tree on one platform. Key routing is demonstrably
platform-specific — `Alt-Left`/`Alt-Right` reach a handler on some focus states and not
others on macOS specifically, because the OS consumes them first. The same suite on Windows
or Linux could plausibly produce a different set of passes, and neither has been built.

**Do not read green here as green everywhere.** The suite costs ~1.1 s, so running it on the
other two platforms is cheap the moment they build at all.

### 11. The 100k-case corpus cannot be built through the write path

Measured in production: 0.12 cases/s serial, and **concurrency plateaus at 1.75× — tripling
it from 8 to 24 bought 6%.** The ceiling is server-side. 100,000 cases is 131 hours at best.

The likely constraint is the single D1 `case_index`, which every write updates and which
serialises writes, while the cases themselves are independent Durable Objects. That is a
hypothesis the measurement points at, not a confirmed cause.

So **R16 is verified by per-case measurement and extrapolation, not by a built corpus** —
130 KB per case against a 10 GB per-object budget, which is the same shape of argument
`docs/02-DATA-MODEL.md` already makes. Anyone who needs the real corpus should first
establish whether `case_index` is the bottleneck and whether its update can be debounced.

## Risks

Ordered by expected cost × probability.

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| 1 | **Linux Vulkan unavailable** on target hardware (VMs, RDP, older Intel) | App will not start | M0 gate; CI-test `lavapipe`; document `VK_ICD_FILENAMES` |
| 2 | **Benches 1 and 2 miss at M1** | Architecture is wrong | M1 is a stop-the-line gate, before anything is built on it |
| 3 | **GPUI is pre-1.0**, pinned to a git SHA of a code editor's internals | Breaking changes on upgrade | Hard `rev` pins, committed lockfile, weekly bump-and-build CI job, `widgets::*` wrapper |
| 4 | **`gpui-component` bus factor** — one company's library, the only thing making forms viable | Form layer orphaned | Apache-2.0; vendoring a fork is the contingency |
| 5 | **`workers-rs` bus factor** — one maintainer, 184 open issues, `send_email` undocumented | Worker blocked | M1 proves the three unknowns first; escape hatch is TS/Hono + `medatat-core` via `wasm-bindgen` |
| 6 | **Form builder is the largest UI**, and gpui-component has no drag-and-drop primitive | M5 overruns | Keyboard and button reorder ships first; drag is explicitly not a gate |
| 7 | **Seeding 100M values is slow or costly** | Bench 4 never runs, R16 unverified | M1 measures 1,000 cases and extrapolates before M7 commits |
| 8 | **Accidental `cx.notify()` per keystroke** makes 300 fields quadratic | "The app feels sluggish" six months on | `subscriptions_fire_once_per_edit` must fail CI |
| 9 | **Swap pages plaintext to disk** despite SQLCipher (only relevant once `phi` is on) | PHI at rest unencrypted | Full-disk encryption as a deployment requirement; core dumps disabled; zeroize on drop |
| 10 | **Email Sending is Beta**, on the login critical path | Nobody can log in | `trait Mailer` seam for Postmark/SES |
| 11 | **Windows GPUI backend least exercised** | IME, DPI, dialog papercuts | M0 covers it; budget explicit time |
| 12 | **WASM bundle exceeds 10 MB or 1 s startup** | Worker will not deploy | Measured at M1 with the real dependency set, not at M8 |

---

## Decisions to revisit if assumptions break

| If this turns out to be true | Revisit |
|---|---|
| Cross-case analytics is a real requirement | Storage topology ([ADR-0001](adr/0001-durable-object-per-case.md)) — before M3 |
| Users need random access to any of 100k cases | Caseload pre-sync, and R15 itself ([04-SYNC.md](04-SYNC.md)) |
| An audit trail is required | Write paths in `medatat-store` and `CaseDO` — **before** real data lands |
| Coded vocabularies (ICD-10) are needed | A server-side lookup field kind |
| "Unknown" vs "absent" must be distinguished | `Value` enum — while the corpus is still synthetic |
| Real patient data becomes a goal | [12-PHI-READINESS.md](12-PHI-READINESS.md) — start the BAA early, it is the long pole |
| Cloudflare has to be abandoned | `medatat-worker` and the DO topology; the client is portable |
