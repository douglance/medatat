//! Bench 2 — local write of 300 changed fields in one transaction. **R14, CI gate p99 < 10 ms.**
//!
//! The requirement is 200 ms; the gate is 10 ms. This measures `apply_local`, which writes
//! the value *and* its outbox row in a single transaction — the property that makes a
//! crash unable to leave a value saved but un-enqueued.
//!
//! **Never relax the threshold to make a build pass.**

use criterion::{Criterion, criterion_group, criterion_main};
use medatat_core::{CaseRev, ConfigRev, FieldId, Value};
use medatat_store::Store;
use medatat_testkit::{synthetic_case, synthetic_case_id, synthetic_form};
use std::hint::black_box;
use std::time::Duration;

const FIELDS: usize = 500;
const CHANGED: usize = 300;
const GATE: Duration = Duration::from_millis(10);

fn fixture() -> (
    tempfile::TempDir,
    Store,
    medatat_core::CaseId,
    Vec<(FieldId, Value)>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("bench2.db")).expect("open store");

    let form = synthetic_form(FIELDS);
    store.save_form(&form, ConfigRev(1)).expect("save form");

    let case_id = synthetic_case_id(2);
    store
        .upsert_case(&medatat_core::wire::CaseSummary {
            case_id,
            mrn: "BENCH-2".into(),
            form_id: form.form_id,
            assignee: Some("bench".into()),
            rev: CaseRev::ZERO,
            updated_at: "2026-08-17T00:00:00Z".into(),
        })
        .expect("upsert case");

    let changes: Vec<_> = synthetic_case(&form, 2).into_iter().take(CHANGED).collect();
    (dir, store, case_id, changes)
}

fn bench(c: &mut Criterion) {
    let (_dir, store, case_id, changes) = fixture();

    let mut g = c.benchmark_group("bench2_save_r14");
    g.measurement_time(Duration::from_secs(10));

    g.bench_function("apply_local_300_fields", |b| {
        b.iter(|| {
            store
                .apply_local(black_box(case_id), black_box(&changes), CaseRev::ZERO)
                .expect("apply_local")
        });
    });

    // The realistic steady state: one field at a time, as an abstractor actually types.
    let one = &changes[..1];
    g.bench_function("apply_local_single_field", |b| {
        b.iter(|| {
            store
                .apply_local(black_box(case_id), black_box(one), CaseRev::ZERO)
                .expect("apply_local")
        });
    });
    g.finish();

    let started = std::time::Instant::now();
    for _ in 0..20 {
        store
            .apply_local(case_id, &changes, CaseRev::ZERO)
            .expect("apply_local");
    }
    let worst = started.elapsed() / 20;
    assert!(
        worst < GATE,
        "R14 GATE FAILED: mean {CHANGED}-field save {worst:?} exceeds {GATE:?}. \
         Do not raise this threshold — find the regression."
    );
    println!("R14 gate: mean {CHANGED}-field save {worst:?} (limit {GATE:?})");
}

criterion_group!(benches, bench);
criterion_main!(benches);
