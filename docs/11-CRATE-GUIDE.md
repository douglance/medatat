# 11 — Crate Implementation Guide

Module layout and public surface for each crate. This is the build order and the contract
between crates. Signatures here are the target; adjust only with a reason you can write down.

Dependency direction is strictly one-way:

```
medatat-core ──► medatat-store ──► medatat-sync ──► medatat-ui
      │                                    ▲
      ├──► medatat-worker                  └── medatat-cli
      └──► medatat-testkit (dev)
```

`medatat-core` depends on nothing in the workspace. Nothing depends on `medatat-ui`.
A cycle is a build error and a design error.

---

## `medatat-core`

Domain types, validation, `FormInstance`, wire types. **No I/O of any kind** — it compiles
to `wasm32-unknown-unknown` and is shared verbatim with the Worker.

```
src/
  lib.rs
  ids.rs          FieldId, CaseId, FormId, SectionId, FieldIdx, CaseRev, OptionCode
  def.rs          FieldDef, FieldKind, FieldOption, SectionDef, SectionField, FormDef
  value/
    mod.rs        Value; redacting Debug + Drop/zeroize under #[cfg(feature = "phi")]
    time.rs       parse_time_24, format_time_24            ← most-tested file in the repo
    numeric.rs    Decimal parse/format, scale handling
    date.rs       ISO parse/format
  validate.rs     validate(&FieldDef, &Value) -> Result<(), ValidationError>
  view.rs         widget_spec, focus_order, fuzzy search — pure presentation logic
  instance.rs     FormInstance, dirty tracking
  layout.rs       effective_columns, col_span clamping
  wire.rs         Envelope, ApiError, ValueRow, PutValuesReq/Resp, ConfigDelta
  error.rs        ValidationError, CoreError
```

```rust
// value/time.rs — R8
pub fn parse_time_24(s: &str) -> Result<NaiveTime, TimeParseError>;
pub fn format_time_24(t: NaiveTime) -> String;          // always "HH:MM"

// validate.rs — called by BOTH the client (feel) and the Worker (truth). Never duplicate.
pub fn validate(def: &FieldDef, value: &Value) -> Result<(), ValidationError>;

// layout.rs — R12
pub fn effective_columns(declared: u8, width_px: f32) -> u8;
pub fn clamp_col_span(col_span: u8, columns: u8) -> u8;

// view.rs — renderer-agnostic presentation. Lives here, NOT in medatat-ui, so that
// R5–R12 rendering coverage needs no window and no GPU. medatat-ui only maps a
// WidgetSpec onto gpui-component widgets.
pub fn widget_spec(sf: &SectionField, inst: &FormInstance, effective_cols: u8) -> WidgetSpec;
pub fn focus_order(def: &FormDef, collapsed: &[bool]) -> Vec<FieldIdx>;
pub fn search_fields(def: &FormDef, needle: &str) -> Vec<FieldIdx>;

// instance.rs
impl FormInstance {
    pub fn new(def: Arc<FormDef>, case_id: CaseId, base_rev: CaseRev,
               values: Vec<(FieldId, Value)>) -> Self;
    pub fn set(&mut self, idx: FieldIdx, v: Value) -> Option<ValidationError>;
    pub fn get(&self, idx: FieldIdx) -> &Value;
    pub fn error(&self, idx: FieldIdx) -> Option<&ValidationError>;
    pub fn pending(&self) -> impl Iterator<Item = (FieldId, &Value)> + '_;   // O(dirty)
    pub fn confirm(&mut self, fields: &[FieldIdx], new_rev: CaseRev);
    pub fn is_dirty(&self) -> bool;
}

// def.rs — maps a kind to its storage column. One function, used by every query builder.
pub fn value_column(kind: &FieldKind) -> ValueColumn;   // Text | Numeric | Date | Time
```

**Rules.** No `std::fs`, no `std::net`, no `tokio`. `values` is a dense `Vec` indexed by
`FieldIdx` — never a `HashMap<FieldId, _>` on a hot path. `set` validates one field and
returns one error; there is no rule graph (see
[ADR-0005](adr/0005-cut-audit-versioning-rules.md)).

**Build gate:** `cargo build -p medatat-core --target wasm32-unknown-unknown` must succeed.

---

## `medatat-store`

Encrypted SQLite. Owns the client schema in [02-DATA-MODEL.md](02-DATA-MODEL.md).

