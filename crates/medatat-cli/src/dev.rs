//! `medatat dev last-code --email <addr>` — recover a sign-in code from the **local**
//! emulator, so `scripts/smoke.sh` can exercise the authenticated half unattended.
//!
//! **There is deliberately no Worker endpoint for this.** An endpoint that returns a live
//! auth code is a credential oracle, and "it is dev-only" is one config mistake away from
//! being production — the `#[cfg(debug_assertions)]` version is still a code path that
//! exists in the binary. Nothing in `medatat-worker` knows this command exists.
//!
//! Instead it reads KV through the `wrangler` CLI and recovers the code by exhaustive
//! search. The Worker stores `sha256(code)`, which is irreversible for a session token —
//! 32 bytes of entropy — but **not** for a six-digit code: there are only 1,000,000 of
//! them, and hashing all of them takes milliseconds. That asymmetry is what makes this
//! possible without touching the server.
//!
//! It cannot work against a deployed Worker, by construction: it needs `--local`, which
//! reads `.wrangler/state` off this machine's disk.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Six digits, as `POST /auth/request` generates them.
const CODE_SPACE: u32 = 1_000_000;

#[derive(Debug, Deserialize)]
struct AuthCodeRecord {
    code_hash: String,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    issued_at: String,
}

/// Must match `medatat_worker::logic::auth::{normalize_email, code_key}`. A mismatch shows
/// up as "no code on file", which is loud rather than subtle.
pub fn code_key(email: &str) -> String {
    format!("auth:{}", email.trim().to_ascii_lowercase())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Recover the six-digit code whose sha256 is `hash`, by trying all of them.
///
/// This is the whole trick, and it is worth being explicit about why it works: the code
/// carries about 20 bits of entropy, so its hash is a lookup table, not a secret. What
/// actually protects a code in production is its 600 s TTL and five-attempt budget — never
/// the fact that it is stored hashed.
pub fn recover_code(hash: &str) -> Option<String> {
    let hash = hash.trim().to_ascii_lowercase();
    (0..CODE_SPACE).find_map(|n| {
        let candidate = format!("{n:06}");
        (sha256_hex(candidate.as_bytes()) == hash).then_some(candidate)
    })
}

/// The exact argv used to read a key. Split out so the `--local` guard is *testable*
/// rather than a promise in a comment: there is no flag on this command that can remove
/// it, and `assert_the_local_guard_cannot_be_removed` fails if one is ever added.
pub fn kv_argv(binding: &str, key: &str) -> Vec<String> {
    ["kv", "key", "get", "--local", "--binding", binding, key]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Read one key out of the local KV namespace via `wrangler`.
fn kv_get_local(binding: &str, key: &str) -> Result<Option<String>> {
    let out = std::process::Command::new("wrangler")
        .args(kv_argv(binding, key))
        .output()
        .context("running `wrangler kv key get` (is wrangler on PATH?)")?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if !out.status.success() {
        // A missing key is a normal outcome, not a failure.
        if stderr.contains("not found") || stdout.contains("not found") {
            return Ok(None);
        }
        bail!("wrangler kv key get failed: {}", stderr.trim());
    }

    // wrangler prints banner lines before the value; the record is the JSON object.
    let value = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('{') && l.ends_with('}'));
    Ok(value.map(str::to_string))
}

pub fn run(args: &[String]) -> Result<()> {
    let mut email: Option<String> = None;
    let mut binding = "AUTH".to_string();

    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("last-code") => {}
        Some("-h") | Some("--help") | None => {
            println!("{HELP}");
            return Ok(());
        }
        Some(other) => bail!("unknown dev subcommand {other}\n\n{HELP}"),
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--email" => {
                email = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--email needs an address"))?
                        .to_string(),
                )
            }
            "--binding" => {
                binding = it
                    .next()
                    .ok_or_else(|| anyhow!("--binding needs a name"))?
                    .to_string()
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            other => bail!("unknown flag {other}"),
        }
    }

    let email = email.ok_or_else(|| anyhow!("--email is required\n\n{HELP}"))?;
    let key = code_key(&email);

    let raw = kv_get_local(&binding, &key)?.ok_or_else(|| {
        anyhow!(
            "no code on file for {email}.\n\
             Request one first (POST /auth/request), and note that codes expire after 600s.\n\
             This reads the LOCAL emulator only — it cannot see a deployed Worker."
        )
    })?;

    let record: AuthCodeRecord =
        serde_json::from_str(&raw).with_context(|| format!("parsing the KV record: {raw}"))?;

    let code = recover_code(&record.code_hash).ok_or_else(|| {
        anyhow!(
            "the stored hash matches no six-digit code. Either the code format changed, \
             or this key holds something else."
        )
    })?;

    if record.attempts > 0 {
        eprintln!(
            "note: {} failed attempt(s) already recorded; 5 invalidates the code",
            record.attempts
        );
    }
    if !record.issued_at.is_empty() {
        eprintln!("note: issued at {} (600s TTL)", record.issued_at);
    }
    println!("{code}");
    Ok(())
}

const HELP: &str = "\
medatat dev last-code --email <addr> [--binding AUTH]

Recovers a sign-in code from the LOCAL wrangler emulator, for unattended smoke tests.

This is local-emulator only, and that is a design choice rather than a limitation. There
is no Worker endpoint for it: an endpoint that returns a live auth code is a credential
oracle, and a dev-only guard is one config mistake from production. This command reads
`.wrangler/state` through `wrangler kv key get --local`, so the capability physically
cannot exist against a deployed Worker.

The Worker stores sha256(code). That is irreversible for a 32-byte session token, but a
six-digit code has only 1,000,000 possible values, so the code is recovered by hashing all
of them. What protects a code in production is its 600s TTL and five-attempt budget.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_code_from_its_hash() {
        for code in ["000000", "418902", "999999", "007"] {
            let padded = format!("{:0>6}", code);
            let hash = sha256_hex(padded.as_bytes());
            assert_eq!(recover_code(&hash).as_deref(), Some(padded.as_str()));
        }
    }

    #[test]
    fn recovery_is_case_insensitive_about_the_stored_hash() {
        let hash = sha256_hex(b"418902");
        assert_eq!(
            recover_code(&hash.to_uppercase()).as_deref(),
            Some("418902")
        );
    }

    #[test]
    fn a_hash_of_something_that_is_not_a_six_digit_code_is_not_recovered() {
        // A session token is 32 bytes; its hash is genuinely irreversible, and this is the
        // asymmetry the command depends on.
        let hash = sha256_hex(b"a-43-character-base64url-session-token-here");
        assert_eq!(recover_code(&hash), None);
    }

    #[test]
    fn assert_the_local_guard_cannot_be_removed() {
        // The whole safety argument for this command is that it physically cannot reach a
        // deployed Worker. Two properties carry that, and both are asserted here rather
        // than described: the argv always says --local, and nothing about the command
        // takes a URL. If someone adds a --remote flag, this fails.
        let argv = kv_argv("AUTH", "auth:a@b.com");
        assert!(
            argv.contains(&"--local".to_string()),
            "argv lost --local: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|a| a == "--remote" || a.starts_with("http")),
            "argv gained a remote target: {argv:?}"
        );
        assert!(
            !HELP.contains("--remote"),
            "the help text offers a remote mode this command must not have"
        );
    }

    #[test]
    fn the_kv_key_matches_the_worker() {
        // medatat_worker::logic::auth::code_key
        assert_eq!(code_key("Smoke@Cetify.Email"), "auth:smoke@cetify.email");
        assert_eq!(code_key("  a@b.com  "), "auth:a@b.com");
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
