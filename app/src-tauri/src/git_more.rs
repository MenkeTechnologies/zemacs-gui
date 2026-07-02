//! Git more — repo-wide history surfaces beside the per-file git in `git_tools.rs` (blame / file log)
//! and the branch/stash management in `git_ext.rs`:
//!
//! * **Repo log** — the whole repository's commit history (not one file's), newest first, with any
//!   ref decorations, for a project-wide "recent commits" picker.
//! * **Show commit** — the full diff a commit introduced across all files (`git show <hash>`).
//! * **Diff revisions** — the diff between any two revisions (`git diff <a> <b>`), optionally scoped
//!   to a single file — the "what changed between v1.0 and main" workflow.
//! * **Commit graph** — the ASCII branch graph (`git log --graph --oneline --decorate --all`) as text
//!   for a read-only pane.
//!
//! Same host contract as the rest of the app: these shell out to `git` and are read-only (nothing here
//! mutates the repo). Revisions are flag-guarded before they reach git.

use serde::Serialize;
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

/// Reject a revision that git could read as an option (leading `-`) or that carries whitespace /
/// control characters. Positional args already block shell injection; this guards the "looks like a
/// flag" foot-gun. Allows the usual rev syntax (`HEAD~2`, `v1.0`, `abc123`, `origin/main`).
fn valid_rev(rev: &str) -> bool {
    !rev.is_empty()
        && !rev.starts_with('-')
        && !rev.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ── dep-free civil date (unix seconds → YYYY-MM-DD, UTC) ─────────────────────────────────────────
// Same Howard-Hinnant conversion as git_tools.rs; kept local so this module has no cross-file coupling.
fn fmt_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

// ── repo-wide log ──────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RepoCommit {
    pub hash: String,
    /// Abbreviated hash (first 8 chars) for the badge column.
    pub short: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    pub subject: String,
    /// Ref decorations (`HEAD -> main, tag: v1.0`), empty when none.
    pub refs: String,
}

/// Parse the `%H\x1f%an\x1f%at\x1f%s\x1f%D` log stream into commits. Pure — unit tested.
fn parse_repo_log(out: &str) -> Vec<RepoCommit> {
    let mut commits = Vec::new();
    for line in out.lines() {
        let mut it = line.split('\x1f');
        let (Some(hash), Some(author), Some(at), Some(subject)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let refs = it.next().unwrap_or("").trim().to_string();
        let time: i64 = at.trim().parse().unwrap_or(0);
        commits.push(RepoCommit {
            hash: hash.to_string(),
            short: hash.chars().take(8).collect(),
            author: author.to_string(),
            date: fmt_date(time),
            subject: subject.to_string(),
            refs,
        });
    }
    commits
}

/// The whole repository's commit history, newest first (`git log`), capped at `limit`.
#[tauri::command]
pub fn git_log_repo(root: String, limit: Option<usize>) -> Result<Vec<RepoCommit>, String> {
    let n = limit.unwrap_or(300).min(5000);
    let max = format!("-n{n}");
    let out = git_in(
        &root,
        &["log", &max, "--format=%H\x1f%an\x1f%at\x1f%s\x1f%D"],
    )?;
    Ok(parse_repo_log(&out))
}

/// The full diff a commit introduced across all files (`git show <hash>`), for a preview pane.
#[tauri::command]
pub fn git_show_commit(root: String, hash: String) -> Result<String, String> {
    if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.len() < 4 {
        return Err("bad commit hash".into());
    }
    git_in(&root, &["show", &hash])
}

/// The diff between two revisions (`git diff <rev_a> <rev_b>`), optionally scoped to one path. The
/// "what changed between these two points" view — works for branches, tags, or commit hashes.
#[tauri::command]
pub fn git_diff_revs(
    root: String,
    rev_a: String,
    rev_b: String,
    path: Option<String>,
) -> Result<String, String> {
    if !valid_rev(&rev_a) || !valid_rev(&rev_b) {
        return Err("invalid revision".into());
    }
    let mut args: Vec<&str> = vec!["diff", &rev_a, &rev_b];
    let path = path.unwrap_or_default();
    if !path.trim().is_empty() {
        args.push("--");
        args.push(&path);
    }
    git_in(&root, &args)
}

/// The ASCII commit graph across all branches (`git log --graph --oneline --decorate --all`), as text.
#[tauri::command]
pub fn git_graph(root: String, limit: Option<usize>) -> Result<String, String> {
    let n = limit.unwrap_or(300).min(5000);
    let max = format!("-n{n}");
    git_in(
        &root,
        &[
            "log",
            &max,
            "--graph",
            "--oneline",
            "--decorate",
            "--all",
            "--date-order",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_rev_guards() {
        assert!(valid_rev("main"));
        assert!(valid_rev("HEAD~2"));
        assert!(valid_rev("v1.0.0"));
        assert!(valid_rev("origin/feature"));
        assert!(!valid_rev(""));
        assert!(!valid_rev("--all"));
        assert!(!valid_rev("has space"));
    }

    #[test]
    fn fmt_date_known_epochs() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(1_609_459_200), "2021-01-01");
    }

    #[test]
    fn parse_repo_log_fields() {
        let out = "abc123def456\x1fJane\x1f1609459200\x1finit commit\x1fHEAD -> main\n\
                   0badc0de0000\x1fJoe\x1f0\x1fsecond\x1f\n";
        let commits = parse_repo_log(out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].short, "abc123de");
        assert_eq!(commits[0].author, "Jane");
        assert_eq!(commits[0].date, "2021-01-01");
        assert_eq!(commits[0].subject, "init commit");
        assert_eq!(commits[0].refs, "HEAD -> main");
        assert_eq!(commits[1].refs, "");
    }

    // End-to-end against a throwaway repo. Skipped gracefully if `git` isn't on PATH.
    #[test]
    fn repo_log_show_diff_in_temp_repo() {
        let dir = std::env::temp_dir().join(format!(
            "zemacs-gui-gitmore-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().into_owned();

        if git_in(&root, &["init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return; // git unavailable — skip
        }
        let _ = git_in(&root, &["config", "user.email", "t@t"]);
        let _ = git_in(&root, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        git_in(&root, &["add", "."]).unwrap();
        git_in(&root, &["commit", "-q", "-m", "first"]).unwrap();
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        git_in(&root, &["add", "."]).unwrap();
        git_in(&root, &["commit", "-q", "-m", "second"]).unwrap();

        // Repo log lists both commits, newest first.
        let log = git_log_repo(root.clone(), None).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "second");
        assert_eq!(log[1].subject, "first");

        // Show the newest commit → its diff adds "two".
        let show = git_show_commit(root.clone(), log[0].hash.clone()).unwrap();
        assert!(show.contains("+two"), "show diff should add the new line");

        // Diff first..second → the added line.
        let diff =
            git_diff_revs(root.clone(), log[1].hash.clone(), log[0].hash.clone(), None).unwrap();
        assert!(diff.contains("+two"), "rev diff should show the added line");

        // Graph is non-empty and mentions both subjects.
        let graph = git_graph(root.clone(), None).unwrap();
        assert!(graph.contains("first") && graph.contains("second"));

        // A bad hash is rejected before reaching git.
        assert!(git_show_commit(root, "zzzz".into()).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
