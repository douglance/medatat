//! Magic-code auth (R1), as pure functions over records.
//!
//! There is no password anywhere in this system, so there is no KDF and no argon2 CPU
//! budget hazard inside the Worker (`docs/adr/0004`). What is left is a short-lived code
//! and a high-entropy session token, and the rules that keep both honest:
//!
//! - `POST /auth/request` returns 204 for an unknown or inactive email. No enumeration.
//! - The code is 6 digits from `OsRng`; only `sha256(code)` is stored, with a 600 s TTL.
//! - Five failed attempts invalidate the code. A correct code is single-use.
//! - The session token is 32 bytes from `OsRng`, base64url. Only `sha256(token)` is stored
//!   — `sha256` and not a slow KDF because the token is already 256 bits of entropy.
//!
//! IO lives in `store/kv.rs` and `routes/auth.rs`; every decision lives here.

use crate::error::{LogicError, LogicResult};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use medatat_core::wire::UserInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CODE_DIGITS: u32 = 6;
pub const CODE_TTL_SECONDS: u64 = 600;
pub const SESSION_TTL_SECONDS: u64 = 43_200;
pub const MAX_ATTEMPTS: u32 = 5;
pub const TOKEN_BYTES: usize = 32;

/// Rate limits from `docs/03-API.md`: 3 requests per email and 20 per IP, per 15 minutes.
pub const REQUESTS_PER_EMAIL: u32 = 3;
pub const REQUESTS_PER_IP: u32 = 20;
pub const RATE_WINDOW_SECONDS: i64 = 900;

pub fn code_key(email: &str) -> String {
    format!("auth:{}", normalize_email(email))
}

pub fn session_key(token_hash: &str) -> String {
    format!("session:{token_hash}")
}

pub fn email_rate_key(email: &str) -> String {
    format!("rate:email:{}", normalize_email(email))
}

pub fn ip_rate_key(ip: &str) -> String {
    format!("rate:ip:{ip}")
}

/// A row of `app_user`. `is_active` is separate from [`UserInfo`] because it decides
/// whether a code is issued at all, and never reaches a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub user: UserInfo,
    pub is_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCodeRecord {
    pub code_hash: String,
    pub attempts: u32,
    pub issued_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub user_id: String,
    pub issued_at: String,
    pub expires_at: String,
}

/// A fixed-window counter, stored in KV per email and per IP.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateWindow {
    pub count: u32,
    pub window_start: String,
}

/// What `POST /auth/request` should do. Both arms answer the caller with 204.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Unknown or inactive email: do nothing at all, and still return 204.
    Silent,
    Issue {
        code: String,
        record: AuthCodeRecord,
        ttl_seconds: u64,
    },
}

/// What `POST /auth/verify` should do. Every arm but `Accept` answers 401.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// No code on file — never issued, already used, or evicted by TTL.
    NoCode,
    /// On file but past its 600 s life. Delete the key.
    Expired,
    /// Wrong code, budget remains. Persist `record` and answer 401.
    Retry { record: AuthCodeRecord },
    /// Wrong code and the attempt budget is spent. Delete the key and answer 401.
    Locked,
    /// Correct. Delete the key — codes are single-use — and mint a session.
    Accept,
}

impl VerifyOutcome {
    /// Whether the stored code must be removed as a result of this outcome.
    pub fn consumes_code(&self) -> bool {
        matches!(
            self,
            VerifyOutcome::Accept | VerifyOutcome::Locked | VerifyOutcome::Expired
        )
    }
}

/// Decide whether to issue a code. Returns [`RequestOutcome::Silent`] for an absent or
/// deactivated account — the caller answers 204 either way.
pub fn handle_auth_request(
    account: Option<&Account>,
    now: DateTime<Utc>,
) -> LogicResult<RequestOutcome> {
    match account {
        Some(a) if a.is_active => {
            let code = generate_code()?;
            Ok(RequestOutcome::Issue {
                record: AuthCodeRecord {
                    code_hash: hash_code(&code),
                    attempts: 0,
                    issued_at: rfc3339(now),
                },
                code,
                ttl_seconds: CODE_TTL_SECONDS,
            })
        }
        _ => Ok(RequestOutcome::Silent),
    }
}

