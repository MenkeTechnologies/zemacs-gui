//! Git extras — branch and stash management beside `git_tools.rs` (blame/history/staging) and
//! `project.rs` (status/branch/diff). Lets the workbench list and switch branches, create a new one,
//! and drive the stash (save / list / pop / drop / show) without leaving the window. Same host
//! contract as the rest of the app: these shell out to `git`; the mutating ones (checkout / create /
//! stash pop / drop) are confirmed in the UI. Nothing here edits buffers — after a checkout the
//! front-end re-opens the current file so the editor picks up the new working tree.

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

/// Reject a ref/branch name that git could mistake for an option (leading `-`) or that carries
/// whitespace / control characters. Passing args positionally already blocks shell injection; this
/// guards the remaining "name looks like a flag" foot-gun before it reaches `git checkout`.
fn valid_ref(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ── branches (list / checkout / create) ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Branch {
    pub name: String,
    /// True for the currently checked-out branch.
    pub current: bool,
    /// Last-commit date on the branch, `YYYY-MM-DD` (from `committerdate:short`).
    pub date: String,
    /// Subject of the branch tip commit.
    pub subject: String,
}

/// Local branches, most-recently-committed first, with the current branch flagged. Uses
/// `for-each-ref` with an ASCII-unit-separator format so subjects with any punctuation parse cleanly.
#[tauri::command]
pub fn git_branches(root: String) -> Result<Vec<Branch>, String> {
    let current = git_in(&root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    let out = git_in(
        &root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)\x1f%(committerdate:short)\x1f%(subject)",
            "refs/heads",
        ],
    )?;
    let mut branches = Vec::new();
    for line in out.lines() {
        let mut it = line.split('\x1f');
        let (Some(name), Some(date), Some(subject)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        branches.push(Branch {
            current: name == current,
            name: name.to_string(),
            date: date.to_string(),
            subject: subject.to_string(),
        });
    }
    Ok(branches)
}

/// Check out an existing branch (`git checkout <name>`). Confirmed in the UI; fails loudly (git's own
/// message) when the working tree would be clobbered, so no changes are lost silently.
#[tauri::command]
pub fn git_checkout_branch(root: String, name: String) -> Result<(), String> {
    if !valid_ref(&name) {
        return Err("invalid branch name".into());
    }
    git_in(&root, &["checkout", &name]).map(|_| ())
}

/// Create and switch to a new branch off the current HEAD (`git checkout -b <name>`).
#[tauri::command]
pub fn git_create_branch(root: String, name: String) -> Result<(), String> {
    if !valid_ref(&name) {
        return Err("invalid branch name".into());
    }
    git_in(&root, &["checkout", "-b", &name]).map(|_| ())
}

// ── stash (list / save / pop / drop / show) ───────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Stash {
    /// 0-based stash index (`stash@{index}`).
    pub index: usize,
    /// The reflog selector, e.g. `stash@{0}` — shown as a badge.
    pub selector: String,
    /// The stash subject (`WIP on <branch>: …` or a custom message).
    pub message: String,
}

/// The stash entries, newest first (`git stash list`). `%gd` is the reflog selector (`stash@{n}`),
/// `%s` the subject.
#[tauri::command]
pub fn git_stash_list(root: String) -> Result<Vec<Stash>, String> {
    let out = git_in(&root, &["stash", "list", "--format=%gd\x1f%s"])?;
    let mut stashes = Vec::new();
    for (i, line) in out.lines().enumerate() {
        let mut it = line.split('\x1f');
        let selector = it.next().unwrap_or("").to_string();
        let message = it.next().unwrap_or("").to_string();
        if selector.is_empty() {
            continue;
        }
        stashes.push(Stash {
            index: i,
            selector,
            message,
        });
    }
    Ok(stashes)
}

