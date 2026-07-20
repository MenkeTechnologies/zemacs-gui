//! Document blame — `git blame` at **document-address** granularity.
//!
//! `git blame` answers "who last changed this line". For a `.xlsx` or a `.pdf` there are no lines:
//! git reports `Binary files differ` and stops. The prevailing workaround is a `textconv` diff
//! driver that shells out to `pandoc` / `unoconv` once per file per revision and flattens the
//! document to a throwaway line stream — so the number you get back is a line of the *rendering*,
//! not an address in the document.
//!
//! This module answers the question in the document's own coordinate system instead:
//!
//! ```text
//! Sheet1!B14   a3f91c2e  2026-03-04  <author>  quarterly figures
//! p. 7         5d10ba71  2026-01-19  <author>  redraft the appendix
//! ```
//!
//! How it works: walk the revisions that touched the file (`git log --follow`), materialise each
//! revision's blob (`git show <rev>:<path-at-that-rev>`), parse every revision **in-process** with
//! the `zoffice-core` / `zpdf-core` engines already linked into this host (see `doc_search.rs`,
//! which owns the same parsers for search/replace), and attribute each address to the newest
//! revision at which that address's content differs from its predecessor's. No subprocess per
//! revision beyond `git` itself, and no external converter.
//!
//! # Scope, stated plainly
//!
//! Only **stable** addresses are blamed: spreadsheet cells ([`DocLocator::Cell`]) and PDF pages
//! ([`DocLocator::Page`]). Paragraph and slide indices *shift* when content is inserted above them,
//! so index-keyed attribution would mis-blame every unit below an insertion — a wrong answer that
//! looks right. Those formats are refused with an explanation rather than answered badly. Fixing
//! them needs content-hash alignment between adjacent revisions, which is not in this pass.
//!
//! # Truncation, stated plainly
//!
//! The walk is capped ([`DEFAULT_REV_CAP`]). The oldest revision walked has no predecessor inside
//! the window, so every address that has not changed within the window attributes *to* it — which
//! would be a lie if real history runs deeper. Those rows carry [`DocBlameEntry::at_or_before`],
//! and the result carries the revision counts, so the UI can say "at or before" instead of
//! asserting an authorship it cannot see. Same convention as `doc_search.rs`'s `whole_run_only`
//! rows: surface the limit, never hide it.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use zoffice_core::calc::Workbook;

use crate::doc_search::{ext_of, snippet, DocLocator, FileKind};

/// Revisions walked when the caller does not say. Chosen because `git log --follow` already
/// narrows to the commits that *touched this one file*, which is usually single digits to low
/// tens — not repo history. The cap is the guard against the pathological file, not the norm.
pub const DEFAULT_REV_CAP: usize = 50;

/// Hard ceiling on the cap a caller can ask for. Blame is O(revisions) full-document parses.
const MAX_REV_CAP: usize = 500;

// ── git plumbing ─────────────────────────────────────────────────────────────────────────────────

