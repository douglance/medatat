# 01 — Architecture

## The one rule

**The UI never awaits the network.**

Every read and every write the user triggers hits local encrypted SQLite synchronously.
Sync to Cloudflare runs on a background executor and may be slow, offline, or retrying
without the user noticing. This single rule is what makes R13, R14, and R15 true rather
than approximated.

## Diagram

```
┌──────────────────────────────────────────────┐
│  GPUI desktop app  (macOS / Windows / Linux) │
│                                              │
│   medatat-ui ──── medatat-core ───┐          │
│        │                          │          │
│        ▼                          ▼          │
│   medatat-store              medatat-sync    │
│   ┌────────────────────────┐      │          │
│   │ SQLCipher SQLite       │      │          │
│   │  form defs             │      │          │
│   │  caseload values       │      │  50–300 µs
│   │  outbox                │      │          │
│   └────────────────────────┘      │          │
└───────────────────────────────────┼──────────┘
                                    │  background only
                                    ▼
                        ┌───────────────────────┐
                        │  Worker (workers-rs)  │
                        │  medatat-worker       │
                        └──┬─────────┬──────────┘
                           ▼         ▼
                    ┌──────────┐  ┌──────────────┐
                    │  D1      │  │  CaseDO      │ × ~100k
                    │  KV      │  │  SQLite      │
                    └──────────┘  │  US-pinned   │
                    forms         └──────────────┘
                    fields        ~1000 values
                    users         ~100 KB each
                    case_index
                    sessions
```

## Latency budget

| Path | Cost | Requirement |
|---|---|---|
| Local SQLite read, ~500 fields, `WITHOUT ROWID` clustered | **50–300 µs** | R13 (200 ms) — ~1000× margin |
| Local SQLite write, 300 fields, one transaction | **< 10 ms** | R14 (200 ms) — ~20× margin |
| Open-case intent → first painted frame | **< 50 ms** | R15 |
| Background sync round trip to a warm CaseDO | 40–90 ms | off the critical path |
| Background sync round trip to a cold/hibernated CaseDO | unmeasured — Bench 4 | off the critical path |

The previous iteration of this design put the network on the critical path and could only
offer a read-only skeleton on a cache miss — a loading state in disguise. Moving to a local
store removed that, and with it removed cold-start latency as a UI risk entirely.

## Component responsibilities

| Crate | Responsibility | May depend on |
|---|---|---|
| `medatat-core` | Domain types, validation, `FormInstance`, wire types. **No I/O whatsoever.** | serde, chrono, rust_decimal |
| `medatat-store` | Encrypted SQLite: schema, queries, outbox. Sync and async APIs. | core, rusqlite (bundled-sqlcipher) |
| `medatat-sync` | `Transport` trait, delta sync loop, conflict handling. **Must not depend on reqwest.** | core, store |
| `medatat-ui` | The GPUI app. **The only crate that may `use gpui`.** | core, store, sync, gpui, gpui-component |
| `medatat-worker` | Cloudflare Worker + `CaseDO`. Compiles to `cdylib` / wasm32. | core, worker |
| `medatat-cli` | incurs-based CLI driver for testing and seeding. | core, testkit, incurs, reqwest, tokio |
| `medatat-testkit` | Synthetic corpus generator, `FakeClock`, `MockTransport`. Dev-dependency only. | core, store |

**Dependency direction is strictly one-way.** `medatat-core` depends on nothing in the
workspace. Nothing depends on `medatat-ui`. A cycle is a build error and a design error.

### Why `medatat-core` is shared with the Worker

Three things, and they are the three that would otherwise silently corrupt clinical data:

1. **One validation engine.** `validate(&FieldDef, &Value)` runs in the keystroke handler
   *and* inside the Worker. Client feel and server enforcement cannot drift.
2. **One `FieldKind` → column mapping.** Used by both the SQLite and the DO query builders.
3. **Identical `parse_time_24` and `Decimal` semantics** on both sides.

This is the reason the Worker is Rust rather than TypeScript/Hono. See
[ADR-0004](adr/0004-workers-rs.md).

## Server storage topology

### Why one Durable Object per case

| | Single D1 | Sharded D1 | **DO-per-case** |
|---|---|---|---|
| Fits 100M values (R16) | ✗ **10 GB hard cap**; 100M EAV rows ≈ 6–10 GB. At the ceiling, writes fail | ~5–10 shards, hand-rolled routing | ✓ ~100 KB of a 10 GB budget each |
| Write concurrency | table locks | table locks | ✓ single-threaded per case; `rev` is race-free with no locking |
| Data residency | primary region only | no per-shard control | ✓ `jurisdiction("us")` pins compute *and* storage |
| Storage cost | $0.75/GB-mo | same | ✓ $0.20/GB-mo ≈ **$2/mo** for 10 GB |
| Cross-case query | ✓ SQL | ✗ no cross-shard joins | ✗ needs a D1 index |

D1's 10 GB cap is the disqualifying fact for R16. Sharded D1 loses cross-case SQL anyway
while gaining none of the isolation or residency control. Full rationale in
[ADR-0001](adr/0001-durable-object-per-case.md).

The cost is real and named: **no cross-case SQL.** See
[10-LIMITATIONS.md](10-LIMITATIONS.md).

### What lives where

| Store | Contents | Size | Consistency |
|---|---|---|---|
| **CaseDO** SQLite | One case's field values | ~100 KB × 100k | strong, single-writer |
| **D1** | forms, sections, fields, field_options, section_fields, app_user, case_index | < 100 MB | strong for config; `case_index` is **eventually consistent** |
| **KV** | auth codes, sessions | tiny | eventually consistent up to 60 s; **max 1 write/sec per key** |
| **Client SQLite** | form defs, the user's caseload, outbox | tens of MB | local truth for the UI |

## Client-side threading

GPUI splits `ForegroundExecutor` (main thread) and `BackgroundExecutor` (pool).

- **UI reads** use a read connection pinned to the main thread. WAL means a reader never
  blocks on the writer, so these are safe to do synchronously inside a render pass.
- **UI writes** go through a write connection behind a mutex, also synchronously — a
  300-field transaction is under 10 ms, well inside a frame budget at the granularity users
  actually save at.
- **Sync** runs entirely on `cx.background_executor().spawn(...)`, returning `Task<T>`.
  Dropping a `Task` cancels it. Nothing async ever appears in the render path.

## Security posture

**This system does not currently handle PHI.** All data is synthetic. Data protections are
implemented but gated behind the `phi` cargo feature, which is **off by default** — see
[12-PHI-READINESS.md](12-PHI-READINESS.md) for what it turns on and the checklist for
turning it on.

Always on, regardless of the feature:

- `actor_id` is resolved server-side from the session token and **never accepted from the
  client**. This is a correctness property, not a privacy one.
- Session tokens are 32 bytes from `OsRng`; only `sha256(token)` is stored.
- Magic-code emails carry the code and nothing else.
- Auth endpoints do not leak account existence.

Behind `--features phi`:

- SQLCipher on the local database, key held in an owner-only file beside it (**not** the
  OS keychain — see [12-PHI-READINESS.md](12-PHI-READINESS.md)).
- `Value` redacts in `Debug` and zeroizes on `Drop`.
- Core dumps disabled at startup.

**PHI mode changes how data is protected, never how it is shaped.** The schema, sync
protocol, API, and UI are identical either way, which is what makes the switch safe to flip
late.
