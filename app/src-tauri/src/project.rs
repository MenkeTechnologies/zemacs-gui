//! Project workbench commands — the app-local IDE surface that the front-end `panels.js` sidebar
//! drives (quick-open fuzzy finder, find-in-files, project tree file ops, recent files, git panel,
//! file stats). Like the rest of the host, these never edit buffers: results are opened by writing
//! `:open <path>:<line>:<col>` into the PTY, so the editor stays the single source of truth. What
//! lives here is the OS-side work a WebView can't do: walking the tree, grepping files, mutating the
//! filesystem, persisting recent files and shelling out to git.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory names never descended into when walking a project (VCS metadata + build output). Keeps
/// the fuzzy finder and grep fast and relevant instead of drowning in `target/` and `node_modules/`.
const PRUNED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".gradle",
    "vendor",
    ".pnpm-store",
];

/// Safety caps so a walk over a huge tree stays bounded and responsive.
const MAX_WALK_FILES: usize = 200_000;
/// Files larger than this are skipped by every content scan (grep, replace, symbols, markers).
pub(crate) const MAX_GREP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8192;

fn is_pruned_dir(name: &str) -> bool {
    PRUNED_DIRS.contains(&name)
}

/// Iteratively collect files under `root` (breadth-unbounded DFS via an explicit stack — no recursion
/// depth limit to blow), pruning VCS/build dirs and (optionally) dotfiles. Returns absolute paths.
pub(crate) fn walk_files(root: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if is_pruned_dir(&name) {
                    continue;
                }
                stack.push(ent.path());
            } else if ft.is_file() {
                out.push(ent.path());
                if out.len() >= MAX_WALK_FILES {
                    return out;
                }
            }
        }
    }
    out
}

// ── fuzzy file finder (⌘P quick-open) ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FileHit {
    /// Absolute path — what the front-end writes into the PTY (`:open <path>`).
    pub path: String,
    /// Path relative to the searched root — what the list renders.
    pub rel: String,
    pub score: i32,
}

/// Subsequence fuzzy score of `needle` within `hay` (both matched case-insensitively). `None` when
/// `needle` is not a subsequence. Rewards matches at path boundaries (`/ _ - . space`) and
/// consecutive runs, penalises gaps and long candidates — enough to float the intended file to the
/// top of a quick-open list without a full fzf implementation.
pub fn fuzzy_score(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay_l = hay.to_lowercase();
    let hb = hay_l.as_bytes();
    let nb = needle.to_lowercase().into_bytes();

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut prev_idx: Option<usize> = None;
    for &nc in &nb {
        let mut idx = None;
        let mut j = hi;
        while j < hb.len() {
            if hb[j] == nc {
                idx = Some(j);
                break;
            }
            j += 1;
        }
        let idx = idx?;
        let boundary = idx == 0 || matches!(hb[idx - 1], b'/' | b'_' | b'-' | b'.' | b' ');
        if boundary {
            score += 10;
        }
        if let Some(p) = prev_idx {
            if idx == p + 1 {
                // A consecutive run outweighs a boundary hit, so contiguous matches (e.g. "main"
                // in "src/main.rs") rank above the same chars scattered across separators.
                score += 15;
            } else {
                score -= (idx - p - 1).min(10) as i32;
            }
        }
        prev_idx = Some(idx);
        hi = idx + 1;
    }
    // Prefer shorter paths (the match is a larger fraction of them).
    score -= (hay.len() as i32) / 20;
    Some(score)
}

/// Fuzzy-rank files under `root` against `query`, returning the top `limit` by score. Empty query
/// lists files in walk order (a plain project listing). The score is computed against the
/// root-relative path so `src/main` matches directory + file together.
#[tauri::command]
pub fn find_files(
    root: String,
    query: String,
    limit: Option<usize>,
    show_hidden: Option<bool>,
) -> Result<Vec<FileHit>, String> {
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);
    let limit = limit.unwrap_or(200).min(2000);
    let files = walk_files(&root, show_hidden.unwrap_or(false));

    let mut hits: Vec<FileHit> = files
        .into_iter()
        .filter_map(|p| {
            let rel = p
                .strip_prefix(&root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            let score = fuzzy_score(&rel, &query)?;
            Some(FileHit {
                path: p.to_string_lossy().into_owned(),
                rel,
                score,
            })
        })
        .collect();

    // Highest score first; ties broken by the shorter (usually more specific) path.
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.rel.len().cmp(&b.rel.len()))
    });
    hits.truncate(limit);
    Ok(hits)
}

