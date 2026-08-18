# 07 — Testing and Performance

## Principle

**Push logic below the GUI line.** Anything testable without a window must be. The GUI test
budget is three tests; everything else is `cargo test`.

The performance benchmarks are not a late validation step — they are an **M1 gate**. The
architecture is justified by a latency argument, so the number gets measured before anything
is built on it.

---

## Performance benchmarks

`criterion` in `medatat-testkit`. Benches 1–3 gate CI. Bench 4 runs nightly.

| Bench | Measures | Target | Measured (macOS, 2026-08-17) | Proves | CI |
|---|---|---|---|---|---|
| **1** | Local SQLite → `FormInstance`, 500 fields, warm | **< 5 ms** | **195 µs** ✅ | R13 | gate |
| **2** | Local write of 300 changed fields, one transaction | **< 10 ms** | **5.2 ms** ✅ | R14 | gate |
| **3** | Open-case intent → first painted frame, 200 cases | **p99 < 50 ms** | R13, R15 | gate |
| **4** | Full sync of one case from a **cold, hibernated** DO; seeding throughput | recorded | R16 | nightly |

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

Also answers a question that must not be discovered late: **is seeding 100k DOs × 1000
values feasible at all?**

Measure throughput and cost on **1,000 cases first**, extrapolate, and record the estimate
in the bench output before M7 commits to the full corpus. If the extrapolation says days or
hundreds of dollars, that is a finding to act on, not to absorb.

Cold-DO measurement requires a DO idle beyond the eviction window (70–140 s). Seed cases,
wait, then read.

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

### `medatat-ui` — exactly three `#[gpui::test]`

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

```yaml
# .github/workflows/ci.yml — shape, not final
jobs:
  check:      # ubuntu — fmt, clippy -D warnings, cargo xtask lint-no-spinner
  test:       # macos, ubuntu, windows — cargo test --workspace
  phi:        # ubuntu — cargo build+test --workspace --features phi (keeps the path alive)
  worker:     # ubuntu — wrangler dev + scripts/smoke.sh
  bench:      # ubuntu — cargo bench, fail on Bench 1–3 threshold regression
  gpui-build: # macos, ubuntu, windows — cargo build -p medatat-ui (compile guard)
  nightly:    # Bench 4 against the seeded corpus
```

Cache `~/.cargo/git` aggressively — Cargo clones ~1 GB of Zed history for the `gpui`
dependency. Set `CARGO_NET_GIT_FETCH_WITH_CLI=true`.

## Test data

`medatat-testkit` generates **synthetic data only**. This system does not handle PHI today;
the checklist for changing that is [12-PHI-READINESS.md](12-PHI-READINESS.md).

```rust
pub fn synthetic_form(field_count: usize) -> FormDef;      // spread across all 7 kinds
pub fn synthetic_case(form: &FormDef, seed: u64) -> Vec<(FieldId, Value)>;
pub fn seed_corpus(cases: usize, out: &Path) -> Result<CorpusStats>;
```

Names, MRNs, and dates come from a fixed generator with a seeded RNG — deterministic, so a
failing bench is reproducible.
