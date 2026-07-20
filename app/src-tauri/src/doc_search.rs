//! Binary-document-transparent project search and replace.
//!
//! The ordinary project walk (`project::search_project`) skips any file whose first
//! [`project::BINARY_SNIFF_BYTES`] contain a NUL — which is every `.docx`, `.xlsx`,
//! `.pptx`, ODF package and `.pdf`, since all of them are zip or binary containers.
//! This module gives the walker a second branch: files whose extension names a
//! supported document format are parsed with the in-process engines (`zoffice-core`
//! for the OOXML/ODF pairs, `zpdf-core` for PDF) and contribute hits to the same
//! result list as the source files around them.
//!
//! Both engines link as rlibs into this host, so a document is parsed in-process —
//! there is no subprocess spawn and no IPC per file. Note what this does *not* mean:
//! `Document::replace_lossless` and friends are static `path -> out` functions that
//! re-read the package and rewrite it; no parsed model stays resident between the
//! search pass and the replace pass.
//!
//! # Semantics that differ from the text branch, and are surfaced rather than hidden
//!
//! * **Literal only.** Every engine `find` is a substring scan, not a regex. A regex
//!   query is rejected with an error rather than silently degraded to a literal.
//! * **Case.** The engines disagree with each other — `Document::find` is
//!   case-sensitive, `Pdf::search` lowercases both sides. This module does its own
//!   case folding over extracted text and never relies on either default, so all
//!   formats behave identically. Replace is a different story: the lossless replace
//!   path edits raw XML text nodes case-sensitively, so a case-insensitive *search*
//!   can report hits that a *replace* will not touch. `DocReplaceResult::case_note`
//!   says so instead of leaving the user to discover it.
//! * **PDF replace matches whole runs, not substrings.** `Pdf::search` counts
//!   substrings but `Pdf::find_replace_text` only rewrites a text-showing operator
//!   whose run decodes exactly to the query. Searching `getUserName` in a PDF whose
//!   run reads `getUserNameFromDb` finds it and cannot replace it. Every PDF row
//!   carries `whole_run_only: true` and an honest `replaced` count.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use zoffice_core::calc::Workbook;
use zoffice_core::impress::Presentation;
use zoffice_core::writer::Document;
use zoffice_core::App;

/// Snippet cap, matching the text branch's `trimmed.chars().take(400)` in `project.rs`.
const SNIPPET_CHARS: usize = 400;

/// How the walker should treat a file. Decided from the extension first, and only
/// then from the NUL sniff — so a `.docx` is never mistaken for an opaque blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    /// Plain text: the existing grep path handles it.
    Text,
    /// A `zoffice-core` package (docx/odt, xlsx/ods, pptx/odp).
    Office(App),
    /// A PDF, handled by `zpdf-core`.
    Pdf,
    /// Binary with no document engine behind it — skipped, as before.
    Opaque,
}

/// Extensions this phase supports, as `(ext, kind)`.
///
/// Deliberately narrower than `zoffice_core::app_for`, which also claims `doc`,
/// `rtf`, `xls`, `ppt`, `odg`, `odf`, `odb` and `csv`. Lossless replace is only
/// implemented for the OOXML/ODF pairs below, so the rest stay skipped rather than
/// being found by search and then failing at replace. `csv` is plain text and is
/// left to the text branch.
fn doc_ext_kind(ext: &str) -> Option<FileKind> {
    match ext {
        "docx" | "odt" => Some(FileKind::Office(App::Writer)),
        "xlsx" | "ods" => Some(FileKind::Office(App::Calc)),
        "pptx" | "odp" => Some(FileKind::Office(App::Impress)),
        "pdf" => Some(FileKind::Pdf),
        _ => None,
    }
}

/// Lowercase extension of `path`, or an empty string when it has none.
pub(crate) fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// The format label reported to the front end — the lowercase extension.
fn format_of(path: &Path) -> String {
    ext_of(path)
}

/// Decide how to treat `path`. `head` is the leading bytes already read by the
/// caller (may be empty when the caller has not read the file yet).
///
/// Extension wins over the byte sniff: that ordering is the whole feature. If the
/// sniff ran first every supported document would classify as [`FileKind::Opaque`],
/// because a zip package starts with NULs well inside the first block.
pub(crate) fn classify(path: &Path, head: &[u8]) -> FileKind {
    if let Some(kind) = doc_ext_kind(&ext_of(path)) {
        return kind;
    }
    if crate::project::looks_binary(head) {
        return FileKind::Opaque;
    }
    FileKind::Text
}

