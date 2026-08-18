# ADR-0005 — Cut audit trail, form versioning, and conditional logic

**Status:** Accepted · **Date:** 2026-08-17

## Context

Earlier drafts of this design included three substantial subsystems:

1. **An audit trail** — per-field change history with actor, timestamps, and a SHA-256 hash
   chain for tamper evidence, streamed to R2 for durable archive.
2. **Form version lifecycle** — draft / published / retired states, per-version field
   placement, cases pinned to the version they were collected under, and a `classify`
   function distinguishing cosmetic from breaking field edits.
3. **Conditional logic** — show-if rules, required-when, cross-field validation, and a
   `RuleGraph` propagating dependencies on every keystroke.

None of these appear in the requirements. They were introduced as assumptions on the grounds
that clinical registries typically want them. The question was asked twice and not answered;
the assumptions were then built on heavily.

Together they accounted for most of the plan's complexity: a partitioned history table with
BRIN indexes, a Queue → R2 pipeline, a per-version schema, a publish diff UI, a dependency
graph, and visibility/enabled bitsets recomputed per edit.

## Decision

**Cut all three.** Scope to exactly what the requirements state.

Type-level validation stays — numeric range, valid 24hr time, max length — because
"numeric field" and "time entry in 24hr format" are meaningless without it.

## Rationale

Building substantial subsystems on unanswered assumptions is how a plan becomes thick where
the requirements are silent and thin where they are explicit. The requirements are specific
about field kinds, column layout, latency, and scale; they say nothing about history,
versioning, or conditional display.

The simplifications compound:

- **`FormInstance::set` becomes strictly O(1)** — validate one field, return one error. No
  dependency graph, no visibility recompute, no layout invalidation.
- **The renderer almost never re-renders mid-edit**, which makes the "never `cx.notify()` on
  a keystroke" rule easy to hold rather than a constant hazard.
- **No `form_version` table, no publish step, no version pinning**, and no migration story
  for rolling a form change across 100k cases — a problem that simply stops existing.
- **No audit table in either store**, no hash chain, no Queue, no R2 pipeline.

### What replaces form versioning

One invariant:

> `field.id` is global and stable. A field's `kind` is never re-typed in place.

Changing a field's kind creates a **new** field; the old field's values remain intact and
viewable. Moving, relabelling, or removing a placement never touches values. That gives the
safety property versioning was there to provide — structural edits never orphan or corrupt
collected data — without any lifecycle machinery. Enforced by `PATCH /config/fields/{id}`
returning 422 on a `kind` change ([03-API.md](../03-API.md)).

## Consequences

Each cut is additive later, at very different costs.

| Cut | Cost to reverse | Notes |
|---|---|---|
| **Audit trail** | **High** | Wants to be written in the same transaction as the value, in both the client store and the `CaseDO`. Touching every write path. History would begin the day it ships. **Decide before real data lands.** |
| **Form versioning** | Medium | Requires a version table, per-version placement, and case pinning. The stable-id invariant means no data is corrupted in the meantime, but "what did this form look like then?" is unanswerable retroactively |
| **Conditional logic** | **Low** | Reintroduces a `RuleGraph` and visibility bitset in `FormInstance`. Per-keystroke cost stops being strictly O(1) |

The audit trail is the one to watch. Clinical registries and 21 CFR Part 11 environments
commonly require it, and it is the most expensive of the three to retrofit. It was cut
because the requirements do not mention it — a defensible reading, and a reversible one, but
only cheaply so while the corpus is still synthetic.

Full statement of what this leaves undone:
[10-LIMITATIONS.md](../10-LIMITATIONS.md#5-no-audit-trail).