/// Decide the fate of a submitted code.
pub fn verify_code(
    record: Option<&AuthCodeRecord>,
    supplied: &str,
    now: DateTime<Utc>,
) -> VerifyOutcome {
    let Some(record) = record else {
        return VerifyOutcome::NoCode;
    };

    // KV's TTL is the primary expiry; this is the belt to its braces, and the only part
    // that can be tested without a clock in the loop.
    if code_is_expired(record, now) {
        return VerifyOutcome::Expired;
    }

    if record.attempts >= MAX_ATTEMPTS {
        return VerifyOutcome::Locked;
    }

    if constant_time_eq(&hash_code(supplied), &record.code_hash) {
        return VerifyOutcome::Accept;
    }

    let attempts = record.attempts + 1;
    if attempts >= MAX_ATTEMPTS {
        VerifyOutcome::Locked
    } else {
        VerifyOutcome::Retry {
            record: AuthCodeRecord {
                attempts,
                ..record.clone()
            },
        }
    }
}

pub fn code_is_expired(record: &AuthCodeRecord, now: DateTime<Utc>) -> bool {
    match parse_rfc3339(&record.issued_at) {
        // An unparseable timestamp is treated as expired: fail closed.
        Err(_) => true,
        Ok(issued) => {
            now.signed_utc_duration_since_compat(issued)
                > TimeDelta::seconds(CODE_TTL_SECONDS as i64)
        }
    }
}

/// A freshly minted session. `token` is returned to the client exactly once; only
/// `token_hash` is ever persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedSession {
    pub token: String,
    pub token_hash: String,
    pub record: SessionRecord,
    pub ttl_seconds: u64,
}

pub fn mint_session(user_id: &str, now: DateTime<Utc>) -> LogicResult<MintedSession> {
    let token = generate_token()?;
    let expires = now + TimeDelta::seconds(SESSION_TTL_SECONDS as i64);
    Ok(MintedSession {
        token_hash: hash_token(&token),
        token,
        record: SessionRecord {
            user_id: user_id.to_string(),
            issued_at: rfc3339(now),
            expires_at: rfc3339(expires),
        },
        ttl_seconds: SESSION_TTL_SECONDS,
    })
}

pub fn session_is_live(record: &SessionRecord, now: DateTime<Utc>) -> bool {
    match parse_rfc3339(&record.expires_at) {
        Err(_) => false,
        Ok(exp) => now < exp,
    }
}

/// Advance a fixed window, or refuse. `Err(RateLimited)` is the only failure.
pub fn check_rate(
    window: Option<&RateWindow>,
    now: DateTime<Utc>,
    limit: u32,
) -> LogicResult<RateWindow> {
    let fresh = RateWindow {
        count: 1,
        window_start: rfc3339(now),
    };
    let Some(w) = window else { return Ok(fresh) };
    let Ok(start) = parse_rfc3339(&w.window_start) else {
        return Ok(fresh);
    };
    if now.signed_utc_duration_since_compat(start) >= TimeDelta::seconds(RATE_WINDOW_SECONDS) {
        return Ok(fresh);
    }
    if w.count >= limit {
        return Err(LogicError::RateLimited);
    }
    Ok(RateWindow {
        count: w.count + 1,
        window_start: w.window_start.clone(),
    })
}

/// Extract the bearer token from an `Authorization` header value.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let raw = header?.trim();
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

// ------------------------------------------------------------------ primitives

pub fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

pub fn hash_code(code: &str) -> String {
    sha256_hex(code.trim().as_bytes())
}

