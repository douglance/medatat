//! Intra-documentation link checking.
//!
//! `docs/` is the authoritative specification and it cross-references itself heavily. A
//! link that rots is worse than a missing one: it reads as a citation and leads nowhere,
//! so a reader concludes the claim is supported when nobody can reach the support.
//!
//! Two failures were found the day this was written, both introduced by renaming a heading
//! and a file in the same session that linked to them. Neither was visible to any other
//! check.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A link that does not resolve.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Broken {
    pub from: PathBuf,
    pub link: String,
    pub reason: String,
}

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} — {}",
            self.from.display(),
            self.link,
            self.reason
        )
    }
}

/// GitHub's heading-anchor algorithm, as far as it matters here: lowercase, drop
/// everything that is not alphanumeric / space / hyphen / underscore, then turn each
/// remaining space into a hyphen.
///
/// The last step is the one that is easy to get wrong. An em dash surrounded by spaces
/// leaves *two* spaces behind, which become *two* hyphens — collapsing them produces an
/// anchor that looks right and matches nothing.
fn slug(heading: &str) -> String {
    let kept: String = heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect();
    kept.replace(' ', "-")
}

/// Every anchor a Markdown file defines, from its ATX headings.
fn anchors(markdown: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(rest) = t.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if !title.is_empty() {
                out.insert(slug(title));
            }
        }
    }
    out
}

/// Every `](target)` in a Markdown file, in source order.
fn links(markdown: &str) -> Vec<String> {
    let bytes: Vec<char> = markdown.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == ']' && bytes[i + 1] == '(' {
            let mut j = i + 2;
            let mut depth = 1;
            let mut buf = String::new();
            while j < bytes.len() {
                match bytes[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                buf.push(bytes[j]);
                j += 1;
            }
            if depth == 0 && !buf.is_empty() {
                out.push(buf);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// The set of paths git tracks, or `None` when git cannot answer.
///
/// A link to a file that exists on *this* disk but is not in the repository resolves
/// locally and is broken for everyone else. That is not hypothetical: three documents
/// linked to `AGENTS.md`, which a global gitignore keeps out of the repo, and the lint
/// passed on the machine that wrote them and failed on the first clone. Checking
/// existence alone would have missed it forever.
fn tracked(root: &Path) -> Option<BTreeSet<PathBuf>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| {
                let p = root.join(s);
                p.canonicalize().unwrap_or(p)
            })
            .collect(),
    )
}

/// Checks every Markdown file under `root`, returning the links that do not resolve.
pub fn lint(root: &Path) -> Result<Vec<Broken>> {
    let mut files = Vec::new();
    collect(root, &mut files)?;
    files.sort();

    // git is asked about the repository root, not `docs/`, so links that climb out of the
    // docs tree are still judged correctly.
    let repo_root = root.parent().unwrap_or(root);
    let tracked_files = tracked(repo_root);

    let mut broken = Vec::new();
    for file in &files {
        let text =
            fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        for link in links(&text) {
            // External links and bare fragments to well-known schemes are not ours to
            // verify; a network check would make this lint flaky and slow.
            if link.starts_with("http://")
                || link.starts_with("https://")
                || link.starts_with("mailto:")
            {
                continue;
            }
            let (path_part, fragment) = match link.split_once('#') {
                Some((p, f)) => (p, Some(f)),
                None => (link.as_str(), None),
            };

            let target = if path_part.is_empty() {
                file.clone()
            } else {
                let dir = file.parent().unwrap_or(root);
                dir.join(path_part)
            };

            if !path_part.is_empty() && !target.exists() {
                broken.push(Broken {
                    from: file.clone(),
                    link: link.clone(),
                    reason: "no such file".into(),
                });
                continue;
            }

            if !path_part.is_empty()
                && let Some(t) = &tracked_files
                && !t.contains(&target.canonicalize().unwrap_or_else(|_| target.clone()))
            {
                broken.push(Broken {
                    from: file.clone(),
                    link: link.clone(),
                    reason: "the file exists here but is not tracked by git, so it is \
                             missing for anyone who clones the repository"
                        .into(),
                });
                continue;
            }

            if let Some(frag) = fragment {
                let target_text = fs::read_to_string(&target)
                    .with_context(|| format!("reading {}", target.display()))?;
                if !anchors(&target_text).contains(frag) {
                    broken.push(Broken {
                        from: file.clone(),
                        link: link.clone(),
                        reason: format!("no heading in {} makes this anchor", target.display()),
                    });
                }
            }
        }
    }
    Ok(broken)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_em_dash_leaves_two_hyphens() {
        // The exact case that made a hand-rolled checker disagree with GitHub.
        assert_eq!(
            slug("Bench 5 — the R13 margin at realistic scale (measured 2026-08-18)"),
            "bench-5--the-r13-margin-at-realistic-scale-measured-2026-08-18"
        );
    }

    #[test]
    fn slugs_drop_punctuation_and_backticks() {
        assert_eq!(slug("The `medatat` CLI"), "the-medatat-cli");
        assert_eq!(slug("10. Keyboard evidence"), "10-keyboard-evidence");
    }

    #[test]
    fn headings_inside_fences_are_not_anchors() {
        let md = "# Real\n\n```sh\n# not a heading\n```\n\n## Also real\n";
        let a = anchors(md);
        assert!(a.contains("real") && a.contains("also-real"));
        assert!(
            !a.contains("not-a-heading"),
            "a shell comment is not a heading"
        );
    }

    #[test]
    fn links_survive_parentheses_in_the_target() {
        let found = links("see [x](a.md#b) and [y](https://e.com/a_(b))");
        assert_eq!(found, vec!["a.md#b", "https://e.com/a_(b)"]);
    }

    #[test]
    fn a_missing_file_and_a_dead_anchor_are_both_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.md"),
            "# Title\n\n[gone](b.md)\n[bad](a.md#nope)\n",
        )
        .unwrap();
        let broken = lint(dir.path()).unwrap();
        assert_eq!(broken.len(), 2, "{broken:?}");
        assert!(broken.iter().any(|b| b.reason == "no such file"));
        assert!(broken.iter().any(|b| b.reason.contains("anchor")));
    }

    #[test]
    fn a_resolving_tree_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.md"),
            "# One\n\n[to b](b.md#two-things)\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.md"), "## Two things\n").unwrap();
        assert!(lint(dir.path()).unwrap().is_empty());
    }

    /// The case that motivated the check: the target is present on disk, so `exists()`
    /// is happy, but git does not track it and a clone would not have it.
    #[test]
    fn a_file_that_exists_but_is_untracked_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git")
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git on this machine; the lint degrades to existence-only anyway
        }
        let docs = repo.join("docs");
        fs::create_dir_all(&docs).unwrap();
        fs::write(repo.join("UNTRACKED.md"), "# nope\n").unwrap();
        fs::write(docs.join("a.md"), "# A\n\n[x](../UNTRACKED.md)\n").unwrap();
        git(&["add", "docs/a.md"]);

        let broken = lint(&docs).unwrap();
        assert_eq!(broken.len(), 1, "{broken:?}");
        assert!(
            broken[0].reason.contains("not tracked"),
            "got: {}",
            broken[0].reason
        );
    }

    #[test]
    fn external_links_are_not_checked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "[x](https://nope.invalid/a#b)\n").unwrap();
        assert!(lint(dir.path()).unwrap().is_empty());
    }
}