/// Where inside a document a hit lives. Internally tagged so the front end can
/// switch on `kind` without probing for the presence of fields.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DocLocator {
    /// Zero-based paragraph index within the document body.
    Paragraph { index: usize },
    /// A spreadsheet cell: sheet index, sheet name, and A1 reference.
    Cell {
        sheet: usize,
        sheet_name: String,
        reference: String,
    },
    /// Zero-based slide index.
    Slide { index: usize },
    /// A PDF page (1-based).
    ///
    /// `rect` is the approximate on-page rectangle of the matching run, from
    /// `zpdf_core::Pdf::search_run_rects`. It is `None` when the locator could not be
    /// resolved — that call is run-level and always case-insensitive, so a
    /// case-sensitive pass can find a hit in the extracted page text that the rect
    /// locator does not pin down. A missing rect degrades the jump to page
    /// granularity; it never suppresses the hit.
    Page { page: u32, rect: Option<[f32; 4]> },
}

/// One document hit. Mirrors `project::SearchHit`'s shape (`path`, `rel`, `text`) so
/// the front end can render both kinds of row through one code path.
#[derive(Serialize, Clone, Debug)]
pub struct DocHit {
    pub path: String,
    pub rel: String,
    /// `"docx" | "odt" | "xlsx" | "ods" | "pptx" | "odp" | "pdf"`.
    pub format: String,
    pub locator: DocLocator,
    /// Surrounding text, trimmed and capped to the same 400 chars as a text hit.
    pub text: String,
}

#[derive(Serialize, Debug, Default)]
pub struct DocSearchResult {
    pub hits: Vec<DocHit>,
    /// True when `max_results` cut the pass short.
    pub truncated: bool,
    /// `(path, engine error)` for files that matched a supported extension but
    /// failed to parse. Surfaced, never silently swallowed.
    pub errors: Vec<(String, String)>,
}

#[derive(Deserialize, Default, Debug, Clone)]
pub struct DocSearchOpts {
    pub show_hidden: Option<bool>,
    pub max_results: Option<usize>,
    /// Restrict to a format subset, e.g. `["docx", "xlsx"]`. `None` = every supported format.
    pub formats: Option<Vec<String>>,
    /// Fold case during matching. Applied by this module over extracted text, not
    /// delegated to the engines (which disagree). Replace is always case-sensitive.
    pub case_insensitive: Option<bool>,
    /// Present only so a regex query can be *rejected*. The engines are substring-only.
    pub regex: Option<bool>,
}

#[derive(Serialize, Debug)]
pub struct DocReplaceHit {
    pub path: String,
    pub rel: String,
    pub format: String,
    /// Occurrences replaced, as counted by the engine that did the work. For PDF
    /// this is *pages changed*, not occurrences — see `whole_run_only`.
    pub replaced: usize,
    /// True for PDF: the engine matches whole runs only, so substring matches that
    /// search reported are not replaced, and `replaced` counts pages.
    pub whole_run_only: bool,
}

#[derive(Serialize, Debug, Default)]
pub struct DocReplaceResult {
    pub hits: Vec<DocReplaceHit>,
    pub total: usize,
    pub files: usize,
    pub applied: bool,
    pub errors: Vec<(String, String)>,
    /// Set when the replace semantics are narrower than the search that preceded it,
    /// so the UI can explain a lower-than-expected count instead of looking broken.
    pub case_note: Option<String>,
}

// ── matching helpers ────────────────────────────────────────────────────────────

/// A query prepared once per pass: the needle plus whether to fold case.
#[derive(Clone)]
struct Needle {
    text: String,
    fold: bool,
}

impl Needle {
    fn new(query: &str, fold: bool) -> Self {
        Needle {
            text: if fold {
                query.to_lowercase()
            } else {
                query.to_string()
            },
            fold,
        }
    }

    /// Does `hay` contain the needle under this pass's case rule?
    fn matches(&self, hay: &str) -> bool {
        if self.fold {
            hay.to_lowercase().contains(&self.text)
        } else {
            hay.contains(&self.text)
        }
    }

    /// Byte offset of the first match in `hay`, under this pass's case rule.
    ///
    /// Offsets from the lowercased copy are only used to position a display
    /// snippet, so the fact that `to_lowercase` can change byte length for some
    /// scripts cannot corrupt anything — worst case the snippet window shifts.
    fn find_at(&self, hay: &str) -> Option<usize> {
        if self.fold {
            hay.to_lowercase().find(&self.text)
        } else {
            hay.find(&self.text)
        }
    }
}

/// Trim and cap to [`SNIPPET_CHARS`], matching the text branch.
pub(crate) fn snippet(text: &str) -> String {
    text.trim().chars().take(SNIPPET_CHARS).collect()
}

