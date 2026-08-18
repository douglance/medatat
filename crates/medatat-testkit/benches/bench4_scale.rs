//! Bench 4 — cold-DO full case sync and seeding throughput. R16, nightly, no gate.
//!
//! TODO(M1): wire to medatat-sync and medatat-worker once it lands.
//!
//! Placeholder. The measurement this file owns needs `medatat-sync and medatat-worker`, which does not exist yet, so
//! the bench registers an empty group instead of a fake number: `cargo bench` runs
//! end-to-end today, and the gate arrives with the crate it measures. See
//! `docs/07-TESTING.md`. **Never relax a threshold to make a build pass.**

use criterion::{Criterion, criterion_group, criterion_main};

fn not_yet_implemented(_c: &mut Criterion) {}

criterion_group!(benches, not_yet_implemented);
criterion_main!(benches);
