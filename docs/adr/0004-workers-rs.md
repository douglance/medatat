# ADR-0004 — The Worker is written in Rust (`workers-rs`)

**Status:** Accepted · **Date:** 2026-08-17

## Context

The client is Rust. The Worker could be TypeScript with Hono — which is the conventional
Cloudflare choice, matches the surrounding codebase's existing patterns, and has
first-class support for Durable Objects, D1, and the `send_email` binding.

## Decision

**Rust, via the `worker` crate (workers-rs) 0.8.5**, so that `medatat-core` is shared
verbatim between client and server.

## Rationale

Three things become impossible to get wrong, and they are exactly the three that would
otherwise silently corrupt clinical data:

1. **One validation engine.** `validate(&FieldDef, &Value)` runs in the keystroke handler
   *and* in the Worker's write path. Client feel and server enforcement cannot drift,
   because they are the same function.
2. **One `FieldKind` → storage-column mapping**, used by the SQLite and DO query builders alike.
3. **Identical `parse_time_24` and `Decimal` semantics** on both sides. A 24hr time that
   parses on the client parses identically on the server, by construction.

For a tool whose entire job is transcribing clinical values accurately, that is worth more
than matching the surrounding TypeScript convention.

### Feasibility was verified, not assumed

The blocking question was whether workers-rs exposes SQLite-backed Durable Object storage.
**It does.** `worker::SqlStorage` wraps `ctx.storage.sql` with no feature flag:

```rust
pub fn exec(&self, query: &str, bindings: impl Into<Option<Vec<SqlStorageValue>>>) -> Result<SqlCursor>
pub fn database_size(&self) -> usize
```

`SqlCursor::to_array::<T: Deserialize>()` gives serde-typed rows. D1 (`prepare`, `batch`,
`exec`), KV, R2, Queues, and the **`send_email` binding**
(`worker::email::{SendEmail, SendEmailBuilder, EmailAddress}`) are all present.

## Consequences

**Verified caveats, each provable at M1 before anything is built on it:**

- `send_email` is present in code but **undocumented in the README**, and
  [workers-rs#732](https://github.com/cloudflare/workers-rs/issues/732) is still open. Only
  the *outbound* path matters here; the inbound email-trigger handler is the weak spot.
- **Bundle limit is 10 MB compressed with a 1 s startup budget** (error `10021` on breach).
  Ship `lto = true`, `strip = true`, and let `worker-build` run `wasm-opt`. Realistic Rust
  Workers land 200 KB–1.5 MB gzipped — measure with the real dependency set.
- **`getrandom` needs the `js` backend** on `wasm32-unknown-unknown`; it arrives
  transitively via `uuid` v4 and token generation.
- `chrono`, `regex`, `rust_decimal`, and `uuid` compile cleanly with default features trimmed.
- The argon2 CPU-budget hazard **does not apply** — magic-code auth means there is no
  password KDF anywhere in the system.

**Bus factor.** workers-rs is effectively one maintainer, with 184 open issues and ~48% of
the crate documented. Cloudflare documents Rust as a first-class Workers language and the
repo is not archived, so this is a maintenance-velocity risk, not an abandonment risk. It is
still a crate to watch rather than trust blindly.

**Escape hatch.** If workers-rs proves untenable, the fallback is a TypeScript/Hono Worker
importing `medatat-core` as a `wasm-bindgen` module. That preserves the single-validation-engine
property at the cost of a serialization boundary per call and two build pipelines. The API
contract in [03-API.md](../03-API.md) is language-neutral, so this swap does not touch the
client.

**Testing shape.** Handlers are written as pure functions over a storage trait, so most
logic tests natively without WASM. The thin binding layer is covered by `wrangler dev`
driven through `medatat-cli` ([07-TESTING.md](../07-TESTING.md)).
