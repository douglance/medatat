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
32-byte `OsRng` key held in the OS keychain (Keychain / Credential Manager / Secret
Service). Default builds use plain SQLite, because the system currently handles synthetic
data only. See [12-PHI-READINESS.md](../12-PHI-READINESS.md).

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
headline requirement. Encryption at rest with an OS-keychain-held key is the standard,
defensible answer for PHI on an endpoint, costs the latency requirement nothing, and can be
switched on when it is actually needed.

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

**Accepted:** under `phi` on Linux without a Secret Service daemon, the app falls back to an
in-memory database and re-syncs each launch. It must never silently write plaintext when the
caller asked for encryption. Default builds are unaffected.

## Verification

Benches 1–3 ([07-TESTING.md](../07-TESTING.md)) gate CI at 5 ms, 10 ms, and 50 ms — targets
set far below the 200 ms requirement so a regression trips long before it is user-visible.
`EXPLAIN QUERY PLAN` asserts the case-load query is a primary-key range scan; that test
protects the margin.
