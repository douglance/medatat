# ADR-0002 — Encrypted local SQLite is the UI's system of record

**Status:** Accepted · **Date:** 2026-08-17 · **Drives:** R13, R14, R15 · **Supersedes:** an earlier network-only design

## Context

R13 and R14 require form load and save under 200 ms with hundreds of fields. R15 says
"No loading spinners. Ever."

An earlier iteration put Cloudflare on the UI critical path: read a case by round-tripping
to its Durable Object. Measured expectations were ~40–90 ms warm for a US client to a
US-pinned DO, plus an unmeasured cold-start cost after the 70–140 s hibernation window.

That design answered a cache miss by rendering the form chrome with fields **read-only and
empty**, then filling them. On review this was called what it is: **a loading state wearing
a different hat.** An abstractor sees a form full of blanks they cannot type into, and may
reasonably conclude the case is empty. It also could not satisfy "Ever" — a network drop
produces a degraded banner, which is a loading state by another name.

A further problem: with 100k cases and few abstractors, essentially every case open would
hit a *cold* DO. The design's headline claim rested on a number nobody had measured.

## Decision

**The client keeps a local SQLite database as the UI's system of record.** Every
user-triggered read and write is local and synchronous. Sync to Cloudflare runs on the
background executor and is never awaited by the UI.

Encryption is **conditional**: under `--features phi` the database is SQLCipher with a
32-byte `OsRng` key. Default builds use plain SQLite, because the system currently handles
synthetic data only. See [12-PHI-READINESS.md](../12-PHI-READINESS.md).

> **Amendment, 2026-08-18.** This ADR originally placed the key in the OS keychain
> (Keychain / Credential Manager / Secret Service). It is now a hex-encoded `medatat.key`
> file with mode 0600, beside the database, at the explicit direction of the system owner.
> The keychain argument — that a key file travels with the ciphertext it protects, so a
> copied directory is a copied secret — still stands and is recorded as a known limitation;
> it did not override a direct instruction. `keyring` is not a dependency of any crate.
> The locality decision below is unaffected.

The locality decision — local store on the critical path — is independent of the encryption
decision, and is the part that matters for R13/R14/R15.

## Rationale

| Path | Latency |
|---|---|
| Local SQLite, ~500 fields, `WITHOUT ROWID` clustered | **50–300 µs** |
| Round trip to a warm Durable Object | 40–90 ms |
| Round trip to a cold Durable Object | unmeasured |

Against a 200 ms budget the local path has roughly **1000× margin** rather than ~2×. More
importantly, R15 becomes *structurally* true rather than approximated: combined with
caseload pre-sync ([04-SYNC.md](../04-SYNC.md#caseload-pre-sync)), the user only ever opens
cases that are already on disk. There is no miss to paper over.

Three risks disappear rather than being mitigated:

- DO cold-start latency stops being a UI concern.
- Network variance stops being a UI concern.
- Offline stops being a failure mode; it becomes a normal operating state.

**On the constraint this reverses.** An earlier decision — "no PHI on workstation disks" —
was taken during a self-hosted Postgres design and carried into the Cloudflare design
without being re-examined. It was the single constraint most responsible for failing the
headline requirement. Encryption at rest is the standard, defensible answer for PHI on an
endpoint, costs the latency requirement nothing, and can be switched on when it is actually
needed. (Where the key lives was revised — see the amendment above.)

## Consequences

**Gained:** R13/R14 with enormous margin; R15 structurally; full offline operation; crash
safety (a local write is synchronous and transactional, so at most the current keystroke is
lost); a durable outbox, so quitting with unsynced work loses nothing.

**Accepted:** case data now exists on endpoint disks. While the corpus is synthetic this
costs nothing. Once `phi` is on, the OS may still page plaintext to swap, so **full-disk
encryption becomes a deployment requirement** — it is the only complete answer — alongside
disabled core dumps and zeroize-on-drop.

**Accepted:** sync complexity returns — an outbox, a drain loop, conflict handling. This is
deliberately kept to delta sync with no CRDT ([04-SYNC.md](../04-SYNC.md)), because cases are
effectively single-writer in practice and the sync path is not latency-sensitive.

**Accepted:** the key file sits beside the ciphertext it protects, so a copied directory is
a copied secret, and it protects nothing from another process running as the same user. This
is the downgrade the amendment above records; it is adequate for a synthetic corpus and is a
decision to revisit before real patient data. A missing or malformed `medatat.key` reports
"locked" and never mints a replacement — doing so would render the database it guarded
permanently unreadable, which presents to the user as total data loss. Default builds are
unaffected.

*(This entry previously described a Secret Service fallback on Linux. No keyring daemon is
involved any more, so that consequence no longer applies.)*

## Validated at scale — 2026-08-18

The decision rested on a margin measured against a store holding **one case**. It has now
been measured against **500 cases × 1,000 fields = 500,000 rows**, which is what an
abstractor's assigned caseload actually looks like.

**The margin is flat.** First-touch open: 328 µs at 1 case, 276 µs at 500. Open-a-case mean
**590 µs** against a 5 ms gate and a 200 ms requirement. The query plan at 500k rows is
still `SEARCH field_value USING PRIMARY KEY`, so the `WITHOUT ROWID` clustering does what it
was chosen to do. SQLCipher costs about 1.25× on read and nothing measurable on save.

That is the claim in this ADR, tested rather than argued. See
[07-TESTING.md](../07-TESTING.md#bench-5--the-r13-margin-at-realistic-scale-measured-2026-08-18).

## Verification

Benches 1 and 2 ([07-TESTING.md](../07-TESTING.md)) assert hard thresholds at 5 ms and
10 ms — targets
set far below the 200 ms requirement so a regression trips long before it is user-visible.
`EXPLAIN QUERY PLAN` asserts the case-load query is a primary-key range scan; that test
protects the margin.
