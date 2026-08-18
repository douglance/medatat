# 03 — API Contract

Base URL: `https://medatat.<subdomain>.workers.dev` (dev: `http://localhost:8787`).

All request and response bodies are JSON. All types are defined in
`medatat_core::wire` and shared verbatim by client, Worker, and CLI — there is no
hand-written duplicate schema anywhere.

## Conventions

- **Auth:** `Authorization: Bearer <token>` on everything except `/auth/*` and `/health`.
- **Envelope:** every response is `{ "ok": bool, "data": T, "error": E }`, where the unused
  half is **omitted entirely** rather than sent as `null` — a success body has no `error`
  key at all, and a failure body has no `data` key. Branch on `ok`, not on key presence.
- **Errors:** `{"ok": false, "error": {"code": "...", "message": "...", "detail": {...}}}`.
- **Time:** all timestamps are RFC-3339 UTC strings.
- **Ids:** UUIDs as lowercase hyphenated strings.
- **`actor_id` is never accepted from the client.** It is resolved from the bearer token.

### Error codes

| Code | HTTP | Meaning |
|---|---|---|
| `unauthorized` | 401 | Missing, expired, or invalid token |
| `forbidden` | 403 | Authenticated but not permitted (e.g. non-admin editing forms) |
| `not_found` | 404 | Unknown case, form, or field |
| `conflict` | 409 | Per-field revision conflict; `detail` carries the server values |
| `validation` | 422 | Value failed `medatat_core::validate` |
| `rate_limited` | 429 | Too many auth attempts |
| `internal` | 500 | Unexpected |

---

## Auth (R1)

Magic-code login. No passwords anywhere, so there is no password KDF and no argon2
CPU-budget hazard inside the Worker.

### `POST /auth/request`

```json
{ "email": "abstractor@example.com" }
```

**Always returns `204 No Content`**, whether or not the account exists. This is deliberate:
it prevents account enumeration. Behaviour:

1. Look up `app_user` by email. If absent or inactive, return 204 and do nothing else.
2. Generate a 6-digit code from `OsRng` (uniform over `000000..=999999`).
3. Store in KV at key `auth:{lowercased_email}`:
   `{"code_hash": "<sha256 hex>", "attempts": 0, "issued_at": "..."}` with
   `expirationTtl = 600`.
4. Send via the `send_email` binding from `noreply@cetify.email`.

The email body carries **the code and nothing else** — no case identifiers, no patient
data, no PHI of any kind.

Rate limits: 3 requests per email per 15 min; 20 per IP per 15 min. Exceeding either
returns `429` (this is the one case where the endpoint does not return 204, and it leaks
nothing because it is IP- and email-scoped, not existence-scoped).

### `POST /auth/verify`

```json
{ "email": "abstractor@example.com", "code": "418902" }
```

→ `200 { "ok": true, "data": { "token": "<43-char base64url>", "user": {...},
   "expires_at": "..." } }`

1. Read `auth:{email}` from KV. Absent → `401`.
2. Increment `attempts`. If it reaches 5, delete the key and return `401`.
3. Compare `sha256(code)` against the stored hash in **constant time**.
4. On success, delete the KV key (codes are single-use), mint 32 bytes from `OsRng`,
   base64url-encode, store `session:{sha256(token)}` →
   `{"user_id": "...", "issued_at": "...", "expires_at": "..."}` with
   `expirationTtl = 43200` (12 h).

The raw token is returned exactly once. Only `sha256(token)` is ever stored. `sha256` is
correct here rather than a slow KDF because the token is already 256 bits of entropy.

### `POST /auth/logout`

→ `204`. Deletes the session key.

### `GET /auth/me`

→ `200 { "ok": true, "data": { "user_id", "email", "display_name", "role" } }`

`last_seen` is updated **at most once per minute** — KV allows a maximum of 1 write/sec
per key, and a per-request write would exceed it.

---

## Config (R3, R4)

`GET` endpoints are open to any authenticated user. All mutations require `role = admin`.
Every mutation bumps `config_version.rev` in the same D1 batch.

### `GET /config?since_rev=<n>`

→ the full configuration if `since_rev` is absent or stale; `304` if the client is current.

```json
{ "ok": true, "data": {
    "config_rev": 41,
    "forms": [{
      "form_id": "…", "name": "Intake",
      "sections": [{
        "section_id": "…", "title": "Demographics", "ordinal": 0,
        "columns": 2, "default_collapsed": false,
        "fields": [{
          "field": {
            "field_id": "…", "key": "dob",
            "kind": { "kind": "date" }
          },
          "label": "Date of birth", "ordinal": 0, "col_span": 1, "required": true
        }]
      }]
    }]
} }
```

The shape is `medatat_core::wire::ConfigDelta` serialised directly — this body is the
`FormDef` tree, not a separate DTO, which is what keeps client and Worker from drifting.
Three consequences are easy to get wrong when writing a client:

- **`sections[].fields[]` are *placements*, not fields.** Each entry is a `SectionField`,
  so the definition is nested under `.field` and the id is `.field.field_id`.
