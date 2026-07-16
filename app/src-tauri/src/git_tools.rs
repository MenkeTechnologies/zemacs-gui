//! Git tools — a deeper git surface beside `project.rs`'s status/branch/diff: per-line **blame**,
//! per-file **history** (log) with **show-commit** diffs, and working-tree **staging** (stage /
//! unstage / discard). Plus a general **two-file compare** via `git diff --no-index`, which works even
//! outside a repository. Same contract as the rest of the host: navigation opens `:open <path>:<line>`
//! in the PTY; only the staging commands and `discard` mutate git state (all confirmed in the UI).

use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// Run `git -C <dir> <args…>`, returning stdout on success or the trimmed stderr as the error.
fn git_in(dir: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git not available: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The directory to run git in for a given file path (its parent, so `-C` lands inside the repo).
fn dir_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

// ── dep-free civil date (unix seconds → YYYY-MM-DD, UTC) ─────────────────────────────────────────
// Howard Hinnant's days-from-civil inverse; avoids pulling in chrono just to label a commit date.
fn ymd_from_unix(secs: i64) -> (i64, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn fmt_date(secs: i64) -> String {
    let (y, m, d) = ymd_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

// ── blame (per-line author / commit / date) ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct BlameLine {
    /// 1-based line number in the current file.
    pub line: usize,
    /// Abbreviated commit hash (first 8 chars), or "00000000" for an uncommitted line.
    pub commit: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    /// First line of the commit message.
    pub summary: String,
}

/// Parse `git blame --line-porcelain` output. Each source line begins with a `<40-hex> <orig> <final>`
/// header; a commit's metadata (author / summary / author-time) is emitted once, then referenced by
/// hash for later lines — so metadata is cached by hash. The content line itself starts with a TAB.
fn parse_blame(porcelain: &str) -> Vec<BlameLine> {
    use std::collections::HashMap;
    let mut meta: HashMap<String, (String, String, i64)> = HashMap::new();
    let mut out = Vec::new();

    let mut cur_hash = String::new();
    let mut cur_author = String::new();
    let mut cur_summary = String::new();
    let mut cur_time = 0i64;
    let mut line_no = 0usize;

    for raw in porcelain.lines() {
        if let Some(rest) = raw.strip_prefix('\t') {
            // Content line: emit a record for the current commit at this final line number.
            let _ = rest;
            let (author, summary, time) = meta.get(&cur_hash).cloned().unwrap_or((
                cur_author.clone(),
                cur_summary.clone(),
                cur_time,
            ));
            let short: String = cur_hash.chars().take(8).collect();
            out.push(BlameLine {
                line: line_no,
                commit: short,
                author,
                date: fmt_date(time),
                summary,
            });
            continue;
        }
        if let Some(a) = raw.strip_prefix("author ") {
            cur_author = a.to_string();
        } else if let Some(s) = raw.strip_prefix("summary ") {
            cur_summary = s.to_string();
        } else if let Some(t) = raw.strip_prefix("author-time ") {
            cur_time = t.trim().parse().unwrap_or(0);
        } else {
            // A header line: `<hash> <origLine> <finalLine> [<numLines>]`.
            let mut it = raw.split(' ');
            if let (Some(h), _, Some(fin)) = (it.next(), it.next(), it.next()) {
                if h.len() >= 7 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                    cur_hash = h.to_string();
                    line_no = fin.parse().unwrap_or(line_no + 1);
                    if !meta.contains_key(&cur_hash) {
                        // Placeholder; the author/summary/time fields follow on the next lines and are
                        // committed to the cache when the content line is reached.
                        meta.insert(cur_hash.clone(), (String::new(), String::new(), 0));
                    }
                }
            }
        }
        // Once the metadata fields for a freshly-seen commit are parsed, refresh the cache entry.
        if !cur_hash.is_empty() {
            meta.insert(
                cur_hash.clone(),
                (cur_author.clone(), cur_summary.clone(), cur_time),
            );
        }
    }
    out
}

/// Per-line blame for a tracked file (`git blame --line-porcelain`).
#[tauri::command]
pub fn git_blame(path: String) -> Result<Vec<BlameLine>, String> {
    let dir = dir_of(&path);
    let out = git_in(&dir, &["blame", "--line-porcelain", "--", &path])?;
    Ok(parse_blame(&out))
}

// ── per-file history (log) + show-commit ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Commit {
    pub hash: String,
    /// Abbreviated hash (first 8 chars) for the badge column.
    pub short: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    pub summary: String,
}