// ── find-in-files (project-wide text / regex search) ────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct SearchOpts {
    pub regex: Option<bool>,
    pub case_insensitive: Option<bool>,
    pub whole_word: Option<bool>,
    pub show_hidden: Option<bool>,
    pub max_results: Option<usize>,
}

#[derive(Serialize)]
pub struct SearchHit {
    pub path: String,
    pub rel: String,
    /// 1-based line number (what `:open path:line:col` expects).
    pub line: usize,
    /// 1-based column of the first match on the line (character count, not bytes).
    pub col: usize,
    /// The matching line, trimmed of leading whitespace and length-capped for the results list.
    pub text: String,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// True when `max_results` cut the search short — the UI shows a "more…" hint.
    pub truncated: bool,
}

pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// Project-wide line search. `query` is a literal substring unless `opts.regex` is set, in which case
/// it is a full regular expression. `whole_word` wraps it in word boundaries; `case_insensitive`
/// folds case. Binary and oversized files are skipped. Results carry `path:line:col` so a click opens
/// the editor exactly on the match.
#[tauri::command]
pub fn search_project(
    root: String,
    query: String,
    opts: Option<SearchOpts>,
) -> Result<SearchResult, String> {
    if query.is_empty() {
        return Ok(SearchResult {
            hits: Vec::new(),
            truncated: false,
        });
    }
    let opts = opts.unwrap_or_default();
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);
    let max = opts.max_results.unwrap_or(1000).min(20_000);

    let mut pattern = if opts.regex.unwrap_or(false) {
        query.clone()
    } else {
        regex::escape(&query)
    };
    if opts.whole_word.unwrap_or(false) {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(opts.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|e| format!("bad pattern: {e}"))?;

    let mut hits = Vec::new();
    let mut truncated = false;
    'files: for path in walk_files(&root, opts.show_hidden.unwrap_or(false)) {
        if fs::metadata(&path)
            .map(|m| m.len() > MAX_GREP_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if looks_binary(&bytes) {
            continue;
        }
        let content = String::from_utf8_lossy(&bytes);
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let path_s = path.to_string_lossy().into_owned();
        for (i, raw) in content.lines().enumerate() {
            if let Some(m) = re.find(raw) {
                // Column = 1-based character index of the match start.
                let col = raw[..m.start()].chars().count() + 1;
                let trimmed = raw.trim_start();
                let text: String = trimmed.chars().take(400).collect();
                hits.push(SearchHit {
                    path: path_s.clone(),
                    rel: rel.clone(),
                    line: i + 1,
                    col,
                    text,
                });
                if hits.len() >= max {
                    truncated = true;
                    break 'files;
                }
            }
        }
    }
    Ok(SearchResult { hits, truncated })
}

// ── project tree file operations (new / rename / delete / copy) ─────────────────────────────────

/// Create a file (with any missing parent dirs) or an empty directory. Fails if it already exists so
/// the UI never silently clobbers.
#[tauri::command]
pub fn create_path(path: String, is_dir: bool) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if p.exists() {
        return Err(format!("already exists: {path}"));
    }
    if is_dir {
        fs::create_dir_all(&p).map_err(|e| e.to_string())
    } else {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::File::create(&p).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Rename / move a path. Fails if the destination exists.
#[tauri::command]
pub fn rename_path(from: String, to: String) -> Result<(), String> {
    let to_p = PathBuf::from(&to);
    if to_p.exists() {
        return Err(format!("destination exists: {to}"));
    }
    if let Some(parent) = to_p.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::rename(&from, &to_p).map_err(|e| e.to_string())
}

/// Delete a file or a directory (recursively). The front-end confirms first.
#[tauri::command]
pub fn delete_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    let meta = fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        fs::remove_dir_all(&p).map_err(|e| e.to_string())
    } else {
        fs::remove_file(&p).map_err(|e| e.to_string())
    }
}

