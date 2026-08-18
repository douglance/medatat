# 07 — Testing and Performance

## Principle

**Push logic below the GUI line.** Anything testable without a window must be. The GUI test
budget is three tests; everything else is `cargo test`.

The performance benchmarks are not a late validation step — they are an **M1 gate**. The
architecture is justified by a latency argument, so the number gets measured before anything
is built on it.

---

## Performance benchmarks

`criterion` in `medatat-testkit`. **Benches 1 and 2 gate today**; 3 and 4 do not yet, and
each says so in its own module docs rather than asserting a number it cannot measure.

| Bench | Measures | Target | Measured (macOS, 2026-08-17) | Proves | CI |
|---|---|---|---|---|---|
| **1** | Local SQLite → `FormInstance`, 500 fields, warm | **< 5 ms** | **195 µs** ✅ | R13 | gate |
| **2** | Local write of 300 changed fields, one transaction | **< 10 ms** | **5.2 ms** ✅ | R14 | gate |
| **3** | Open-case intent → first painted frame, 200 cases | **p99 < 50 ms** | — | R13, R15 | not yet |
| **4** | Full sync of one case from a **cold, hibernated** DO; seeding throughput | recorded | — | R16 | placeholder |

Bench 3 measures only `core.build_instance` today, because two of its four spans live in
`medatat-ui`, which is still being built. The 50 ms gate belongs to the assembled bench and
arrives with it. Bench 4 registers an empty group until `medatat-sync` and `medatat-worker`
can be driven end to end. Both are deliberate: a bench that asserts a number it did not
measure is worse than one that admits it is not ready.