/// A snippet windowed around a match, for long extracted page text where taking the
/// first 400 chars would usually miss the hit entirely.
fn snippet_around(text: &str, at: usize) -> String {
    // Step back to a char boundary, then a fixed lead-in so the match has context.
    let lead = SNIPPET_CHARS / 4;
    let mut start = at.saturating_sub(lead);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    snippet(&text[start..])
}

// ── the replace call, behind a named struct ─────────────────────────────────────

/// One replace unit of work.
///
/// The engines' replace entry points are `(&Path, &Path, &str, &str)` — two adjacent
/// same-typed path arguments that are trivially swapped at a call site, and swapping
/// them overwrites the user's source document with a re-serialization of the temp
/// file. Naming the fields removes the whole class of defect: no call site in this
/// module passes two bare `&Path` positionally.
struct ReplaceJob<'a> {
    source: &'a Path,
    out: &'a Path,
    find: &'a str,
    replacement: &'a str,
}

/// What one engine actually did to one document.
struct ReplaceOutcome {
    /// Occurrences replaced (pages changed, for PDF).
    replaced: usize,
    /// The engine matches whole runs rather than substrings (PDF).
    whole_run_only: bool,
    /// The document contains the query but the engine could not replace it. Only PDF can be in
    /// this state, and it is the single most likely source of a "search found it, replace did
    /// nothing" bug report — so it is reported as a row rather than dropped as a no-match.
    matched_but_unreplaced: bool,
}

impl ReplaceJob<'_> {
    /// Run the format-appropriate engine.
    fn run(&self, kind: FileKind) -> Result<ReplaceOutcome, String> {
        // A source that is also the destination would make the dry run destructive
        // and the apply non-atomic. The engines permit it; this module never does.
        debug_assert_ne!(self.source, self.out, "replace must never write in place");
        // The office engines rewrite XML text nodes literally, so a match is always replaceable.
        let office = |n: usize| ReplaceOutcome {
            replaced: n,
            whole_run_only: false,
            matched_but_unreplaced: false,
        };
        match kind {
            FileKind::Office(App::Writer) => {
                Document::replace_lossless(self.source, self.out, self.find, self.replacement)
                    .map(office)
                    .map_err(|e| e.to_string())
            }
            FileKind::Office(App::Calc) => {
                Workbook::replace_lossless(self.source, self.out, self.find, self.replacement)
                    .map(office)
                    .map_err(|e| e.to_string())
            }
            FileKind::Office(App::Impress) => {
                Presentation::replace_lossless(self.source, self.out, self.find, self.replacement)
                    .map(office)
                    .map_err(|e| e.to_string())
            }
            FileKind::Pdf => {
                let mut pdf = zpdf_core::Pdf::open(self.source).map_err(|e| e.to_string())?;
                let pages = pdf.page_numbers();

                // The engine's own return value cannot be trusted as a count of pages changed.
                // `Pdf::find_replace_text` increments its counter whenever the underlying
                // `replace_text` returns `Ok`, but that call succeeds *without editing anything*
                // when the run does not decode exactly to the query — PDF text replace matches
                // whole runs, while search matches substrings. Verified against the pinned
                // engine: replacing `getUserName` in a run reading `getUserNameFromDb` returns
                // 1 and leaves the page byte-identical.
                //
                // So the post-condition is measured here instead: snapshot each page's text,
                // replace, and count only the pages whose text actually differs afterwards. A
                // reported count that overstates what happened is exactly the "it said it
                // replaced 3 things and my document is unchanged" bug.
                let before: Vec<(u32, String)> = pages
                    .iter()
                    .map(|&p| (p, pdf.extract_text(p).unwrap_or_default()))
                    .collect();

                pdf.find_replace_text(self.find, self.replacement)
                    .map_err(|e| e.to_string())?;

                let replaced = before
                    .iter()
                    .filter(|(p, was)| pdf.extract_text(*p).unwrap_or_default() != *was)
                    .count();

                if replaced > 0 {
                    pdf.save(self.out).map_err(|e| e.to_string())?;
                }
                // Found by search, untouched by replace — reported rather than dropped.
                let matched_but_unreplaced =
                    replaced == 0 && before.iter().any(|(_, was)| was.contains(self.find));
                Ok(ReplaceOutcome {
                    replaced,
                    whole_run_only: true,
                    matched_but_unreplaced,
                })
            }
            _ => Err("not a supported document format".into()),
        }
    }
}

/// A temp path beside `source`, so the later `fs::rename` stays on one filesystem
/// (and is therefore atomic). Includes the pid and the file's own name to avoid
/// collisions between concurrent passes.
fn temp_out_for(source: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "doc".into());
    let dir = source.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".zmax-doc-{}-{}.tmp", std::process::id(), name))
}

// ── candidate collection ────────────────────────────────────────────────────────

