# 09 — Setup

## Prerequisites

| Tool | Version | Check |
|---|---|---|
| Rust | stable, edition 2024 | `rustc --version` |
| wrangler | **≥ 4.123.0** | `wrangler --version` |
| Node | ≥ 20 (for wrangler) | `node --version` |
| worker-build | latest | `cargo install worker-build` |
| jq | any | `jq --version` |

**wrangler is currently 4.60.0 on this machine and must be upgraded** — Email Sending
subcommands do not exist before 4.123.0 (`wrangler email sending list` errors with
"Unknown arguments").

```bash
pnpm add -g wrangler@latest
rustup target add wasm32-unknown-unknown
cargo install worker-build
```

### Platform build dependencies

**Linux** — GPUI is Vulkan-backed and will not start without an ICD:

```bash
sudo apt install -y libxkbcommon-dev libwayland-dev libxcb1-dev libssl-dev \
                    libasound2-dev libfontconfig1-dev mesa-vulkan-drivers
# CI / headless / VM fallback:
sudo apt install -y mesa-vulkan-drivers   # provides lavapipe (software Vulkan)
```

**Windows** — the Zed checkout has deep paths:

```powershell
git config --global core.longpaths true
```

**macOS** — Xcode command line tools only.

### Cargo git cache

Cargo clones the entire `zed-industries/zed` history (~1 GB) for the `gpui` dependency.

```bash
export CARGO_NET_GIT_FETCH_WITH_CLI=true
```

Set this in CI too, and cache `~/.cargo/git` aggressively or every cold job pays minutes of
clone time.

---

## Cloudflare account

| | |
|---|---|
| Account | `Doug.lance@gmail.com's Account` |
| Account ID | `88f9cbf5c4f4e217079bcbf0ca6cb181` |
| Auth | OAuth, already carries `d1`, `workers`, `workers_kv`, `email_sending (write)` |

```bash
wrangler whoami   # verify before starting
```

> **Synthetic data only.** This system does not handle PHI today, and PHI protections are
> behind the `phi` cargo feature (off by default). All development and benchmarking uses
> `medatat-testkit`. Before real patient data: [12-PHI-READINESS.md](12-PHI-READINESS.md) —
> the long-lead item is an Enterprise BAA with Cloudflare, so start it early.

### Domain — `cetify.email`

**This domain currently returns NXDOMAIN.** `dig cetify.email` finds no NS delegation and
whois falls through to the `.email` TLD registry. Before M1:

1. Confirm the domain is registered.
2. Add it to Cloudflare DNS and delegate the nameservers at the registrar.
3. Onboard it to Email Sending:

```bash
wrangler email sending enable cetify.email
wrangler email sending list          # confirm it appears
```

Onboarding auto-provisions MX and SPF on `cf-bounce.cetify.email`, DKIM at
`cf-bounce._domainkey`, and a `_dmarc` record. Verify:

```bash
dig +short TXT cf-bounce._domainkey.cetify.email
dig +short TXT _dmarc.cetify.email
```

Email Sending is **Beta**: 3,000 messages/month included, then $0.35/1,000; the daily quota
starts conservative and scales with sender reputation. Keep the sender behind
`trait Mailer` so Postmark or SES can be substituted without touching the auth flow.

### Provision resources

```bash
wrangler d1 create medatat
wrangler kv namespace create MEDATAT_AUTH
# record the returned ids in wrangler.jsonc
```

### `wrangler.jsonc`

```jsonc
{
  "name": "medatat",
  "main": "build/worker/shim.mjs",
  "compatibility_date": "2026-08-01",
  // --profile release-wasm, not --release: the release profile's `strip` breaks
  // the wasm-bindgen bundle step. See Troubleshooting.
  "build": { "command": "cargo install -q worker-build && worker-build --profile release-wasm" },

  "d1_databases": [
    { "binding": "DB", "database_name": "medatat", "database_id": "<from d1 create>" }
  ],
  "kv_namespaces": [
    { "binding": "AUTH", "id": "<from kv namespace create>" }
  ],
  "send_email": [
    { "name": "EMAIL" }
  ],
  "durable_objects": {
    "bindings": [{ "name": "CASE", "class_name": "CaseDO" }]
  },
  "migrations": [
    { "tag": "v1", "new_sqlite_classes": ["CaseDO"] }
  ],
  "observability": { "enabled": true }
}
```

`new_sqlite_classes` — **not** `new_classes`. SQLite-backed Durable Objects require it, and
the distinction is not recoverable after the fact without a new class name.

### Apply D1 migrations

```bash
wrangler d1 migrations apply medatat --local     # dev
wrangler d1 migrations apply medatat --remote    # deployed
```

---

## Workspace bootstrap

