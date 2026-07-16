//! Text tools — on-disk buffer/file transforms and code navigation beside the search/replace family
//! in `editor_tools.rs`. Four surfaces:
//!
//! * **File cleanup / convert** — normalise line endings (LF/CRLF), trim trailing whitespace, expand
//!   tabs to spaces or tabify leading indentation, and enforce a final newline. A dry-run preview
//!   reports what would change; apply rewrites the file.
//! * **Sort lines** — reorder a file's lines (reverse / case-insensitive / numeric / unique).
//! * **Find definition** — jump to where a symbol is *defined* (not every occurrence): reuses the
//!   `editor_tools` symbol rules to locate `fn`/`struct`/`class`/… declarations of an exact name
//!   across the tree.
//! * **Batch rename** — rename files whose base name matches a find → replace rule (literal or
//!   regex), previewed before it touches disk.
//!
//! Same host contract as the rest of the workbench: only the apply paths mutate the filesystem
//! (mirroring `replace_project`); the front-end re-opens the file afterward so the editor reloads it.

use crate::editor_tools::{ext_of, extract_symbols, symbol_rules, Symbol};
use crate::project::{looks_binary, walk_files, MAX_GREP_FILE_BYTES};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ── file cleanup / convert (line endings, trailing ws, tabs, final newline) ────────────────────────

#[derive(Deserialize, Default)]
pub struct ConvertOpts {
    /// Target line ending: `"lf"`, `"crlf"`, or `None` to keep (CRLF if the file has any, else LF).
    pub eol: Option<String>,
    /// Strip trailing spaces/tabs from every line.
    pub trim_trailing: Option<bool>,
    /// Tab handling: `"expand"` (tabs → spaces), `"tabify"` (leading spaces → tabs), or `None`.
    pub tabs: Option<String>,
    /// Spaces per tab stop for `expand`/`tabify` (default 4).
    pub tab_width: Option<usize>,
    /// Ensure the file ends with exactly one newline (`None` keeps the original trailing state).
    pub final_newline: Option<bool>,
    /// When true, rewrite the file; otherwise a dry-run.
    pub apply: Option<bool>,
}

