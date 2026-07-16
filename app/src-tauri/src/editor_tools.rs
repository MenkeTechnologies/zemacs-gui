//! Editor tools — a second layer of app-local workbench commands beside `project.rs`: persisted
//! **bookmarks**, project-wide **search & replace** (regex, preview then apply on disk), a workspace
//! **symbol outline** (go-to-symbol), and a **TODO / marker** scan. Same contract as the rest of the
//! host: navigation results are opened by writing `:open <path>:<line>:<col>` into the PTY, so the
//! editor stays the source of truth — only search-&-replace mutates files, mirroring the existing
//! tree file ops in `project.rs`. The OS-side work a WebView can't do (walking, grepping, rewriting,
//! persistence) lives here; the front-end `panels.js` is the UI + PTY bridge.

use crate::project::{looks_binary, walk_files, MAX_GREP_FILE_BYTES};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ── bookmarks (persisted named file:line marks) ──────────────────────────────────────────────────

/// A saved location: an absolute file path, a 1-based line, and a user label.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Bookmark {
    pub path: String,
    pub line: usize,
    pub label: String,
}

const BOOKMARK_CAP: usize = 500;

fn bookmark_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().ok()?;
    let _ = fs::create_dir_all(&dir);
    // Already the APP DATA dir; routed through the shared helper for a single resolution path.
    Some(zgui_shell::store_path(Some(dir.clone()), dir.join("bookmarks.json"), "bookmarks.json"))
}

fn load_bookmarks_at(p: &Path) -> Vec<Bookmark> {
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Bookmark>>(&s).ok())
        .unwrap_or_default()
}

fn save_bookmarks_at(p: &Path, list: &[Bookmark]) -> Result<(), String> {
    let json = serde_json::to_string(list).map_err(|e| e.to_string())?;
    fs::write(p, json).map_err(|e| e.to_string())
}

/// Insert a bookmark at the head, replacing any existing mark for the same `path:line`, capped.
fn upsert_bookmark(list: &mut Vec<Bookmark>, bm: Bookmark) {
    list.retain(|b| !(b.path == bm.path && b.line == bm.line));
    list.insert(0, bm);
    list.truncate(BOOKMARK_CAP);
}

/// Add (or update) a bookmark and return the new list.
#[tauri::command]
pub fn bookmark_add(
    app: tauri::AppHandle,
    path: String,
    line: Option<usize>,
    label: Option<String>,
) -> Result<Vec<Bookmark>, String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    let file = bookmark_path(&app).ok_or("no app data dir")?;
    let mut list = load_bookmarks_at(&file);
    let line = line.unwrap_or(1).max(1);
    let label = label.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
        let name = path.rsplit('/').next().unwrap_or(&path);
        format!("{name}:{line}")
    });
    upsert_bookmark(&mut list, Bookmark { path, line, label });
    save_bookmarks_at(&file, &list)?;
    Ok(list)
}

/// The bookmark list, filtered to files that still exist on disk.
#[tauri::command]
pub fn bookmark_list(app: tauri::AppHandle) -> Vec<Bookmark> {
    match bookmark_path(&app) {
        Some(p) => load_bookmarks_at(&p)
            .into_iter()
            .filter(|b| Path::new(&b.path).exists())
            .collect(),
        None => Vec::new(),
    }
}

/// Remove the bookmark at `path:line` and return the new list.
#[tauri::command]
pub fn bookmark_remove(
    app: tauri::AppHandle,
    path: String,
    line: usize,
) -> Result<Vec<Bookmark>, String> {
    let file = bookmark_path(&app).ok_or("no app data dir")?;
    let mut list = load_bookmarks_at(&file);
    list.retain(|b| !(b.path == path && b.line == line));
    save_bookmarks_at(&file, &list)?;
    Ok(list)
}

/// Clear all bookmarks.
#[tauri::command]
pub fn bookmark_clear(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(p) = bookmark_path(&app) {
        let _ = fs::remove_file(p);
    }
    Ok(())
}

// ── project-wide search & replace (regex, preview + apply on disk) ────────────────────────────────

