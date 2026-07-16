//! Workbench extras — two app-local surfaces that don't touch git and don't fit the search/replace
//! family: persisted **code snippets** (a named text library the front-end pastes into the editor via
//! bracketed paste) and a read-only **project stats** report (file / line counts, broken down by
//! extension). Snippets persist in the app data dir like bookmarks/recent; stats reuse the
//! `project.rs` tree walker so pruning and binary/size skipping stay identical to the search tools.

use crate::project::{looks_binary, walk_files, MAX_GREP_FILE_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// ── snippets (persisted named text library) ──────────────────────────────────────────────────────

/// A saved snippet: a unique name and the literal body pasted into the editor.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Snippet {
    pub name: String,
    pub body: String,
}

const SNIPPET_CAP: usize = 500;

fn snippet_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().ok()?;
    let _ = fs::create_dir_all(&dir);
    // Already the APP DATA dir; routed through the shared helper for a single resolution path.
    Some(zgui_shell::store_path(Some(dir.clone()), dir.join("snippets.json"), "snippets.json"))
}

fn load_snippets_at(p: &Path) -> Vec<Snippet> {
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Snippet>>(&s).ok())
        .unwrap_or_default()
}

fn save_snippets_at(p: &Path, list: &[Snippet]) -> Result<(), String> {
    let json = serde_json::to_string(list).map_err(|e| e.to_string())?;
    fs::write(p, json).map_err(|e| e.to_string())
}

/// Insert (or replace) a snippet by name at the head of the list, capped. Name match is exact.
fn upsert_snippet(list: &mut Vec<Snippet>, snip: Snippet) {
    list.retain(|s| s.name != snip.name);
    list.insert(0, snip);
    list.truncate(SNIPPET_CAP);
}

/// Add or update a snippet and return the new list.
#[tauri::command]
pub fn snippet_add(
    app: tauri::AppHandle,
    name: String,
    body: String,
) -> Result<Vec<Snippet>, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("empty name".into());
    }
    let file = snippet_path(&app).ok_or("no app data dir")?;
    let mut list = load_snippets_at(&file);
    upsert_snippet(&mut list, Snippet { name, body });
    save_snippets_at(&file, &list)?;
    Ok(list)
}

/// The full snippet library.
#[tauri::command]
pub fn snippet_list(app: tauri::AppHandle) -> Vec<Snippet> {
    match snippet_path(&app) {
        Some(p) => load_snippets_at(&p),
        None => Vec::new(),
    }
}

/// Remove a snippet by name and return the new list.
#[tauri::command]
pub fn snippet_remove(app: tauri::AppHandle, name: String) -> Result<Vec<Snippet>, String> {
    let file = snippet_path(&app).ok_or("no app data dir")?;
    let mut list = load_snippets_at(&file);
    list.retain(|s| s.name != name);
    save_snippets_at(&file, &list)?;
    Ok(list)
}

/// Clear all snippets.
#[tauri::command]
pub fn snippet_clear(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(p) = snippet_path(&app) {
        let _ = fs::remove_file(p);
    }
    Ok(())
}

// ── project stats (file / line counts by extension) ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct ExtStat {
    /// Extension without the dot (or "(none)" for extensionless files).
    pub ext: String,
    pub files: usize,
    pub lines: usize,
    pub bytes: u64,
}

#[derive(Serialize)]
pub struct ProjectStats {
    /// Total files walked (after VCS/build pruning), including binaries.
    pub files: usize,
    /// Text files whose lines were counted (binaries / oversized files excluded).
    pub counted_files: usize,
    pub total_lines: usize,
    pub total_bytes: u64,
    /// Per-extension breakdown, sorted by line count descending.
    pub by_ext: Vec<ExtStat>,
}

fn ext_key(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into())
}

/// Walk the project tree and tally file + line + byte counts, broken down by extension. Binaries and
/// oversized files still count toward `files`/`total_bytes` but their lines are skipped (they aren't
/// text), matching what the search tools would scan. `top` caps the per-extension list length.
#[tauri::command]
pub fn project_stats(
    root: String,
    show_hidden: Option<bool>,
    top: Option<usize>,
) -> Result<ProjectStats, String> {
    let top = top.unwrap_or(40).min(500);
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);

    let mut agg: HashMap<String, ExtStat> = HashMap::new();
    let mut files = 0usize;
    let mut counted_files = 0usize;
    let mut total_lines = 0usize;
    let mut total_bytes = 0u64;

    for path in walk_files(&root, show_hidden.unwrap_or(false)) {
        files += 1;
        let key = ext_key(&path);
        let meta_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        total_bytes += meta_len;

        let entry = agg.entry(key.clone()).or_insert(ExtStat {
            ext: key,
            files: 0,
            lines: 0,
            bytes: 0,
        });
        entry.files += 1;
        entry.bytes += meta_len;

        // Count lines only for text files within the size cap (mirrors the search-tool skip rules).
        if meta_len <= MAX_GREP_FILE_BYTES {
            if let Ok(bytes) = fs::read(&path) {
                if !looks_binary(&bytes) {
                    let n = String::from_utf8_lossy(&bytes).lines().count();
                    entry.lines += n;
                    total_lines += n;
                    counted_files += 1;
                }
            }
        }
    }

    let mut by_ext: Vec<ExtStat> = agg.into_values().collect();
    by_ext.sort_by(|a, b| {
        b.lines
            .cmp(&a.lines)
            .then_with(|| b.files.cmp(&a.files))
            .then_with(|| a.ext.cmp(&b.ext))
    });
    by_ext.truncate(top);

    Ok(ProjectStats {
        files,
        counted_files,
        total_lines,
        total_bytes,
        by_ext,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_upsert_dedupes_by_name_front() {
        let mut list = Vec::new();
        upsert_snippet(
            &mut list,
            Snippet {
                name: "hdr".into(),
                body: "// v1".into(),
            },
        );
        upsert_snippet(
            &mut list,
            Snippet {
                name: "log".into(),
                body: "println!()".into(),
            },
        );
        // Re-add "hdr" with a new body → moves to front, replaces body, no duplicate.
        upsert_snippet(
            &mut list,
            Snippet {
                name: "hdr".into(),
                body: "// v2".into(),
            },
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "hdr");
        assert_eq!(list[0].body, "// v2");
    }

    #[test]
    fn snippets_round_trip_on_disk() {
        let dir = tempdir();
        let f = dir.join("s.json");
        let list = vec![Snippet {
            name: "greet".into(),
            body: "hello".into(),
        }];
        save_snippets_at(&f, &list).unwrap();
        let back = load_snippets_at(&f);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "greet");
        assert_eq!(back[0].body, "hello");
        cleanup(&dir);
    }

    #[test]
    fn project_stats_counts_by_ext_and_skips_binary() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(dir.join("src/c.rs"), "fn c() {}\n").unwrap();
        std::fs::write(dir.join("readme.md"), "# hi\ntext\n").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();

        let s = project_stats(dir.to_string_lossy().into(), None, None).unwrap();
        assert_eq!(s.files, 4); // all four files walked
        assert_eq!(s.counted_files, 3); // binary excluded from line counting
        assert_eq!(s.total_lines, 5); // 2 + 1 + 2

        let rs = s.by_ext.iter().find(|e| e.ext == "rs").unwrap();
        assert_eq!(rs.files, 2);
        assert_eq!(rs.lines, 3);
        // rs (3 lines) sorts ahead of md (2 lines).
        assert_eq!(s.by_ext[0].ext, "rs");
        cleanup(&dir);
    }

    // ── tiny tempdir helpers (no external dev-dep) ──
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-wx-test-{}-{}",
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
