# ADR-0001 — One Durable Object per patient case

**Status:** Accepted · **Date:** 2026-08-17 · **Drives:** R16

## Context

R16 requires ~100 million field values across ~100,000 patient cases, on Cloudflare.

The hot path is *read all ~1000 values for one case*. The cold path is *query across cases*.

## Options

**A. Single D1 database.** One EAV table, `PRIMARY KEY (case_id, field_id)`.

**B. Sharded D1.** 5–10 databases, routed by a hash of `case_id`.

**C. One Durable Object per case.** Each case gets its own SQLite instance.

## Decision

**C — one Durable Object per case**, created with `jurisdiction("us")`.

## Rationale

**D1's 10 GB per-database cap is disqualifying for A.** 100M EAV rows at ~60–100 bytes plus
indexes lands at 8–15 GB. At the ceiling D1 does not degrade — writes fail with
`Exceeded maximum DB size`, and the documented remedy is "delete rows or shard". Building a
system whose success condition is exceeding its store's hard limit is not a plan.

**B inherits A's worst property while gaining none of C's.** Sharding by case loses
cross-shard joins — the same loss as C — but keeps table-level write contention, gives no
per-shard residency control, and adds hand-rolled routing that must be maintained forever.

**C's numbers are not close.** ~1000 values ≈ 60–100 KB per object against a 10 GB
per-object limit: roughly 100,000× headroom. 100k objects ≈ 10 GB total at $0.20/GB-month
≈ **$2/month**, cheaper than D1's $0.75/GB-month.

Three properties fall out for free:

1. **Single-threaded per case.** The `rev` counter is race-free with no locking. The
   `SELECT ... FOR UPDATE` a relational design would need does not exist.
2. **`jurisdiction("us")` pins compute *and* storage.** D1 offers no equivalent per-database
   residency control — relevant for a system that will eventually hold PHI.
3. **Hot-path locality.** One round trip reaches the object; the ~1000-row read is local
   SQLite, sub-millisecond.

## Consequences

**Accepted cost: no cross-case SQL.** Durable Objects cannot be queried across. This
requires a D1 `case_index` maintained by the DOs, which is eventually consistent
([10-LIMITATIONS.md](../10-LIMITATIONS.md#2-the-d1-case-index-is-eventually-consistent)),
and pushes analytics to an offline export
([10-LIMITATIONS.md](../10-LIMITATIONS.md#1-cross-case-reporting-is-not-built)).

This is the sharpest trade in the design. It is acceptable because the requirements ask for
storage and retrieval at scale, not for analytics — but it is the first thing to revisit if
that turns out to be wrong, and the right moment to revisit it is **before M3**, not after
M7.

Also accepted: bulk migrations become fan-out jobs rather than one `ALTER TABLE`, and
backup/PITR is per-object rather than per-database.

**Not a consequence:** DO cold-start latency. Because [ADR-0002](0002-encrypted-local-sqlite.md)
puts a local store on the UI critical path, cold starts affect background sync only. This
was the largest unquantified risk in an earlier draft, and the local-store decision removed
it rather than estimating around it.

## Implementation notes

- `new_sqlite_classes` in the wrangler migration, **not** `new_classes`.
- **No DDL in the DO constructor.** Create the schema lazily on first write; the constructor
  runs on every cold start.
- `value_numeric` is TEXT holding a decimal string. SQLite has no exact numeric type and
  `REAL` would silently corrupt clinical values.
