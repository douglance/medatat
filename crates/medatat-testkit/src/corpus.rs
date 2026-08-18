//! Bulk synthetic corpus generation.
//!
//! Bench 4 has to answer a question before M7 commits to it: is seeding 100k cases × 1000
//! values feasible at all? That question is only answerable if generation throughput is
//! measured, so [`seed_corpus`] reports what it did rather than just succeeding.

use crate::cases::{synthetic_case_id, synthetic_mrn, synthetic_name, synthetic_value_rows};
use crate::forms::synthetic_form_seeded;
use anyhow::{Context, Result};
use medatat_core::def::FormDef;
use medatat_core::ids::{CaseId, CaseRev, FormId};
use medatat_core::wire::ValueRow;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// The form definition every case in a corpus shares.
pub const FORM_FILE: &str = "form.json";
/// One JSON object per line, so the file streams without holding the corpus in memory.
pub const CASES_FILE: &str = "cases.jsonl";

/// What a corpus run actually produced. Reported, not just returned, so a nightly bench
/// can extrapolate to the full M7 corpus and record the estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusStats {
    pub cases: usize,
    pub values: usize,
    pub elapsed: Duration,
    pub bytes: u64,
}

impl CorpusStats {
    /// Cases per second, or 0.0 for an instantaneous run.
    pub fn cases_per_sec(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 { self.cases as f64 / s } else { 0.0 }
    }

    /// How long the same rate would take for `target` cases. The number Bench 4 exists to
    /// produce: if this says days, that is a finding, not an inconvenience.
    pub fn extrapolate(&self, target: usize) -> Duration {
        if self.cases == 0 {
            return Duration::ZERO;
        }
        self.elapsed.mul_f64(target as f64 / self.cases as f64)
    }
}

/// One case as written to `cases.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusCase {
    pub case_id: CaseId,
    pub form_id: FormId,
    pub mrn: String,
    /// Synthetic, from a fixed word list. Never a real name.
    pub patient_name: String,
    pub rev: CaseRev,
    pub values: Vec<ValueRow>,
}

/// Writes a synthetic corpus into `out`, which is created as a directory holding
/// [`FORM_FILE`] and [`CASES_FILE`].
///
/// Deterministic: the same `(cases, fields_per_case)` always produces the same bytes.
pub fn seed_corpus(cases: usize, fields_per_case: usize, out: &Path) -> Result<CorpusStats> {
    seed_corpus_seeded(cases, fields_per_case, out, crate::forms::DEFAULT_SEED)
}

/// [`seed_corpus`] with an explicit seed, so two corpora can be generated side by side.
pub fn seed_corpus_seeded(
    cases: usize,
    fields_per_case: usize,
    out: &Path,
    seed: u64,
) -> Result<CorpusStats> {
    let started = Instant::now();

    std::fs::create_dir_all(out)
        .with_context(|| format!("creating corpus directory {}", out.display()))?;

    let form = synthetic_form_seeded(fields_per_case, seed);

    let form_path = out.join(FORM_FILE);
    let form_json = serde_json::to_vec_pretty(&form).context("serialising the form")?;
    std::fs::write(&form_path, &form_json)
        .with_context(|| format!("writing {}", form_path.display()))?;

    let cases_path = out.join(CASES_FILE);
    let file =
        File::create(&cases_path).with_context(|| format!("creating {}", cases_path.display()))?;
    let mut w = BufWriter::new(file);

    let mut values = 0usize;
    let mut bytes = form_json.len() as u64;

    for i in 0..cases {
        // Derive each case's seed from the corpus seed so a single case can be regenerated
        // on its own, without replaying the whole corpus.
        let case_seed = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let rows = synthetic_value_rows(&form, case_seed, CaseRev(1));
        values += rows.len();

        let record = CorpusCase {
            case_id: synthetic_case_id(case_seed),
            form_id: form.form_id,
            mrn: synthetic_mrn(case_seed),
            patient_name: synthetic_name(case_seed),
            rev: CaseRev(1),
            values: rows,
        };

        let line = serde_json::to_vec(&record).context("serialising a case")?;
        w.write_all(&line)
            .and_then(|()| w.write_all(b"\n"))
            .with_context(|| format!("writing case {i}"))?;
        bytes += line.len() as u64 + 1;
    }

    w.flush().context("flushing the corpus")?;

    Ok(CorpusStats {
        cases,
        values,
        elapsed: started.elapsed(),
        bytes,
    })
}