#[derive(Deserialize, Default)]
pub struct ReplaceOpts {
    pub regex: Option<bool>,
    pub case_insensitive: Option<bool>,
    pub whole_word: Option<bool>,
    pub show_hidden: Option<bool>,
    /// When true, matching files are rewritten on disk; otherwise the call is a dry-run preview.
    pub apply: Option<bool>,
    pub max_results: Option<usize>,
}

#[derive(Serialize)]
pub struct ReplaceHit {
    pub path: String,
    pub rel: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column of the first match on the line.
    pub col: usize,
    /// The line before replacement (trimmed, length-capped for the preview list).
    pub before: String,
    /// The line after replacement (trimmed, length-capped).
    pub after: String,
}

#[derive(Serialize)]
pub struct ReplaceResult {
    pub hits: Vec<ReplaceHit>,
    /// Total number of individual matches across the project (not just previewed lines).
    pub total: usize,
    /// Number of files that contain at least one match.
    pub files: usize,
    /// True when the files were actually rewritten (`apply`), false for a preview.
    pub applied: bool,
    /// True when `max_results` cut the preview list short (the totals are still complete).
    pub truncated: bool,
}

/// Compile the search regex the same way `search_project` does: literal unless `regex`, wrapped in
/// word boundaries for `whole_word`, case folded for `case_insensitive`.
fn build_regex(query: &str, opts: &ReplaceOpts) -> Result<regex::Regex, String> {
    let mut pattern = if opts.regex.unwrap_or(false) {
        query.to_string()
    } else {
        regex::escape(query)
    };
    if opts.whole_word.unwrap_or(false) {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(opts.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|e| format!("bad pattern: {e}"))
}

fn cap(s: &str) -> String {
    s.trim().chars().take(400).collect()
}

/// Replace `re` with `replacement` (supporting `$1`/`${name}` capture references) throughout one
/// file's `content`, line by line so `^`/`$` anchor per line — matching the per-line semantics of
/// `search_project`. Splitting on `\n` and re-joining on `\n` is lossless for both LF and CRLF files
/// (a trailing `\r` rides along inside each line). Returns the rewritten content, the per-line hits,
/// and the total match count.
fn replace_in_content(
    re: &regex::Regex,
    content: &str,
    replacement: &str,
) -> (String, Vec<(usize, usize, String, String)>, usize) {
    let mut out: Vec<std::borrow::Cow<str>> = Vec::new();
    let mut hits = Vec::new();
    let mut total = 0usize;
    for (i, line) in content.split('\n').enumerate() {
        let n = re.find_iter(line).count();
        if n == 0 {
            out.push(std::borrow::Cow::Borrowed(line));
            continue;
        }
        total += n;
        let after = re.replace_all(line, replacement);
        let col = re
            .find(line)
            .map(|m| line[..m.start()].chars().count() + 1)
            .unwrap_or(1);
        hits.push((i + 1, col, cap(line), cap(&after)));
        out.push(std::borrow::Cow::Owned(after.into_owned()));
    }
    (out.join("\n"), hits, total)
}

/// Preview (or, with `opts.apply`, perform) a project-wide search & replace. The preview list is
/// capped at `max_results` lines but `total`/`files` always reflect the whole project. Binary and
/// oversized files are skipped, just like the search.
#[tauri::command]
pub fn replace_project(
    root: String,
    query: String,
    replacement: String,
    opts: Option<ReplaceOpts>,
) -> Result<ReplaceResult, String> {
    if query.is_empty() {
        return Ok(ReplaceResult {
            hits: Vec::new(),
            total: 0,
            files: 0,
            applied: false,
            truncated: false,
        });
    }
    let opts = opts.unwrap_or_default();
    let apply = opts.apply.unwrap_or(false);
    let max = opts.max_results.unwrap_or(1000).min(20_000);
    let re = build_regex(&query, &opts)?;

    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);

    let mut hits = Vec::new();
    let mut total = 0usize;
    let mut files = 0usize;
    let mut truncated = false;

    for path in walk_files(&root, opts.show_hidden.unwrap_or(false)) {
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
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let (new_content, line_hits, n) = replace_in_content(&re, &content, &replacement);
        if n == 0 {
            continue;
        }
        files += 1;
        total += n;
        if apply && new_content != content {
            fs::write(&path, new_content).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        if !truncated {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let path_s = path.to_string_lossy().into_owned();
            for (line, col, before, after) in line_hits {
                hits.push(ReplaceHit {
                    path: path_s.clone(),
                    rel: rel.clone(),
                    line,
                    col,
                    before,
                    after,
                });
                if hits.len() >= max {
                    truncated = true;
                    break;
                }
            }
        }
    }

    Ok(ReplaceResult {
        hits,
        total,
        files,
        applied: apply,
        truncated,
    })
}

// ── workspace symbol outline (go-to-symbol) ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Symbol {
    pub path: String,
    pub rel: String,
    pub name: String,
    /// Symbol kind label ("fn", "struct", "class", "def", "H2", …) for the badge column.
    pub kind: String,
    pub line: usize,
    pub col: usize,
}