/// Stash the working tree (`git stash push`), optionally with a message and optionally including
/// untracked files (`-u`). Returns git's own summary line.
#[tauri::command]
pub fn git_stash_save(
    root: String,
    message: Option<String>,
    include_untracked: Option<bool>,
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["stash", "push"];
    if include_untracked.unwrap_or(false) {
        args.push("-u");
    }
    let msg = message.unwrap_or_default();
    let msg = msg.trim();
    if !msg.is_empty() {
        args.push("-m");
        args.push(msg);
    }
    git_in(&root, &args).map(|s| s.trim().to_string())
}

/// The `stash@{index}` selector for a numeric index (validated: git only has non-negative indices).
fn stash_ref(index: usize) -> String {
    format!("stash@{{{index}}}")
}

/// Apply and drop a stash entry (`git stash pop stash@{index}`). Confirmed in the UI. Conflicts leave
/// the stash intact (git's own behaviour) and surface as an error.
#[tauri::command]
pub fn git_stash_pop(root: String, index: usize) -> Result<String, String> {
    git_in(&root, &["stash", "pop", &stash_ref(index)]).map(|s| s.trim().to_string())
}

/// Delete a stash entry without applying it (`git stash drop stash@{index}`). Confirmed in the UI —
/// this discards those changes.
#[tauri::command]
pub fn git_stash_drop(root: String, index: usize) -> Result<String, String> {
    git_in(&root, &["stash", "drop", &stash_ref(index)]).map(|s| s.trim().to_string())
}

/// The full patch a stash entry holds (`git stash show -p stash@{index}`), for a preview pane.
#[tauri::command]
pub fn git_stash_show(root: String, index: usize) -> Result<String, String> {
    git_in(&root, &["stash", "show", "-p", &stash_ref(index)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ref_rejects_flags_and_whitespace() {
        assert!(valid_ref("feature/x"));
        assert!(valid_ref("main"));
        assert!(!valid_ref(""));
        assert!(!valid_ref("-f"));
        assert!(!valid_ref("--force"));
        assert!(!valid_ref("has space"));
        assert!(!valid_ref("tab\tname"));
    }

    #[test]
    fn stash_ref_formats_selector() {
        assert_eq!(stash_ref(0), "stash@{0}");
        assert_eq!(stash_ref(12), "stash@{12}");
    }

    // End-to-end against a throwaway repo: create a branch, stash a change, list/show/pop it. Skipped
    // gracefully if `git` isn't on PATH so the suite still runs in a minimal environment.
    #[test]
    fn branch_and_stash_roundtrip_in_temp_repo() {
        let dir = std::env::temp_dir().join(format!(
            "zmax-gui-gitext-{}-{}",
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
        // Deterministic identity + initial commit (config is local to this repo).
        let _ = git_in(&root, &["config", "user.email", "t@t"]);
        let _ = git_in(&root, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        git_in(&root, &["add", "."]).unwrap();
        git_in(&root, &["commit", "-q", "-m", "init"]).unwrap();

        // A new branch appears in the list and is flagged current after creation.
        git_create_branch(root.clone(), "feature/test".into()).unwrap();
        let branches = git_branches(root.clone()).unwrap();
        let feat = branches.iter().find(|b| b.name == "feature/test").unwrap();
        assert!(feat.current, "new branch should be current");

        // Stash a working-tree change, confirm it lists, shows a patch, and pops back.
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        git_stash_save(root.clone(), Some("wip".into()), None).unwrap();
        let stashes = git_stash_list(root.clone()).unwrap();
        assert_eq!(stashes.len(), 1);
        assert_eq!(stashes[0].index, 0);
        assert!(stashes[0].selector.starts_with("stash@{"));
        let patch = git_stash_show(root.clone(), 0).unwrap();
        assert!(
            patch.contains("f.txt"),
            "stash patch names the file: {patch}"
        );
        // Popping restores the change and empties the stash.
        git_stash_pop(root.clone(), 0).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "two\n");
        assert!(git_stash_list(root).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