/// Copy a file, or a directory recursively, to `to`. Fails if the destination exists.
#[tauri::command]
pub fn copy_path(from: String, to: String) -> Result<(), String> {
    let from_p = PathBuf::from(&from);
    let to_p = PathBuf::from(&to);
    if to_p.exists() {
        return Err(format!("destination exists: {to}"));
    }
    copy_recursive(&from_p, &to_p).map_err(|e| e.to_string())
}

fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for ent in fs::read_dir(from)?.flatten() {
            copy_recursive(&ent.path(), &to.join(ent.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to).map(|_| ())
    }
}

// ── recent files (persisted across launches) ────────────────────────────────────────────────────

const RECENT_CAP: usize = 50;

fn recent_file(app: &tauri::AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().ok()?;
    let _ = fs::create_dir_all(&dir);
    // Already the APP DATA dir; routed through the shared helper for a single resolution path.
    Some(zgui_shell::store_path(Some(dir.clone()), dir.join("recent-files.json"), "recent-files.json"))
}

fn read_recent(app: &tauri::AppHandle) -> Vec<String> {
    recent_file(app)
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// Record a freshly-opened file at the head of the MRU list (deduped, capped). The front-end calls
/// this whenever it writes `:open` into the PTY.
#[tauri::command]
pub fn recent_add(app: tauri::AppHandle, path: String) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    let mut list = read_recent(&app);
    list.retain(|p| p != &path);
    list.insert(0, path);
    list.truncate(RECENT_CAP);
    let file = recent_file(&app).ok_or("no app data dir")?;
    let json = serde_json::to_string(&list).map_err(|e| e.to_string())?;
    fs::write(file, json).map_err(|e| e.to_string())
}

/// The recent-files list, filtered to entries that still exist on disk.
#[tauri::command]
pub fn recent_list(app: tauri::AppHandle) -> Vec<String> {
    read_recent(&app)
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .collect()
}

/// Clear the recent-files list.
#[tauri::command]
pub fn recent_clear(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(file) = recent_file(&app) {
        let _ = fs::remove_file(file);
    }
    Ok(())
}

// ── file statistics (⌘I / status) ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FileStats {
    pub path: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub lines: usize,
    pub words: usize,
    pub chars: usize,
}

/// Line / word / character / byte counts for a file (a GUI `wc`), or a child count for a directory.
#[tauri::command]
pub fn file_stats(path: String) -> Result<FileStats, String> {
    let p = PathBuf::from(&path);
    let meta = fs::metadata(&p).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        let children = fs::read_dir(&p).map(|rd| rd.flatten().count()).unwrap_or(0);
        return Ok(FileStats {
            path,
            is_dir: true,
            bytes: meta.len(),
            lines: 0,
            words: 0,
            chars: children,
        });
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    let content = String::from_utf8_lossy(&bytes);
    Ok(FileStats {
        path,
        is_dir: false,
        bytes: meta.len(),
        lines: content.lines().count(),
        words: content.split_whitespace().count(),
        chars: content.chars().count(),
    })
}

// ── git integration (status / branch / diff) ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct GitEntry {
    /// Absolute path to the changed file.
    pub path: String,
    /// Path relative to the repo root — what the panel renders.
    pub rel: String,
    /// Two-char porcelain status code (e.g. " M", "??", "A ").
    pub status: String,
}