- **`kind` is an internally-tagged enum**, so the discriminant is `.field.kind.kind`, and
  the kind-specific configuration is flattened alongside it rather than sitting in a
  separate `config` object. A numeric field reads
  `"kind": {"kind":"numeric","min":"0","max":"300","scale":2}`, and a select reads
  `"kind": {"kind":"select","options":[{"code":"M","label":"Male","ordinal":0}],"searchable":true}`.
- **A section's heading is `title`**, not `name`. `name` is what the *request* bodies below
  use, because that is the D1 column; the response carries `title` because that is the
  `SectionDef` field. They are the same string.

`kind` is one of `text | numeric | date | time | radio | select | textarea` (R5–R11).
The kind-specific keys are flattened into the `kind` object on the wire, and stored in
D1's `field.config` column as the JSON below:

| kind | keys alongside `"kind"` | `field.config` column |
|---|---|---|
| `text` | `max_len` | `{ "max_len": 255 }` |
| `textarea` | `rows`, `max_len` | `{ "rows": 4, "max_len": 4000 }` |
| `numeric` | `min`, `max`, `scale` | `{ "min": "0", "max": "300", "scale": 2 }` — min/max are decimal **strings** |
| `date`, `time` | none | `{}` |
| `radio` | `options` | `{}` — choices live in the `field_option` table |
| `select` | `options`, `searchable` | `{ "searchable": true }` |

Config is small enough that partial sync is not worth the complexity. The client stores
`config_rev` and re-fetches wholesale when it changes.

### Form, section, field mutations

| Method | Path | Body | OK | Notes |
|---|---|---|---|---|
| `POST` | `/config/forms` | `{key, name}` | `201 {form_id}` | |
| `PATCH` | `/config/forms/{form_id}` | `{name?, archived?}` | `204` | |
| `POST` | `/config/forms/{form_id}/sections` | `{name, ordinal, columns}` | `201 {section_id}` | `columns` ∈ 1..3 (R12) |
| `PATCH` | `/config/sections/{section_id}` | `{name?, ordinal?, columns?}` | `204` | **Reducing `columns` clamps every child `col_span`** in the same transaction |
| `DELETE` | `/config/sections/{section_id}` | | `204` | Cascades placements. **Never deletes values** |
| `POST` | `/config/fields` | `{key, kind, …kind keys}` | `201 {field_id}` | The kind keys are **flattened**, not nested under `config` — see below |
| `PATCH` | `/config/fields/{field_id}` | `{config?, options?}` | `204` | **`kind` is not patchable** — see below. Here `config` *is* a nested object; its keys are merged over the stored ones |
| `POST` | `/config/sections/{section_id}/fields` | `{field_id, ordinal, col_span, label, required}` | `204` | `col_span <= section.columns`, else `422` |
| `PATCH` | `/config/sections/{sid}/fields/{fid}` | `{ordinal?, col_span?, label?, required?}` | `204` | An absent key keeps its stored value; `col_span` is re-validated even when the body does not mention it |
| `DELETE` | `/config/sections/{sid}/fields/{fid}` | | `204` | Removes the placement only. **Values survive** |

`POST /config/fields` takes a flattened `CreateFieldReq`, so the kind keys sit beside
`kind` rather than inside a `config` object — the same shape the `GET /config` response
uses:

```json
{ "key": "dose", "kind": "numeric", "min": "0", "max": "300", "scale": 2 }
{ "key": "sex",  "kind": "radio",   "options": [{"code":"M","label":"Male","ordinal":0}] }
```

`PATCH /config/fields/{field_id}` is the one place `config` *is* a nested object, because
it is a partial merge over what is stored rather than a whole definition.

**`kind` is immutable.** Attempting to change it returns `422` with
`{"code":"validation","message":"field kind is immutable; create a replacement field"}`.
The builder surfaces this as a "Replace field" action that creates a new `field_id`,
places it, and leaves the old field's values intact and viewable. This one rule is what
lets the entire form-version lifecycle stay cut.

---

## Cases and values (R2, R16)

### `GET /cases?assignee=me&since=<cursor>&limit=<n>`