pub fn hash_token(token: &str) -> String {
    sha256_hex(token.as_bytes())
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

/// Length-independent, branch-free comparison. Both operands here are hex digests of
/// identical length, but the property is worth stating in code rather than assuming.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Six digits, uniform over `000000..=999999`, by rejection sampling so no modulo bias.
pub fn generate_code() -> LogicResult<String> {
    const RANGE: u32 = 1_000_000;
    // Largest multiple of RANGE that fits in u32; anything above it is rejected.
    const LIMIT: u32 = u32::MAX - (u32::MAX % RANGE) - 1;
    for _ in 0..64 {
        let mut buf = [0u8; 4];
        fill_random(&mut buf)?;
        let n = u32::from_le_bytes(buf);
        if n <= LIMIT {
            return Ok(format!(
                "{:0width$}",
                n % RANGE,
                width = CODE_DIGITS as usize
            ));
        }
    }
    Err(LogicError::Internal("code generation failed".into()))
}

/// 32 bytes from `OsRng`, base64url without padding — 43 characters.
pub fn generate_token() -> LogicResult<String> {
    let mut buf = [0u8; TOKEN_BYTES];
    fill_random(&mut buf)?;
    Ok(base64url_encode(&buf))
}

fn fill_random(buf: &mut [u8]) -> LogicResult<()> {
    getrandom::getrandom(buf).map_err(|e| LogicError::Internal(format!("csprng unavailable: {e}")))
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        let keep = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for &i in idx.iter().take(keep) {
            out.push(B64URL[i as usize] as char);
        }
    }
    out
}

pub fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn parse_rfc3339(s: &str) -> LogicResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| LogicError::Internal(format!("bad timestamp: {e}")))
}

/// `chrono`'s `signed_duration_since` under a name that says which direction it runs.
trait SignedSince {
    fn signed_utc_duration_since_compat(self, earlier: DateTime<Utc>) -> TimeDelta;
}
impl SignedSince for DateTime<Utc> {
    fn signed_utc_duration_since_compat(self, earlier: DateTime<Utc>) -> TimeDelta {
        self.signed_duration_since(earlier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::wire::Role;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn now() -> DateTime<Utc> {
        parse_rfc3339("2026-08-17T12:00:00Z").unwrap()
    }

    fn later(secs: i64) -> DateTime<Utc> {
        now() + TimeDelta::seconds(secs)
    }

    fn account(active: bool) -> Account {
        Account {
            user: UserInfo {
                user_id: "u-1".into(),
                email: "abstractor@example.com".into(),
                display_name: "Abstractor".into(),
                role: Role::Abstractor,
            },
            is_active: active,
        }
    }

    /// Applies the outcomes of the pure functions, so single-use and lockout can be proved
    /// end to end without KV.
    #[derive(Default)]
    struct MemAuth {
        codes: RefCell<HashMap<String, AuthCodeRecord>>,
        sessions: RefCell<HashMap<String, SessionRecord>>,
    }

    impl MemAuth {
        fn request(
            &self,
            email: &str,
            account: Option<&Account>,
            now: DateTime<Utc>,
        ) -> Option<String> {
            match handle_auth_request(account, now).unwrap() {
                RequestOutcome::Silent => None,
                RequestOutcome::Issue { code, record, .. } => {
                    self.codes.borrow_mut().insert(code_key(email), record);
                    Some(code)
                }
            }
        }

        fn verify(&self, email: &str, supplied: &str, now: DateTime<Utc>) -> VerifyOutcome {
            let key = code_key(email);
            let record = self.codes.borrow().get(&key).cloned();
            let outcome = verify_code(record.as_ref(), supplied, now);
            match &outcome {
                VerifyOutcome::Retry { record } => {
                    self.codes.borrow_mut().insert(key, record.clone());
                }
                o if o.consumes_code() => {
                    self.codes.borrow_mut().remove(&key);
                }
                _ => {}
            }
            if matches!(outcome, VerifyOutcome::Accept) {
                let s = mint_session("u-1", now).unwrap();
                self.sessions
                    .borrow_mut()
                    .insert(s.token_hash.clone(), s.record);
            }
            outcome
        }

        fn code_on_file(&self, email: &str) -> bool {
            self.codes.borrow().contains_key(&code_key(email))
        }
    }

    // ------------------------------------------------------------ enumeration

    #[test]
    fn r1_unknown_email_issues_nothing() {
        let auth = MemAuth::default();
        assert_eq!(auth.request("nobody@example.com", None, now()), None);
        assert!(!auth.code_on_file("nobody@example.com"));
    }

    #[test]
    fn r1_inactive_account_issues_nothing() {
        let auth = MemAuth::default();
        assert_eq!(
            auth.request("abstractor@example.com", Some(&account(false)), now()),
            None
        );
        assert!(!auth.code_on_file("abstractor@example.com"));
    }

    #[test]
    fn r1_unknown_and_known_emails_are_indistinguishable_to_the_caller() {
        // Both arms of RequestOutcome answer 204; the difference is invisible on the wire.
        let known = handle_auth_request(Some(&account(true)), now()).unwrap();
        let unknown = handle_auth_request(None, now()).unwrap();
        assert!(matches!(known, RequestOutcome::Issue { .. }));
        assert_eq!(unknown, RequestOutcome::Silent);
    }

    // ------------------------------------------------------------------ codes

    #[test]
    fn r1_code_is_six_digits_and_only_its_hash_is_stored() {
        let outcome = handle_auth_request(Some(&account(true)), now()).unwrap();
        let RequestOutcome::Issue {
            code,
            record,
            ttl_seconds,
        } = outcome
        else {
            panic!("expected Issue");
        };
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(ttl_seconds, 600);
        assert_eq!(record.attempts, 0);
        assert_eq!(record.code_hash.len(), 64);
        assert!(
            !record.code_hash.contains(&code),
            "the code itself must not be stored"
        );
        assert_eq!(record.code_hash, hash_code(&code));
    }

    #[test]
    fn r1_generated_codes_span_the_full_range() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(generate_code().unwrap());
        }
        assert!(
            seen.len() > 150,
            "codes look degenerate: {} distinct",
            seen.len()
        );
    }