```
medatat/
├── Cargo.toml              # workspace
├── rust-toolchain.toml
├── wrangler.jsonc
├── docs/
├── scripts/smoke.sh
├── xtask/
└── crates/
    ├── medatat-core/
    ├── medatat-store/
    ├── medatat-sync/
    ├── medatat-ui/
    ├── medatat-worker/
    ├── medatat-cli/
    └── medatat-testkit/
```

`Cargo.toml` and `rust-toolchain.toml` are committed at the repo root — see those files for
the pinned dependency set.

---

## Running

```bash
# Worker, locally, with real DO SQLite and D1 in workerd
wrangler dev

# Desktop app against local Worker
MEDATAT_API=http://localhost:8787 cargo run -p medatat-ui

# Desktop app against deployed Worker
MEDATAT_API=https://medatat.<subdomain>.workers.dev cargo run -p medatat-ui

# Tests
cargo test --workspace
cargo test --workspace --features phi     # keeps the PHI path from rotting
cargo bench -p medatat-testkit            # Benches 1-2 assert their own thresholds

# Worker bundle — the real gate. A green wasm32 compile does NOT imply this passes.
cd crates/medatat-worker && worker-build --profile release-wasm
cargo xtask lint-no-spinner

# Smoke
./scripts/smoke.sh http://localhost:8787
```

### Linux headless / VM

```bash
# If the app fails with "no Vulkan device", force software rendering:
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json cargo run -p medatat-ui
```

---

## The `medatat` CLI

`medatat-cli` is built on `incurs` (Rust crate, v0.5.3, at
`/Users/douglance/Developer/lv/incurs`). It is a **CLI framework**, not a test runner — it
provides a curl-style fetch gateway; assertions come from `jq` or `cargo test`.

```bash
cargo install --path /Users/douglance/Developer/lv/incurs/crates/incurs-cli  # not prebuilt
cargo install --path crates/medatat-cli
```

`medatat-cli` implements `incurs::fetch::FetchHandler` over `reqwest`, so it works against
`wrangler dev` or the deployed Worker. Reserved flags: `-X/--method`, `-d/--data/--body`,
`-H/--header`. Unknown `--key value` pairs become query parameters.

```bash
medatat api health
medatat api auth request -X POST -d '{"email":"me@example.com"}'
medatat api auth verify  -X POST -d '{"email":"me@example.com","code":"418902"}'
medatat api cases --assignee me -H "Authorization: Bearer $TOK"
medatat api cases 01H8X values -H "Authorization: Bearer $TOK"
medatat api cases 01H8X values -X POST -H "Authorization: Bearer $TOK" \
  -d '{"base_rev":7,"changes":[{"field_id":"...","value":{"Time":"09:30"}}]}'
```

Seeding the perf corpus:

```bash
medatat seed --cases 1000  --fields 1000 --out ./corpus     # M1 extrapolation run
medatat seed --cases 100000 --fields 1000 --out ./corpus    # M7 full corpus
```

---

## Secrets

```bash
wrangler secret put SESSION_PEPPER      # extra input to session token hashing
```

Never commit secrets. `.dev.vars` is gitignored and holds local-only values.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Unknown arguments: email, sending` | wrangler < 4.123.0 | Upgrade wrangler |
| `no Vulkan device` on Linux | No ICD installed | Install `mesa-vulkan-drivers`; set `VK_ICD_FILENAMES` for lavapipe |
| Clone fails on Windows | Deep Zed paths | `git config --global core.longpaths true` |
| `SQLITE_NOTADB` on open (`--features phi`) | Wrong SQLCipher key | Keychain entry missing or changed. Report "cannot unlock"; never recreate the DB |
| `Locked` on open with `--features phi` | `medatat.key` missing or malformed beside the database | Restore the key file. Never delete the database — it is recoverable only with that key |
| Email never arrives | Domain not onboarded | `wrangler email sending list`; check `_dmarc` and DKIM records |
| `Script startup exceeded CPU time limit` (10021) | WASM bundle too heavy | Keep `lto` and `opt-level = "z"`; check `worker-build` ran `wasm-opt`. Do **not** add `strip` to the wasm profile — it breaks the bundle (row below) and buys nothing, since `wasm-opt` strips anyway |
| `externref table required for catch wrappers` | `strip = "symbols"` removed the externref table wasm-bindgen needs. Happens in the bundle step, not the compile | Bundle with `worker-build --profile release-wasm`, which turns `strip` off. Not caused by `panic = "abort"` and not fixed by `+reference-types` — both were bisected and ruled out |
| DO writes fail after deploy | `new_classes` instead of `new_sqlite_classes` | Fix the migration and use a new class name |
| Cold CI builds take minutes | Zed git clone | `CARGO_NET_GIT_FETCH_WITH_CLI=true`, cache `~/.cargo/git` |
