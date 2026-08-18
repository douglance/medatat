# 04 — Sync Protocol

Owned by `medatat-sync`. Simple delta sync with a single direction of truth per field.
**No CRDT.** The justification is in [ADR-0002](adr/0002-encrypted-local-sqlite.md): because
sync is off the UI critical path and cases are effectively single-writer in practice, a
convergence engine would be complexity with no payoff.

## The invariant

> Local SQLite is the truth the UI reads. The server is the truth that survives.

Every user action commits to local SQLite **synchronously** and enqueues to `outbox`. The
sync loop drains `outbox` in the background. The UI never awaits it, never shows its state
as a blocking element, and never blocks on its failure.

## The `Transport` trait

`medatat-sync` must **not** depend on `reqwest`. That is what makes it testable against
`medatat_testkit::MockTransport`, and what lets `medatat-cli` reuse it.

```rust
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn config(&self, since_rev: ConfigRev) -> Result<Option<ConfigDelta>, TransportError>;
    async fn list_cases(&self, q: CaseQuery)     -> Result<CasePage, TransportError>;
    async fn get_values(&self, case_id: CaseId, since_rev: CaseRev)
        -> Result<ValuePage, TransportError>;
    async fn put_values(&self, case_id: CaseId, req: PutValuesReq)
        -> Result<PutValuesResp, TransportError>;
}

pub enum TransportError {
    Offline,                       // no network — expected, not an error condition
    Unauthorized,                  // token expired; triggers in-app re-auth
    RateLimited(Duration),
    Server { status: u16, message: String },
    Malformed(String),
}

impl TransportError {
    /// Whether retrying could plausibly succeed: `Offline` and `RateLimited` always,
    /// `Server` only on 5xx. `Unauthorized` and `Malformed` never — retrying either just
    /// burns the same failure again.
    pub fn is_retryable(&self) -> bool;
}
```

`TransportError::Offline` is a normal operating state, not a failure. The UI shows an
unobtrusive indicator; nothing blocks.

## Write path

```
keystroke
   └─► FormInstance::set(idx, value)        in-memory, O(1), validates one field
         └─► store::put_value(...)          local SQLite, synchronous, pending = 1
               └─► store::enqueue(...)      outbox upsert, coalescing
                     └─► (background) sync loop drains outbox
```

Both statements go in **one transaction**, so a crash cannot leave a value saved but
un-enqueued:

```rust
pub fn apply_local(
    &self, case_id: CaseId, changes: &[(FieldId, Value)], base_rev: CaseRev,
) -> Result<(), StoreError> {
    let mut conn = self.writer()?;
    let tx = conn.transaction()?;
    let now = now();
    for (field_id, value) in changes {
        values::upsert(&tx, case_id, *field_id, value, base_rev, /* pending = */ true)?;
        outbox::enqueue(&tx, case_id, *field_id, value, base_rev, &now)?;
    }
    cases::touch_local(&tx, case_id, &now)?;
    tx.commit()
}
```