    #[test]
    fn r1_correct_code_is_accepted() {
        let auth = MemAuth::default();
        let code = auth
            .request("abstractor@example.com", Some(&account(true)), now())
            .unwrap();
        assert_eq!(
            auth.verify("abstractor@example.com", &code, now()),
            VerifyOutcome::Accept
        );
    }

    #[test]
    fn r1_code_is_single_use() {
        let auth = MemAuth::default();
        let code = auth
            .request("abstractor@example.com", Some(&account(true)), now())
            .unwrap();
        assert_eq!(
            auth.verify("abstractor@example.com", &code, now()),
            VerifyOutcome::Accept
        );
        assert!(!auth.code_on_file("abstractor@example.com"));
        assert_eq!(
            auth.verify("abstractor@example.com", &code, now()),
            VerifyOutcome::NoCode,
            "replaying an accepted code must fail"
        );
    }

    #[test]
    fn r1_code_expires_after_its_ttl() {
        let auth = MemAuth::default();
        let code = auth
            .request("abstractor@example.com", Some(&account(true)), now())
            .unwrap();

        assert_eq!(
            verify_code(
                auth.codes.borrow().get(&code_key("abstractor@example.com")),
                &code,
                later(CODE_TTL_SECONDS as i64)
            ),
            VerifyOutcome::Accept,
            "exactly at the TTL boundary the code is still good"
        );

        assert_eq!(
            auth.verify(
                "abstractor@example.com",
                &code,
                later(CODE_TTL_SECONDS as i64 + 1)
            ),
            VerifyOutcome::Expired
        );
        assert!(
            !auth.code_on_file("abstractor@example.com"),
            "an expired code is deleted, not left to be brute-forced"
        );
    }