Reads `case_index` in D1. Eventually consistent — see
[10-LIMITATIONS.md](10-LIMITATIONS.md#2-the-d1-case-index-is-eventually-consistent).

```json
{ "ok": true, "data": {
    "cases": [{ "case_id": "…", "mrn": "…", "form_id": "…", "rev": 12,
                "assignee": "…", "updated_at": "…" }],
    "cursor": "…", "has_more": false } }
```

### `POST /cases`

```json
{ "mrn": "MRN-00042", "form_id": "…", "assignee": "…" }
```

→ `201 { "ok": true, "data": { "case_id": "…", "rev": 0 } }`

Inserts the `case_index` row, then calls the `CaseDO` to record `mrn`, `form_id`,
`assignee`, and `created_at` in its `meta` table. That first `meta` write is what creates
the DO's schema — no DDL runs in the object's constructor, which is on every cold-start
path. The case starts at `rev` 0; the first *value* write takes it to 1.

### `GET /cases/{case_id}/values?since_rev=<n>`

Routes to the `CaseDO`. Returns every value with `rev > since_rev`; omit `since_rev` for
a full read.

```json
{ "ok": true, "data": {
    "case_id": "…", "rev": 12,
    "values": [
      { "field_id": "…", "value": { "Time": "09:30" },   "rev": 12 },
      { "field_id": "…", "value": { "Num": "12.50" },    "rev": 7  },
      { "field_id": "…", "value": { "Text": "…" },       "rev": 3  }
    ] } }
```

`Value` serialises as an externally-tagged enum: `"Null"`, `{"Text": "…"}`,
`{"Num": "12.50"}` (decimal **string**), `{"Date": "2026-08-17"}`, `{"Time": "09:30"}`,
`{"Opt": "code"}`.

### `POST /cases/{case_id}/values`

The write path. Batched: the client sends everything the outbox has for this case.

```json
{ "base_rev": 12,
  "changes": [ { "field_id": "…", "value": { "Time": "09:30" } },
               { "field_id": "…", "value": "Null" } ] }
```

**Success** → `200 { "ok": true, "data": { "rev": 13, "applied": ["<field_id>", …] } }`

**Conflict** → `409`:

```json
{ "ok": false, "error": { "code": "conflict", "message": "…",
  "detail": { "server_rev": 15,
    "conflicts": [ { "field_id": "…", "value": {"Text":"…"},
                     "updated_by": "…", "updated_at": "…" } ] } } }
```

Conflict detection is **per field, not per case**:

```sql
SELECT field_id, value_text, value_numeric, value_date, value_time, updated_by, updated_at
  FROM field_value
 WHERE field_id IN (<the changed fields>) AND rev > :base_rev;
```

If that returns rows, reject **the whole batch** and return them. Otherwise apply all
changes, bump `meta.rev`, and write back. Two abstractors editing *different* fields of the
same case both succeed; only a genuine same-field race conflicts. Rationale in
[04-SYNC.md](04-SYNC.md#conflict-detection).

Every value is re-validated server-side with `medatat_core::validate` before it is written.
A failure returns `422` with the offending `field_id`. The client validates too, for feel;
the server validates for truth. Same function, so they cannot drift.

After a successful write the DO updates its `case_index` row in D1. **This write may fail
independently** — that is the eventual-consistency seam.

### `GET /health`

→ `200 { "ok": true, "data": { "version": "…", "config_rev": 41 } }`. No auth.

---

## Bulk endpoints (seeding and export)

Admin-only. Used by `medatat-cli` for the perf corpus and by the export path.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/bulk/cases` | Create up to 100 cases in one request. Body `{cases: [{mrn, form_id, assignee?}]}` → `201 {created: [case_id, …]}` |
| `GET` | `/bulk/export?form_id=…&cursor=…&limit=…` | Stream the worklist index as NDJSON, one `CaseSummary` per line, `Content-Type: application/x-ndjson`. See [10-LIMITATIONS.md](10-LIMITATIONS.md#1-cross-case-reporting-is-not-built) |
| `POST` | `/admin/reindex` | **Not implemented.** Returns `404` |

Two of these do less than their names suggest, and a client should not assume otherwise:

- **`/bulk/cases` carries no values.** It creates `case_index` rows and nothing else — it
  does not fan out to the Durable Objects, so the cases it creates are empty until
  something writes to them through `POST /cases/{case_id}/values`. Seeding a corpus with
  values is therefore still a per-case round trip.
- **`/bulk/export` exports the index, not the data.** Every line is a `CaseSummary`
  — `case_id`, `mrn`, `form_id`, `assignee`, `rev`, `updated_at` — because the values live
  in the DOs and are not reachable from a D1 scan. It is a manifest, not a data export.
- **`/admin/reindex` answers `404` with an explanation.** Walking 100k Durable Objects
  needs a Queue or an Alarm binding to survive the Worker CPU budget, and neither is bound
  in `wrangler.jsonc` yet. Returning a job id for work that never started would be worse
  than saying so.

---

## Wire types

```rust
// medatat-core::wire — shared by client, Worker, and CLI. Never duplicated.
#[derive(Serialize, Deserialize)]
pub struct Envelope<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

#[derive(Serialize, Deserialize)]
pub struct ApiError { pub code: ErrorCode, pub message: String, pub detail: Option<JsonValue> }

#[derive(Serialize, Deserialize)]
pub struct ValueRow {
    pub field_id: FieldId,
    pub value: Value,
    pub rev: CaseRev,
    // Present on a conflict response; omitted on a plain read.
    pub updated_by: Option<ActorId>,
    pub updated_at: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PutValuesReq { pub base_rev: CaseRev, pub changes: Vec<ValueChange> }

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum PutValuesResp {
    Applied  { rev: CaseRev, applied: Vec<FieldId> },
    Conflict { server_rev: CaseRev, conflicts: Vec<ValueRow> },
}
```
