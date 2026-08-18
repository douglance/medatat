//! R15 enforcement: no spinner, progress bar, skeleton, shimmer, or "Loading…" anywhere in
//! `medatat-ui`.
//!
//! R15 says "Ever", so it is enforced mechanically rather than by review discipline
//! (`docs/05-UI-SPEC.md`). The rule is satisfiable because it is structurally true: reads
//! are local and take microseconds, and the caseload is synced before the user opens
//! anything.
//!
//! The check ignores comments — a comment explaining *why* there is no spinner must not
//! trip the lint that keeps it that way — but it deliberately does **not** ignore string
//! literals, because `"Loading…"` in a string is precisely the violation being hunted.

use anyhow::{Context, Result};
use std::fmt;
use std::path::{Path, PathBuf};

/// Matched case-insensitively against non-comment source text.
pub const BANNED: &[&str] = &[
    "spinner",
    "loading...",
    "progressbar",
    "progress_bar",
    "skeleton",
    "shimmer",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub pattern: &'static str,
    pub text: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: R15 violation: {:?}\n    {}",
            self.file.display(),
            self.line,
            self.column,
            self.pattern,
            self.text.trim()
        )
    }
}

/// Scans every `.rs` file under `root`, recursively. A missing `root` yields no
/// violations — the caller decides whether that is a skip or a failure.
pub fn lint(root: &Path) -> Result<Vec<Violation>> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files)?;
    // Sorted so the report is stable across platforms and reruns.
    files.sort();

    let mut out = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        out.extend(scan(&file, &source));
    }
    Ok(out)
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("in {}", dir.display()))?
            .path();
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Finds banned words in `source`, ignoring anything inside a comment.
pub fn scan(file: &Path, source: &str) -> Vec<Violation> {
    let masked = mask_comments(source).to_ascii_lowercase();
    let mut out = Vec::new();

    for pattern in BANNED {
        let mut from = 0usize;
        while let Some(hit) = masked[from..].find(pattern) {
            let at = from + hit;
            let (line, column) = line_col(source, at);
            out.push(Violation {
                file: file.to_path_buf(),
                line,
                column,
                pattern,
                text: source.lines().nth(line - 1).unwrap_or_default().to_string(),
            });
            from = at + pattern.len();
        }
    }

    out.sort_by_key(|v| (v.line, v.column));
    out
}