/// The commit history touching a single file, newest first. Uses an ASCII-unit-separator format so
/// summaries with any punctuation parse cleanly.
#[tauri::command]
pub fn git_log_file(path: String, limit: Option<usize>) -> Result<Vec<Commit>, String> {
    let dir = dir_of(&path);
    let n = limit.unwrap_or(200).min(2000);
    let max = format!("-n{n}");
    // %H hash, %an author, %at author-time (unix), %s subject — joined by \x1f, rows by \n.
    let out = git_in(
        &dir,
        &[
            "log",
            &max,
            "--follow",
            "--format=%H\x1f%an\x1f%at\x1f%s",
            "--",
            &path,
        ],
    )?;
    let mut commits = Vec::new();
    for line in out.lines() {
        let mut it = line.split('\x1f');
        let (Some(hash), Some(author), Some(at), Some(summary)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let time: i64 = at.trim().parse().unwrap_or(0);
        commits.push(Commit {
            hash: hash.to_string(),
            short: hash.chars().take(8).collect(),
            author: author.to_string(),
            date: fmt_date(time),
            summary: summary.to_string(),
        });
    }
    Ok(commits)
}

/// The diff a single commit introduced for one file (`git show <hash> -- <path>`), for a preview pane.
#[tauri::command]
pub fn git_show(path: String, hash: String) -> Result<String, String> {
    if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.len() < 4 {
        return Err("bad commit hash".into());
    }
    let dir = dir_of(&path);
    git_in(&dir, &["show", &hash, "--", &path])
}

// ── staging (stage / unstage / discard) ──────────────────────────────────────────────────────────

/// Stage a path (`git add -- <path>`).
#[tauri::command]
pub fn git_stage(path: String) -> Result<(), String> {
    git_in(&dir_of(&path), &["add", "--", &path]).map(|_| ())
}

/// Unstage a path (`git reset -q HEAD -- <path>`).
#[tauri::command]
pub fn git_unstage(path: String) -> Result<(), String> {
    git_in(&dir_of(&path), &["reset", "-q", "HEAD", "--", &path]).map(|_| ())
}

/// Discard working-tree changes for a path (`git checkout -- <path>`). Confirmed in the UI first — this
/// is destructive. Untracked files aren't affected by checkout (git leaves them; the UI notes this).
#[tauri::command]
pub fn git_discard(path: String) -> Result<(), String> {
    git_in(&dir_of(&path), &["checkout", "--", &path]).map(|_| ())
}

// ── two-file compare (git diff --no-index, works outside a repo) ─────────────────────────────────

/// A unified diff between two arbitrary files. `git diff --no-index` exits 1 when the files differ,
/// which is the normal case — so a status of 0 (identical) or 1 (differs) is success; anything else is
/// a real error. Runs without `-C` so it works on files in different repos or none at all.
#[tauri::command]
pub fn diff_files(left: String, right: String) -> Result<String, String> {
    let out = Command::new("git")
        .args(["diff", "--no-index", "--", &left, &right])
        .output()
        .map_err(|e| format!("git not available: {e}"))?;
    match out.status.code() {
        Some(0) | Some(1) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_known_epochs() {
        assert_eq!(fmt_date(0), "1970-01-01");
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(fmt_date(1_609_459_200), "2021-01-01");
        // 2000-02-29 (leap day) 12:00 UTC = 951825600
        assert_eq!(fmt_date(951_825_600), "2000-02-29");
    }

    #[test]
    fn parse_blame_extracts_line_commit_author() {
        // A minimal two-line porcelain stream: one commit, header then metadata then TAB content.
        let porcelain = "\
abcdef1234567890abcdef1234567890abcdef12 1 1 2
author Jane Doe
author-time 1609459200
summary initial commit
	first line
abcdef1234567890abcdef1234567890abcdef12 2 2
	second line
";
        let lines = parse_blame(porcelain);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, 1);
        assert_eq!(lines[0].commit, "abcdef12");
        assert_eq!(lines[0].author, "Jane Doe");
        assert_eq!(lines[0].date, "2021-01-01");
        assert_eq!(lines[0].summary, "initial commit");
        // Second line references the same commit by hash only; metadata carries over from the cache.
        assert_eq!(lines[1].line, 2);
        assert_eq!(lines[1].author, "Jane Doe");
        assert_eq!(lines[1].summary, "initial commit");
    }

    #[test]
    fn diff_files_reports_difference() {
        let dir = std::env::temp_dir().join(format!("zmax-gui-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "one\ntwo\n").unwrap();
        std::fs::write(&b, "one\nTWO\n").unwrap();
        let d = diff_files(a.to_string_lossy().into(), b.to_string_lossy().into()).unwrap();
        // A real difference produces a non-empty unified diff containing the changed lines.
        assert!(d.contains("-two"), "diff should show removed line: {d}");
        assert!(d.contains("+TWO"), "diff should show added line: {d}");

        // Identical files → empty diff, still Ok.
        std::fs::write(&b, "one\ntwo\n").unwrap();
        let same = diff_files(a.to_string_lossy().into(), b.to_string_lossy().into()).unwrap();
        assert!(same.trim().is_empty(), "identical files → empty diff");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
