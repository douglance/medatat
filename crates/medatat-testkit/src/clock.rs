//! Time, injectable.
//!
//! `medatat-sync` backs off with `min(60s, 2^attempts * 500ms) ± 20%` jitter. Verifying
//! that schedule against a wall clock would mean a test that sleeps for minutes and still
//! flakes, so the engine takes a `Clock` and the tests hand it a [`FakeClock`].

use chrono::{DateTime, Utc};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The trait `medatat-sync` depends on. Mirrored here so the double exists before the
/// crate that consumes it — the definitions must stay identical.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The real clock. Used everywhere outside tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when a test moves it.
///
/// Behind `&self`, because the engine under test holds the clock shared and the test still
/// has to advance it.
#[derive(Debug)]
pub struct FakeClock {
    now: Mutex<DateTime<Utc>>,
}

impl FakeClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        FakeClock {
            now: Mutex::new(start),
        }
    }

    /// Starts at 2026-01-01T00:00:00Z — a fixed, obviously-synthetic instant, so a
    /// timestamp in a failure message is recognisable as fixture output.
    pub fn at_default_epoch() -> Self {
        const Y2026: i64 = 1_767_225_600;
        Self::new(DateTime::from_timestamp(Y2026, 0).unwrap_or(DateTime::UNIX_EPOCH))
    }

    pub fn set(&self, t: DateTime<Utc>) {
        *self.lock() = t;
    }

    /// Moves time forward. Saturates rather than wrapping if a test asks for something
    /// absurd, so an arithmetic overflow can never look like a backoff bug.
    pub fn advance(&self, by: Duration) {
        // A duration chrono cannot represent saturates at a century, so an overflow can
        // never be mistaken for a backoff bug.
        let delta =
            chrono::Duration::from_std(by).unwrap_or_else(|_| chrono::Duration::days(36_500));
        let mut g = self.lock();
        *g = g.checked_add_signed(delta).unwrap_or(*g);
    }

    pub fn advance_secs(&self, secs: u64) {
        self.advance(Duration::from_secs(secs));
    }

    pub fn advance_millis(&self, millis: u64) {
        self.advance(Duration::from_millis(millis));
    }

    fn lock(&self) -> MutexGuard<'_, DateTime<Utc>> {
        self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::at_default_epoch()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Arc;

    #[test]
    fn a_fake_clock_does_not_move_on_its_own() {
        let c = FakeClock::default();
        let t0 = c.now();
        for _ in 0..1000 {
            assert_eq!(c.now(), t0);
        }
    }

    #[test]
    fn advance_moves_time_forward_exactly() {
        let c = FakeClock::default();
        let t0 = c.now();
        c.advance_secs(90);
        assert_eq!((c.now() - t0).num_seconds(), 90);
        c.advance_millis(500);
        assert_eq!((c.now() - t0).num_milliseconds(), 90_500);
    }

    #[test]
    fn set_replaces_the_instant() {
        let c = FakeClock::default();
        let t = Utc.with_ymd_and_hms(2030, 6, 1, 12, 0, 0).single().unwrap();
        c.set(t);
        assert_eq!(c.now(), t);
    }

    #[test]
    fn a_fake_clock_is_shareable_as_a_trait_object() {
        let c = Arc::new(FakeClock::default());
        let as_trait: Arc<dyn Clock> = c.clone();
        let t0 = as_trait.now();
        c.advance_secs(1);
        assert_eq!((as_trait.now() - t0).num_seconds(), 1);
    }

    #[test]
    fn the_system_clock_moves() {
        let a = SystemClock.now();
        assert!(a > Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).single().unwrap());
    }
}