    #[test]
    fn r1_five_attempts_lock_the_code_out() {
        let auth = MemAuth::default();
        let code = auth
            .request("abstractor@example.com", Some(&account(true)), now())
            .unwrap();
        let wrong = if code == "000000" { "111111" } else { "000000" };

        for attempt in 1..=4 {
            match auth.verify("abstractor@example.com", wrong, now()) {
                VerifyOutcome::Retry { record } => assert_eq!(record.attempts, attempt),
                other => panic!("attempt {attempt}: expected Retry, got {other:?}"),
            }
        }

        assert_eq!(
            auth.verify("abstractor@example.com", wrong, now()),
            VerifyOutcome::Locked,
            "the fifth failure locks it"
        );
        assert!(!auth.code_on_file("abstractor@example.com"));

        assert_eq!(
            auth.verify("abstractor@example.com", &code, now()),
            VerifyOutcome::NoCode,
            "even the correct code is dead once the budget is spent"
        );
    }

    #[test]
    fn r1_a_wrong_code_never_reveals_the_right_one() {
        let record = AuthCodeRecord {
            code_hash: hash_code("418902"),
            attempts: 0,
            issued_at: rfc3339(now()),
        };
        let VerifyOutcome::Retry { record: next } = verify_code(Some(&record), "000000", now())
        else {
            panic!("expected Retry");
        };
        assert_eq!(next.code_hash, record.code_hash);
        assert_eq!(next.attempts, 1);
    }

    // --------------------------------------------------------------- sessions

    #[test]
    fn r1_session_token_is_high_entropy_and_only_hashed_at_rest() {
        let s = mint_session("u-1", now()).unwrap();
        assert_eq!(s.token.len(), 43, "32 bytes base64url without padding");
        assert!(
            s.token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "token must be url-safe: {}",
            s.token
        );
        assert_eq!(s.token_hash, hash_token(&s.token));
        assert_eq!(s.ttl_seconds, 43_200);
        assert_eq!(s.record.expires_at, rfc3339(later(43_200)));
        assert!(session_is_live(&s.record, later(43_199)));
        assert!(!session_is_live(&s.record, later(43_201)));
    }

    #[test]
    fn r1_tokens_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            assert!(seen.insert(generate_token().unwrap()), "token collision");
        }
    }

    #[test]
    fn bearer_header_parsing() {
        assert_eq!(bearer_token(Some("Bearer abc123")), Some("abc123"));
        assert_eq!(bearer_token(Some("bearer  abc123 ")), Some("abc123"));
        assert_eq!(bearer_token(Some("Basic abc123")), None);
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(Some("abc123")), None);
        assert_eq!(bearer_token(None), None);
    }

    // ------------------------------------------------------------ rate limits

    #[test]
    fn r1_email_rate_limit_trips_on_the_fourth_request_in_a_window() {
        let mut w = check_rate(None, now(), REQUESTS_PER_EMAIL).unwrap();
        assert_eq!(w.count, 1);
        w = check_rate(Some(&w), later(10), REQUESTS_PER_EMAIL).unwrap();
        w = check_rate(Some(&w), later(20), REQUESTS_PER_EMAIL).unwrap();
        assert_eq!(w.count, 3);
        assert_eq!(
            check_rate(Some(&w), later(30), REQUESTS_PER_EMAIL).unwrap_err(),
            LogicError::RateLimited
        );
        assert_eq!(LogicError::RateLimited.http_status(), 429);
    }

    #[test]
    fn r1_rate_window_resets_after_fifteen_minutes() {
        let w = RateWindow {
            count: REQUESTS_PER_EMAIL,
            window_start: rfc3339(now()),
        };
        let next = check_rate(Some(&w), later(RATE_WINDOW_SECONDS), REQUESTS_PER_EMAIL).unwrap();
        assert_eq!(next.count, 1);
    }

    // ------------------------------------------------------------- primitives

    #[test]
    fn email_lookup_is_case_and_whitespace_insensitive() {
        assert_eq!(
            normalize_email("  Abstractor@Example.COM "),
            "abstractor@example.com"
        );
        assert_eq!(code_key("A@B.com"), code_key("a@b.com"));
    }

    #[test]
    fn constant_time_eq_is_still_correct() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn base64url_matches_rfc4648_without_padding() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url_encode(&[0xfb, 0xff, 0xfe]), "-__-");
    }
}