#[derive(Serialize)]
pub struct ConvertResult {
    /// Number of lines whose text content changed (line-ending-only changes aren't counted here).
    pub changed_lines: usize,
    /// True when the resulting bytes differ from the original (includes ending-only changes).
    pub differs: bool,
    /// True when the file was rewritten (`apply` and `differs`).
    pub applied: bool,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// Convert leading whitespace of `s` to tabs at `width`-column tab stops, preserving the rest.
fn tabify_leading(s: &str, width: usize) -> String {
    let indent_len = s.len() - s.trim_start_matches([' ', '\t']).len();
    let (leading, rest) = s.split_at(indent_len);
    let mut col = 0usize;
    for c in leading.chars() {
        if c == '\t' {
            col += width - (col % width);
        } else {
            col += 1;
        }
    }
    let tabs = col / width;
    let spaces = col % width;
    format!("{}{}{}", "\t".repeat(tabs), " ".repeat(spaces), rest)
}

/// Pure text transform for the cleanup/convert command — returns the rewritten text and the count of
/// content-changed lines. Unit tested directly.
fn transform_text(content: &str, opts: &ConvertOpts) -> (String, usize) {
    let had_crlf = content.contains("\r\n");
    let had_trailing_nl = content.ends_with('\n');
    let eol = match opts.eol.as_deref() {
        Some("crlf") => "\r\n",
        Some("lf") => "\n",
        _ => {
            if had_crlf {
                "\r\n"
            } else {
                "\n"
            }
        }
    };
    let tab_width = opts.tab_width.unwrap_or(4).max(1);
    let trim = opts.trim_trailing.unwrap_or(false);
    let tabs_mode = opts.tabs.as_deref();

    let mut raw: Vec<&str> = content.split('\n').collect();
    if had_trailing_nl {
        raw.pop(); // drop the empty element after the final '\n'
    }

    let mut changed = 0usize;
    let mut out_lines: Vec<String> = Vec::with_capacity(raw.len());
    for line in raw {
        let logical = line.strip_suffix('\r').unwrap_or(line);
        let mut s = logical.to_string();
        if trim {
            s = s.trim_end_matches([' ', '\t']).to_string();
        }
        match tabs_mode {
            Some("expand") => s = s.replace('\t', &" ".repeat(tab_width)),
            Some("tabify") => s = tabify_leading(&s, tab_width),
            _ => {}
        }
        if s != logical {
            changed += 1;
        }
        out_lines.push(s);
    }

    let mut out = out_lines.join(eol);
    let want_nl = opts.final_newline.unwrap_or(had_trailing_nl);
    if want_nl && !out.is_empty() {
        out.push_str(eol);
    }
    (out, changed)
}

/// Preview (or apply) a file cleanup/convert. Skips binary and oversized files.
#[tauri::command]
pub fn convert_file(path: String, opts: Option<ConvertOpts>) -> Result<ConvertResult, String> {
    let opts = opts.unwrap_or_default();
    let p = PathBuf::from(&path);
    if fs::metadata(&p)
        .map(|m| m.len() > MAX_GREP_FILE_BYTES)
        .unwrap_or(true)
    {
        return Err("file too large or unreadable".into());
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    if looks_binary(&bytes) {
        return Err("binary file".into());
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    let (out, changed_lines) = transform_text(&content, &opts);
    let differs = out != content;
    let apply = opts.apply.unwrap_or(false);
    let applied = apply && differs;
    if applied {
        fs::write(&p, out.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(ConvertResult {
        changed_lines,
        differs,
        applied,
        bytes_before: content.len(),
        bytes_after: out.len(),
    })
}

// ── sort lines ─────────────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct SortOpts {
    pub reverse: Option<bool>,
    pub case_insensitive: Option<bool>,
    /// Sort by the leading numeric value of each line (non-numeric lines sort as 0).
    pub numeric: Option<bool>,
    /// Drop adjacent duplicate lines after sorting (a sorted `uniq`).
    pub unique: Option<bool>,
    pub apply: Option<bool>,
}

#[derive(Serialize)]
pub struct SortResult {
    pub lines_before: usize,
    pub lines_after: usize,
    pub differs: bool,
    pub applied: bool,
}

/// Parse the leading numeric value of a line (optional sign, digits, optional decimals). `0.0` when
/// there's no leading number — so mixed files sort those lines first without erroring.
fn leading_number(s: &str) -> f64 {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut end = 0usize;
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    let mut seen_dot = false;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_digit() {
            end += 1;
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
            end += 1;
        } else {
            break;
        }
    }
    t[..end].parse().unwrap_or(0.0)
}

/// Pure line-sort — returns the rewritten text. Unit tested directly. Preserves the file's dominant
/// line ending and its trailing-newline state.
fn sort_lines_text(content: &str, opts: &SortOpts) -> String {
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

    let ci = opts.case_insensitive.unwrap_or(false);
    let numeric = opts.numeric.unwrap_or(false);
    if numeric {
        lines.sort_by(|a, b| {
            leading_number(a)
                .partial_cmp(&leading_number(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });
    } else if ci {
        lines.sort_by(|a, b| {
            a.to_lowercase()
                .cmp(&b.to_lowercase())
                .then_with(|| a.cmp(b))
        });
    } else {
        lines.sort();
    }
    if opts.reverse.unwrap_or(false) {
        lines.reverse();
    }
    if opts.unique.unwrap_or(false) {
        if ci && !numeric {
            lines.dedup_by(|a, b| a.to_lowercase() == b.to_lowercase());
        } else {
            lines.dedup();
        }
    }

    let mut out = lines.join(eol);
    if had_trailing_nl && !out.is_empty() {
        out.push_str(eol);
    }
    out
}

/// Preview (or apply) a line sort over one file. Skips binary/oversized files.
#[tauri::command]
pub fn sort_file_lines(path: String, opts: Option<SortOpts>) -> Result<SortResult, String> {
    let opts = opts.unwrap_or_default();
    let p = PathBuf::from(&path);
    if fs::metadata(&p)
        .map(|m| m.len() > MAX_GREP_FILE_BYTES)
        .unwrap_or(true)
    {
        return Err("file too large or unreadable".into());
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    if looks_binary(&bytes) {
        return Err("binary file".into());
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    let out = sort_lines_text(&content, &opts);
    let differs = out != content;
    let applied = opts.apply.unwrap_or(false) && differs;
    if applied {
        fs::write(&p, out.as_bytes()).map_err(|e| e.to_string())?;
    }
    let count = |s: &str| if s.is_empty() { 0 } else { s.lines().count() };
    Ok(SortResult {
        lines_before: count(&content),
        lines_after: count(&out),
        differs,
        applied,
    })
}

// ── find definition (jump to where a symbol is declared) ───────────────────────────────────────────

/// Find declaration sites of an exact symbol `name` across the tree, reusing the `editor_tools`
/// per-language symbol rules (`fn`/`struct`/`class`/`def`/…). Unlike `search_project` this returns
/// only definitions, and unlike `project_symbols` it targets one name — the "go to definition of the
/// thing under the cursor" workflow. Case-sensitive exact match. Capped at `limit`.
#[tauri::command]
pub fn find_definition(
    root: String,
    name: String,
    limit: Option<usize>,
    show_hidden: Option<bool>,
) -> Result<Vec<Symbol>, String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.unwrap_or(200).min(5000);
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
        for sym in extract_symbols(&rel, &path_s, &ext, &content) {
            if sym.name == name {
                out.push(sym);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

// ── batch rename (find → replace on file base names) ───────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct BatchRenameOpts {
    pub regex: Option<bool>,
    pub case_insensitive: Option<bool>,
    pub show_hidden: Option<bool>,
    pub apply: Option<bool>,
    pub max_results: Option<usize>,
}

#[derive(Serialize)]
pub struct RenamePlan {
    pub from: String,
    pub to: String,
    /// Repo-relative source path (what the preview renders).
    pub from_rel: String,
    pub to_rel: String,
    /// Set when this rename couldn't be applied (destination exists / collision); rendered as a note.
    pub skipped: Option<String>,
}

#[derive(Serialize)]
pub struct BatchRenameResult {
    pub plans: Vec<RenamePlan>,
    /// Number of files whose base name matched (before collision checks).
    pub matched: usize,
    /// Number actually renamed (`apply`).
    pub renamed: usize,
    pub applied: bool,
    pub truncated: bool,
}

/// Apply the find→replace to a base name, returning the new name only when it changes and stays a
/// single path component (no `/` introduced). Pure — unit tested.
fn rename_basename(name: &str, re: &regex::Regex, replacement: &str) -> Option<String> {
    if !re.is_match(name) {
        return None;
    }
    let new = re.replace_all(name, replacement).into_owned();
    if new == name || new.is_empty() || new.contains('/') {
        return None;
    }
    Some(new)
}

/// Preview (or apply) a batch rename: rename every file whose *base name* matches `find` → `replace`
/// (literal unless `regex`). Files stay in their directory. Collisions (destination already exists,
/// or two sources mapping to one target) are reported as skipped rather than silently clobbering.
#[tauri::command]
pub fn batch_rename(
    root: String,
    find: String,
    replace: String,
    opts: Option<BatchRenameOpts>,
) -> Result<BatchRenameResult, String> {
    if find.is_empty() {
        return Ok(BatchRenameResult {
            plans: Vec::new(),
            matched: 0,
            renamed: 0,
            applied: false,
            truncated: false,
        });
    }
    let opts = opts.unwrap_or_default();
    let apply = opts.apply.unwrap_or(false);
    let max = opts.max_results.unwrap_or(1000).min(20_000);

    let pattern = if opts.regex.unwrap_or(false) {
        find.clone()
    } else {
        regex::escape(&find)
    };
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(opts.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|e| format!("bad pattern: {e}"))?;

    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);

    let mut plans = Vec::new();
    let mut matched = 0usize;
    let mut renamed = 0usize;
    let mut truncated = false;
    // Track planned targets so two sources can't collide onto one name in a single pass.
    let mut planned_targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for path in walk_files(&root, opts.show_hidden.unwrap_or(false)) {
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => continue,
        };
        let Some(new_name) = rename_basename(&name, &re, &replace) else {
            continue;
        };
        matched += 1;
        let dest = path.with_file_name(&new_name);
        let rel = |p: &Path| {
            p.strip_prefix(&root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        };

        let mut skipped = None;
        if dest.exists() || planned_targets.contains(&dest) {
            skipped = Some("destination exists".into());
        } else if apply {
            match fs::rename(&path, &dest) {
                Ok(()) => renamed += 1,
                Err(e) => skipped = Some(e.to_string()),
            }
        }
        if skipped.is_none() {
            planned_targets.insert(dest.clone());
        }

        plans.push(RenamePlan {
            from_rel: rel(&path),
            to_rel: rel(&dest),
            from: path.to_string_lossy().into_owned(),
            to: dest.to_string_lossy().into_owned(),
            skipped,
        });
        if plans.len() >= max {
            truncated = true;
            break;
        }
    }

    Ok(BatchRenameResult {
        plans,
        matched,
        renamed,
        applied: apply,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_trims_and_final_newline() {
        let opts = ConvertOpts {
            trim_trailing: Some(true),
            final_newline: Some(true),
            ..Default::default()
        };
        let (out, changed) = transform_text("a   \nb\t\nc", &opts);
        assert_eq!(out, "a\nb\nc\n");
        assert_eq!(changed, 2); // lines a and b had trailing ws; c unchanged
    }

    #[test]
    fn transform_eol_crlf_and_back() {
        let to_crlf = ConvertOpts {
            eol: Some("crlf".into()),
            ..Default::default()
        };
        let (out, _) = transform_text("a\nb\n", &to_crlf);
        assert_eq!(out, "a\r\nb\r\n");
        let to_lf = ConvertOpts {
            eol: Some("lf".into()),
            ..Default::default()
        };
        let (back, _) = transform_text("a\r\nb\r\n", &to_lf);
        assert_eq!(back, "a\nb\n");
    }

    #[test]
    fn transform_expand_and_tabify() {
        let expand = ConvertOpts {
            tabs: Some("expand".into()),
            tab_width: Some(2),
            ..Default::default()
        };
        let (out, _) = transform_text("\tx\n", &expand);
        assert_eq!(out, "  x\n");

        let tabify = ConvertOpts {
            tabs: Some("tabify".into()),
            tab_width: Some(4),
            ..Default::default()
        };
        let (out2, _) = transform_text("        y\n", &tabify); // 8 spaces → 2 tabs
        assert_eq!(out2, "\t\ty\n");
    }

    #[test]
    fn sort_basic_reverse_unique_numeric() {
        assert_eq!(
            sort_lines_text("b\na\nc\n", &SortOpts::default()),
            "a\nb\nc\n"
        );
        let rev = SortOpts {
            reverse: Some(true),
            ..Default::default()
        };
        assert_eq!(sort_lines_text("a\nb\nc\n", &rev), "c\nb\na\n");
        let uniq = SortOpts {
            unique: Some(true),
            ..Default::default()
        };
        assert_eq!(sort_lines_text("a\nb\na\n", &uniq), "a\nb\n");
        let num = SortOpts {
            numeric: Some(true),
            ..Default::default()
        };
        // Lexical sort would put "10" before "9"; numeric keeps 2 < 9 < 10.
        assert_eq!(sort_lines_text("10\n2\n9\n", &num), "2\n9\n10\n");
    }

    #[test]
    fn leading_number_parses_prefix() {
        assert_eq!(leading_number("42 apples"), 42.0);
        assert_eq!(leading_number("-3.5x"), -3.5);
        assert_eq!(leading_number("none"), 0.0);
    }

    #[test]
    fn rename_basename_literal_and_guard() {
        let re = regex::Regex::new(&regex::escape("foo")).unwrap();
        assert_eq!(
            rename_basename("foo_bar.txt", &re, "baz"),
            Some("baz_bar.txt".into())
        );
        // No match → None.
        assert_eq!(rename_basename("x.txt", &re, "baz"), None);
        // A replacement that would introduce a path separator is rejected.
        let re2 = regex::Regex::new("_").unwrap();
        assert_eq!(rename_basename("a_b", &re2, "/"), None);
    }

    #[test]
    fn convert_and_sort_apply_on_disk() {
        let dir = tempdir();
        let f = dir.join("a.txt");
        std::fs::write(&f, "b  \na\n").unwrap();

        // Cleanup: trim trailing ws.
        let r = convert_file(
            f.to_string_lossy().into(),
            Some(ConvertOpts {
                trim_trailing: Some(true),
                apply: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(r.applied && r.differs);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "b\na\n");

        // Sort the trimmed file.
        let s = sort_file_lines(
            f.to_string_lossy().into(),
            Some(SortOpts {
                apply: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(s.applied);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\n");
        cleanup(&dir);
    }

    #[test]
    fn find_definition_locates_exact_name() {
        let dir = tempdir();
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "pub fn alpha() {}\n").unwrap();
        let defs =
            find_definition(dir.to_string_lossy().into(), "alpha".into(), None, None).unwrap();
        assert_eq!(defs.len(), 2, "alpha defined in two files");
        assert!(defs.iter().all(|d| d.name == "alpha"));
        assert!(
            find_definition(dir.to_string_lossy().into(), "gamma".into(), None, None)
                .unwrap()
                .is_empty()
        );
        cleanup(&dir);
    }

    #[test]
    fn batch_rename_preview_then_apply_and_collision() {
        let dir = tempdir();
        std::fs::write(dir.join("old_one.txt"), "1").unwrap();
        std::fs::write(dir.join("old_two.txt"), "2").unwrap();
        let root: String = dir.to_string_lossy().into();

        // Preview: two matches, nothing renamed, files intact.
        let pv = batch_rename(root.clone(), "old".into(), "new".into(), None).unwrap();
        assert_eq!(pv.matched, 2);
        assert_eq!(pv.renamed, 0);
        assert!(!pv.applied);
        assert!(dir.join("old_one.txt").exists());

        // Apply.
        let ap = batch_rename(
            root,
            "old".into(),
            "new".into(),
            Some(BatchRenameOpts {
                apply: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(ap.renamed, 2);
        assert!(dir.join("new_one.txt").exists() && dir.join("new_two.txt").exists());
        assert!(!dir.join("old_one.txt").exists());
        cleanup(&dir);
    }

    // ── tiny tempdir helpers (no external dev-dep) ──
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-tt-test-{}-{}",
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