```
src/
  lib.rs
  schema.sql        the DDL from 02-DATA-MODEL.md
  migrations.rs     versioned, forward-only
  keyring.rs        #[cfg(feature = "phi")] key gen + mode-0600 key file (NOT the OS keychain)
  conn.rs           read conn (UI thread) + write conn (mutex); PRAGMA setup
  forms.rs          form + field def load/store (JSON blobs)
  cases.rs          patient_case CRUD, worklist queries
  values.rs         field_value read/write; apply_local
  outbox.rs         enqueue, next_batch, confirm, bump_attempts, drop
  conflicts.rs      record, list, resolve
```

```rust
pub struct Store { /* read_conn: Connection, write_conn: Mutex<Connection> */ }

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError>;      // key from medatat.key beside it
    pub fn open_in_memory() -> Result<Self, StoreError>;       // no-key fallback, tests

    // --- read path: synchronous, UI thread, WAL means it never blocks on a writer ---
    pub fn load_form(&self, form_id: FormId) -> Result<Arc<FormDef>, StoreError>;
    pub fn load_all_forms(&self) -> Result<Vec<Arc<FormDef>>, StoreError>;
    pub fn save_fields(&self, fields: &[FieldDef]) -> Result<(), StoreError>;  // upsert only
    pub fn all_fields(&self) -> Result<Vec<FieldDef>, StoreError>;             // placed or not
    pub fn load_case_values(&self, case_id: CaseId)
        -> Result<Vec<(FieldId, Value)>, StoreError>;          // R13 — must be a PK range scan
    pub fn worklist(&self, assignee: &str, q: &WorklistQuery)
        -> Result<Vec<CaseRow>, StoreError>;

    // --- write path: synchronous, one transaction, value + outbox together ---
    pub fn apply_local(&self, case_id: CaseId, changes: &[(FieldId, Value)],
                       base_rev: CaseRev) -> Result<(), StoreError>;

    // --- sync-facing ---
    pub fn next_outbox_batch(&self, limit: usize) -> Result<Vec<OutboxRow>, StoreError>;
    pub fn confirm(&self, case_id: CaseId, fields: &[FieldId], rev: CaseRev)
        -> Result<(), StoreError>;
    pub fn apply_server_values(&self, case_id: CaseId, rows: &[ValueRow], rev: CaseRev)
        -> Result<(), StoreError>;                             // skips pending = 1
    pub fn record_conflicts(&self, case_id: CaseId, rows: &[ValueRow])
        -> Result<(), StoreError>;
}
```

**Rules.** Under `--features phi`, `PRAGMA key` is the **first** statement after open, and a
wrong key surfaces as `StoreError::Locked` — never as corruption, and never triggering a
recreate, which would look like total data loss. Default builds use plain SQLite.
`apply_local` writes the value and the outbox row in **one transaction**. `field_value` and
`outbox` are `WITHOUT ROWID`.

---

## `medatat-sync`

Delta sync. **Must not depend on `reqwest`** — that is what makes it testable and reusable.

```
src/
  lib.rs
  transport.rs    Transport trait, TransportError
  engine.rs       SyncEngine: drain pass, caseload pre-sync, config refresh, conflicts
  backoff.rs      delay_for(attempts, seed) — exponential, capped, jittered
```

```rust
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn config(&self, since: ConfigRev) -> Result<Option<ConfigDelta>, TransportError>;
    async fn list_cases(&self, q: CaseQuery) -> Result<CasePage, TransportError>;
    async fn get_values(&self, case_id: CaseId, since_rev: CaseRev)
        -> Result<ValuePage, TransportError>;
    async fn put_values(&self, case_id: CaseId, req: PutValuesReq)
        -> Result<PutValuesResp, TransportError>;
}

pub struct SyncEngine<T: Transport> { /* … */ }
impl<T: Transport> SyncEngine<T> {
    pub fn new(store: Arc<Store>, transport: T) -> Self;
    pub async fn drain_once(&self) -> Result<DrainReport, SyncError>;
    pub async fn sync_caseload(&self, assignee: &str) -> Result<CaseloadStats, SyncError>;
    pub async fn sync_config(&self) -> Result<Option<ConfigRev>, SyncError>;
    pub fn next_delay(&self) -> Result<Option<Duration>, SyncError>;
    pub fn status(&self) -> Arc<SyncStatus>;   // unsynced, conflicts, SyncState
}
```

