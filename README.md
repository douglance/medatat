# medatat

A data abstraction tool for medical data. Native desktop app (Rust + GPUI) over a Cloudflare
backend, built for arbitrary runtime-defined forms at ~100M field values.

> **Status: implemented and running on macOS; unverified elsewhere.**
>
> All seven crates build and pass, the Worker cross-compiles and bundles to 391 KB gzipped,
> and the desktop app opens, persists, and edits forms. The performance gates that justify
> the architecture are **measured**, not assumed.
>
> What is *not* true yet: it has never been built on Windows or Linux, never deployed to
> Cloudflare, and holds no real patient data (see [Before you start](#before-you-start)).
> Start at [docs/08-MILESTONES.md](docs/08-MILESTONES.md) for what is done and what is not.

## Measured, not assumed

| | Requirement | Gate | Measured |
|---|---|---|---|
| **R13** form load, 500 fields | 200 ms | 5 ms | **195 µs** |
| **R14** form save, 300 fields | 200 ms | 10 ms | **5.2 ms** |
| R14 single field (the steady state) | — | — | **86 µs** |
| **R16** 100M values, 100k cases | works at scale | 1.5× | **0.97× read, 1.07× write** |
| Worker bundle | 10 MB | — | **391 KB** gzipped |

R13 has roughly a thousandfold margin over the requirement. That is the whole payoff of
making local encrypted SQLite the UI's system of record rather than putting the network on
the critical path — see [ADR-0002](docs/adr/0002-encrypted-local-sqlite.md).

R16 is the one that took longest to settle, and it is measured the only way worth believing:
a corpus of **100,000,000 field values across 100,000 cases**, with a one-case control
interleaved in the same process so host load falls on both equally. A hundred thousand times
the data costs 0.97× per load and 1.07× per save — no measurable penalty at all. That is the
`WITHOUT ROWID` layout keyed on `(case_id, field_id)` doing what
[ADR-0002](docs/adr/0002-encrypted-local-sqlite.md) said it would: opening a case touches the
same handful of pages whether the store holds one case or a hundred thousand.

Two numbers were once deliberately **absent** rather than estimated — seeding throughput and
the Durable Object cold-wake time — because the local figures turned out to be emulator
artifacts. Both have since been measured against the real deployment: **0.12 cases/s** server
seeding, and **0.4–1.0 s** cold-DO wake against 0.16 s warm. Neither touches R13: the cold
wake is three to six times the entire 200 ms budget, which is precisely why the network is
not on the critical path. Server seeding throughput and client storage scale are different
questions, and only the second is what R16 asks. See [07-TESTING.md](docs/07-TESTING.md).

## What it does

Clinical abstractors transcribe hundreds of fields per patient case into structured forms.
Study coordinators define those forms — sections, fields, column layout — through a UI, with
no code changes and no deploys.

Field kinds: text, numeric, date, **24-hour time**, radio, select, textarea. Sections render
at 1, 2, or 3 columns, and fields can span columns.

## The three things that shape the design

1. **Load and save under 200 ms with hundreds of fields, and no loading spinners. Ever.**
   Met by making encrypted local SQLite the UI's system of record. Reads are 50–300 µs;
   the network is never on the critical path. The user's whole assigned caseload is synced
   before they open anything, so a cache miss is not part of normal operation.

2. **~100 million values across ~100,000 cases.** D1 caps at 10 GB, which 100M EAV rows
   would exceed. Each case therefore gets its **own Durable Object** with its own SQLite —
   ~100 KB of a 10 GB budget, and single-threaded writes for free.

3. **Arbitrary, runtime-defined fields.** `field.id` is global and stable; a field's `kind`
   is never re-typed in place. That one invariant replaces an entire form-version lifecycle.

## Architecture at a glance

```
GPUI desktop app ──► encrypted local SQLite   (every read/write, 50–300 µs)
        │
        └──background──► Worker (Rust) ──► D1        config, users, case index
                                       └──► CaseDO   one per case, ~100 KB
```

## Documentation

| Doc | Read it for |
|---|---|
| [00-REQUIREMENTS](docs/00-REQUIREMENTS.md) | The requirements, IDs R1–R16, and the compliance matrix. **Start here** |
| [01-ARCHITECTURE](docs/01-ARCHITECTURE.md) | Components, latency budget, threading, security posture |
| [02-DATA-MODEL](docs/02-DATA-MODEL.md) | All three schemas and the Rust type mapping |
| [03-API](docs/03-API.md) | The HTTP contract |
| [04-SYNC](docs/04-SYNC.md) | Outbox, drain loop, conflict detection, caseload pre-sync |
| [05-UI-SPEC](docs/05-UI-SPEC.md) | Widgets, column layout, the 24hr time field, keyboard model |
| [06-FORM-BUILDER](docs/06-FORM-BUILDER.md) | The configuration UI |
| [07-TESTING](docs/07-TESTING.md) | Test layers and the four performance benchmarks |
| [08-MILESTONES](docs/08-MILESTONES.md) | M0–M8 with exit criteria |
| [09-SETUP](docs/09-SETUP.md) | Toolchain, Cloudflare provisioning, running, troubleshooting |
| [10-LIMITATIONS](docs/10-LIMITATIONS.md) | What this does not do, and the risks |
| [11-CRATE-GUIDE](docs/11-CRATE-GUIDE.md) | Module layout and public API per crate. **Read before writing code** |
| [12-PHI-READINESS](docs/12-PHI-READINESS.md) | The `phi` feature, and the checklist before real patient data |
| [ADRs](docs/adr/) | Why the five load-bearing decisions were made |

## Before you start

- **Synthetic data only.** This system does not handle PHI today. The protections for real
  patient data — SQLCipher, redaction, zeroize, core-dump suppression — are implemented
  behind the `phi` cargo feature, **off by default**, so development is unencumbered. The
  checklist for turning it on is [12-PHI-READINESS](docs/12-PHI-READINESS.md).
- **`cetify.email` does not currently resolve.** Magic-code login needs it registered,
  delegated to Cloudflare DNS, and onboarded to Email Sending. This is the one thing that
  blocks development — see [10-LIMITATIONS](docs/10-LIMITATIONS.md#gates).

## Quick start

```bash
# See docs/09-SETUP.md for full prerequisites — wrangler must be >= 4.123.0
wrangler dev                                        # Worker, local
MEDATAT_API=http://localhost:8787 cargo run -p medatat-ui

cargo test --workspace
cargo bench -p medatat-testkit                      # Benches 1–3 gate CI
cargo xtask lint-no-spinner                          # enforces R15
./scripts/smoke.sh http://localhost:8787
```

## Layout

```
crates/
  medatat-core/      domain types, validation, FormInstance — no I/O, shared with the Worker
  medatat-store/     encrypted SQLite (SQLCipher), outbox
  medatat-sync/      Transport trait, delta sync, conflict handling
  medatat-ui/        the GPUI app — the only crate that may `use gpui`
  medatat-worker/    Cloudflare Worker + CaseDO (workers-rs)
  medatat-cli/       incurs-based CLI for testing and seeding
  medatat-testkit/   synthetic corpus, MockTransport, FakeClock
```

Module layout and the public surface of each crate: [docs/11-CRATE-GUIDE.md](docs/11-CRATE-GUIDE.md).

## License

Proprietary. Not for distribution.