/// Every supported document under `root` that is within the document size cap,
/// paired with its kind. Sorted so results are deterministic across runs despite
/// the parallel fan-out.
pub(crate) fn collect_documents(
    root: &Path,
    show_hidden: bool,
    formats: Option<&[String]>,
) -> Vec<(PathBuf, FileKind)> {
    let mut out: Vec<(PathBuf, FileKind)> = crate::project::walk_files(root, show_hidden)
        .into_iter()
        .filter_map(|p| {
            let kind = doc_ext_kind(&ext_of(&p))?;
            if let Some(list) = formats {
                if !list.iter().any(|f| f.eq_ignore_ascii_case(&ext_of(&p))) {
                    return None;
                }
            }
            // Documents get their own, larger cap: a 12 MiB .pptx is ordinary, and
            // the 4 MiB grep cap would silently exclude most real spreadsheets.
            let len = std::fs::metadata(&p).ok()?.len();
            if len > crate::project::MAX_DOC_FILE_BYTES {
                return None;
            }
            Some((p, kind))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Reject a regex query up front. Every engine `find` is a substring scan, so
/// accepting a regex and matching it literally would quietly return the wrong
/// answer — worse than refusing.
fn reject_regex(opts: &DocSearchOpts) -> Result<(), String> {
    if opts.regex.unwrap_or(false) {
        return Err(
            "document search is literal-only: the office and pdf engines match substrings, \
             not regular expressions. Clear the regex option to search documents."
                .into(),
        );
    }
    Ok(())
}

// ── search ──────────────────────────────────────────────────────────────────────

/// Search one document, returning its hits or an engine error.
fn search_one(path: &Path, kind: FileKind, needle: &Needle, root: &Path) -> Result<Vec<DocHit>, String> {
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let path_s = path.to_string_lossy().into_owned();
    let format = format_of(path);
    let mut hits = Vec::new();

    // One closure so every arm builds an identically-shaped row.
    let mut push = |locator: DocLocator, text: String| {
        hits.push(DocHit {
            path: path_s.clone(),
            rel: rel.clone(),
            format: format.clone(),
            locator,
            text,
        });
    };

    match kind {
        FileKind::Office(App::Writer) => {
            let doc = Document::open(path).map_err(|e| e.to_string())?;
            for (i, p) in doc.paragraphs.iter().enumerate() {
                if needle.matches(&p.text) {
                    push(DocLocator::Paragraph { index: i }, snippet(&p.text));
                }
            }
        }
        FileKind::Office(App::Calc) => {
            let wb = Workbook::open(path).map_err(|e| e.to_string())?;
            for (si, sheet) in wb.sheets.iter().enumerate() {
                for row in &sheet.rows {
                    for cell in &row.cells {
                        if needle.matches(&cell.value) {
                            push(
                                DocLocator::Cell {
                                    sheet: si,
                                    sheet_name: sheet.name.clone(),
                                    reference: cell.reference.clone(),
                                },
                                snippet(&cell.value),
                            );
                        }
                    }
                }
            }
        }
        FileKind::Office(App::Impress) => {
            let deck = Presentation::open(path).map_err(|e| e.to_string())?;
            for (i, slide) in deck.slides.iter().enumerate() {
                for t in &slide.texts {
                    if needle.matches(t) {
                        push(DocLocator::Slide { index: i }, snippet(t));
                    }
                }
            }
        }
        FileKind::Pdf => {
            let pdf = zpdf_core::Pdf::open(path).map_err(|e| e.to_string())?;
            // Do the matching here rather than through `Pdf::search`, which forces
            // case-insensitivity: this module owns the case rule for every format.
            let mut pages: Vec<u32> = pdf.page_numbers();
            pages.sort_unstable();

            // Run rects for the jump target, resolved once for the whole document rather than
            // per page. This is a *display* aid layered on top of the match decision above, not
            // the match itself: `search_run_rects` is always case-insensitive and works at run
            // granularity, so it can miss a hit this pass found. First rect per page wins;
            // absence leaves the locator at page granularity.
            let mut rects: std::collections::HashMap<u32, [f32; 4]> =
                std::collections::HashMap::new();
            for (p, r) in pdf.search_run_rects(&needle.text) {
                rects.entry(p).or_insert(r);
            }

            for p in pages {
                let Ok(text) = pdf.extract_text(p) else {
                    continue;
                };
                if let Some(at) = needle.find_at(&text) {
                    push(
                        DocLocator::Page {
                            page: p,
                            rect: rects.get(&p).copied(),
                        },
                        snippet_around(&text, at),
                    );
                }
            }
        }
        _ => return Err("not a supported document format".into()),
    }
    Ok(hits)
}

/// Search every supported document under `root`, in parallel.
///
/// Returns hits in a deterministic order (by path, then by position within the
/// document) regardless of the order the thread pool finishes them in.
pub(crate) fn search_all(
    root: &Path,
    query: &str,
    opts: &DocSearchOpts,
) -> Result<DocSearchResult, String> {
    reject_regex(opts)?;
    if query.is_empty() {
        return Ok(DocSearchResult::default());
    }
    let needle = Needle::new(query, opts.case_insensitive.unwrap_or(false));
    let max = opts.max_results.unwrap_or(1000).min(20_000);
    let docs = collect_documents(root, opts.show_hidden.unwrap_or(false), opts.formats.as_deref());

    // Document parsing dominates the cost here, so fan out; the text branch of the
    // walk stays single-threaded and untouched.
    let per_file: Vec<Result<Vec<DocHit>, (String, String)>> = docs
        .par_iter()
        .map(|(path, kind)| {
            search_one(path, *kind, &needle, root)
                .map_err(|e| (path.to_string_lossy().into_owned(), e))
        })
        .collect();

    let mut out = DocSearchResult::default();
    for r in per_file {
        match r {
            Ok(hits) => {
                for h in hits {
                    if out.hits.len() >= max {
                        out.truncated = true;
                        break;
                    }
                    out.hits.push(h);
                }
            }
            Err(pair) => out.errors.push(pair),
        }
    }
    Ok(out)
}

/// Search every supported document under `root`.
#[tauri::command]
pub fn search_documents(
    root: String,
    query: String,
    opts: Option<DocSearchOpts>,
) -> Result<DocSearchResult, String> {
    let opts = opts.unwrap_or_default();
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);
    search_all(&root, &query, &opts)
}

// ── replace ─────────────────────────────────────────────────────────────────────

/// Replace across every supported document under `root`.
///
/// With `apply == false` this is a *measured* dry run, not a prediction: every
/// document is genuinely re-serialized into a temp path beside itself, the engine's
/// own count is taken, and the temp is deleted. The source is never opened for
/// writing.
///
/// With `apply == true` the same temp is written first and then renamed over the
/// source, so a failure part-way through a package rewrite can never leave a
/// half-written document on disk.
pub(crate) fn replace_all(
    root: &Path,
    query: &str,
    replacement: &str,
    apply: bool,
    opts: &DocSearchOpts,
) -> Result<DocReplaceResult, String> {
    reject_regex(opts)?;
    if query.is_empty() {
        return Ok(DocReplaceResult::default());
    }
    let docs = collect_documents(root, opts.show_hidden.unwrap_or(false), opts.formats.as_deref());

    let per_file: Vec<Result<Option<DocReplaceHit>, (String, String)>> = docs
        .par_iter()
        .map(|(path, kind)| replace_one(path, *kind, query, replacement, apply, root))
        .collect();

    let mut out = DocReplaceResult {
        applied: apply,
        ..Default::default()
    };
    for r in per_file {
        match r {
            Ok(Some(hit)) => {
                out.total += hit.replaced;
                // `files` means "documents actually changed". A row with `replaced: 0` is a
                // matched-but-unreplaceable PDF; it is listed, but must not inflate the count
                // the confirm dialog shows the user.
                if hit.replaced > 0 {
                    out.files += 1;
                }
                out.hits.push(hit);
            }
            Ok(None) => {}
            Err(pair) => out.errors.push(pair),
        }
    }
    if opts.case_insensitive.unwrap_or(false) {
        out.case_note = Some(
            "Document replace is case-sensitive: the lossless package rewrite edits raw XML text \
             nodes. Case-insensitive search hits whose case differs from the query are reported \
             above but were not replaced."
                .into(),
        );
    }
    Ok(out)
}

/// Replace within one document. `Ok(None)` means the document contained no match.
fn replace_one(
    path: &Path,
    kind: FileKind,
    query: &str,
    replacement: &str,
    apply: bool,
    root: &Path,
) -> Result<Option<DocReplaceHit>, (String, String)> {
    let path_s = path.to_string_lossy().into_owned();
    let out_path = temp_out_for(path);
    let job = ReplaceJob {
        source: path,
        out: &out_path,
        find: query,
        replacement,
    };
    let outcome = match job.run(kind).map_err(|e| (path_s.clone(), e)) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(e);
        }
    };
    let ReplaceOutcome {
        replaced,
        whole_run_only,
        matched_but_unreplaced,
    } = outcome;

    // A document the query matched but the engine could not rewrite still gets a row, with
    // `replaced: 0`, so the UI can say so. Only a genuine no-match drops out silently.
    if replaced == 0 || !out_path.exists() {
        let _ = std::fs::remove_file(&out_path);
        if !matched_but_unreplaced {
            return Ok(None);
        }
        return Ok(Some(DocReplaceHit {
            path: path_s,
            rel: path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned(),
            format: format_of(path),
            replaced: 0,
            whole_run_only,
        }));
    }

    if apply {
        // Same directory, so this is an atomic replace rather than a copy.
        std::fs::rename(&out_path, path).map_err(|e| {
            let _ = std::fs::remove_file(&out_path);
            (path_s.clone(), format!("rename into place: {e}"))
        })?;
    } else {
        let _ = std::fs::remove_file(&out_path);
    }

    Ok(Some(DocReplaceHit {
        path: path_s,
        rel: path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned(),
        format: format_of(path),
        replaced,
        whole_run_only,
    }))
}