/// The (kind, regex) rule set for a file, chosen by extension. Each regex must have one capture group
/// holding the symbol name. `None` means the file type has no outline support (skipped).
pub(crate) fn symbol_rules(ext: &str) -> Option<Vec<(&'static str, regex::Regex)>> {
    let rx = |p: &str| regex::Regex::new(p).unwrap();
    let rules: Vec<(&'static str, regex::Regex)> = match ext {
        "rs" => vec![
            (
                "fn",
                rx(
                    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
                ),
            ),
            (
                "struct",
                rx(r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            (
                "enum",
                rx(r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            (
                "trait",
                rx(r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            (
                "impl",
                rx(
                    r"^\s*impl(?:\s*<[^>]*>)?\s+(?:[A-Za-z0-9_:<>]+\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)",
                ),
            ),
            (
                "type",
                rx(r"^\s*(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            ("macro", rx(r"^\s*macro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)")),
            (
                "mod",
                rx(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            (
                "const",
                rx(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
        ],
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => vec![
            (
                "fn",
                rx(
                    r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][A-Za-z0-9_$]*)",
                ),
            ),
            (
                "class",
                rx(
                    r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                ),
            ),
            (
                "const",
                rx(
                    r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(?:async\s*)?\(?[^=]*=>",
                ),
            ),
            (
                "method",
                rx(r"^\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*:\s*(?:async\s+)?function"),
            ),
        ],
        "py" => vec![
            (
                "def",
                rx(r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            ("class", rx(r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)")),
        ],
        "go" => vec![
            (
                "func",
                rx(r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            ("type", rx(r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)")),
        ],
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => vec![
            (
                "struct",
                rx(r"^\s*(?:typedef\s+)?(?:struct|enum|union)\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ),
            ("class", rx(r"^\s*(?:class)\s+([A-Za-z_][A-Za-z0-9_]*)")),
            (
                "fn",
                rx(r"^[A-Za-z_][\w\s\*]+\s+\*?([A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*\)\s*\{?\s*$"),
            ),
        ],
        "rb" => vec![
            ("def", rx(r"^\s*def\s+([A-Za-z_][A-Za-z0-9_?!]*)")),
            ("class", rx(r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)")),
            ("module", rx(r"^\s*module\s+([A-Za-z_][A-Za-z0-9_]*)")),
        ],
        "sh" | "bash" | "zsh" => vec![
            (
                "fn",
                rx(r"^\s*(?:function\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*\)"),
            ),
            ("fn", rx(r"^\s*function\s+([A-Za-z_][A-Za-z0-9_]*)")),
        ],
        "lua" => vec![(
            "fn",
            rx(r"^\s*(?:local\s+)?function\s+([A-Za-z_][A-Za-z0-9_.:]*)"),
        )],
        "stk" | "pl" | "pm" => vec![("fn", rx(r"^\s*(?:fn|sub)\s+([A-Za-z_][A-Za-z0-9_]*)"))],
        "md" | "markdown" => vec![
            ("H1", rx(r"^#\s+(.+?)\s*#*\s*$")),
            ("H2", rx(r"^##\s+(.+?)\s*#*\s*$")),
            ("H3", rx(r"^###\s+(.+?)\s*#*\s*$")),
        ],
        _ => return None,
    };
    Some(rules)
}

/// Extract symbols from one file's `content` using the rules for `ext`. Pure (no I/O) so it is unit
/// tested directly. The first rule that matches a line wins (rules are ordered specific-first).
pub(crate) fn extract_symbols(rel: &str, path: &str, ext: &str, content: &str) -> Vec<Symbol> {
    let Some(rules) = symbol_rules(ext) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        for (kind, re) in &rules {
            if let Some(caps) = re.captures(line) {
                if let Some(m) = caps.get(1) {
                    let name = m.as_str().trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let col = line[..m.start()].chars().count() + 1;
                    out.push(Symbol {
                        path: path.to_string(),
                        rel: rel.to_string(),
                        name,
                        kind: kind.to_string(),
                        line: i + 1,
                        col,
                    });
                    break;
                }
            }
        }
    }
    out
}

pub(crate) fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Extract a workspace-wide symbol list (functions / types / headings) across the project tree, for a
/// go-to-symbol picker. Capped at `limit` symbols so a huge tree stays responsive.
#[tauri::command]
pub fn project_symbols(
    root: String,
    limit: Option<usize>,
    show_hidden: Option<bool>,
) -> Result<Vec<Symbol>, String> {
    let limit = limit.unwrap_or(5000).min(50_000);
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);

    let mut out = Vec::new();
    for path in walk_files(&root, show_hidden.unwrap_or(false)) {
        let ext = ext_of(&path);
        if symbol_rules(&ext).is_none() {
            continue;
        }
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
        out.extend(extract_symbols(&rel, &path_s, &ext, &content));
        if out.len() >= limit {
            out.truncate(limit);
            break;
        }
    }
    Ok(out)
}

// ── TODO / marker scan ───────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Marker {
    pub path: String,
    pub rel: String,
    pub line: usize,
    pub col: usize,
    /// The marker tag (TODO / FIXME / HACK / …).
    pub kind: String,
    /// The comment text after the tag (trimmed, length-capped).
    pub text: String,
}

/// The recognised marker tags. Word-boundary matched, case-sensitive (these are conventionally caps).
const MARKER_TAGS: &[&str] = &[
    "TODO", "FIXME", "HACK", "XXX", "BUG", "NOTE", "OPTIMIZE", "WARNING",
];

fn marker_regex() -> regex::Regex {
    let alt = MARKER_TAGS.join("|");
    regex::Regex::new(&format!(r"\b({alt})\b[:\-]?\s*(.*)$")).unwrap()
}

/// Pure marker extraction for one file — unit tested directly.
fn scan_markers_in(rel: &str, path: &str, content: &str, re: &regex::Regex) -> Vec<Marker> {
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            let m = caps.get(1).unwrap();
            let col = line[..m.start()].chars().count() + 1;
            let text: String = caps
                .get(2)
                .map(|t| t.as_str().trim())
                .unwrap_or("")
                .chars()
                .take(300)
                .collect();
            out.push(Marker {
                path: path.to_string(),
                rel: rel.to_string(),
                line: i + 1,
                col,
                kind: m.as_str().to_string(),
                text,
            });
        }
    }
    out
}

/// Scan the project tree for TODO / FIXME / HACK / … markers in comments, each with `path:line:col`
/// so a click jumps to it. Capped at `limit`.
#[tauri::command]
pub fn scan_markers(
    root: String,
    limit: Option<usize>,
    show_hidden: Option<bool>,
) -> Result<Vec<Marker>, String> {
    let limit = limit.unwrap_or(5000).min(50_000);
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);
    let re = marker_regex();

    let mut out = Vec::new();
    for path in walk_files(&root, show_hidden.unwrap_or(false)) {
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
        out.extend(scan_markers_in(&rel, &path_s, &content, &re));
        if out.len() >= limit {
            out.truncate(limit);
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_upsert_dedupes_and_caps_front() {
        let mut list = Vec::new();
        upsert_bookmark(
            &mut list,
            Bookmark {
                path: "a".into(),
                line: 3,
                label: "x".into(),
            },
        );
        upsert_bookmark(
            &mut list,
            Bookmark {
                path: "b".into(),
                line: 1,
                label: "y".into(),
            },
        );
        // Re-adding a:3 moves it to the front and replaces the label (no duplicate).
        upsert_bookmark(
            &mut list,
            Bookmark {
                path: "a".into(),
                line: 3,
                label: "z".into(),
            },
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "a");
        assert_eq!(list[0].label, "z");
    }

    #[test]
    fn bookmarks_round_trip_on_disk() {
        let dir = tempdir();
        let f = dir.join("bm.json");
        let list = vec![Bookmark {
            path: "/p/q.rs".into(),
            line: 42,
            label: "here".into(),
        }];
        save_bookmarks_at(&f, &list).unwrap();
        let back = load_bookmarks_at(&f);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].line, 42);
        assert_eq!(back[0].label, "here");
        cleanup(&dir);
    }

    #[test]
    fn replace_preview_and_apply_on_disk() {
        let dir = tempdir();
        let f = dir.join("a.txt");
        std::fs::write(&f, "foo bar foo\nno match\nfoo end\n").unwrap();
        let root: String = dir.to_string_lossy().into();

        // Preview: 3 matches across 1 file, nothing written.
        let pv = replace_project(root.clone(), "foo".into(), "baz".into(), None).unwrap();
        assert_eq!(pv.total, 3);
        assert_eq!(pv.files, 1);
        assert!(!pv.applied);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "foo bar foo\nno match\nfoo end\n"
        );

        // Apply.
        let ap = replace_project(
            root,
            "foo".into(),
            "baz".into(),
            Some(ReplaceOpts {
                apply: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(ap.total, 3);
        assert!(ap.applied);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "baz bar baz\nno match\nbaz end\n"
        );
        cleanup(&dir);
    }

    #[test]
    fn replace_regex_capture_reference() {
        let dir = tempdir();
        let f = dir.join("b.txt");
        std::fs::write(&f, "name=alice\nname=bob\n").unwrap();
        let root: String = dir.to_string_lossy().into();
        let r = replace_project(
            root,
            r"name=(\w+)".into(),
            "user:$1".into(),
            Some(ReplaceOpts {
                regex: Some(true),
                apply: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "user:alice\nuser:bob\n"
        );
        cleanup(&dir);
    }

    #[test]
    fn replace_preserves_crlf_lines() {
        // split('\n')/join('\n') keeps the trailing \r inside each line, so CRLF survives.
        let re = build_regex("a", &ReplaceOpts::default()).unwrap();
        let (out, _, n) = replace_in_content(&re, "a\r\nb\r\n", "X");
        assert_eq!(n, 1);
        assert_eq!(out, "X\r\nb\r\n");
    }

    #[test]
    fn symbols_rust_and_markdown() {
        let rs = "pub fn hello() {}\nstruct Foo;\n    async fn nested() {}\n";
        let syms = extract_symbols("x.rs", "/x.rs", "rs", rs);
        assert!(syms.iter().any(|s| s.name == "hello" && s.kind == "fn"));
        assert!(syms.iter().any(|s| s.name == "Foo" && s.kind == "struct"));
        assert!(syms.iter().any(|s| s.name == "nested" && s.line == 3));

        let md = "# Title\n## Section\ntext\n";
        let h = extract_symbols("r.md", "/r.md", "md", md);
        assert!(h.iter().any(|s| s.name == "Title" && s.kind == "H1"));
        assert!(h.iter().any(|s| s.name == "Section" && s.kind == "H2"));
    }

    #[test]
    fn symbols_unknown_ext_empty() {
        assert!(extract_symbols("a.xyz", "/a.xyz", "xyz", "whatever\n").is_empty());
    }

    #[test]
    fn markers_extracts_tag_and_text() {
        let re = marker_regex();
        let src = "// TODO: wire this up\nlet x = 1; // FIXME broken\nplain line\n";
        let ms = scan_markers_in("s.rs", "/s.rs", src, &re);
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].kind, "TODO");
        assert_eq!(ms[0].text, "wire this up");
        assert_eq!(ms[1].kind, "FIXME");
        assert_eq!(ms[1].line, 2);
    }

    // ── tiny tempdir helpers (no external dev-dep) ──
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-et-test-{}-{}",
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