/// Replaces every comment byte with a space, preserving byte offsets so positions still
/// map back onto the original source.
///
/// Tracks string and character literals so that a `//` inside a string is not mistaken for
/// a comment, and a `"` inside a comment does not open a string.
fn mask_comments(source: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Code,
        LineComment,
        BlockComment(u32),
        Str,
        RawStr(usize),
    }

    let b = source.as_bytes();
    let mut out: Vec<u8> = b.to_vec();
    let mut mode = Mode::Code;
    let mut i = 0usize;

    while i < b.len() {
        match mode {
            Mode::Code => {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                    mode = Mode::LineComment;
                    blank(&mut out, i, 2);
                    i += 2;
                } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    mode = Mode::BlockComment(1);
                    blank(&mut out, i, 2);
                    i += 2;
                } else if let Some((hashes, skip)) = raw_string_start(b, i) {
                    mode = Mode::RawStr(hashes);
                    i += skip;
                } else if b[i] == b'"' {
                    mode = Mode::Str;
                    i += 1;
                } else if let Some(len) = char_literal_len(b, i) {
                    // A char literal, not the start of a lifetime. Skipping it wholesale
                    // means '"' cannot open a phantom string.
                    i += len;
                } else {
                    i += 1;
                }
            }
            Mode::LineComment => {
                if b[i] == b'\n' {
                    mode = Mode::Code;
                } else {
                    out[i] = b' ';
                }
                i += 1;
            }
            Mode::BlockComment(depth) => {
                if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    blank(&mut out, i, 2);
                    i += 2;
                    // Rust block comments nest.
                    mode = if depth <= 1 {
                        Mode::Code
                    } else {
                        Mode::BlockComment(depth - 1)
                    };
                } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    blank(&mut out, i, 2);
                    i += 2;
                    mode = Mode::BlockComment(depth + 1);
                } else {
                    if b[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            Mode::Str => {
                if b[i] == b'\\' {
                    i += 2;
                } else {
                    if b[i] == b'"' {
                        mode = Mode::Code;
                    }
                    i += 1;
                }
            }
            Mode::RawStr(hashes) => {
                if b[i] == b'"' && b[i + 1..].iter().take(hashes).all(|&c| c == b'#') {
                    mode = Mode::Code;
                    i += 1 + hashes;
                } else {
                    i += 1;
                }
            }
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

fn blank(out: &mut [u8], at: usize, len: usize) {
    for byte in out.iter_mut().skip(at).take(len) {
        *byte = b' ';
    }
}

/// `r"…"`, `r#"…"#`, and friends. Returns the hash count and how far to advance.
fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if b[i] != b'r' {
        return None;
    }
    // `br"…"` is handled by the caller's `b` byte falling through to this on the next pass.
    let mut hashes = 0usize;
    while i + 1 + hashes < b.len() && b[i + 1 + hashes] == b'#' {
        hashes += 1;
    }
    if b.get(i + 1 + hashes) == Some(&b'"') {
        Some((hashes, hashes + 2))
    } else {
        None
    }
}

/// Length of a character literal starting at `i`, or `None` if this quote opens a lifetime.
fn char_literal_len(b: &[u8], i: usize) -> Option<usize> {
    if b[i] != b'\'' {
        return None;
    }
    if b.get(i + 1) == Some(&b'\\') {
        // Escapes are at most `'\u{10FFFF}'`.
        // Search past the escaped byte, so `'\''` closes on its real quote.
        let end = (i + 3..(i + 12).min(b.len())).find(|&j| b[j] == b'\'')?;
        return Some(end - i + 1);
    }
    // A single-byte char followed by a closing quote. Multi-byte chars fall through and are
    // treated as code, which is harmless.
    if b.get(i + 2) == Some(&b'\'') {
        Some(3)
    } else {
        None
    }
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset.min(source.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rfind('\n').map_or(offset, |nl| offset - nl - 1) + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn a_clean_tree_passes() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "status.rs",
            r#"
            //! Peripheral status only -- never a spinner, never a progress bar.
            /// Renders the "N unsynced" count. See R15.
            pub fn unsynced_label(n: usize) -> String {
                format!("{n} unsynced")
            }
            "#,
        );
        write(
            dir.path(),
            "widgets/time_input.rs",
            "pub const HINT: &str = \"HH:MM\"; // no skeleton here, obviously\n",
        );
        assert_eq!(lint(dir.path()).unwrap(), vec![]);
    }

    #[test]
    fn a_spinner_in_code_is_caught() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "form/view.rs", "fn show_spinner() {}\n");
        let found = lint(dir.path()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pattern, "spinner");
        assert_eq!(found[0].line, 1);
    }

    #[test]
    fn loading_text_in_a_string_literal_is_caught() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "fn label() -> &'static str { \"Loading... please wait\" }\n",
        );
        let found = lint(dir.path()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pattern, "loading...");
    }

    #[test]
    fn every_banned_word_is_caught_case_insensitively() {
        for pattern in BANNED {
            let dir = tempfile::tempdir().unwrap();
            write(
                dir.path(),
                "a.rs",
                &format!("let x = \"{}\";\n", pattern.to_uppercase()),
            );
            let found = lint(dir.path()).unwrap();
            assert_eq!(found.len(), 1, "missed {pattern}");
            assert_eq!(found[0].pattern, *pattern);
        }
    }

    #[test]
    fn comments_are_not_violations() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "// no spinner, no skeleton, no shimmer\n\
             /* progressbar and progress_bar are banned too */\n\
             /* nested /* Loading... */ still a comment */\n\
             fn ok() {}\n",
        );
        assert_eq!(lint(dir.path()).unwrap(), vec![]);
    }

    #[test]
    fn a_comment_marker_inside_a_string_does_not_hide_the_rest_of_the_line() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "let url = \"https://example.test\"; let bad = \"shimmer\";\n",
        );
        let found = lint(dir.path()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pattern, "shimmer");
    }

    #[test]
    fn a_quote_in_a_char_literal_does_not_open_a_string() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "fn q(c: char) -> bool { c == '\"' } // spinner mentioned only in a comment\n",
        );
        assert_eq!(lint(dir.path()).unwrap(), vec![]);
    }

    #[test]
    fn raw_strings_are_still_scanned() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "let s = r#\"Skeleton\"#;\n");
        let found = lint(dir.path()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pattern, "skeleton");
    }

    #[test]
    fn nested_directories_are_walked() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a/b/c/deep.rs",
            "// fine\nlet x = \"Shimmer\";\n",
        );
        write(dir.path(), "a/b/notes.md", "shimmer shimmer shimmer\n");
        let found = lint(dir.path()).unwrap();
        assert_eq!(found.len(), 1, "only .rs files are scanned: {found:?}");
        assert_eq!(found[0].line, 2);
    }

    #[test]
    fn a_missing_directory_yields_no_violations() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(lint(&dir.path().join("nope")).unwrap(), vec![]);
    }

    #[test]
    fn positions_point_at_the_offending_line() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "fn a() {}\nfn b() {}\nlet x = \"spinner\";\n",
        );
        let found = lint(dir.path()).unwrap();
        assert_eq!(found[0].line, 3);
        assert!(found[0].text.contains("spinner"));
    }
}
