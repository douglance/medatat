//! Retry pacing.
//!
//! Jitter is not optional. Without it, every client that lost connectivity at the same
//! moment retries at the same moment, and the reconnect storm is worse than the outage.

use std::time::Duration;

pub const BASE_MS: u64 = 500;
pub const CAP: Duration = Duration::from_secs(60);
const JITTER_PCT: u64 = 20;

/// `min(60s, 2^attempts * 500ms)` with ±20% jitter.
///
/// `seed` makes the jitter deterministic per (case, field, attempt) so tests are
/// reproducible and two fields never lock step.
pub fn delay_for(attempts: u32, seed: u64) -> Duration {
    let exp = BASE_MS.saturating_mul(1u64 << attempts.min(20));
    let base = Duration::from_millis(exp).min(CAP);

    // SplitMix64 finalizer: cheap, well-distributed, no dependency.
    let mut z = seed
        .wrapping_add(u64::from(attempts))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;

    let span = base.as_millis() as u64 * JITTER_PCT / 100;
    if span == 0 {
        return base;
    }
    let offset = (z % (span * 2)) as i64 - span as i64;
    let ms = (base.as_millis() as i64 + offset).max(1) as u64;
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_exponentially_then_caps() {
        let d0 = delay_for(0, 1);
        let d3 = delay_for(3, 1);
        assert!(d3 > d0);
        for a in 8..25 {
            assert!(delay_for(a, 1) <= CAP + Duration::from_millis(CAP.as_millis() as u64 / 4));
        }
    }

    #[test]
    fn never_returns_zero() {
        for a in 0..25 {
            for s in 0..50 {
                assert!(delay_for(a, s) >= Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn jitter_spreads_clients_apart() {
        let d: Vec<_> = (0..40).map(|seed| delay_for(4, seed)).collect();
        let unique: std::collections::HashSet<_> = d.iter().collect();
        assert!(
            unique.len() > 20,
            "jitter must actually spread retries: {unique:?}"
        );
    }

    #[test]
    fn is_deterministic_for_a_given_seed() {
        assert_eq!(delay_for(3, 42), delay_for(3, 42));
    }
}
