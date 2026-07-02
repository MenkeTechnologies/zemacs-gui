//! Edit ops — on-disk buffer transforms beside `text_tools.rs` that reshape *content* rather than
//! line order or line endings. Two surfaces:
//!
//! * **Align columns** — align each line on a delimiter (literal or regex), the way Emacs
//!   `align-regexp` lines up `=` signs, `:` map keys or `//` trailing comments into a single column.
//!   A dry-run preview reports how many lines would move; apply rewrites the file.
//! * **Comment toggle** — comment or uncomment a range of lines with the language-appropriate
//!   line-comment prefix (`//`, `#`, `--`, `;`, `"`), chosen by extension. If every non-blank line in
//!   the range is already commented it uncomments; otherwise it comments.
//!
//! Same host contract as the rest of the workbench: only the apply paths mutate the filesystem
//! (mirroring `text_tools`/`editor_tools`), and the front-end re-opens the file afterward so the
//! editor reloads it. The pure transforms are unit-tested directly.

use crate::project::{looks_binary, MAX_GREP_FILE_BYTES};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

// ── shared helpers ───────────────────────────────────────────────────────────────────────────────

/// Split `content` into logical lines, remembering the dominant EOL and whether a trailing newline
/// was present, so a rebuilt file matches the original's line-ending style.
fn split_lines(content: &str) -> (Vec<String>, &'static str, bool) {
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let had_trailing_nl = content.ends_with('\n');
    let mut lines: Vec<String> = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if had_trailing_nl {
        lines.pop();
    }
    (lines, eol, had_trailing_nl)
}

fn join_lines(lines: &[String], eol: &str, had_trailing_nl: bool) -> String {
    let mut out = lines.join(eol);
    if had_trailing_nl && !out.is_empty() {
        out.push_str(eol);
    }
    out
}