fn run_git(root: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("git not available: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The current git branch of the repo containing `root`, or an empty string when not a repo.
#[tauri::command]
pub fn git_branch(root: String) -> String {
    run_git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Changed/untracked files (`git status --porcelain`), each with its absolute + repo-relative path.
#[tauri::command]
pub fn git_status(root: String) -> Result<Vec<GitEntry>, String> {
    let top = run_git(&root, &["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string();
    let out = run_git(&root, &["status", "--porcelain"])?;
    let mut entries = Vec::new();
    for line in out.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = line[..2].to_string();
        // Porcelain rename lines are "R  old -> new"; keep the new path.
        let rel_raw = &line[3..];
        let rel = rel_raw
            .rsplit(" -> ")
            .next()
            .unwrap_or(rel_raw)
            .trim()
            .to_string();
        let path = Path::new(&top).join(&rel).to_string_lossy().into_owned();
        entries.push(GitEntry { path, rel, status });
    }
    Ok(entries)
}

/// The unified `git diff` for a single file (working tree vs index+HEAD), for a preview pane.
#[tauri::command]
pub fn git_file_diff(path: String) -> Result<String, String> {
    let dir = Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut diff = run_git(&dir, &["diff", "--", &path])?;
    if diff.trim().is_empty() {
        // Staged-only change: show the staged diff instead of nothing.
        diff = run_git(&dir, &["diff", "--cached", "--", &path])?;
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranks_boundary_and_consecutive_over_scattered() {
        // "mn" should rank main.rs (consecutive-ish, boundary) above a scattered match.
        let good = fuzzy_score("src/main.rs", "main").unwrap();
        let scattered = fuzzy_score("a/m_x_a_i_n.rs", "main").unwrap();
        assert!(
            good > scattered,
            "boundary/consecutive should win: {good} vs {scattered}"
        );
    }

    #[test]
    fn fuzzy_non_subsequence_is_none() {
        assert!(fuzzy_score("abc", "xyz").is_none());
        assert!(fuzzy_score("main.rs", "zzz").is_none());
    }

    #[test]
    fn fuzzy_empty_query_matches_everything() {
        assert_eq!(fuzzy_score("anything.rs", ""), Some(0));
    }

    #[test]
    fn find_files_prunes_and_ranks() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "x").unwrap();

        let hits = find_files(dir.to_string_lossy().into(), "main".into(), None, None).unwrap();
        assert!(
            hits.iter().any(|h| h.rel.ends_with("main.rs")),
            "should find main.rs"
        );
        assert!(
            !hits.iter().any(|h| h.rel.contains("node_modules")),
            "node_modules must be pruned"
        );
        // main.rs ranks first for query "main".
        assert!(hits[0].rel.ends_with("main.rs"));
        cleanup(&dir);
    }

    #[test]
    fn search_literal_and_regex_and_word() {
        let dir = tempdir();
        std::fs::write(dir.join("a.txt"), "hello world\nHELLO again\nother line\n").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 1, 2, b'h', b'i']).unwrap(); // binary, skipped

        let root: String = dir.to_string_lossy().into();

        // Literal, case-sensitive: one hit on line 1 only.
        let r = search_project(root.clone(), "hello".into(), None).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].line, 1);
        assert_eq!(r.hits[0].col, 1);

        // Case-insensitive: two hits.
        let ci = search_project(
            root.clone(),
            "hello".into(),
            Some(SearchOpts {
                case_insensitive: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(ci.hits.len(), 2);

        // Regex.
        let rx = search_project(
            root.clone(),
            r"o\w+".into(),
            Some(SearchOpts {
                regex: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(rx.hits.iter().any(|h| h.text.contains("other")));

        // whole_word: "line" matches, "lin" does not.
        let ww = search_project(
            root,
            "lin".into(),
            Some(SearchOpts {
                whole_word: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(ww.hits.len(), 0);

        cleanup(&dir);
    }

    #[test]
    fn file_ops_create_rename_copy_delete() {
        let dir = tempdir();
        let f = dir.join("x.txt");
        let g = dir.join("sub/y.txt");
        let h = dir.join("z.txt");

        create_path(f.to_string_lossy().into(), false).unwrap();
        assert!(f.exists());
        // create fails on existing
        assert!(create_path(f.to_string_lossy().into(), false).is_err());

        rename_path(f.to_string_lossy().into(), g.to_string_lossy().into()).unwrap();
        assert!(!f.exists() && g.exists());

        copy_path(g.to_string_lossy().into(), h.to_string_lossy().into()).unwrap();
        assert!(g.exists() && h.exists());

        delete_path(h.to_string_lossy().into()).unwrap();
        assert!(!h.exists());

        cleanup(&dir);
    }

    #[test]
    fn file_stats_counts_lines_words() {
        let dir = tempdir();
        let f = dir.join("w.txt");
        std::fs::write(&f, "one two three\nfour five\n").unwrap();
        let s = file_stats(f.to_string_lossy().into()).unwrap();
        assert_eq!(s.lines, 2);
        assert_eq!(s.words, 5);
        assert!(!s.is_dir);
        cleanup(&dir);
    }

    // ── tiny tempdir helpers (no external dev-dep) ──
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