/// Run `git -C <dir> <args…>`, returning stdout **as text** or the trimmed stderr as the error.
/// Mirrors `git_tools.rs` / `git_more.rs`'s helper of the same name.
fn git_in(dir: &str, args: &[&str]) -> Result<String, String> {
    let out = git_raw(dir, args)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Run `git -C <dir> <args…>`, returning stdout as **raw bytes**.
///
/// The bytes variant is load-bearing rather than a nicety: `git show <rev>:<path>` on a `.xlsx` or
/// a `.pdf` emits a zip package or a binary PDF, and `String::from_utf8_lossy` would replace every
/// invalid sequence with U+FFFD — producing a blob that no engine can parse.
fn git_raw(dir: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("{GIT_MISSING}: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(out.stdout)
}

/// The directory to run git in for a given file path (its parent, so `-C` lands inside the repo).
fn dir_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

/// Dep-free civil date (unix seconds → `YYYY-MM-DD`, UTC). Howard Hinnant's days-from-civil
/// inverse, as in `git_tools.rs` — kept local so this module has no cross-file coupling.
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
    format!("{y:04}-{m:02}-{d:02}")
}

// ── revision walk ────────────────────────────────────────────────────────────────────────────────

/// One revision that touched the document, with the path the file had **at that revision**.
#[derive(Clone, Debug)]
struct Rev {
    hash: String,
    author: String,
    time: i64,
    summary: String,
    /// Repo-relative path in this revision. Differs from the current path across a rename, which
    /// is exactly why `--name-only` is parsed instead of reusing the caller's path everywhere.
    path_at_rev: String,
}

/// Parse `git log --follow --format=%x00%H\x1f%an\x1f%at\x1f%s --name-only` output.
///
/// Each record starts with a NUL, then the `\x1f`-joined header, then a blank line, then the file
/// name(s) that commit touched. `--follow` restricts that to the single followed path, so the
/// first name line is the path at that revision.
fn parse_rev_log(out: &str) -> Vec<Rev> {
    let mut revs = Vec::new();
    for record in out.split('\u{0}') {
        let record = record.trim_start_matches('\n');
        if record.trim().is_empty() {
            continue;
        }
        let mut lines = record.lines();
        let Some(header) = lines.next() else { continue };
        let mut f = header.split('\u{1f}');
        let (Some(hash), Some(author), Some(time), Some(summary)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        // The first non-empty line after the header is the path in this revision. A commit with no
        // name line (an empty diff against the followed path) keeps the record but has no blob to
        // read, so it is skipped rather than blamed.
        let Some(path_at_rev) = lines.map(str::trim).find(|l| !l.is_empty()) else {
            continue;
        };
        revs.push(Rev {
            hash: hash.to_string(),
            author: author.to_string(),
            time: time.trim().parse().unwrap_or(0),
            summary: summary.to_string(),
            path_at_rev: path_at_rev.to_string(),
        });
    }
    revs
}

/// Prefix `git_raw` puts on a *spawn* failure, as opposed to git running and exiting non-zero.
const GIT_MISSING: &str = "git not available";

/// The revisions that touched `path`, newest first, capped at `cap`.
///
/// A non-zero `git log` means there is nothing to walk — no commits yet, path never tracked, or not
/// a repository at all — and every one of those is "no history", which the caller already words
/// better than git's own `fatal:` line. Only a failure to *run* git is propagated, because that is
/// a broken environment rather than an answer.
fn revisions_for(dir: &str, path: &str, cap: usize) -> Result<Vec<Rev>, String> {
    let max = format!("-n{cap}");
    let out = git_in(
        dir,
        &[
            "log",
            &max,
            "--follow",
            "--name-only",
            "--format=%x00%H\u{1f}%an\u{1f}%at\u{1f}%s",
            "--",
            path,
        ],
    );
    match out {
        Ok(text) => Ok(parse_rev_log(&text)),
        Err(e) if e.starts_with(GIT_MISSING) => Err(e),
        Err(_) => Ok(Vec::new()),
    }
}

/// Whether more revisions exist beyond the ones walked — asked separately (and cheaply, with
/// `--format=%H` only) so the UI can say how deep real history runs instead of guessing from a
/// full window.
fn total_revisions(dir: &str, path: &str) -> usize {
    git_in(
        dir,
        &["log", "--follow", "--format=%H", "--", path],
    )
    .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
    .unwrap_or(0)
}

// ── addressed snapshots ──────────────────────────────────────────────────────────────────────────

/// One revision of a document, flattened to `address key -> (locator, content)`.
///
/// The key is a canonical string form of the address, used only for map identity; the [`DocLocator`]
/// beside it is what reaches the front end, which already knows how to render it (`locatorLabel`
/// in `panels.js`).
type Snapshot = BTreeMap<String, (DocLocator, String)>;

/// Canonical key for a cell address: sheet index first, so the map orders by sheet then by the
/// reference's own text.
fn cell_key(sheet: usize, reference: &str) -> String {
    format!("c{sheet:04}:{reference}")
}

/// Canonical key for a page address.
fn page_key(page: u32) -> String {
    format!("p{page:06}")
}

/// A temp path for a historical blob. Lands in the system temp dir rather than beside the source
/// (unlike `doc_search.rs`'s `temp_out_for`, which needs same-filesystem rename atomicity) because
/// nothing here is renamed into place — and writing N historical revisions of a file into the
/// user's project directory would be visible churn for no gain.
///
/// The extension is preserved because the engines dispatch on it: `Workbook::open` matches
/// `xlsx`/`ods`/… by extension (`zoffice-core/src/calc.rs:797`), so an extension-less temp file
/// fails to open no matter what the bytes are.
fn temp_blob_path(ext: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        ".zmax-blame-{}-{}.{ext}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Materialise `rev`'s blob to a temp file, parse it, and delete the temp file.
///
/// The temp file exists only because every engine exposes a path-only `open`; the same constraint
/// (and the same answer) is already in `doc_search.rs`'s dry-run path.
fn snapshot_at(dir: &str, rev: &Rev, kind: FileKind, ext: &str) -> Result<Snapshot, String> {
    let spec = format!("{}:{}", rev.hash, rev.path_at_rev);
    let bytes = git_raw(dir, &["show", &spec])?;
    let tmp = temp_blob_path(ext);
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write temp blob: {e}"))?;
    let parsed = snapshot_of(&tmp, kind);
    let _ = std::fs::remove_file(&tmp);
    parsed
}

/// Flatten the document at `path` into its addressed units.
///
/// Only the address kinds this module can blame honestly are produced — see the module docs on why
/// paragraphs and slides are excluded rather than approximated.
fn snapshot_of(path: &Path, kind: FileKind) -> Result<Snapshot, String> {
    let mut out = Snapshot::new();
    match kind {
        FileKind::Office(zoffice_core::App::Calc) => {
            let wb = Workbook::open(path).map_err(|e| e.to_string())?;
            for (si, sheet) in wb.sheets.iter().enumerate() {
                for row in &sheet.rows {
                    for cell in &row.cells {
                        // An empty cell is an absent address, not an address holding "". Keeping
                        // blanks would blame every cell a spreadsheet engine happens to emit a
                        // stub for, which differs between .xlsx and .ods for the same content.
                        if cell.value.trim().is_empty() {
                            continue;
                        }
                        out.insert(
                            cell_key(si, &cell.reference),
                            (
                                DocLocator::Cell {
                                    sheet: si,
                                    sheet_name: sheet.name.clone(),
                                    reference: cell.reference.clone(),
                                },
                                cell.value.clone(),
                            ),
                        );
                    }
                }
            }
        }
        FileKind::Pdf => {
            let pdf = zpdf_core::Pdf::open(path).map_err(|e| e.to_string())?;
            let mut pages = pdf.page_numbers();
            pages.sort_unstable();
            for p in pages {
                let Ok(text) = pdf.extract_text(p) else {
                    continue;
                };
                out.insert(
                    page_key(p),
                    // `rect` stays `None`: a rect is a *search-hit* highlight resolved from a query
                    // run, and blame has no query. The locator is honest at page granularity.
                    (DocLocator::Page { page: p, rect: None }, text),
                );
            }
        }
        _ => return Err(unsupported_message(kind)),
    }
    Ok(out)
}

/// Why a format is refused, in the terms of the limitation rather than a bare "unsupported".
fn unsupported_message(kind: FileKind) -> String {
    match kind {
        FileKind::Office(zoffice_core::App::Writer) => "document blame does not cover word-processor \
             files yet: a paragraph index shifts when a paragraph is inserted above it, so \
             index-keyed attribution would mis-blame every paragraph below an insertion. Spreadsheet \
             cells and PDF pages have stable addresses and are supported."
            .into(),
        FileKind::Office(zoffice_core::App::Impress) => "document blame does not cover presentations \
             yet: a slide index shifts when a slide is inserted before it, so index-keyed \
             attribution would mis-blame every later slide. Spreadsheet cells and PDF pages have \
             stable addresses and are supported."
            .into(),
        _ => "document blame supports spreadsheets (.xlsx/.ods) and PDF. Text files are covered by \
              the per-line blame panel."
            .into(),
    }
}

// ── attribution ──────────────────────────────────────────────────────────────────────────────────

/// One blamed address.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct DocBlameEntry {
    /// The address, in the same enum the search panel already renders.
    pub locator: DocLocator,
    /// Current content at that address, capped like a search snippet.
    pub text: String,
    /// Abbreviated commit hash (first 8 chars), matching `git_tools.rs`'s blame badge.
    pub commit: String,
    pub hash: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    pub summary: String,
    /// True when this address's last change is pinned to the **oldest revision walked** while
    /// deeper history exists — i.e. the real change may be older than the window. The row is
    /// "changed at or before this commit", not "changed by it".
    pub at_or_before: bool,
}

/// A blame pass over one document.
#[derive(Serialize, Debug, Default)]
pub struct DocBlameResult {
    pub path: String,
    /// `"xlsx" | "ods" | "pdf"`.
    pub format: String,
    /// Addresses present in the newest revision, blamed. Ordered by address.
    pub entries: Vec<DocBlameEntry>,
    /// Revisions actually parsed.
    pub revisions_walked: usize,
    /// Revisions that touched the file in total, however deep. Never equal to
    /// `revisions_walked` when the cap bit — that difference is the whole point of reporting both.
    pub revisions_total: usize,
    /// True when the cap cut the walk short.
    pub truncated: bool,
    /// `(revision, engine error)` for revisions that failed to parse. Surfaced, never swallowed:
    /// a revision this pass could not read is a gap in the attribution, not a non-event.
    pub errors: Vec<(String, String)>,
}

/// Attribute each address in the newest snapshot to the newest revision that changed it.
///
/// `snaps` is ordered **oldest first** and pairs each revision with its parsed snapshot. An address
/// is "changed at" revision *i* when its content differs from revision *i-1*'s content at the same
/// address, or when it did not exist at *i-1*. Walking forward once and overwriting gives each
/// address its newest such revision without an O(revisions²) rescan.
fn attribute(snaps: &[(Rev, Snapshot)], truncated: bool) -> Vec<DocBlameEntry> {
    let mut last_change: HashMap<&str, usize> = HashMap::new();
    for (i, (_rev, snap)) in snaps.iter().enumerate() {
        for (key, (_loc, value)) in snap {
            let changed = match i.checked_sub(1) {
                None => true,
                Some(prev) => snaps[prev]
                    .1
                    .get(key)
                    .map(|(_, pv)| pv != value)
                    .unwrap_or(true),
            };
            if changed {
                last_change.insert(key.as_str(), i);
            }
        }
    }

    // The output is keyed to the newest revision: an address that existed only in history is not a
    // row in the current document.
    let Some((_, newest)) = snaps.last() else {
        return Vec::new();
    };
    newest
        .iter()
        .map(|(key, (loc, value))| {
            let idx = last_change.get(key.as_str()).copied().unwrap_or(0);
            let rev = &snaps[idx].0;
            DocBlameEntry {
                locator: loc.clone(),
                text: snippet(value),
                commit: rev.hash.chars().take(8).collect(),
                hash: rev.hash.clone(),
                author: rev.author.clone(),
                date: fmt_date(rev.time),
                summary: rev.summary.clone(),
                // Only the oldest walked revision can be an artefact of the window: every later
                // attribution was proved by a real difference against its predecessor.
                at_or_before: truncated && idx == 0,
            }
        })
        .collect()
}

// ── command ──────────────────────────────────────────────────────────────────────────────────────

/// Blame a binary document at document-address granularity.
///
/// `limit` caps the revision walk (default [`DEFAULT_REV_CAP`]). Errors are returned for a format
/// whose addresses are not stable enough to blame — see [`unsupported_message`] — rather than
/// answering with attribution that would be quietly wrong.
#[tauri::command]
pub fn doc_blame(path: String, limit: Option<usize>) -> Result<DocBlameResult, String> {
    let p = PathBuf::from(&path);
    let ext = ext_of(&p);
    let kind = crate::doc_search::classify(&p, &[]);
    if !matches!(
        kind,
        FileKind::Pdf | FileKind::Office(zoffice_core::App::Calc)
    ) {
        return Err(unsupported_message(kind));
    }

    let dir = dir_of(&path);
    let cap = limit.unwrap_or(DEFAULT_REV_CAP).clamp(1, MAX_REV_CAP);
    let mut revs = revisions_for(&dir, &path, cap)?;
    if revs.is_empty() {
        return Err("no history for this document (untracked, or not in a git repository)".into());
    }
    let total = total_revisions(&dir, &path).max(revs.len());
    let truncated = total > revs.len();
    // `git log` is newest-first; attribution walks forward through time.
    revs.reverse();

    let mut snaps: Vec<(Rev, Snapshot)> = Vec::with_capacity(revs.len());
    let mut errors: Vec<(String, String)> = Vec::new();
    for rev in revs {
        match snapshot_at(&dir, &rev, kind, &ext) {
            Ok(snap) => snaps.push((rev, snap)),
            // A revision that will not parse is skipped, not fatal: one corrupt historical blob
            // should not deny blame for the other forty-nine. The gap is reported.
            Err(e) => errors.push((rev.hash.chars().take(8).collect(), e)),
        }
    }
    if snaps.is_empty() {
        return Err(format!(
            "no revision of this document could be parsed ({} failed)",
            errors.len()
        ));
    }

    let walked = snaps.len();
    Ok(DocBlameResult {
        path,
        format: ext,
        entries: attribute(&snaps, truncated),
        revisions_walked: walked,
        revisions_total: total,
        truncated,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zoffice_core::calc::{Cell, Row, Sheet};

    // ── fixtures ────────────────────────────────────────────────────────────────

    /// A directory unique to this call. The counter matters for the same reason it does in
    /// `doc_search.rs`: these run in parallel in one process and a pid-only name collides.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-blame-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An `.xlsx` at `path` with one sheet whose cells are `(reference, value)`.
    fn write_xlsx(path: &Path, cells: &[(&str, &str)]) {
        let wb = Workbook {
            sheets: vec![Sheet {
                name: "Sheet1".into(),
                rows: vec![Row {
                    cells: cells
                        .iter()
                        .map(|(r, v)| Cell {
                            reference: (*r).to_string(),
                            value: (*v).to_string(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        wb.save_xlsx(path).expect("save xlsx fixture");
    }

    /// A one-page PDF drawing `text` as a single Helvetica run. Same construction as
    /// `doc_search.rs`'s fixture, so no binary blobs live in the repo.
    fn write_pdf(path: &Path, text: &str) {
        use lopdf::{dictionary, Document as LDoc, Object, Stream};
        let mut doc = LDoc::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content = format!("BT /F1 24 Tf 72 700 Td ({text}) Tj ET").into_bytes();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1i64,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).expect("save pdf fixture");
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with deterministic identity, so commits are reproducible on a bare CI runner with no
    /// global git config.
    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test User"]);
    }

    fn commit_all(dir: &Path, message: &str) {
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", message]);
    }

    fn entry_for<'a>(res: &'a DocBlameResult, reference: &str) -> &'a DocBlameEntry {
        res.entries
            .iter()
            .find(|e| matches!(&e.locator, DocLocator::Cell { reference: r, .. } if r == reference))
            .unwrap_or_else(|| panic!("no blame entry for {reference} in {:?}", res.entries))
    }

    // ── the premise: an untouched cell keeps its original author ─────────────────

    /// The core claim of the feature, and the thing a regression would break first: editing one
    /// cell must not re-blame the cells beside it. `git` cannot answer this at all — the whole
    /// file is one binary blob, so a line-based tool attributes *everything* to the last commit.
    #[test]
    fn changing_one_cell_does_not_reblame_its_neighbours() {
        let dir = tempdir();
        init_repo(&dir);
        let book = dir.join("figures.xlsx");

        write_xlsx(&book, &[("A1", "revenue"), ("B1", "42")]);
        commit_all(&dir, "first: revenue 42");

        // Only B1 moves. A1's bytes inside the zip change too (shared strings are rewritten), which
        // is exactly why a byte-level tool cannot tell these apart.
        write_xlsx(&book, &[("A1", "revenue"), ("B1", "47")]);
        commit_all(&dir, "second: revenue 47");

        let res = doc_blame(book.to_string_lossy().into_owned(), None).expect("blame");
        assert_eq!(res.revisions_walked, 2, "both revisions parsed");
        assert!(!res.truncated, "history is shorter than the cap");
        assert!(res.errors.is_empty(), "unexpected parse errors: {:?}", res.errors);

        let b1 = entry_for(&res, "B1");
        assert_eq!(b1.summary, "second: revenue 47", "B1 changed in the 2nd commit");
        assert_eq!(b1.text, "47");

        let a1 = entry_for(&res, "A1");
        assert_eq!(
            a1.summary, "first: revenue 42",
            "A1 never changed, so it must still be blamed to the 1st commit"
        );
        assert!(!a1.at_or_before, "history was fully walked, so nothing is approximate");
        cleanup(&dir);
    }

    /// A cell added in a later revision is blamed to that revision, not to the file's first commit.
    #[test]
    fn a_cell_added_later_is_blamed_to_the_revision_that_added_it() {
        let dir = tempdir();
        init_repo(&dir);
        let book = dir.join("figures.xlsx");

        write_xlsx(&book, &[("A1", "revenue")]);
        commit_all(&dir, "first");
        write_xlsx(&book, &[("A1", "revenue"), ("B1", "42")]);
        commit_all(&dir, "adds B1");
        write_xlsx(&book, &[("A1", "revenue"), ("B1", "42"), ("C1", "note")]);
        commit_all(&dir, "adds C1");

        let res = doc_blame(book.to_string_lossy().into_owned(), None).expect("blame");
        assert_eq!(entry_for(&res, "A1").summary, "first");
        assert_eq!(entry_for(&res, "B1").summary, "adds B1");
        assert_eq!(entry_for(&res, "C1").summary, "adds C1");
        cleanup(&dir);
    }

    /// An address that only ever existed in history is not a row: blame describes the document as
    /// it is now, so a deleted cell must not resurface.
    #[test]
    fn a_deleted_cell_is_not_a_blame_row() {
        let dir = tempdir();
        init_repo(&dir);
        let book = dir.join("figures.xlsx");

        write_xlsx(&book, &[("A1", "keep"), ("B1", "drop me")]);
        commit_all(&dir, "first");
        write_xlsx(&book, &[("A1", "keep")]);
        commit_all(&dir, "removes B1");

        let res = doc_blame(book.to_string_lossy().into_owned(), None).expect("blame");
        assert!(
            !res.entries.iter().any(
                |e| matches!(&e.locator, DocLocator::Cell { reference, .. } if reference == "B1")
            ),
            "B1 was deleted and must not appear: {:?}",
            res.entries
        );
        assert_eq!(entry_for(&res, "A1").summary, "first");
        cleanup(&dir);
    }

    /// Truncation must be *labelled*, not silent. With the walk capped below real history, the rows
    /// pinned to the oldest visible revision are only "at or before" it — the honest answer.
    #[test]
    fn a_capped_walk_marks_its_oldest_attributions_as_approximate() {
        let dir = tempdir();
        init_repo(&dir);
        let book = dir.join("figures.xlsx");

        write_xlsx(&book, &[("A1", "stable"), ("B1", "v1")]);
        commit_all(&dir, "first");
        write_xlsx(&book, &[("A1", "stable"), ("B1", "v2")]);
        commit_all(&dir, "second");
        write_xlsx(&book, &[("A1", "stable"), ("B1", "v3")]);
        commit_all(&dir, "third");

        let res = doc_blame(book.to_string_lossy().into_owned(), Some(2)).expect("blame");
        assert_eq!(res.revisions_walked, 2, "the cap held");
        assert_eq!(res.revisions_total, 3, "real history is reported, not the window");
        assert!(res.truncated);

        // A1 never changed inside the window, so its true origin is older than the window can see.
        assert!(
            entry_for(&res, "A1").at_or_before,
            "an unchanged-in-window address must be flagged approximate"
        );
        // B1 changed in the newest revision — proved by a real difference, so it is exact.
        let b1 = entry_for(&res, "B1");
        assert_eq!(b1.summary, "third");
        assert!(!b1.at_or_before, "a proved change is never approximate");
        cleanup(&dir);
    }

    /// A rename must not break the walk. `--follow` gives the historical path, and the blob is
    /// read at *that* path — using the current path would make `git show` fail for every revision
    /// before the rename and silently lose the history.
    #[test]
    fn blame_follows_a_renamed_document() {
        let dir = tempdir();
        init_repo(&dir);
        let old = dir.join("old.xlsx");
        write_xlsx(&old, &[("A1", "original")]);
        commit_all(&dir, "first, as old.xlsx");

        run_git(&dir, &["mv", "old.xlsx", "new.xlsx"]);
        commit_all(&dir, "rename to new.xlsx");

        let new = dir.join("new.xlsx");
        let res = doc_blame(new.to_string_lossy().into_owned(), None).expect("blame");
        assert!(
            res.revisions_walked >= 2,
            "the pre-rename revision must be walked, got {}",
            res.revisions_walked
        );
        assert_eq!(
            entry_for(&res, "A1").summary,
            "first, as old.xlsx",
            "A1 predates the rename and must be blamed to its original commit"
        );
        cleanup(&dir);
    }

    /// The second supported format, which the README claims alongside spreadsheets: a PDF page is a
    /// [`DocLocator::Page`] row blamed to the revision that changed that page's text.
    #[test]
    fn pdf_pages_are_blamed_to_the_revision_that_changed_them() {
        let dir = tempdir();
        init_repo(&dir);
        let pdf = dir.join("report.pdf");

        write_pdf(&pdf, "original wording");
        commit_all(&dir, "first draft");
        write_pdf(&pdf, "revised wording");
        commit_all(&dir, "reword page 1");

        let res = doc_blame(pdf.to_string_lossy().into_owned(), None).expect("blame");
        assert_eq!(res.format, "pdf");
        assert_eq!(res.revisions_walked, 2);
        let page = res
            .entries
            .iter()
            .find(|e| matches!(e.locator, DocLocator::Page { page: 1, .. }))
            .unwrap_or_else(|| panic!("no page-1 row in {:?}", res.entries));
        assert_eq!(
            page.summary, "reword page 1",
            "page 1's text changed in the 2nd commit"
        );
        // A blame pass has no query, so there is no run to highlight — the locator must stay honest
        // at page granularity rather than inventing a rect.
        assert!(
            matches!(page.locator, DocLocator::Page { rect: None, .. }),
            "blame must not fabricate an on-page rect"
        );
        cleanup(&dir);
    }

    // ── refusals, which must explain rather than mis-answer ──────────────────────

    /// Word-processor and presentation formats are refused with the *reason*, because index-keyed
    /// paragraph/slide blame would be wrong in a way that looks right.
    #[test]
    fn shifting_address_formats_are_refused_with_an_explanation() {
        let dir = tempdir();
        init_repo(&dir);
        let doc = dir.join("spec.docx");
        std::fs::write(&doc, b"not really a docx").unwrap();
        commit_all(&dir, "first");

        let err = doc_blame(doc.to_string_lossy().into_owned(), None).unwrap_err();
        assert!(
            err.contains("paragraph index shifts"),
            "the refusal must name the limitation, got: {err}"
        );
        cleanup(&dir);
    }

    /// A document with no git history is an error, not an empty list that reads like "nobody ever
    /// touched this".
    #[test]
    fn an_untracked_document_reports_no_history_rather_than_empty_blame() {
        let dir = tempdir();
        init_repo(&dir);
        let book = dir.join("scratch.xlsx");
        write_xlsx(&book, &[("A1", "never committed")]);

        let err = doc_blame(book.to_string_lossy().into_owned(), None).unwrap_err();
        assert!(err.contains("no history"), "got: {err}");
        cleanup(&dir);
    }

    // ── attribution unit, independent of git ─────────────────────────────────────

    /// The attribution rule itself, exercised without a repo so a failure points at the algorithm
    /// rather than at the git plumbing.
    #[test]
    fn attribution_picks_the_newest_revision_that_changed_each_address() {
        let rev = |h: &str, s: &str| Rev {
            hash: h.into(),
            author: "a".into(),
            time: 0,
            summary: s.into(),
            path_at_rev: "f.xlsx".into(),
        };
        let snap = |pairs: &[(&str, &str)]| -> Snapshot {
            pairs
                .iter()
                .map(|(r, v)| {
                    (
                        cell_key(0, r),
                        (
                            DocLocator::Cell {
                                sheet: 0,
                                sheet_name: "Sheet1".into(),
                                reference: (*r).to_string(),
                            },
                            (*v).to_string(),
                        ),
                    )
                })
                .collect()
        };

        let snaps = vec![
            (rev("aaaaaaaaaa", "r1"), snap(&[("A1", "x"), ("B1", "1")])),
            (rev("bbbbbbbbbb", "r2"), snap(&[("A1", "x"), ("B1", "2")])),
            (rev("cccccccccc", "r3"), snap(&[("A1", "x"), ("B1", "2")])),
        ];
        let entries = attribute(&snaps, false);

        let find = |r: &str| {
            entries
                .iter()
                .find(
                    |e| matches!(&e.locator, DocLocator::Cell { reference, .. } if reference == r),
                )
                .unwrap()
                .clone()
        };
        assert_eq!(find("A1").summary, "r1", "A1 never changed after r1");
        assert_eq!(
            find("B1").summary,
            "r2",
            "B1 last changed in r2; r3 left it alone and must not claim it"
        );
    }

    /// The log parser must survive a summary containing the characters a naive split would choke
    /// on, and must carry the per-revision path through.
    #[test]
    fn rev_log_parser_keeps_punctuated_summaries_and_historical_paths() {
        let out = "\u{0}abc123\u{1f}A U\u{1f}1700000000\u{1f}fix: a, b | c\n\ndir/new.xlsx\n\
                   \u{0}def456\u{1f}B U\u{1f}1600000000\u{1f}initial\n\ndir/old.xlsx\n";
        let revs = parse_rev_log(out);
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].summary, "fix: a, b | c");
        assert_eq!(revs[0].path_at_rev, "dir/new.xlsx");
        assert_eq!(revs[1].path_at_rev, "dir/old.xlsx", "the pre-rename path is preserved");
        assert_eq!(revs[1].time, 1_600_000_000);
    }
}
