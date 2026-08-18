//! Bench 6 — characterising the WAL checkpoint stall. Diagnostic, no gate.
//!
//! Bench 5 found that a minority of `apply_local` calls take 20–50x the median, and that
//! every slow one coincides with the `-wal` file reaching ~4 MB and resetting. SQLite's
//! `wal_autocheckpoint` defaults to 1000 pages, and the commit unlucky enough to cross
//! that line pays to copy the whole WAL back into the database and fsync it.
//!
//! That is a plausible story, not a measurement. This file answers the three questions
//! the story raises before anyone changes a pragma:
//!
//! **A. Does the stall grow with the size of the store?** If checkpoint cost scales with
//! the database, it is a scaling hazard and gets worse for exactly the users who have the
//! most data. If it scales only with the WAL, it is a fixed periodic hitch.
//!
//! **B. Does it scale with the WAL?** If it does, the size of the stall is a tuning knob
//! and the trade is "one long stall" against "many short ones".
//!
//! **C. What does an abstractor actually feel?** Bench 5 wrote 300 fields at a time. A
//! person types one field at a time, so the WAL fills far more slowly and the stall
//! arrives far less often — but each one still costs the same. Frequency and magnitude
//! are different questions and only the pair of them describes the experience.
//!
//! Runs only when asked, because it is a diagnostic rather than a gate:
//!
//! ```sh
//! MEDATAT_CHECKPOINT_PROBE=1 cargo bench -p medatat-testkit --bench bench6_checkpoint
//! ```

use criterion::{Criterion, criterion_group, criterion_main};
use medatat_core::wire::CaseSummary;
use medatat_core::{CaseId, CaseRev, ConfigRev, FieldId, FormDef, Value};
use medatat_store::Store;
use medatat_testkit::{synthetic_case, synthetic_case_id, synthetic_form, synthetic_value_rows};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FIELDS: usize = 1000;

fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

