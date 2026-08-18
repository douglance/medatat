#!/usr/bin/env bash
# End-to-end smoke test. Run against `wrangler dev` in CI and against the
# deployed Worker after deploy. See docs/07-TESTING.md.
#
#   ./scripts/smoke.sh http://localhost:8787
#
# Requires: medatat (from crates/medatat-cli), jq.
set -euo pipefail

API="${1:-http://localhost:8787}"
EMAIL="${SMOKE_EMAIL:-smoke@cetify.email}"
export MEDATAT_API="$API"

pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1" >&2; exit 1; }

echo "smoke: $API"

# --- health ---------------------------------------------------------------
medatat api health | jq -e '.ok == true' >/dev/null || fail "health"
pass "health"

# --- auth: unknown email must still return 204 (no account enumeration) ----
medatat api auth request -X POST -d '{"email":"nobody-'"$RANDOM"'@example.invalid"}' \
  >/dev/null 2>&1 || fail "auth/request leaked a non-204 for an unknown email"
pass "auth/request does not enumerate accounts"

# --- auth: full magic-code round trip -------------------------------------
# In dev the Worker echoes the code when DEV_ECHO_CODE=1; in prod, supply SMOKE_CODE.
medatat api auth request -X POST -d "{\"email\":\"$EMAIL\"}" >/dev/null
CODE="${SMOKE_CODE:-$(medatat dev last-code --email "$EMAIL")}"
TOK=$(medatat api auth verify -X POST -d "{\"email\":\"$EMAIL\",\"code\":\"$CODE\"}" \
        | jq -r '.data.token')
[ -n "$TOK" ] && [ "$TOK" != "null" ] || fail "auth/verify returned no token"
AUTH="Authorization: Bearer $TOK"
pass "auth/verify"

# codes are single-use
medatat api auth verify -X POST -d "{\"email\":\"$EMAIL\",\"code\":\"$CODE\"}" \
  | jq -e '.ok == false' >/dev/null || fail "code was reusable"
pass "codes are single-use"

# --- config ---------------------------------------------------------------
FORM=$(medatat api config -H "$AUTH" | jq -r '.data.forms[0].form_id')
[ "$FORM" != "null" ] || fail "no forms configured — seed one first"

TIME_FIELD=$(medatat api config -H "$AUTH" \
  | jq -r '[.data.forms[0].sections[].fields[] | select(.kind=="time")][0].field_id')
[ "$TIME_FIELD" != "null" ] || fail "no time field in form (R8)"
pass "config exposes a time field"

# --- case create ----------------------------------------------------------
CASE=$(medatat api cases -X POST -H "$AUTH" \
        -d "{\"mrn\":\"SMOKE-$RANDOM\",\"form_id\":\"$FORM\"}" | jq -r '.data.case_id')
[ "$CASE" != "null" ] || fail "case create"
pass "case create"

# --- write a 24hr time value (R8) -----------------------------------------
medatat api cases "$CASE" values -X POST -H "$AUTH" \
  -d "{\"base_rev\":0,\"changes\":[{\"field_id\":\"$TIME_FIELD\",\"value\":{\"Time\":\"09:30\"}}]}" \
  | jq -e '.data.rev == 1' >/dev/null || fail "value write"
pass "value write"

# --- read back ------------------------------------------------------------
medatat api cases "$CASE" values -H "$AUTH" \
  | jq -e '.data.values[] | select(.value.Time == "09:30")' >/dev/null \
  || fail "value round-trip"
pass "value round-trip"

# --- server-side validation rejects an invalid 24hr time (R8) -------------
medatat api cases "$CASE" values -X POST -H "$AUTH" \
  -d "{\"base_rev\":1,\"changes\":[{\"field_id\":\"$TIME_FIELD\",\"value\":{\"Time\":\"24:00\"}}]}" \
  | jq -e '.ok == false and .error.code == "validation"' >/dev/null \
  || fail "server accepted 24:00"
pass "server rejects 24:00"

# --- conflict: stale base_rev on the same field must 409 ------------------
medatat api cases "$CASE" values -X POST -H "$AUTH" \
  -d "{\"base_rev\":0,\"changes\":[{\"field_id\":\"$TIME_FIELD\",\"value\":{\"Time\":\"10:00\"}}]}" \
  | jq -e '.ok == false and .error.code == "conflict"' >/dev/null \
  || fail "stale write was not rejected"
pass "per-field conflict detection"

# --- auth is enforced -----------------------------------------------------
medatat api cases "$CASE" values | jq -e '.error.code == "unauthorized"' >/dev/null \
  || fail "unauthenticated read was permitted"
pass "auth enforced"

echo "smoke: all checks passed"
