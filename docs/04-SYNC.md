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
`MockTransport` with a `FakeClock`, and what lets `medatat-cli` reuse it.

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

One loop per app instance, on the background executor.

`SyncEngine` owns no timer. It exposes one pass — `drain_once` — plus `sync_caseload`,
`sync_config`, and `next_delay`; the caller decides the cadence. That is what lets the whole
engine be driven by a `FakeClock` in tests.

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

Poll `GET /config?since_rev=<n>` every 60 s and on app focus. Config is tiny and
human-paced. On change, replace the local `form` rows wholesale and rebuild the in-memory
`FormRegistry`. Forms open in the UI are **not** hot-swapped mid-edit; the new definition
applies on next open.

### Caseload pre-sync

**This is the mechanism that makes R15 true.** On login, and every 5 minutes thereafter:

1. `GET /cases?assignee=me` — the full assigned list, paged.
2. For each case not present locally, or whose server `rev` exceeds local `synced_rev`,
   `GET /cases/{id}/values?since_rev=<local synced_rev>`.
3. Apply to local SQLite, skipping any row where `pending = 1`.

Sizing: a realistic caseload of a few hundred cases × ~1000 values × ~60 bytes ≈
**tens of MB**. The user only ever opens cases that are already local, so a cache miss is
not part of normal operation.

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

## App close

If `outbox` is non-empty, closing shows a modal offering **Retry now** or
**Quit anyway** (the latter is safe — the outbox is durable on disk and drains on next
launch). Unlike the previous network-only design, quitting with pending work loses nothing.

## Testing

`MockTransport` + `FakeClock` cover, in `medatat-sync`:

- Outbox coalescing: 40 `set` calls on one field produce one outbox row.
- Disjoint-field concurrency: two clients editing different fields both apply.
- Same-field race produces exactly one conflict row, and the loser's outbox row is dropped.
- `pending = 1` rows are never clobbered by an inbound server value.
- Backoff schedule matches expectation across 10 simulated failures.
- Offline → queued → reconnect → drained, with no lost or duplicated writes.
- A crash between value write and outbox insert is impossible (single transaction).