/// Reads back the form written by [`seed_corpus`].
///
/// `FormDef` skips its derived index on the wire, so `finalize` has to run after
/// deserialisation — forgetting it yields a form whose `idx_of` returns `None` for every
/// field, which is a confusing failure a long way from its cause.
pub fn read_corpus_form(dir: &Path) -> Result<FormDef> {
    let path = dir.join(FORM_FILE);
    let bytes = std::fs::read(&path).with_context(|| format!("reading form {}", path.display()))?;
    let mut form: FormDef = serde_json::from_slice(&bytes).context("parsing the form")?;
    form.finalize();
    Ok(form)
}

/// Reads the cases written by [`seed_corpus`]. Loads the whole file; the streaming path is
/// for the seeder, not for tests.
pub fn read_corpus_cases(dir: &Path) -> Result<Vec<CorpusCase>> {
    let path = dir.join(CASES_FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| serde_json::from_str(line).with_context(|| format!("parsing case {i}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::validate::validate;

    #[test]
    fn corpus_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let stats = seed_corpus(10, 40, dir.path()).unwrap();

        assert_eq!(stats.cases, 10);
        assert_eq!(stats.values, 400);
        assert!(stats.bytes > 0);

        let form = read_corpus_form(dir.path()).unwrap();
        assert_eq!(form.field_count(), 40);

        let cases = read_corpus_cases(dir.path()).unwrap();
        assert_eq!(cases.len(), 10);
        assert!(cases.iter().all(|c| c.form_id == form.form_id));
    }

    #[test]
    fn corpus_values_validate_against_the_written_form() {
        let dir = tempfile::tempdir().unwrap();
        seed_corpus(5, 60, dir.path()).unwrap();

        let form = read_corpus_form(dir.path()).unwrap();
        for case in read_corpus_cases(dir.path()).unwrap() {
            for row in &case.values {
                let idx = form.idx_of(row.field_id).expect("field must be placed");
                let sf = form.field_at(idx).expect("index must resolve");
                assert_eq!(validate(&sf.field, &row.value), Ok(()));
            }
        }
    }

    #[test]
    fn corpus_is_deterministic() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        seed_corpus(8, 30, a.path()).unwrap();
        seed_corpus(8, 30, b.path()).unwrap();

        for f in [FORM_FILE, CASES_FILE] {
            assert_eq!(
                std::fs::read(a.path().join(f)).unwrap(),
                std::fs::read(b.path().join(f)).unwrap(),
                "{f} differs between runs"
            );
        }
    }

    #[test]
    fn case_ids_and_mrns_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        seed_corpus(200, 10, dir.path()).unwrap();
        let cases = read_corpus_cases(dir.path()).unwrap();

        let ids: std::collections::HashSet<_> = cases.iter().map(|c| c.case_id).collect();
        assert_eq!(ids.len(), 200, "duplicate case ids in a corpus");
        let mrns: std::collections::HashSet<_> = cases.iter().map(|c| &c.mrn).collect();
        assert_eq!(mrns.len(), 200, "duplicate MRNs in a corpus");
    }

    #[test]
    fn extrapolation_scales_linearly() {
        let stats = CorpusStats {
            cases: 1_000,
            values: 1_000_000,
            elapsed: Duration::from_secs(2),
            bytes: 0,
        };
        assert_eq!(stats.extrapolate(100_000), Duration::from_secs(200));
        assert_eq!(stats.cases_per_sec(), 500.0);
    }

    #[test]
    fn an_empty_corpus_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let stats = seed_corpus(0, 10, dir.path()).unwrap();
        assert_eq!(stats.cases, 0);
        assert_eq!(stats.values, 0);
        assert!(read_corpus_cases(dir.path()).unwrap().is_empty());
        assert_eq!(stats.extrapolate(10), Duration::ZERO);
    }
}