fn file_bytes(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn percentiles(mut xs: Vec<Duration>) -> (Duration, Duration, Duration) {
    xs.sort_unstable();
    let n = xs.len();
    (xs[n / 2], xs[(n * 99 / 100).min(n - 1)], xs[n - 1])
}

fn seed(store: &Store, form: &FormDef, cases: usize) -> Vec<CaseId> {
    (0..cases)
        .map(|i| {
            let case_id = synthetic_case_id(i as u64);
            store
                .upsert_case(&CaseSummary {
                    case_id,
                    mrn: format!("CKPT-{i:05}"),
                    form_id: form.form_id,
                    assignee: Some("bench".into()),
                    rev: CaseRev(1),
                    updated_at: "2026-08-18T00:00:00Z".into(),
                })
                .expect("upsert case");
            store
                .apply_server_values(
                    case_id,
                    &synthetic_value_rows(form, i as u64, CaseRev(1)),
                    CaseRev(1),
                )
                .expect("apply_server_values");
            case_id
        })
        .collect()
}

/// Grows the `-wal` file to at least `target` bytes without letting it check itself in.
///
/// Writes go through the read connection on purpose: `wal_autocheckpoint` is a per-
/// connection setting, so disabling it here and leaving the write connection idle is
/// enough to hold a checkpoint off while the WAL is built to a chosen size. That is the
/// only way to vary the one thing question B is about.
fn grow_wal(store: &Store, db: &Path, case_id: CaseId, target: u64) {
    store
        .with_read(|c| {
            c.execute_batch("PRAGMA wal_autocheckpoint = 0")
                .expect("disable autockpt");
            let mut n = 0u64;
            while file_bytes(&wal_path(db)) < target {
                // Fresh field ids against a real case: ordinary page churn in the hot
                // table, not a synthetic write to somewhere the app never touches.
                let batch: Vec<String> = (0..200)
                    .map(|_| {
                        n += 1;
                        format!(
                            "INSERT OR REPLACE INTO field_value \
                             (case_id, field_id, value_kind, value_text, rev, pending) \
                             VALUES ('{}', '{}', 'text', 'checkpoint probe {n}', 1, 0);",
                            case_id,
                            FieldId::new()
                        )
                    })
                    .collect();
                c.execute_batch(&format!("BEGIN; {} COMMIT;", batch.concat()))
                    .expect("grow wal");
            }
        })
        .expect("with_read");
}

/// Times an explicit full checkpoint, which is what the automatic one does on whichever
/// commit is unlucky enough to trigger it.
fn time_checkpoint(store: &Store) -> Duration {
    store
        .with_read(|c| {
            let t = Instant::now();
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .expect("checkpoint");
            t.elapsed()
        })
        .expect("with_read")
}

fn probe(_c: &mut Criterion) {
    if std::env::var("MEDATAT_CHECKPOINT_PROBE").is_err() {
        println!(
            "bench6: skipped. Set MEDATAT_CHECKPOINT_PROBE=1 to run the checkpoint \
             characterisation (it is a diagnostic, not a gate)."
        );
        return;
    }

    let form = synthetic_form(FIELDS);
    println!("\n=== Bench 6 — WAL checkpoint characterisation ===");

    // ---------------------------------------------------------------- A: vs store size
    println!("\n-- A. checkpoint cost vs database size, WAL held at ~4 MB --");
    println!(
        "{:>7}  {:>9}  {:>12}  {:>12}  {:>12}",
        "cases", "db", "p50", "p99", "max"
    );
    let mut a_rows = Vec::new();
    for &cases in &[1usize, 100, 500] {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("ckpt.db");
        let store = Store::open(&db).expect("open");
        store.save_form(&form, ConfigRev(1)).expect("save form");
        let ids = seed(&store, &form, cases);

        let mut times = Vec::new();
        for _ in 0..15 {
            grow_wal(&store, &db, ids[0], 4 * 1024 * 1024);
            times.push(time_checkpoint(&store));
        }
        let (p50, p99, max) = percentiles(times);
        println!(
            "{cases:>7}  {:>8.1}M  {p50:>12?}  {p99:>12?}  {max:>12?}",
            file_bytes(&db) as f64 / 1_048_576.0
        );
        a_rows.push((cases, p50));
    }
    if let (Some(first), Some(last)) = (a_rows.first(), a_rows.last()) {
        println!(
            "  {}x the cases changed checkpoint cost by {:.2}x",
            last.0 / first.0.max(1),
            last.1.as_secs_f64() / first.1.as_secs_f64().max(f64::MIN_POSITIVE)
        );
    }

    // ---------------------------------------------------------------- B: vs WAL size
    println!("\n-- B. checkpoint cost vs WAL size, 500-case database --");
    println!(
        "{:>9}  {:>12}  {:>12}  {:>14}",
        "wal", "p50", "max", "per MB"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("ckpt.db");
    let store = Store::open(&db).expect("open");
    store.save_form(&form, ConfigRev(1)).expect("save form");
    let ids = seed(&store, &form, 500);

    for &mb in &[1u64, 2, 4, 8, 16] {
        let mut times = Vec::new();
        for _ in 0..10 {
            grow_wal(&store, &db, ids[0], mb * 1024 * 1024);
            times.push(time_checkpoint(&store));
        }
        let (p50, _, max) = percentiles(times);
        println!(
            "{:>8}M  {p50:>12?}  {max:>12?}  {:>13.2?}",
            mb,
            Duration::from_secs_f64(p50.as_secs_f64() / mb as f64)
        );
    }

    // ---------------------------------------------------------------- C: what a user feels
    //
    // Restore the default so this section measures the shipped configuration.
    store
        .with_read(|c| {
            c.execute_batch("PRAGMA wal_autocheckpoint = 1000")
                .expect("restore autockpt")
        })
        .expect("with_read");
    let case_id = ids[250];
    let all = synthetic_case(&form, 250);

    // Three write patterns, because they put very different work into the WAL:
    //
    // - typing one field over and over rewrites the *same* page, and a checkpoint only
    //   copies each page's final version, so the WAL is cheap to check in;
    // - typing across the form touches a new page almost every save, which is what an
    //   abstractor actually does while working down a case, and is the honest worst case
    //   for a single-field write;
    // - a 300-field batch is Bench 2's shape, kept for comparison.
    let patterns: [(&str, usize, bool); 3] = [
        ("one field, same field", 1, false),
        ("one field, moving down the form", 1, true),
        ("300 fields (batch)", 300, false),
    ];

    for (label, changed, cycle) in patterns {
        let saves = if changed == 1 { 3000 } else { 300 };
        let mut times = Vec::with_capacity(saves);
        let mut stalls = Vec::new();
        let mut gaps = Vec::new();
        let mut last_stall: Option<usize> = None;

        for i in 0..saves {
            let changes: Vec<(FieldId, Value)> = if cycle {
                vec![all[i % all.len()].clone()]
            } else {
                all.iter().take(changed).cloned().collect()
            };
            let before = file_bytes(&wal_path(&db));
            let t = Instant::now();
            store
                .apply_local(case_id, &changes, CaseRev(1))
                .expect("apply_local");
            let d = t.elapsed();
            times.push(d);
            // A checkpoint is identifiable by the WAL shrinking, not by the clock.
            if file_bytes(&wal_path(&db)) < before {
                stalls.push(d);
                if let Some(prev) = last_stall {
                    gaps.push(i - prev);
                }
                last_stall = Some(i);
            }
        }

        let (p50, p99, max) = percentiles(times.clone());
        println!("\n-- C. {saves} saves, {label}, shipped configuration --");
        println!("  p50 {p50:?}  p99 {p99:?}  max {max:?}");

        // Two detectors, because the obvious one is build-dependent. Plain SQLite
        // truncates the WAL when it resets, so a checkpoint shows up as the file
        // shrinking; SQLCipher's build does not, so that signal silently vanishes and
        // would read as "no checkpoints" when what actually changed was the detector.
        // The second one counts what a user would notice — a save an order of magnitude
        // slower than the median — and works the same in both builds.
        let stall_floor = (p50 * 10).max(Duration::from_millis(1));
        let slow: Vec<Duration> = times.iter().copied().filter(|d| *d > stall_floor).collect();

        if stalls.is_empty() {
            println!("  WAL-shrink detector: no reset seen (expected under SQLCipher)");
        } else {
            let (sp50, _, smax) = percentiles(stalls.clone());
            let mean_gap = if gaps.is_empty() {
                0
            } else {
                gaps.iter().sum::<usize>() / gaps.len()
            };
            println!(
                "  WAL-shrink detector: {} resets, one every ~{mean_gap} saves; \
                 p50 {sp50:?}, worst {smax:?}",
                stalls.len()
            );
        }

        if slow.is_empty() {
            println!("  slow-save detector: none over {stall_floor:?}");
        } else {
            let (qp50, _, qmax) = percentiles(slow.clone());
            println!(
                "  slow-save detector: {} saves over {stall_floor:?}, one every ~{} saves; \
                 p50 {qp50:?}, worst {qmax:?} ({:.0}x the median save)",
                slow.len(),
                saves / slow.len(),
                qp50.as_secs_f64() / p50.as_secs_f64().max(f64::MIN_POSITIVE)
            );
        }
    }
    println!();
}

criterion_group!(benches, probe);
criterion_main!(benches);
