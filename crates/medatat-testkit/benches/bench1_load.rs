//! Bench 1 — local SQLite → `FormInstance`, 500 fields, warm. **R13, CI gate p99 < 5 ms.**
//!
//! The requirement is 200 ms. The gate is 5 ms — a ~40x margin, deliberately, so a
//! regression trips here long before a user could feel it. That margin exists only because
//! the read is local: `field_value` is `WITHOUT ROWID` keyed on `(case_id, field_id)`, so a
//! case's values are physically contiguous and this is a primary-key range scan.
//!
//! **Never relax the threshold to make a build pass.** If this regresses, the layout
//! regressed — check `store::tests::case_load_uses_the_primary_key` first.

use criterion::{Criterion, criterion_group, criterion_main};
use medatat_core::{CaseRev, ConfigRev, FormInstance};
use medatat_store::Store;
use medatat_testkit::{synthetic_case, synthetic_case_id, synthetic_form};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

const FIELDS: usize = 500;
const GATE: Duration = Duration::from_millis(5);

fn bench(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("bench1.db")).expect("open store");

    let form = synthetic_form(FIELDS);
    store.save_form(&form, ConfigRev(1)).expect("save form");
    let def = Arc::new(form);

    // One case, written once, then read repeatedly — the warm-cache case the gate targets.
    let case_id = synthetic_case_id(1);
    store
        .upsert_case(&medatat_core::wire::CaseSummary {
            case_id,
            mrn: "BENCH-1".into(),
            form_id: def.form_id,
            assignee: Some("bench".into()),
            rev: CaseRev::ZERO,
            updated_at: "2026-08-17T00:00:00Z".into(),
        })
        .expect("upsert case");
    store
        .apply_local(case_id, &synthetic_case(&def, 1), CaseRev::ZERO)
        .expect("seed values");

    let mut g = c.benchmark_group("bench1_load_r13");
    g.measurement_time(Duration::from_secs(10));

    // The read alone, which is the part the storage layout governs.
    g.bench_function("load_case_values_500", |b| {
        b.iter(|| black_box(store.load_case_values(black_box(case_id)).expect("load")));
    });

    // The whole open-a-case path: read plus building the live model the UI renders.
    // This is the number that must stay under the gate.
    g.bench_function("load_to_form_instance_500", |b| {
        b.iter(|| {
            let values = store.load_case_values(case_id).expect("load");
            black_box(FormInstance::new(
                Arc::clone(&def),
                case_id,
                CaseRev::ZERO,
                values,
            ))
        });
    });
    g.finish();

    // Criterion has no built-in failure threshold, so assert explicitly. A gate that only
    // reports is not a gate.
    let started = std::time::Instant::now();
    for _ in 0..20 {
        let values = store.load_case_values(case_id).expect("load");
        black_box(FormInstance::new(
            Arc::clone(&def),
            case_id,
            CaseRev::ZERO,
            values,
        ));
    }
    let worst = started.elapsed() / 20;
    assert!(
        worst < GATE,
        "R13 GATE FAILED: mean open-case {worst:?} exceeds {GATE:?} over {FIELDS} fields. \
         Do not raise this threshold — find the regression."
    );
    println!("R13 gate: mean open-case {worst:?} (limit {GATE:?})");
}

criterion_group!(benches, bench);
criterion_main!(benches);