**The engine owns no timer and no clock.** It exposes one pass, not a `run` loop, and the
caller decides the cadence from `next_delay`. That is what keeps it drivable from a test
without a simulated timeline, and it is why there is no `Clock` trait here.

**Rules.** Backoff is `min(60s, 2^attempts * 500ms) ± 20%` jitter — the jitter is not
optional, or every client that dropped together retries together.
`TransportError::Offline` is a normal state, not an error. Never overwrite a `pending = 1`
row. Never overwrite the focused field (the UI supplies a `deferred_merge` hook).

---

## `medatat-ui`

The GPUI app. **The only crate that may `use gpui`.**

```
src/
  main.rs
  app.rs              App state, executors, startup (store open; under `phi` also
                      core-dump suppression)
  widgets/            ← the ONLY place gpui-component is called
    mod.rs
    form_grid.rs      column layout (R12); swappable fallback lives here
    text_input.rs     R5
    numeric_input.rs  R6
    date_input.rs     R7
    time_input.rs     R8 — the hand-built one
    radio_group.rs    R9
    select_input.rs   R10
    textarea.rs       R11
  form/
    view.rs           FormView, subscriptions, the no-notify rule
    render.rs         maps medatat_core::WidgetSpec -> gpui-component widgets
  builder/            design-mode renderer, panes, inspector (see 06)
  worklist/
  login/
  status.rs           the permitted status indicators (never a spinner)
```

The seam that keeps the GUI test budget at three lives in `medatat-core::view`, not here:
`widget_spec` and `focus_order` are pure functions over domain types, so they are
snapshot-tested without linking gpui. `medatat-ui` consumes a `WidgetSpec` and makes no
presentation decisions of its own.

**Rules.** Every `gpui-component` call goes through `widgets::*`. A keystroke never
`cx.notify()`s the parent. Under `phi`, `FormView::drop` clears every `InputState`. No
spinner, progress bar, skeleton, or "Loading…" anywhere — `cargo xtask lint-no-spinner`
enforces it.

---

## `medatat-worker`

Cloudflare Worker and `CaseDO`. Compiles to `cdylib` / wasm32.

```
src/
  lib.rs            #[event(fetch)] entry, router
  error.rs          LogicError + the one mapping onto status codes
  http.rs           every documented status and envelope, as pure fns — tests natively
  routes/           ← WASM only: parse, authenticate, call logic/, encode
    mod.rs          bindings, session resolution, body/query helpers, Response encoding
    auth.rs         request/verify/logout/me
    config.rs       GET /config, all mutations (admin only)
    cases.rs        list, create, values get/put — proxies to CaseDO
    bulk.rs         bulk create, export, reindex
  case_do.rs        #[durable_object] CaseDO                       ← WASM only
  logic/            ← pure fns over traits: tests natively, no WASM
    values.rs       handle_put_values, conflict detection
    auth.rs         code generation, verification, session minting
    config.rs       config validation (col_span <= columns, kind immutability)
    config_rows.rs  D1 rows <-> ConfigDelta/FieldLookup, field kind <-> (tag, config JSON)
    cases.rs        worklist paging, keyset cursor, bulk bounds
    case_store.rs   CaseStore trait + FieldLookup + in-memory test impl
  store/            ← WASM only
    d1.rs           D1 queries
    kv.rs           auth codes, sessions
    case_sql.rs     CaseStore over a Durable Object's SqlStorage
  mail.rs           trait Mailer + send_email impl  (seam for Postmark/SES)
migrations/
  0001_init.sql
```

`routes/`, `store/`, and `case_do.rs` are behind `#[cfg(target_arch = "wasm32")]`, so
`cargo test -p medatat-worker` does not compile them at all. Cross-compiling is not
optional — see the traps in [AGENTS.md](../AGENTS.md).

```rust
// logic/case_store.rs — pure, tested natively without WASM
pub trait CaseStore {
    fn rev(&self) -> LogicResult<CaseRev>;
    fn changed_since(&self, fields: &[FieldId], since: CaseRev) -> LogicResult<Vec<ValueRow>>;
    fn get_all(&self, since: CaseRev) -> LogicResult<Vec<ValueRow>>;
    fn put(&self, changes: &[ValueChange], rev: CaseRev, actor: &ActorId) -> LogicResult<()>;
}

// logic/values.rs
pub fn handle_put_values<S: CaseStore>(
    store: &S, req: PutValuesReq, actor: &ActorId, defs: &FieldLookup,
) -> LogicResult<PutValuesResp>;
```