fn read_text_file(p: &PathBuf) -> Result<String, String> {
    if fs::metadata(p)
        .map(|m| m.len() > MAX_GREP_FILE_BYTES)
        .unwrap_or(true)
    {
        return Err("file too large or unreadable".into());
    }
    let bytes = fs::read(p).map_err(|e| e.to_string())?;
    if looks_binary(&bytes) {
        return Err("binary file".into());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ── align columns (align-regexp) ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct AlignOpts {
    /// The delimiter to align on: a literal substring unless `regex` is set.
    pub separator: String,
    pub regex: Option<bool>,
    /// When true, rewrite the file; otherwise a dry-run preview.
    pub apply: Option<bool>,
}

#[derive(Serialize)]
pub struct AlignResult {
    /// Number of lines whose text changed (got re-padded).
    pub changed_lines: usize,
    /// Number of lines that contained the delimiter (participated in the alignment).
    pub matched_lines: usize,
    pub differs: bool,
    pub applied: bool,
}

/// Align every line that contains `re` so the delimiter starts at the same column. The text before
/// the first delimiter is right-trimmed and padded (in characters, not bytes) to the widest such
/// prefix; the delimiter follows after one space, then the remainder with its leading whitespace
/// collapsed to a single space. Lines without the delimiter pass through unchanged. Pure — unit
/// tested. Returns the rebuilt text and the count of changed lines.
fn align_text(content: &str, re: &regex::Regex) -> (String, usize, usize) {
    let (lines, eol, had_trailing_nl) = split_lines(content);

    // First pass: find the widest left-hand side (char count) among matching lines.
    let mut parts: Vec<Option<(String, String, String)>> = Vec::with_capacity(lines.len());
    let mut max_left = 0usize;
    let mut matched = 0usize;
    for line in &lines {
        match re.find(line) {
            Some(m) if m.start() > 0 || !m.as_str().is_empty() => {
                let left = line[..m.start()].trim_end().to_string();
                let sep = m.as_str().to_string();
                let rest = line[m.end()..].trim_start().to_string();
                max_left = max_left.max(left.chars().count());
                matched += 1;
                parts.push(Some((left, sep, rest)));
            }
            _ => parts.push(None),
        }
    }

    // Second pass: re-pad each matching line to the common column.
    let mut changed = 0usize;
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (orig, part) in lines.iter().zip(parts.into_iter()) {
        match part {
            Some((left, sep, rest)) => {
                let pad = " ".repeat(max_left - left.chars().count());
                let rebuilt = if rest.is_empty() {
                    format!("{left}{pad} {sep}")
                } else {
                    format!("{left}{pad} {sep} {rest}")
                };
                if &rebuilt != orig {
                    changed += 1;
                }
                out.push(rebuilt);
            }
            None => out.push(orig.clone()),
        }
    }
    (join_lines(&out, eol, had_trailing_nl), changed, matched)
}

/// Preview (or apply) a column alignment over one file. Skips binary/oversized files.
#[tauri::command]
pub fn align_columns(path: String, opts: AlignOpts) -> Result<AlignResult, String> {
    let sep = opts.separator.trim();
    if sep.is_empty() {
        return Err("empty delimiter".into());
    }
    let pattern = if opts.regex.unwrap_or(false) {
        sep.to_string()
    } else {
        regex::escape(sep)
    };
    let re = regex::Regex::new(&pattern).map_err(|e| format!("bad pattern: {e}"))?;

    let p = PathBuf::from(&path);
    let content = read_text_file(&p)?;
    let (out, changed_lines, matched_lines) = align_text(&content, &re);
    let differs = out != content;
    let applied = opts.apply.unwrap_or(false) && differs;
    if applied {
        fs::write(&p, out.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(AlignResult {
        changed_lines,
        matched_lines,
        differs,
        applied,
    })
}

// ── comment toggle (language-aware line comments over a range) ─────────────────────────────────────

/// The line-comment prefix for a file extension, or `None` when the type has no known line comment.
pub(crate) fn comment_prefix(ext: &str) -> Option<&'static str> {
    let p = match ext {
        "rs" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cc" | "cpp"
        | "cxx" | "hpp" | "hh" | "java" | "swift" | "kt" | "kts" | "scala" | "dart" | "php"
        | "cs" | "zig" => "//",
        "py" | "rb" | "sh" | "bash" | "zsh" | "pl" | "pm" | "stk" | "yaml" | "yml" | "toml"
        | "conf" | "cfg" | "ini" | "r" | "jl" | "ex" | "exs" | "nim" | "tcl" | "mk" | "make"
        | "dockerfile" | "gitignore" => "#",
        "lua" | "sql" | "hs" | "elm" | "adb" | "ads" => "--",
        "lisp" | "el" | "clj" | "cljs" | "scm" | "rkt" | "asm" | "s" => ";",
        "vim" | "vimrc" => "\"",
        _ => return None,
    };
    Some(p)
}

#[derive(Serialize)]
pub struct CommentResult {
    /// True when the range ended up commented (false when it was uncommented).
    pub commented: bool,
    /// Number of lines whose text changed.
    pub changed_lines: usize,
    pub differs: bool,
    pub applied: bool,
    /// The prefix that was used, echoed for the UI note.
    pub prefix: String,
}

/// Toggle line comments over `lines[start..=end]` (0-based, clamped) using `prefix`. If every
/// non-blank line in the range already starts (after indentation) with the prefix, the range is
/// uncommented; otherwise it is commented. Comments are inserted at each line's own indentation
/// boundary so nesting is preserved; uncommenting removes the prefix and one following space if
/// present. Pure — returns (rebuilt lines, commented?, changed count). Unit tested.
fn toggle_comment(
    mut lines: Vec<String>,
    start: usize,
    end: usize,
    prefix: &str,
) -> (Vec<String>, bool, usize) {
    let end = end.min(lines.len().saturating_sub(1));
    if lines.is_empty() || start > end {
        return (lines, false, 0);
    }
    // Decide direction: uncomment only if every non-blank line in range is already commented.
    let all_commented = (start..=end)
        .map(|i| &lines[i])
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with(prefix));

    let mut changed = 0usize;
    for line in lines.iter_mut().take(end + 1).skip(start) {
        let indent_len = line.len() - line.trim_start().len();
        let (indent, body) = line.split_at(indent_len);
        if all_commented {
            if let Some(rest) = body.strip_prefix(prefix) {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                let rebuilt = format!("{indent}{rest}");
                if &rebuilt != line {
                    changed += 1;
                }
                *line = rebuilt;
            }
        } else if !body.is_empty() {
            *line = format!("{indent}{prefix} {body}");
            changed += 1;
        }
    }
    (lines, !all_commented, changed)
}

/// Comment or uncomment lines `start_line..=end_line` (1-based inclusive) of a file, using the
/// language's line-comment prefix. Skips binary/oversized files and files with no known comment
/// syntax.
#[tauri::command]
pub fn comment_toggle(
    path: String,
    start_line: usize,
    end_line: usize,
    apply: Option<bool>,
) -> Result<CommentResult, String> {
    let p = PathBuf::from(&path);
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let prefix = comment_prefix(&ext).ok_or_else(|| format!("no line comment for .{ext}"))?;

    let content = read_text_file(&p)?;
    let (lines, eol, had_trailing_nl) = split_lines(&content);
    // Clamp to 0-based indices within the file.
    let start = start_line.saturating_sub(1);
    let end = end_line.saturating_sub(1);
    if start >= lines.len() {
        return Err("start line past end of file".into());
    }
    let (new_lines, commented, changed_lines) = toggle_comment(lines, start, end, prefix);
    let out = join_lines(&new_lines, eol, had_trailing_nl);
    let differs = out != content;
    let applied = apply.unwrap_or(false) && differs;
    if applied {
        fs::write(&p, out.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(CommentResult {
        commented,
        changed_lines,
        differs,
        applied,
        prefix: prefix.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn align_pads_to_common_column() {
        let re = regex::Regex::new(&regex::escape("=")).unwrap();
        let (out, changed, matched) = align_text("a = 1\nfoo = 2\nxy = 3\n", &re);
        assert_eq!(out, "a   = 1\nfoo = 2\nxy  = 3\n");
        assert_eq!(matched, 3);
        // "a = 1" and "xy = 3" move; "foo = 2" already at the column.
        assert_eq!(changed, 2);
    }

    #[test]
    fn align_leaves_non_matching_lines() {
        let re = regex::Regex::new(&regex::escape(":")).unwrap();
        let (out, _, matched) = align_text("k: v\nno delim here\nlong: w\n", &re);
        // Padding lands before the delimiter, so the colons line up in one column.
        assert_eq!(out, "k    : v\nno delim here\nlong : w\n");
        assert_eq!(matched, 2);
    }

    #[test]
    fn align_regex_delimiter() {
        // Align on "=>" via regex.
        let re = regex::Regex::new(r"=>").unwrap();
        let (out, _, matched) = align_text("a => 1\nbbb => 2\n", &re);
        assert_eq!(out, "a   => 1\nbbb => 2\n");
        assert_eq!(matched, 2);
    }

    #[test]
    fn comment_prefix_by_ext() {
        assert_eq!(comment_prefix("rs"), Some("//"));
        assert_eq!(comment_prefix("py"), Some("#"));
        assert_eq!(comment_prefix("lua"), Some("--"));
        assert_eq!(comment_prefix("el"), Some(";"));
        assert_eq!(comment_prefix("vim"), Some("\""));
        assert_eq!(comment_prefix("unknownext"), None);
    }

    #[test]
    fn comment_then_uncomment_roundtrip() {
        let lines = v(&["    let x = 1;", "", "    let y = 2;"]);
        // Comment the whole range.
        let (commented, is_commented, changed) = toggle_comment(lines, 0, 2, "//");
        assert!(is_commented);
        assert_eq!(changed, 2); // blank line untouched
        assert_eq!(commented[0], "    // let x = 1;");
        assert_eq!(commented[2], "    // let y = 2;");
        assert_eq!(commented[1], "");

        // Toggle again → uncomment back to the original.
        let (back, is_commented2, _) = toggle_comment(commented, 0, 2, "//");
        assert!(!is_commented2);
        assert_eq!(back[0], "    let x = 1;");
        assert_eq!(back[2], "    let y = 2;");
    }

    #[test]
    fn comment_mixed_range_comments_all() {
        // One line already commented, one not → not all-commented → comment (prefix the bare line).
        let lines = v(&["// done", "todo"]);
        let (out, is_commented, _) = toggle_comment(lines, 0, 1, "//");
        assert!(is_commented);
        assert_eq!(out[0], "// // done");
        assert_eq!(out[1], "// todo");
    }

    #[test]
    fn align_and_comment_apply_on_disk() {
        let dir = tempdir();
        let f = dir.join("a.rs");
        std::fs::write(&f, "a = 1\nfoo = 2\n").unwrap();

        let r = align_columns(
            f.to_string_lossy().into(),
            AlignOpts {
                separator: "=".into(),
                regex: None,
                apply: Some(true),
            },
        )
        .unwrap();
        assert!(r.applied && r.differs);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a   = 1\nfoo = 2\n");

        // Comment lines 1-2 of a .rs file → "//" prefix.
        let c = comment_toggle(f.to_string_lossy().into(), 1, 2, Some(true)).unwrap();
        assert!(c.applied && c.commented);
        assert_eq!(c.prefix, "//");
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "// a   = 1\n// foo = 2\n"
        );
        cleanup(&dir);
    }

    // ── tiny tempdir helpers (no external dev-dep) ──
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zemacs-gui-eo-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
