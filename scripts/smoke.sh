#!/usr/bin/env bash
# End-to-end smoke test. Run against `wrangler dev` in CI and against the
# deployed Worker after deploy. See docs/07-TESTING.md.
#
#   ./scripts/smoke.sh http://localhost:8787
#   SMOKE_CODE=418902 ./scripts/smoke.sh http://localhost:8787
#
# Requires: medatat (from crates/medatat-cli), jq.
set -euo pipefail

API="${1:-http://localhost:8787}"
EMAIL="${SMOKE_EMAIL:-smoke@cetify.email}"
export MEDATAT_API="$API"

# Every authenticated call below passes its own Authorization header. An inherited
# MEDATAT_TOKEN would also be applied to the unauthenticated check at the bottom, making
# it pass for the wrong reason, so drop it.
unset MEDATAT_TOKEN

pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1" >&2; exit 1; }
note() { printf '  --   %s\n' "$1"; }

# `medatat api` is an incurs fetch gateway. Two of its behaviours have to be normalised
# before jq sees anything, and both of them silently broke every negative-path assertion
# in the first version of this script:
#
#   1. It exits 1 on any non-2xx. Under `set -e` with `pipefail`, a pipeline whose first
#      stage is a deliberate 401/409/422 aborts the script — or, with `|| fail`, reports
#      a failure for a response that was exactly what the test wanted.
#   2. On a non-2xx it re-wraps the body as `{ok, status, error: <the Worker envelope>}`,
#      so the Worker's own `{ok, error: {code}}` sits one level deeper than on success and
#      `.error.code` reads `null`.
#
# `api` unwraps both, so every assertion below reads the Worker's envelope directly and
# `.error.code` means the same thing on every status.
api() {
  local out
  out="$(medatat api "$@" 2>/dev/null)" || true
  [ -n "$out" ] || { printf '{}'; return 0; }
  jq -c 'if type == "object" and has("status") and has("error") then .error else . end' \
     <<<"$out" 2>/dev/null || printf '{}'
}

# The HTTP status, via `--verbose` — the only incurs mode that exposes it. Needed because
# a 204 has no body to assert on, and "always 204" is the whole point of /auth/request.
api_status() {
  local out
  out="$(medatat api --verbose "$@" 2>/dev/null)" || true
  jq -r '.meta.status // 0' <<<"$out" 2>/dev/null || echo 0
}

echo "smoke: $API"

# --- health ---------------------------------------------------------------
api health | jq -e '.ok == true' >/dev/null || fail "health"
pass "health"

# --- auth: unknown email must still return 204 (no account enumeration) ----
UNKNOWN_STATUS=$(api_status auth request -X POST \
  -d '{"email":"nobody-'"$RANDOM"'@example.invalid"}')
[ "$UNKNOWN_STATUS" = "204" ] \
  || fail "auth/request returned $UNKNOWN_STATUS for an unknown email, not 204"
pass "auth/request does not enumerate accounts"

# --- auth: full magic-code round trip -------------------------------------
KNOWN_STATUS=$(api_status auth request -X POST -d "{\"email\":\"$EMAIL\"}")
[ "$KNOWN_STATUS" = "204" ] \
  || fail "auth/request returned $KNOWN_STATUS for a known email, not 204"
pass "auth/request accepts a known email"

# The code is delivered by email and only its sha256 is ever stored, so there is no way to
# read it back out of the Worker or out of KV. It has to be supplied.
if [ -z "${SMOKE_CODE:-}" ]; then
  note "a sign-in code has been emailed to $EMAIL"
  note "re-run with it to exercise the authenticated half:"
  note "    SMOKE_CODE=<code> $0 $API"
  echo "smoke: unauthenticated checks passed; authenticated checks skipped"
  exit 0
fi
CODE="$SMOKE_CODE"

TOK=$(api auth verify -X POST -d "{\"email\":\"$EMAIL\",\"code\":\"$CODE\"}" \
        | jq -r '.data.token // empty')
[ -n "$TOK" ] || fail "auth/verify returned no token"
AUTH="Authorization: Bearer $TOK"
pass "auth/verify"

# codes are single-use: the second attempt with the same code must be refused
api auth verify -X POST -d "{\"email\":\"$EMAIL\",\"code\":\"$CODE\"}" \
  | jq -e '.ok == false and .error.code == "unauthorized"' >/dev/null \
  || fail "code was reusable"
pass "codes are single-use"

# --- config ---------------------------------------------------------------
# One fetch, then read both values out of it.
CONFIG=$(api config -H "$AUTH")

FORM=$(jq -r '.data.forms[0].form_id // empty' <<<"$CONFIG")
[ -n "$FORM" ] || fail "no forms configured — seed one first"

# `sections[].fields[]` are *placements* (`SectionField`), so the field definition is
# nested under `.field`, and `FieldKind` is internally tagged — the discriminant is
# `.field.kind.kind`, not `.kind`. See the GET /config body in docs/03-API.md.
TIME_FIELD=$(jq -r \
  '[.data.forms[0].sections[].fields[] | select(.field.kind.kind == "time")][0].field.field_id // empty' \
  <<<"$CONFIG")
[ -n "$TIME_FIELD" ] || fail "no time field in form (R8)"
pass "config exposes a time field"

# --- case create ----------------------------------------------------------
CASE=$(api cases -X POST -H "$AUTH" \
        -d "{\"mrn\":\"SMOKE-$RANDOM\",\"form_id\":\"$FORM\"}" | jq -r '.data.case_id // empty')
[ -n "$CASE" ] || fail "case create"
pass "case create"

# --- write a 24hr time value (R8) -----------------------------------------
api cases "$CASE" values -X POST -H "$AUTH" \
  -d "{\"base_rev\":0,\"changes\":[{\"field_id\":\"$TIME_FIELD\",\"value\":{\"Time\":\"09:30\"}}]}" \
  | jq -e '.data.rev == 1' >/dev/null || fail "value write"
pass "value write"

# --- read back ------------------------------------------------------------
# `Value::Time` serialises as HH:MM, so this is the exact string that was written.
api cases "$CASE" values -H "$AUTH" \
  | jq -e '.data.values[] | select(.value.Time == "09:30")' >/dev/null \
  || fail "value round-trip"
pass "value round-trip"

# --- the server refuses an invalid 24hr time (R8) -------------------------
# 24:00 is refused while the body is being deserialised — `Value::Time` cannot hold it —
# rather than by `validate`. Either way the write never lands and the answer is a 422.
api cases "$CASE" values -X POST -H "$AUTH" \
  -d "{\"base_rev\":1,\"changes\":[{\"field_id\":\"$TIME_FIELD\",\"value\":{\"Time\":\"24:00\"}}]}" \
  | jq -e '.ok == false and .error.code == "validation"' >/dev/null \
  || fail "server accepted 24:00"
pass "server rejects 24:00"

# --- conflict: stale base_rev on the same field must 409 ------------------
api cases "$CASE" values -X POST -H "$AUTH" \
  -d "{\"base_rev\":0,\"changes\":[{\"field_id\":\"$TIME_FIELD\",\"value\":{\"Time\":\"10:00\"}}]}" \
  | jq -e '.ok == false and .error.code == "conflict"' >/dev/null \
  || fail "stale write was not rejected"
pass "per-field conflict detection"

# --- auth is enforced -----------------------------------------------------
api cases "$CASE" values | jq -e '.error.code == "unauthorized"' >/dev/null \
  || fail "unauthenticated read was permitted"
pass "auth enforced"

echo "smoke: all checks passed"
