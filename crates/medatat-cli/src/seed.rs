//! Synthetic corpus generation.
//!
//! **Synthetic data only.** Never point this at a real clinical source — see
//! `docs/12-PHI-READINESS.md`.

use anyhow::{Result, bail};
use std::path::PathBuf;

pub fn run(args: &[String]) -> Result<()> {
    let mut cases = 1_000usize;
    let mut fields = 1_000usize;
    let mut out = PathBuf::from("./corpus");

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--cases" => cases = next_num(&mut it, "--cases")?,
            "--fields" => fields = next_num(&mut it, "--fields")?,
            "--out" => {
                out = PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--out needs a path"))?,
                )
            }
            "-h" | "--help" => {
                println!(
                    "medatat seed --cases <n> --fields <n> --out <dir>\n\n\
                     Generates a synthetic corpus. Run the 1,000-case extrapolation before\n\
                     committing to the full 100,000-case corpus (docs/08-MILESTONES.md M1)."
                );
                return Ok(());
            }
            other => bail!("unknown flag {other}"),
        }
    }

    let stats = medatat_testkit::seed_corpus(cases, fields, &out)?;
    println!(
        "seeded {} cases x {} fields = {} values in {:.1}s ({:.1} MB)",
        stats.cases,
        fields,
        stats.values,
        stats.elapsed.as_secs_f64(),
        stats.bytes as f64 / 1_048_576.0
    );

    // The point of the small run is the extrapolation, so print it rather than making the
    // operator do the arithmetic.
    if cases < 100_000 {
        let factor = 100_000.0 / cases as f64;
        println!(
            "extrapolated to 100,000 cases: ~{:.0} min, ~{:.1} GB",
            stats.elapsed.as_secs_f64() * factor / 60.0,
            stats.bytes as f64 * factor / 1_073_741_824.0
        );
    }
    Ok(())
}

fn next_num<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<usize> {
    it.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a number"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("{flag} must be a number"))
}