`values::upsert` also writes `value_kind`, the `Value` discriminant, because the four typed
columns cannot distinguish `Value::Text` from `Value::Opt` on read
([02-DATA-MODEL.md](02-DATA-MODEL.md#store-1--client-encrypted-sqlite-sqlcipher)).
`outbox::enqueue` is an upsert that resets `attempts` to 0 and clears `last_error`, and:

**`base_rev` is deliberately not refreshed on conflict.** It records the server rev the
edit *chain* for that field started from, which is exactly what per-field conflict
detection compares against. Advancing it on each keystroke would quietly hide a genuine
same-field race — the second writer would look like it had seen the first writer's value.

**Outbox coalescing is the load-bearing detail.** The primary key is
`(case_id, field_id)`, so forty keystrokes in one field collapse to one row. Sync volume is
bounded by *fields touched*, not by keystrokes.

There is no debounce on the local write — writing to local SQLite *is* the save, and it
costs microseconds. Debouncing would only add a window in which a crash loses data.

## Drain loop

One drain loop per app instance, on the background executor — but the loop lives in the
caller, not in the engine.

`SyncEngine` owns no timer and no clock. It exposes a single pass — `drain_once` — plus
`sync_caseload`, `sync_config`, and `next_delay`; the caller decides the cadence. A test can
therefore drive an entire scenario by calling `drain_once` when it chooses, with no
simulated timeline and nothing to wait for.

```rust
pub async fn drain_once(&self) -> Result<DrainReport, SyncError> {
    let batch = self.store.next_outbox_batch(64)?;   // only rows whose backoff has elapsed
    // ... on an empty batch: report remaining, set Idle or Syncing, return.

    for (case_id, rows) in group_by_case(batch) {    // worklist order
        match self.push_case(case_id, &rows).await {
            Ok(PushOutcome::Applied(n))    => report.applied += n,
            Ok(PushOutcome::Conflicted(n)) => report.conflicted += n,
            Err(TransportError::Offline)      => { report.offline = true;    break; }
            Err(TransportError::Unauthorized) => { report.needs_auth = true; break; }
            Err(other) => {
                // Per row, not per case: each queued edit carries its own attempt count.
                for r in &rows { self.store.bump_attempts(case_id, r.field_id, &other)?; }
            }
        }
    }
    report.remaining = self.store.unsynced_count()?;
    Ok(report)
}
```

`Offline` and `Unauthorized` break the loop rather than continuing — the rest of the batch
would fail identically, and hammering it just burns battery and attempt counts.

Within one case, the batch is sent with **the lowest `base_rev` among its rows**. A stale
base can only cause a conflict, which is correct and recoverable; a base that is too new
would suppress a real one, which is a silent lost update.

`drain_once` returns a `DrainReport` (`applied`, `conflicted`, `failed`, `remaining`,
`needs_auth`, `offline`) and updates a `SyncStatus` holding `unsynced`, `conflicts`, and a
`SyncState` of `Idle | Syncing | Offline | NeedsAuth`. That status is what the peripheral
chrome renders. **It is never a spinner** (R15).

### The unsynced count must come from `unsynced_count`, not the batch

`next_outbox_batch` returns only rows that are *due* — `next_attempt_at <= now`.
`unsynced_count` returns every queued edit, including ones waiting out a backoff.

The indicator must use `unsynced_count`. Counting the due batch reports **zero while edits
are still queued**, telling the abstractor their work is safe when it is not. This was a
real bug, and it is the reason the two methods are separate rather than one with a flag.

Backoff is exponential with jitter, capped at 60 s: `min(60s, 2^attempts * 500ms) ± 20%`,
seeded per field so two fields never lock step. The jitter matters — without it, every
client that lost connectivity together retries together.

## Read path

### Config

Poll `GET /config?since_rev=<n>` every 60 s and on app focus; a `304` means nothing
changed. Config is tiny and human-paced, so the refresh is wholesale rather than a delta.

`ConfigDelta` carries two lists, and **the order they are saved in is load-bearing**:

```rust
if !delta.fields.is_empty() { store.save_fields(&delta.fields)?; }   // fields first
for form in &delta.forms   { store.save_form(form, delta.config_rev)?; }
store.set_sync_state("config_rev", &delta.config_rev.0.to_string())?;
```

`delta.fields` is **every** field that exists, including ones placed in no form. A form can
only ever describe *placed* fields, so without that list a client has no route back to a
field a coordinator has unplaced — its values sit in `field_value` with nothing referencing
them. The local `field` table is what backs the builder's "Unplaced fields" drawer and what
lets that drawer survive a restart ([06-FORM-BUILDER.md](06-FORM-BUILDER.md)). Fields go
first because a form references them.

The values themselves are never at risk either way: they are keyed by `field_id`, which is
global and never reused, so unplacing a field hides it without touching a single row.

Forms open in the UI are **not** hot-swapped mid-edit; the new definition applies on next
open.

### Caseload pre-sync

**This is the mechanism that makes R15 true.** On login, and every 5 minutes thereafter:

1. `GET /cases?assignee=me` — the full assigned list, paged 200 at a time, following
   `cursor` while `has_more`.
2. **Read the local `synced_rev` before upserting the case summary.** `upsert_case` writes
   the server's `rev` into the local row, so upserting first makes every case look current
   and nothing is ever pulled. This ordering is the difference between a working pre-sync
   and one that silently fetches nothing.
3. For each case whose server `rev` exceeds that local `synced_rev`,
   `GET /cases/{id}/values?since_rev=<local synced_rev>`.
4. Apply to local SQLite, skipping any row where `pending = 1`.

Sizing, **measured** rather than estimated (Bench 5, `crates/medatat-testkit/benches/`):
500 cases × 1000 values is **63.5 MB on disk — 130 KB per case, 133 bytes per stored
value**, including indexes and the WAL. So a caseload in the low hundreds is tens of MB and
one of a few thousand is still under a gigabyte. SQLCipher costs ~2% on top (65.1 MB for the
same corpus). The user only ever opens cases that are already local, so a cache miss is not
part of normal operation.

**Order matters.** Sync cases in worklist order, so the ones at the top of the user's screen
land first. Report progress in an unobtrusive status line — never as a modal, never as a
spinner over content.

If the assumption in [00-REQUIREMENTS.md](00-REQUIREMENTS.md#assumptions-of-record) ever
breaks — a user needing random access to any of 100k cases — this section is what must be
revisited, and R15 would need renegotiating.

### Applying server values without clobbering the user

Two rules, both non-negotiable:

1. **Never overwrite a row with `pending = 1`.** That is an unsynced local edit.
2. **Never overwrite the field the user is currently focused in.** Queue those into
   `deferred_merge` and apply on blur. Rewriting text under a cursor is the single most
   infuriating bug class in collaborative editors.

## Conflict detection

Per field, not per case. Detail in [03-API.md](03-API.md#post-casescase_idvalues).

Case-level `base_rev` equality would be far too strict: with per-keystroke local writes and
a background drain, two abstractors working *different sections* of the same case would
conflict constantly. Comparing `field_value.rev > base_rev` for only the changed fields
means a conflict signals a genuine same-field race.

### Resolution

Conflicts persist in the local `conflict` table and survive restart. The UI renders an
inline per-field strip — "keep mine / take theirs", showing both values, who wrote the
other, and when.

- **Never modal.** A conflict on field 200 must not block work on field 1.
- **Never automatic.** This is clinical data; last-write-wins silently is unacceptable.
- Choosing "keep mine" re-enqueues with the new `base_rev`. Choosing "take theirs" clears
  the local value and the conflict row.

## Failure and recovery

| Failure | Behaviour |
|---|---|
| Network down | Outbox accumulates. Status shows "N unsynced". All work continues |
| Token expired | In-app re-auth modal. **Form state stays in memory.** Drain resumes after |
| Server 5xx | Backoff; `attempts` and `last_error` recorded; surfaced only after 5 failures |
| Conflict | Recorded, rendered inline; that field's outbox row is dropped |
| Crash mid-edit | At most the current keystroke is lost — local write is synchronous and transactional |
| Local DB unopenable | Report "cannot unlock local data". **Never** silently recreate; that would appear as total data loss |

### Known gap: an edit made while its field is in flight

`Store::confirm` clears `pending` and drops the outbox rows for the fields the server
accepted. If the abstractor edited one of those fields *after* the batch was sent, the
outbox row now holds the newer value, and dropping it discards that edit — the value stays
in `field_value` with `pending` cleared, so nothing ever sends it.

The store cannot detect this: it keeps no record of what was sent. Closing it needs either a
sequence number on the outbox row that `confirm` carries, or an in-flight set held by the
sync engine that `confirm` is filtered through. **Neither exists yet**, and no test covers
the window. The exposure is narrow — it needs a keystroke inside one round trip on a field
already in flight — but it is a silent lost update, which is the one failure class this
design is otherwise built to rule out.

## App close

If `outbox` is non-empty, closing shows a modal offering **Retry now** or
**Quit anyway** (the latter is safe — the outbox is durable on disk and drains on next
launch). Unlike the previous network-only design, quitting with pending work loses nothing.

## Testing

`medatat_testkit::MockTransport` covers, in `medatat-sync` (12 tests):

- A local edit reaches the server, and an empty outbox makes no network call at all.
- Outbox coalescing: repeated `set` calls on one field produce one round trip.
- Disjoint-field concurrency: two clients editing different fields both apply.
- A same-field race produces exactly one conflict row, and does not spin the drain loop.
- `pending = 1` rows are never clobbered by an inbound server value.
- Offline → queued → reconnect → drained, with no lost or duplicated writes.
- A transient failure is retried rather than dropped.
- Caseload sync pulls assigned cases and their values.
- Config sync stores forms, records the revision, and persists **unplaced** fields.

Backoff is covered separately, as a pure function in `backoff.rs`: exponential growth to the
60 s cap, never zero, deterministic per seed, and jitter that actually spreads clients apart.
`SyncEngine` holds no clock — `delay_for` is pure and the caller owns the cadence — so these
are unit tests over an input, not a simulated timeline. `medatat-testkit` does provide a
`FakeClock`, but nothing in `medatat-sync` currently needs one.

A crash between the value write and the outbox insert is impossible by construction — one
transaction — and is asserted in `medatat-store` rather than here.

**Not covered:** the in-flight re-enqueue window described under *Known gap* above.