**Rules.** **No DDL in the `CaseDO` constructor** — it runs on every cold start; create the
schema lazily on first write. `actor_id` comes from the session, never the request body.
Every value is re-validated with `medatat_core::validate`. Keep handler logic in `logic/`
so it tests without workerd.

---

## `medatat-cli`

`incurs`-based driver for testing and seeding.

```
src/
  main.rs       `seed` and `push` are handled locally; everything else goes to the gateway
  fetch.rs      impl incurs::fetch::FetchHandler over reqwest
  seed.rs       corpus generation to a directory
  push.rs       corpus generation INTO a running Worker, through the ordinary write path
```

There is **no `dev` subcommand**. A sign-in code is delivered only by email and only its
sha256 is stored, so nothing can read one back; `scripts/smoke.sh` takes it in `SMOKE_CODE`
and skips the authenticated half when it is absent.

`seed` writes a corpus to disk. `push` writes one into a Worker, and is what makes Bench 4
runnable — before it, nothing could get a corpus in, because `POST /bulk/cases` creates
index rows without values.

```
medatat push --cases 1000 --fields 1000 --batch 50   # MEDATAT_API, MEDATAT_TOKEN (admin)
```

Three things about `push` are load-bearing rather than incidental:

1. **It publishes the generated form to `/config` first.** A form generated in-process
   exists nowhere on the server, and the write path re-validates every value against D1, so
   an unpublished form makes every single value a 404.
2. **It writes in batches, not one shot.** `--batch 50` over a 1,000-field case produces 20
   revs. A whole-case write would stamp every row `rev 1`, and both per-field conflict
   detection and `since_rev` delta reads are benchmarked against the spread — a flat corpus
   would make delta sync unbenchmarkable *and* flatter the numbers.
3. **It namespaces every key it creates.** `field.key` and `form.key` are `UNIQUE` in D1 and
   the generator's keys are a pure function of position (`f0000_text`), so without a
   per-run prefix the second run dies on a constraint violation.

```rust
pub struct HttpFetch { base: String, client: reqwest::Client, default_token: Option<String> }

#[async_trait::async_trait]
impl incurs::fetch::FetchHandler for HttpFetch {
    async fn handle(&self, req: incurs::fetch::FetchInput) -> incurs::fetch::FetchOutput;
}
```

Reserved flags come from `incurs`: `-X/--method`, `-d/--data/--body`, `-H/--header`;
unknown `--key value` pairs become query parameters. `incurs` provides no assertion DSL —
assertions live in `scripts/smoke.sh` (jq) and `cargo test`.

---

## `medatat-testkit`

Dev-dependency only. **Synthetic data exclusively** — no real PHI enters this system.

```
src/
  lib.rs
  forms.rs      synthetic_form(field_count) — spread across all seven kinds
  cases.rs      synthetic_case(form, seed) — deterministic
  corpus.rs     seed_corpus(cases, out) -> CorpusStats
  mock.rs       MockTransport
  clock.rs      FakeClock
benches/
  bench1_load.rs      R13 — gate 5 ms
  bench2_save.rs      R14 — gate 10 ms
  bench3_open.rs      R13/R15 — gate 50 ms
  bench4_scale.rs     R16 — nightly, no gate
```

All generators take a seed, so a failing benchmark is reproducible. **Synthetic data only** —
see [12-PHI-READINESS.md](12-PHI-READINESS.md).

---

## `xtask`

```
src/
  main.rs
  lint_no_spinner.rs   R15 enforcement
  bump_gpui.rs         weekly rev bump + build
  seed.rs              wraps medatat-cli seed for CI
```

```bash
cargo xtask lint-no-spinner
cargo xtask bump-gpui
```

---

## Build order

Follows [08-MILESTONES.md](08-MILESTONES.md):

1. `medatat-ui` skeleton (M0 — prove the platform, throwaway)
2. `medatat-store` + `medatat-worker` skeleton (M1 — prove the numbers, throwaway)
3. `medatat-core`, then `medatat-store` (M2)
4. `medatat-worker`, then `medatat-sync`, then `medatat-cli` (M3)
5. `medatat-ui` form rendering (M4)
6. `medatat-ui` builder (M5)
7. `medatat-ui` worklist + `medatat-sync` caseload (M6)
8. `medatat-testkit` full corpus (M7)

M0 and M1 are deliberately throwaway. Their job is to answer two questions — does GPUI work
on all three platforms, and do the benchmarks pass — before either answer can cost a
rewrite.
