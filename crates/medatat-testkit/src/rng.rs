//! A tiny deterministic PRNG.
//!
//! Written here rather than pulled from `rand` on purpose: the testkit stays
//! dependency-light, and — more importantly — the sequence is frozen by this file, so a
//! benchmark that fails on CI reproduces byte-for-byte on a laptop years later. A crate
//! bump could quietly change a generator and take the reproducibility with it.
//!
//! The algorithm is SplitMix64 (Steele, Lea & Flood 2014), the same finalizer used to seed
//! xoshiro. It is not cryptographic and must never be used for anything but test data.

use uuid::Uuid;

const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// Derives an independent stream from this one. Used so that, say, section titles and
    /// field kinds do not share a sequence — changing one would otherwise shift the other.
    pub fn fork(&self, label: u64) -> Self {
        Rng::new(self.state ^ label.wrapping_mul(GOLDEN_GAMMA))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN_GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `0..n`. The modulo bias is irrelevant for test data and keeping it
    /// simple keeps the sequence easy to reason about. Returns 0 when `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Inclusive range. `lo` is returned when `hi < lo`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            lo
        } else {
            lo + self.below(hi - lo + 1)
        }
    }

    /// Inclusive signed range.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            lo
        } else {
            let span = hi.wrapping_sub(lo) as u64;
            lo.wrapping_add(self.below(span.saturating_add(1)) as i64)
        }
    }

    /// True `percent` times in 100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }

    /// A deterministic UUID shaped like a v4 so it is indistinguishable downstream.
    pub fn uuid(&mut self) -> Uuid {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.next_u64().to_le_bytes());
        b[8..].copy_from_slice(&self.next_u64().to_le_bytes());
        b[6] = (b[6] & 0x0F) | 0x40; // version 4
        b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
        Uuid::from_bytes(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_yields_the_same_sequence() {
        let a: Vec<u64> = (0..64)
            .scan(Rng::new(7), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..64)
            .scan(Rng::new(7), |r, _| Some(r.next_u64()))
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_diverge() {
        let a: Vec<u64> = (0..16)
            .scan(Rng::new(1), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..16)
            .scan(Rng::new(2), |r, _| Some(r.next_u64()))
            .collect();
        assert_ne!(a, b);
    }

    #[test]
    fn ranges_stay_inside_their_bounds() {
        let mut r = Rng::new(99);
        for _ in 0..10_000 {
            let v = r.range(3, 9);
            assert!((3..=9).contains(&v), "{v}");
            let s = r.range_i64(-5, 5);
            assert!((-5..=5).contains(&s), "{s}");
            assert_eq!(r.below(0), 0);
            assert_eq!(r.range(4, 4), 4);
        }
    }

    #[test]
    fn uuids_are_v4_shaped_and_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            let u = a.uuid();
            assert_eq!(u, b.uuid());
            assert_eq!(u.get_version_num(), 4);
        }
    }

    #[test]
    fn forked_streams_are_independent() {
        let base = Rng::new(5);
        let mut x = base.fork(1);
        let mut y = base.fork(2);
        assert_ne!(x.next_u64(), y.next_u64());
        assert_eq!(base.fork(1).next_u64(), base.fork(1).next_u64());
    }
}