Requirements R13 and R14 state 200 ms. The gates are set at 5 ms and 10 ms — a ~20–40×
margin. That margin is deliberate: it means an unnoticed regression trips the gate long
before it becomes user-visible, and it is only achievable because reads and writes are local
([01-ARCHITECTURE.md](01-ARCHITECTURE.md#the-one-rule)).

### What the first run said

**R13 has roughly 1000x margin over the requirement.** A 500-field read is ~227 µs; read
plus `FormInstance` construction is ~280 µs; the assert loop measured a 195 µs mean against
a 200 ms requirement. This is the number the whole architecture was predicated on
([ADR-0002](adr/0002-encrypted-local-sqlite.md) estimated 50–300 µs), and it holds.

**R14 has ~20x margin over the requirement but only ~2x over its gate.** A single-field
save — the realistic steady state, since an abstractor saves per field — is **86 µs**. The
300-field bulk transaction is where it tightens: the assert loop measured 5.2 ms mean while
criterion put the median at 9.0 ms with a 8.1–9.9 ms range, close to the 10 ms gate.

That gap is worth naming rather than smoothing over. The bulk path is rare and still sits
20x inside the actual 200 ms requirement, so the *requirement* is not at risk — but the
*gate* has little headroom and may flake on a loaded CI machine. If it starts failing
intermittently, investigate before widening it: the likely cause is per-row statement
overhead in `apply_local`, which batching would fix properly.

### Bench 3 detail

The one that actually proves the headline requirement. Instrument with `tracing` spans:

```
span "open_case"
  ├── span "store.load_values"      local SQLite read
  ├── span "core.build_instance"    FormInstance construction
  ├── span "ui.create_widgets"      300 Entity allocations
  └── span "ui.first_paint"         to frame presented
```

Run over 200 distinct cases with a cold in-memory cache each time (the SQLite page cache
may stay warm — that is the real-world condition). Report p50/p95/p99 per span so a
regression is attributable, not just visible.

### Bench 4 detail

**Bench 4 does not need a large corpus, and an earlier version of this document wrongly
implied it did.**

"Cold" is a property of *time and eviction*, not of corpus size. Every Durable Object is an
independent SQLite database with its own storage, so a cold read of DO #1 and a cold read of
DO #99,999 are the same operation over the same ~1,000 values. Adding 99,950 neighbours does
not make any individual object colder, larger, or slower to wake. What makes it cold is
elapsed time without traffic.

So Bench 4 wants **20–50 full-size cases at the real 1,000 fields, left untouched past the
eviction window (70–140 s), then read.** That is minutes of seeding, repeatable often enough
to actually gate on.

### What the 100k corpus is actually for

Conflating the two experiments made M1 depend on a 15-hour job it never needed. They are
separate:

| Question | Needs |
|---|---|
| Does a hibernated DO read back quickly? (Bench 4) | 20–50 cold cases. Minutes |
| Does the topology hold at 100k cases? (R16, M7) | Mostly answerable by measuring per-case storage on a small corpus and multiplying — the DO sizing in [02-DATA-MODEL.md](02-DATA-MODEL.md) already is that extrapolation |
| How does D1 `case_index` behave at 100k rows? | Genuinely needs the rows — but only **index** rows, which carry no values. `POST /bulk/cases` creates exactly those, cheaply. This is the one job that endpoint is well shaped for |

### Throughput is not credibly measurable on the local emulator

An earlier version of this document recorded **1.90 cases/sec** as a measured figure and
extrapolated 100,000 cases to 14.6 hours. **Both were artifacts.** Local throughput degrades
with the size of the store:

| run | `.wrangler/state` before | rate |
|---|---|---|
| first | empty | 100 cases in 65 s → **1.90 cases/s** |
| second | 249 cases | 100 cases in 92 s → **1.19 cases/s** |

Same code, same batch size, same machine; the only variable is how much was already stored.
So 1.90 is a best case on an empty store, not a steady state, and any linear extrapolation
from it is optimistic by an unknown factor.

The degradation is almost certainly miniflare keeping every Durable Object in one process
rather than anything about production — which is precisely why the real number cannot be
obtained from here. **Seeding throughput needs one run against a deployed Worker with real
D1 and KV.** Until then the honest status is: *harness built and proven end to end;
throughput not credibly measured.*

If the real number does turn out to be slow, the remedy is client-side concurrency in
`medatat-cli` — N cases in flight, no new server surface.

**Seeding goes through the normal write path deliberately.** A bulk endpoint that writes a
whole case in one shot stamps every row with the same `rev`. Per-field conflict detection
and `since_rev` delta reads both key off that spread, so a corpus where every row is rev 1
would make delta sync unbenchmarkable *and* flatter the numbers — a benchmark that is both
wrong and reassuring.

### A local-emulator limit worth knowing

`wrangler dev` keeps every Durable Object resident in one process. A 1,000-case push died at
**case 249** with V8 heap exhaustion at ~1.3 GB — 249 live DOs × 1,000 values in a single
heap. A second attempt died **earlier, at case 138**.

Two things make it worse than it first looks:

- **`NODE_OPTIONS=--max-old-space-size` does not work.** Proven from the process tree:

  ```
  sh
  └── node          ← wrangler; this is what NODE_OPTIONS reaches
      ├── esbuild
      └── workerd   ← a separate C++ binary embedding its own V8
  ```

  A restart with `12288` aborted at the same **1398 MB** as an unset run. The flag reaches
  the Node process; the heap that dies belongs to `workerd`. The real remedy is to **chunk
  the run and restart the Worker between chunks** — `medatat push --start <n>` resumes at a
  case index, and seeds derive from that index, so a resumed run produces exactly the cases
  an uninterrupted one would.
- **Residual state compounds it.** `.wrangler/state` held 52 MB of the first run's objects,
  and the second run loaded those *plus* its own, which is why it failed sooner. **Clear
  `.wrangler/state` between seeding runs** or each retry starts further into the hole.

**The ceiling is memory, not a case count.** It is roughly **1.4 GB of workerd heap**, which
translates to a different number of cases depending on fields per case and how much state is
already on disk: at 1,000 fields per case it aborted at **249 cases on empty state** and
**138 on a 52 MB store**. Quoting it as "about 200 cases" would mislead anyone trying it
with a different form size.

### The OOM proves the emulator never evicts — so "cold" cannot be made here

The crash is not just an obstacle; it is the evidence. **Memory grew monotonically with the
number of Durable Objects touched** — 1.3 GB at 249 objects — and the second run died sooner
because the first run's state was already on disk to load. If miniflare hibernated or evicted
idle objects, that memory would have been reclaimed and the ceiling never reached. It was
not, so it does not.

That settles what Bench 4 can and cannot measure locally. The only "cold" obtainable here is
**a fresh `workerd` process re-opening SQLite from disk**. That is a real cold *storage*
read, and comparing it against a warm read tells you what the disk open costs — worth having.
It is **not** Cloudflare's hibernation wake path, because nothing here ever hibernates: no
eviction, no cross-colo placement.

So a local figure is a **floor, and must be labelled as one**. Published as the production
number it would be worse than nothing, because it would read as evidence and stop anyone
measuring the real one. **Bench 4's real answer needs a deployment.**

---

## Test layers

### `medatat-core` — pure, no I/O

| Area | Tests |
|---|---|
| R8 time | `parse_time_24` proptest round-trip; table test over every accepted/rejected string in [05](05-UI-SPEC.md#the-24-hour-time-field-r8) |
| R6 numeric | `Decimal` round-trip; min/max/scale enforcement; **assert `f64` appears nowhere** in the value path |
| R7 date | ISO round-trip; rejects `2026-02-30` |
| R12 layout | `effective_columns` at every width tier; `col_span` clamping |
| R5–R11 | `validate(&FieldDef, &Value)` accepts and rejects correctly per kind |
| `FormInstance` | `set` returns the right error; `pending()` is O(dirty) not O(n) |
| PHI (`phi` feature) | `debug_redacts`: `format!("{:?}", value)` contains no plaintext. Runs in the `--features phi` CI job only |

```rust
#[test]
fn pending_is_proportional_to_dirty_not_total() {
    let mut inst = fixture_form(1000);
    inst.set(FieldIdx(3), Value::Text("x".into()));
    assert_eq!(inst.pending().count(), 1);   // not 1000
}
```

### `medatat-store` — rusqlite tempfile

- Schema applies; `schema_version` round-trips.
- **SQLCipher (`phi` feature): opening with the wrong key fails**, and surfaces as "cannot
  unlock", never as corruption. Default builds use plain SQLite.
- Outbox coalescing: 40 `apply_local` calls on one field → exactly one outbox row.
- `apply_local` is atomic: a value is never committed without its outbox row.
- `WITHOUT ROWID` locality asserted via `EXPLAIN QUERY PLAN` — the case-load query must be a
  primary-key range scan, not a table scan. **This test protects the R13 margin**; if it
  regresses, Bench 1 will too.
- Wrong-schema-version database is rejected rather than silently migrated.

### `medatat-sync` — `MockTransport` + `FakeClock`

Full list in [04-SYNC.md](04-SYNC.md#testing). The load-bearing ones:

- Disjoint-field concurrency: two clients, different fields, both apply.
- Same-field race: exactly one conflict row; loser's outbox row dropped.
- `pending = 1` never clobbered by an inbound server value.
- Offline → queued → reconnect → drained, nothing lost or duplicated.
- Backoff schedule matches expectation across 10 simulated failures.

### `medatat-worker` — logic native, bindings in workerd

Worker handlers are written as pure functions over a storage trait, so **most of the logic
tests natively** without WASM:

```rust
pub trait CaseStore { fn get_all(&self) -> Result<Vec<ValueRow>>; /* … */ }
pub fn handle_put_values<S: CaseStore>(store: &S, req: PutValuesReq, actor: ActorId)
    -> Result<PutValuesResp>;
```

Native tests: rev monotonicity; per-field conflict precision; server-side validation
rejecting bad values; a write with no resolved actor is refused; magic-code expiry, attempt
limit, and single-use; `col_span > columns` rejected with 422; `kind` patch rejected.

The thin `workers-rs` binding layer is covered by integration tests driving `wrangler dev`
through `medatat-cli` — real DO SQLite, real D1, real KV.

### `medatat-ui` — as many `#[gpui::test]` as there is *wiring* to prove

**The budget was three. It is now bounded by kind, not by count**, because the original
number was a proxy for the wrong thing.

The rule it was protecting is still right: **push logic below the GUI line.** Anything that
can be decided by a pure function belongs in `medatat_core::view` or `medatat_core::builder`
and gets snapshot- or unit-tested with no window. That is unchanged, and most of R5–R12 is
covered exactly that way.

But **wiring is not logic, and cannot be pushed below the line.** Focus, key routing, event
dispatch, and who actually receives a keystroke are properties of the element tree, and no
pure function can observe them. Two findings settled this:

- `#[gpui::test]` / `TestAppContext` runs **fully headlessly** — no window, no display —
  and `simulate_input` / `simulate_keystrokes` dispatch real key events through the real
  dispatch tree. The cost the budget was rationing does not exist.
- Dispatching two keystrokes found a bug that had survived four review passes: **Tab did
  nothing on a freshly-opened case**, because nothing held focus, so events never reached
  the root element and the capture handler never ran. Every handler was correct; the wiring
  above them had never been exercised. A keyboard-only abstractor would have found the form
  dead on arrival.

So: **write a `#[gpui::test]` for any behaviour that only exists once elements are wired
together, and for nothing that a pure function could have decided.** If a test would pass
identically against a `WidgetSpec`, it belongs below the line.

#### The three that must always exist

1. **`subscriptions_fire_once_per_edit`** — 300 subscriptions, one edit, counter == 1. The
   anti-quadratic guard. Must fail CI when broken.
2. **`tab_order_matches_focus_order`** — including that collapsed sections contribute no stops.
3. **`closing_case_clears_inputs`** — PHI hygiene. `#[cfg(feature = "phi")]`.

Everything else tests `widget_spec(&SectionField, &FormInstance) -> WidgetSpec` by snapshot.
Full R5–R12 coverage, no window, no GPU.

### R15 lint

```bash
cargo xtask lint-no-spinner
```

Fails if `spinner`, `loading...`, `progressbar`, `skeleton`, or `shimmer` appears anywhere
in `crates/medatat-ui/src/`. R15 says "Ever", so it is enforced mechanically rather than by
review.

---

## Integration testing with `medatat-cli`

`incurs`-based; see [10 §CLI](09-SETUP.md#the-medatat-cli). Smoke script in
`scripts/smoke.sh`, run against `wrangler dev` in CI and against the deployed Worker after
deploy:

```bash
set -euo pipefail
medatat api health | jq -e '.ok'

TOK=$(medatat api auth verify -X POST -d "{\"email\":\"$E\",\"code\":\"$CODE\"}" | jq -r .data.token)
CASE=$(medatat api cases -X POST -H "Authorization: Bearer $TOK" \
        -d '{"mrn":"SMOKE-1","form_id":"'$FORM'"}' | jq -r .data.case_id)

medatat api cases "$CASE" values -X POST -H "Authorization: Bearer $TOK" \
  -d '{"base_rev":0,"changes":[{"field_id":"'$TIME_FIELD'","value":{"Time":"09:30"}}]}' \
  | jq -e '.data.rev == 1'

medatat api cases "$CASE" values -H "Authorization: Bearer $TOK" \
  | jq -e '.data.values[0].value.Time == "09:30"'

# conflict path: a stale base_rev on the same field must 409
medatat api cases "$CASE" values -X POST -H "Authorization: Bearer $TOK" \
  -d '{"base_rev":0,"changes":[{"field_id":"'$TIME_FIELD'","value":{"Time":"10:00"}}]}' \
  | jq -e '.ok == false and .error.code == "conflict"'
```

---

## CI pipeline

`.github/workflows/ci.yml` exists and is committed. **It has never run** — the repository
has no remote yet — so until a green run has been seen, every gate is still enforced by
running it yourself.

```yaml
# .github/workflows/ci.yml
jobs:
  preflight:  # ubuntu — reject absolute path deps; `cargo metadata --locked` resolves
  check:      # ubuntu — fmt, clippy -D warnings, cargo xtask lint-no-spinner
  test:       # macos, ubuntu, windows — cargo test --workspace
  phi:        # ubuntu — cargo build+test --workspace --features phi (keeps the path alive)
  wasm:       # ubuntu — cross-compile medatat-worker, then `worker-build`, then bundle size
  bench:      # ubuntu — cargo bench, fail on a Bench 1 or 2 threshold regression
  gpui-build: # macos, ubuntu, windows — cargo build -p medatat-ui (compile guard)
```

`preflight` exists because Cargo resolves the whole workspace graph even for a single-crate
build, so one unresolvable dependency fails every job at once; catching it in one place
turns six confusing failures into one actionable message.

There is no `worker` job yet. `scripts/smoke.sh` needs a sign-in code that only email
delivers, and `wrangler.jsonc` still carries placeholder D1 and KV ids, so the job would be
permanently red — which teaches everyone to ignore red. It arrives when both are resolved.

Cache `~/.cargo/git` aggressively — Cargo clones ~1 GB of Zed history for the `gpui`
dependency. Set `CARGO_NET_GIT_FETCH_WITH_CLI=true`.

## Test data

`medatat-testkit` generates **synthetic data only**. This system does not handle PHI today;
the checklist for changing that is [12-PHI-READINESS.md](12-PHI-READINESS.md).

```rust
pub fn synthetic_form(field_count: usize) -> FormDef;      // spread across all 7 kinds
pub fn synthetic_case(form: &FormDef, seed: u64) -> Vec<(FieldId, Value)>;
pub fn seed_corpus(cases: usize, fields_per_case: usize, out: &Path) -> Result<CorpusStats>;
```

Names, MRNs, and dates come from a fixed generator with a seeded RNG — deterministic, so a
failing bench is reproducible.
