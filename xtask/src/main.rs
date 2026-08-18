//! Repository automation.
//!
//! ```bash
//! cargo xtask lint-no-spinner
//! cargo xtask bump-gpui
//! ```
//!
//! `anyhow`, `unwrap`, and `expect` are permitted here — see AGENTS.md.

mod lint_no_spinner;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "xtask", about = "medatat repository automation", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// R15: fail if a spinner, progress bar, skeleton, shimmer, or "Loading…" appears in
    /// `medatat-ui`.
    LintNoSpinner,
    /// Bump the pinned `gpui` revisions in the root Cargo.toml, then build.
    BumpGpui,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::LintNoSpinner => lint_no_spinner_cmd(&workspace_root()),
        Command::BumpGpui => bump_gpui_cmd(),
    };

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The repository root, derived from this crate's manifest rather than the current
/// directory, so the lint reports the same thing wherever it is invoked from.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Returns whether the tree is clean. `false` becomes exit code 1.
fn lint_no_spinner_cmd(root: &Path) -> Result<bool> {
    let ui_src = root.join("crates").join("medatat-ui").join("src");

    if !ui_src.is_dir() {
        // M0 has not built the UI crate yet. A lint that fails on absence would just be
        // noise in CI until then.
        println!("no UI crate yet, skipping");
        return Ok(true);
    }

    let violations = lint_no_spinner::lint(&ui_src)?;
    if violations.is_empty() {
        println!("R15 clean: {}", ui_src.display());
        return Ok(true);
    }

    eprintln!(
        "R15 violated: {} occurrence(s) of a banned loading affordance in {}",
        violations.len(),
        ui_src.display()
    );
    for v in &violations {
        eprintln!("{v}");
    }
    eprintln!(
        "\nR15 says \"Ever\". Background status belongs in peripheral chrome — a count in \
         the window frame, a footer line — never where content goes. See \
         docs/05-UI-SPEC.md and docs/00-REQUIREMENTS.md."
    );
    Ok(false)
}

fn bump_gpui_cmd() -> Result<bool> {
    // TODO(M0): implement once the `gpui` revs are pinned for real. Until then this
    // reports what it would do rather than rewriting a manifest that still says
    // REPLACE_AT_M0.
    println!(
        "cargo xtask bump-gpui (not implemented yet)\n\
         \n\
         Intended behaviour:\n\
         1. Resolve the current head of zed-industries/zed and longbridge/gpui-component.\n\
         2. Rewrite the `rev` of [workspace.dependencies.gpui], .gpui_platform,\n\
         and .gpui-component in the root Cargo.toml. Pin by rev, never by\n\
         branch (docs/adr/0003-gpui-component.md).\n\
         3. cargo build -p medatat-ui on this platform, and report the new revs.\n\
         4. Leave Cargo.lock updated and staged; a human commits it.\n\
         \n\
         Nothing was changed."
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_missing_ui_crate_is_a_skip_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            lint_no_spinner_cmd(dir.path()).unwrap(),
            "an absent UI crate must skip, not fail"
        );
    }

    #[test]
    fn a_clean_ui_crate_passes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/medatat-ui/src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("status.rs"),
            "pub fn unsynced(n: usize) -> String { format!(\"{n} unsynced\") }\n",
        )
        .unwrap();
        assert!(lint_no_spinner_cmd(dir.path()).unwrap());
    }

    #[test]
    fn a_violating_ui_crate_fails() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/medatat-ui/src/form");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("view.rs"), "let msg = \"Loading...\";\n").unwrap();
        assert!(
            !lint_no_spinner_cmd(dir.path()).unwrap(),
            "a \"Loading...\" string must fail the lint"
        );
    }

    #[test]
    fn bump_gpui_changes_nothing_and_succeeds() {
        assert!(bump_gpui_cmd().unwrap());
    }

    #[test]
    fn the_cli_parses_both_subcommands() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        assert!(matches!(
            Cli::parse_from(["xtask", "lint-no-spinner"]).command,
            Command::LintNoSpinner
        ));
        assert!(matches!(
            Cli::parse_from(["xtask", "bump-gpui"]).command,
            Command::BumpGpui
        ));
    }
}