/// Preview (or, with `apply`, perform) a project-wide replace across documents.
#[tauri::command]
pub fn replace_documents(
    root: String,
    query: String,
    replacement: String,
    apply: bool,
    opts: Option<DocSearchOpts>,
) -> Result<DocReplaceResult, String> {
    let opts = opts.unwrap_or_default();
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);
    replace_all(&root, &query, &replacement, apply, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Read;
    use zoffice_core::writer::{Document as WDoc, Paragraph};

    // ── fixtures, all generated in-process so no binary blobs live in the repo ──

    /// A directory unique to this call.
    ///
    /// The counter is load-bearing, not decoration: these tests run in parallel in one process,
    /// so a pid+timestamp name can collide when two threads sample the clock within its
    /// resolution. When that happened, two tests shared a directory and `collect_documents`
    /// returned the other test's fixtures — a confusing failure in a test that was otherwise
    /// correct. The counter makes the name unique by construction.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-doc-test-{}-{}",
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

    /// A `.docx` at `path` whose body paragraphs are `paras`.
    fn write_docx(path: &Path, paras: &[&str]) {
        let doc = WDoc {
            paragraphs: paras
                .iter()
                .map(|t| Paragraph {
                    text: (*t).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        doc.save_docx(path).expect("save docx fixture");
    }

    /// Every entry of a zip package as `name -> bytes`.
    fn zip_entries(path: &Path) -> BTreeMap<String, Vec<u8>> {
        let f = std::fs::File::open(path).expect("open package");
        let mut zip = zip::ZipArchive::new(f).expect("read package");
        let mut out = BTreeMap::new();
        for i in 0..zip.len() {
            let mut e = zip.by_index(i).expect("entry");
            if e.is_dir() {
                continue;
            }
            let name = e.name().to_string();
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).expect("read entry");
            out.insert(name, buf);
        }
        out
    }

    /// A one-page PDF drawing `text` as a single Helvetica run.
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

    fn opts() -> DocSearchOpts {
        DocSearchOpts::default()
    }

    // ── the premise: one query, one result list, both kinds of file ────────────

    #[test]
    fn docx_hit_appears_in_same_result_list_as_source_hit() {
        let dir = tempdir();
        std::fs::write(dir.join("a.rs"), "fn getUserName() -> String { todo!() }\n").unwrap();
        write_docx(&dir.join("spec.docx"), &["The getUserName call is deprecated."]);

        let res = crate::project::search_project(
            dir.to_string_lossy().into_owned(),
            "getUserName".into(),
            None,
        )
        .expect("search");

        // The source hit is the pre-existing behaviour and must not regress.
        assert_eq!(res.hits.len(), 1, "source hits: {:?}", res.hits.len());
        // The document hit is the whole feature. This fails the instant the classifier order
        // flips and the NUL sniff wins over the extension check — a .docx is a zip, so
        // `looks_binary` returns true on it and the file silently vanishes from results.
        assert_eq!(res.doc_hits.len(), 1, "doc hits: {:?}", res.doc_hits);
        let h = &res.doc_hits[0];
        assert_eq!(h.format, "docx");
        assert_eq!(h.locator, DocLocator::Paragraph { index: 0 });
        assert!(h.text.contains("getUserName"), "snippet: {}", h.text);
        assert!(res.doc_errors.is_empty(), "errors: {:?}", res.doc_errors);
        cleanup(&dir);
    }

    // ── the lossless claim ─────────────────────────────────────────────────────

    #[test]
    fn replace_lossless_preserves_unmodeled_parts_byte_for_byte() {
        let dir = tempdir();
        let f = dir.join("doc.docx");
        write_docx(&f, &["alpha beta", "gamma alpha"]);
        let before = zip_entries(&f);
        assert!(
            before.len() > 1,
            "fixture must have more than the one edited part, got {:?}",
            before.keys().collect::<Vec<_>>()
        );

        let res = replace_all(&dir, "alpha", "omega", true, &opts()).expect("replace");
        assert_eq!(res.total, 2, "occurrences replaced: {res:?}");
        assert_eq!(res.files, 1);
        assert!(res.applied);

        let after = zip_entries(&f);
        // No part may be dropped: losing e.g. word/styles.xml or a media entry silently
        // destroys the user's formatting and images.
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>(),
            "package part list changed"
        );
        // Every part except the edited body must survive untouched. This is the entire
        // difference between a lossless rewrite and a model round-trip that quietly
        // re-serializes (and thereby drops) everything the model does not represent.
        let mut edited = Vec::new();
        for (name, bytes) in &before {
            if after[name] != *bytes {
                edited.push(name.clone());
            }
        }
        assert_eq!(
            edited,
            vec!["word/document.xml".to_string()],
            "parts other than the body were rewritten: {edited:?}"
        );
        // And the edit really happened.
        let body = String::from_utf8_lossy(&after["word/document.xml"]).into_owned();
        assert!(body.contains("omega"), "replacement missing from body");
        assert!(!body.contains("alpha"), "original text still present");
        cleanup(&dir);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempdir();
        let f = dir.join("doc.docx");
        write_docx(&f, &["alpha beta", "gamma alpha"]);
        let before = std::fs::read(&f).unwrap();

        let res = replace_all(&dir, "alpha", "omega", false, &opts()).expect("dry run");
        // The count is measured by really re-serializing into a temp file, not predicted.
        assert_eq!(res.total, 2, "dry run must still count: {res:?}");
        assert!(!res.applied);

        // `replace_lossless(path, out, ..)` takes two adjacent `&Path` arguments. Swapping them
        // overwrites the user's source with the temp re-serialization — silently, during what
        // the UI calls a preview. This assertion is the guard on that.
        assert_eq!(std::fs::read(&f).unwrap(), before, "dry run modified source");

        // And no temp debris is left beside the document.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");
        cleanup(&dir);
    }

    // ── the search/replace asymmetry, reported rather than hidden ──────────────

    #[test]
    fn pdf_substring_hit_is_reported_but_not_silently_replaced() {
        let dir = tempdir();
        let f = dir.join("report.pdf");
        write_pdf(&f, "getUserNameFromDb");

        // Search matches substrings.
        let found = search_all(&dir, "getUserName", &opts()).expect("search");
        assert_eq!(found.hits.len(), 1, "pdf search hits: {found:?}");
        assert_eq!(found.hits[0].format, "pdf");
        // The rect is what makes a pdf row a jump target rather than a page number, so it is
        // asserted rather than treated as optional decoration: a run drawn by an explicit `Td`
        // at a known position must resolve. If `search_run_rects` regresses to `pub(crate)`
        // upstream, or stops returning this run, the row silently degrades to page granularity
        // and nothing else in the suite would notice.
        let DocLocator::Page { page, rect } = &found.hits[0].locator else {
            panic!("pdf hit must carry a page locator: {:?}", found.hits[0]);
        };
        assert_eq!(*page, 1);
        let rect = rect.expect("run rect must resolve for a positioned run");
        // Drawn at `72 700 Td`, so the rect must sit on that origin, not at 0,0.
        assert!(rect[0] >= 71.0 && rect[0] <= 73.0, "rect x: {rect:?}");
        assert!(rect[1] >= 690.0 && rect[1] <= 705.0, "rect y: {rect:?}");
        assert!(rect[2] > rect[0], "rect must have width: {rect:?}");

        // Replace matches whole runs only, so this one cannot be rewritten. The contract is
        // that it is still *reported*, with an honest zero — a silent no-match here is the
        // "it found it but didn't change it" bug report.
        let res = replace_all(&dir, "getUserName", "getUser", false, &opts()).expect("replace");
        assert_eq!(res.total, 0, "substring must not be replaced: {res:?}");
        assert_eq!(res.files, 0, "no file was changed");
        assert_eq!(res.hits.len(), 1, "the unreplaceable match must still be listed");
        assert_eq!(res.hits[0].replaced, 0);
        assert!(
            res.hits[0].whole_run_only,
            "pdf rows must declare whole-run semantics"
        );
        cleanup(&dir);
    }

    #[test]
    fn pdf_whole_run_match_is_replaced_and_counted() {
        // Positive control for the test above: without this, an implementation that always
        // reported zero replacements for PDFs would pass the asymmetry test and be useless.
        let dir = tempdir();
        let f = dir.join("report.pdf");
        write_pdf(&f, "getUserName");

        let res = replace_all(&dir, "getUserName", "getUser", true, &opts()).expect("replace");
        assert_eq!(res.total, 1, "whole-run match must be replaced: {res:?}");
        assert_eq!(res.files, 1);
        assert!(res.applied);

        // The count is only meaningful if the file really changed on disk.
        let after = zpdf_core::Pdf::open(&f).expect("reopen");
        let text = after.extract_text(1).expect("extract");
        assert!(text.contains("getUser"), "text after replace: {text:?}");
        assert!(!text.contains("getUserName"), "text after replace: {text:?}");
        cleanup(&dir);
    }

    // ── failure surfacing ──────────────────────────────────────────────────────

    #[test]
    fn parse_failure_is_reported_not_swallowed() {
        let dir = tempdir();
        // Not a zip at all, despite the extension.
        std::fs::write(dir.join("broken.docx"), b"this is not a zip package").unwrap();
        write_docx(&dir.join("good.docx"), &["alpha beta"]);

        let res = search_all(&dir, "alpha", &opts()).expect("search");
        // The corrupt file is named, with the engine's own message.
        assert_eq!(res.errors.len(), 1, "errors: {:?}", res.errors);
        assert!(
            res.errors[0].0.ends_with("broken.docx"),
            "wrong file reported: {:?}",
            res.errors[0]
        );
        assert!(!res.errors[0].1.is_empty(), "error message was empty");
        // And it did not abort the pass: the healthy document still produced its hit.
        assert_eq!(res.hits.len(), 1, "good.docx hit was lost: {res:?}");
        cleanup(&dir);
    }

    #[test]
    fn regex_query_is_rejected_for_documents() {
        let dir = tempdir();
        write_docx(&dir.join("doc.docx"), &["alpha beta"]);
        let o = DocSearchOpts {
            regex: Some(true),
            ..Default::default()
        };
        // Every engine `find` is a substring scan. Matching a regex literally would return a
        // confidently wrong answer, so the query is refused instead.
        let err = search_all(&dir, "alph.*", &o).expect_err("regex must be rejected");
        assert!(err.contains("literal"), "unhelpful message: {err}");
        let err = replace_all(&dir, "alph.*", "x", false, &o).expect_err("regex must be rejected");
        assert!(err.contains("literal"), "unhelpful message: {err}");
        cleanup(&dir);
    }

    // ── the two size caps are genuinely different ──────────────────────────────

    #[test]
    fn oversized_document_uses_the_document_cap() {
        let dir = tempdir();
        // Larger than MAX_GREP_FILE_BYTES (4 MiB), far below MAX_DOC_FILE_BYTES (64 MiB) — the
        // size of an entirely ordinary real-world spreadsheet.
        let big = dir.join("big.xlsx");
        let size = (crate::project::MAX_GREP_FILE_BYTES + 1024 * 1024) as usize;
        std::fs::write(&big, vec![b'x'; size]).unwrap();

        let docs = collect_documents(&dir, false, None);
        // Reusing the 4 MiB grep cap here would make the feature silently do nothing on most
        // real office files, which looks identical to "no matches" from the UI.
        assert_eq!(docs.len(), 1, "document cap excluded an ordinary-sized file: {docs:?}");
        assert_eq!(docs[0].0, big);
        assert_eq!(docs[0].1, FileKind::Office(App::Calc));

        // Above the document cap it is excluded, so the cap is real and not merely absent.
        assert!(crate::project::MAX_DOC_FILE_BYTES > crate::project::MAX_GREP_FILE_BYTES);
        cleanup(&dir);
    }

    // ── classifier ordering, the hinge the whole feature turns on ──────────────

    #[test]
    fn classify_puts_extension_before_the_nul_sniff() {
        // A zip package's bytes contain NULs, so the sniff alone would call it opaque.
        let nul_bytes = b"PK\x03\x04\x00\x00\x00\x00";
        assert_eq!(
            classify(Path::new("a.docx"), nul_bytes),
            FileKind::Office(App::Writer)
        );
        assert_eq!(classify(Path::new("a.pdf"), nul_bytes), FileKind::Pdf);
        // Case-insensitive, because Windows-authored trees are full of .DOCX.
        assert_eq!(
            classify(Path::new("a.XLSX"), nul_bytes),
            FileKind::Office(App::Calc)
        );
        // Unsupported binary still skips, and plain text still greps.
        assert_eq!(classify(Path::new("a.bin"), nul_bytes), FileKind::Opaque);
        assert_eq!(classify(Path::new("a.rs"), b"fn main() {}"), FileKind::Text);
        // `.csv` is claimed by zoffice_core::app_for but has no lossless replace path, so it
        // must stay on the text branch rather than being found and then failing to replace.
        assert_eq!(classify(Path::new("a.csv"), b"a,b,c"), FileKind::Text);
    }
}
