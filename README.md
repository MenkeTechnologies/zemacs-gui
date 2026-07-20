```
███████╗███████╗███╗   ███╗ █████╗  ██████╗███████╗       ██████╗ ██╗   ██╗██╗
╚══███╔╝██╔════╝████╗ ████║██╔══██╗██╔════╝██╔════╝      ██╔════╝ ██║   ██║██║
  ███╔╝ █████╗  ██╔████╔██║███████║██║     ███████╗█████╗██║  ███╗██║   ██║██║
 ███╔╝  ██╔══╝  ██║╚██╔╝██║██╔══██║██║     ╚════██║╚════╝██║   ██║██║   ██║██║
███████╗███████╗██║ ╚═╝ ██║██║  ██║╚██████╗███████║      ╚██████╔╝╚██████╔╝██║
╚══════╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝ ╚═════╝╚══════╝       ╚═════╝  ╚═════╝ ╚═╝
```

![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
![GUI](https://img.shields.io/badge/GUI-windowed%20editor-ff2a6d?style=flat-square)
![license](https://img.shields.io/badge/license-MPL--2.0-39ff14?style=flat-square)

### `[A NATIVE DESKTOP GUI FOR ZMAX // THE WAY MACVIM WRAPS VIM]`

**zmax-gui** is a native desktop GUI for the
[`zmax`](https://github.com/MenkeTechnologies/zmax) editor — the Rust Emacs
port (a Helix/Vim-style modal core built out toward Spacemacs). It wraps the
zmax modal-editing core in a windowed front-end, the way MacVim wraps the Vim
CLI editor: the same editor underneath, a native window on top. Free and open
source.

## Architecture

A thin **Tauri v2** shell that runs the `zmax` binary in an **embedded PTY terminal**
([`zpwr-embed-terminal`](https://github.com/MenkeTechnologies/zpwr-embed-terminal)) filling the
window, wrapped in the shared **zgui-core** app baseline (`ZGui.appShell`: command palette, colour
schemes, settings, CRT/splash). The editor is the same modal core; the window, chrome and theming are
the GUI. Standard MenkeTechnologies GUI layout — see `GUI_APP_ARCHITECTURE.md` in the meta repo.

The host is thin with one deliberate exception: the **office** and **PDF** engines link into it as
rlibs so that project search, replace and git blame can see inside binary documents in-process. See
[Documents are searchable](#documents-are-searchable) and
[Documents are blamable](#documents-are-blamable).

```
zmax-gui/
├─ app/src-tauri/        Tauri host: terminal + fs + window + open-intake + project commands
│   ├─ terminal.rs       PTY spawn/write/resize/kill
│   ├─ fs_ops.rs         list_dir/home_dir — backs the Open dialog
│   ├─ window_ops.rs     fullscreen / translucency (blur) / focus
│   ├─ project.rs        fuzzy find-files, find-in-files (regex), tree file ops, recent files,
│   │                    file stats, git status/branch/diff — the project workbench backend
│   ├─ editor_tools.rs   bookmarks, project search & replace, go-to-symbol, TODO/markers
│   ├─ doc_search.rs     binary-document search & lossless replace (docx/odt/xlsx/ods/pptx/odp/pdf)
│   ├─ doc_blame.rs      git blame at document-address granularity (xlsx/ods cell, pdf page)
│   ├─ git_tools.rs      git blame, per-file history + show-commit, stage/unstage/discard, file compare
│   ├─ git_ext.rs        git branches (list/checkout/create) + stash (save/list/pop/drop/show)
│   ├─ text_tools.rs     file cleanup/convert, sort lines, find-definition, batch rename
│   ├─ edit_ops.rs       align columns on a delimiter + language-aware comment toggle
│   ├─ encoding_ops.rs   detect + transcode a file's character encoding (UTF-8/16, Latin-1)
│   ├─ git_more.rs       repo-wide log, show-commit, diff two revisions, commit graph
│   ├─ workbench_ext.rs  persisted snippets + project code-stats (files/lines by extension)
│   └─ open_intake.rs    CLI / Finder / mvim:// file opens → :open in the PTY
├─ crates/
│   ├─ zmax            the editor — vendored submodule, built → bundled sidecar
│   ├─ zpwr-embed-terminal   shared PTY engine (submodule)
│   ├─ zpwr-file-browser     shared multi-pane file browser: `crate/` (fs_* commands, watcher) + webui
│   ├─ zpwr-i18n             shared 27-locale i18n runtime + catalogs (submodule)
│   ├─ zoffice-core          office engine (docx/odt/xlsx/ods/pptx/odp), linked as an rlib
│   └─ zpdf-core             PDF engine, linked as an rlib
├─ scripts/
│   ├─ mvim              terminal launcher (open files in the running window)
│   ├─ copy-{webui,embed-terminal,i18n,file-browser}.mjs   sync shared webui into frontend/
│   └─ prepare-{zmax,stryke}-sidecar.mjs   stage the bundled binaries
└─ frontend/
   ├─ index.html · main.js      mounts ZGui.appShell + the fullscreen terminal
   ├─ menu.js                   the MacVim GUI surface (all zgui widgets → PTY)
   ├─ panels.js · panels.css    the project workbench overlays (quick-open, find-in-files, …)
   ├─ fb-backend.js             Tauri fs bridge + host shims for the shared file browser
   └─ lib/zgui-core             the shared widget library (submodule)
```

## Project workbench

On top of the MacVim menu surface, the app adds an IDE-style **project workbench** — all reachable
from the **⌘K command palette** (and dedicated shortcuts). Every result is opened by driving the
editor (`:open <path>:<line>:<col>`); the OS-side work (walking the tree, grepping, filesystem
mutations, git) lives in the Rust `project.rs` / `editor_tools.rs` / `git_tools.rs` / `git_ext.rs` /
`text_tools.rs` / `edit_ops.rs` / `encoding_ops.rs` / `git_more.rs` / `workbench_ext.rs` commands, so
results are fast and the editor stays the single source of truth.

- **Quick Open** (`⌘P`) — fuzzy file finder over the project tree (VCS/build dirs pruned), boundary-
  and run-aware ranking; type to filter, `↑`/`↓`/`Enter` to open.
- **Find in Files** (`⇧⌘J`) — project-wide text search with **regex**, **match-case** and
  **whole-word** toggles; click a match to jump to its exact `line:col`, or **★** to bookmark it.
  **Binary documents are searched too**, in the same query and the same ranked list — see
  [Documents are searchable](#documents-are-searchable) below.
- **Search & Replace** (`⇧⌘H`) — project-wide replace with **regex** (including `$1` capture
  references), match-case and whole-word; a live **preview** of every before → after line, then
  **Replace All** rewrites the matching files on disk (confirmed first). Oversized files, and
  binaries with no document engine behind them, are skipped like the search; supported documents
  are previewed and rewritten losslessly.
- **Go to Symbol** (`⇧⌘O`) — a workspace outline picker: functions, structs/classes/enums/traits,
  types, modules, methods and Markdown headings across the tree (Rust, JS/TS, Python, Go, C/C++,
  Ruby, shell, Lua, stryke/Perl, Markdown); type to filter, `Enter` to jump.
- **Find Definition** (`⇧⌘D`) — jump to where an *exact* symbol name is **declared** (not every
  occurrence): reuses the Go-to-Symbol language rules to locate `fn`/`struct`/`class`/`def`/… sites
  across the tree; type a name, `Enter` to jump.
- **TODO / Markers** (`⇧⌘T`) — a scan for `TODO` / `FIXME` / `HACK` / `XXX` / `BUG` / `NOTE` /
  `OPTIMIZE` / `WARNING` comment markers across the tree; filter and jump to each.
- **Bookmarks** (`⌘B`) — a persisted list of named `file:line` marks (survives restarts); add from a
  file picker or the **★** button on a search hit, jump on click, remove per-row or **Clear**.
- **Recent Files** (`⌘E`) — a persisted MRU list (survives restarts; every open, from any route, is
  recorded), filterable, with **Clear**.
- **Project Files** (`⇧⌘E`) — a tree file manager: **New File / New Folder**, **Rename**,
  **Duplicate**, **Delete** (confirmed), and **File Info** (line/word/char/byte counts) via the
  right-click menu; click a file to open it.
- **File Browser** — the shared multi-pane
  [`zpwr-file-browser`](https://github.com/MenkeTechnologies/zpwr-file-browser) (same component as
  zemail / ztranslator / zstation), opened as a full-screen overlay: multiple panes and tabs,
  sortable + resizable columns, fuzzy filter, color labels, folder-tree sidebar, text/hex/image
  quicklook + preview pane, git status, dedup, diff, grep, compress/extract, hash, xattrs,
  disk-usage and live fs-change watch. Double-click (or Enter) opens the file **in the zmax
  buffer** — the browser's "open" is wired to drive the editor, not the OS default app. Backed by
  the crate's `fs_*` Tauri commands + the directory watcher (`zpwr_file_browser::commands`); the
  front end is synced into `frontend/` by `copy-file-browser.mjs`, bridged through `fb-backend.js`.
  Esc or the bar's **✕** closes it.
- **Batch Rename** — rename every file whose **base name** matches a find → replace rule (literal or
  **regex** with `$1` capture refs); a live **preview** of every `from → to` (collisions flagged),
  then **Rename All** applies it on disk (confirmed). Files stay in their directory.
- **Sort Lines** — reorder a file's lines on disk: **reverse**, **ignore-case**, **numeric** and
  **unique** (a sorted `uniq`) toggles, with a dry-run preview of the line-count delta; the file is
  reloaded in the editor after apply.
- **File Cleanup** — normalise a file: convert line endings (**LF**/**CRLF**), **trim trailing
  whitespace**, **expand tabs → spaces** or **tabify** leading indent, and **ensure a final
  newline**; a preview reports the changed-line count and byte delta before apply. Binary/oversized
  files are skipped, like the search tools.
- **Align Columns** — align every line of a file on a delimiter (literal or **regex**), the way Emacs
  `align-regexp` lines up `=` signs, `:` map keys or `//` trailing comments into one column; a preview
  reports how many lines participate and change before apply.
- **Comment / Uncomment** (`⇧⌘/`) — toggle line comments over a line range using the language's
  comment prefix (`//`, `#`, `--`, `;`, `"`, chosen by extension). If every non-blank line is already
  commented it uncomments, else it comments; the end line is pre-filled to the file length.
- **File Encoding** — detect a file's character encoding (BOM, **UTF-8**, **UTF-16LE/BE**, **Latin-1**)
  and line ending, then transcode it to **UTF-8**, **UTF-16LE/BE** or **Latin-1** (UTF-8 output is
  BOM-free; UTF-16 output is BOM-prefixed); a preview shows the source → target and byte delta.
- **Snippets** (`⇧⌘I`) — a persisted named text library; pick one to insert it into the editor via
  bracketed paste (multi-line bodies land verbatim, no auto-indent), add / remove / **Clear**.
- **Git Changes** — the current branch + `git status` list; click a file for its unified **diff**;
  **Stage** / **Unstage** / **Discard** (confirmed) each file inline, **Refresh**, jump to **Blame**,
  or open it in the editor.
- **Git Blame** (`⇧⌘B`) — per-line commit / author / date for a chosen file (`git blame`
  `--line-porcelain`); click a line to jump there.
- **Document Blame** (`⇧⌘Y`) — the same question for a binary document, answered at the document's
  own addresses instead of at lines. See [Documents are blamable](#documents-are-blamable).
- **File History** — the commit log touching a file (`git log --follow`); click a commit for the
  **diff it introduced** (`git show`), or open the file.
- **Repository Log** — the whole repo's commit history (`git log`, newest first, with ref
  decorations); click a commit for the full **diff it introduced across all files** (`git show`).
- **Commit Graph** — the ASCII branch graph across all refs (`git log --graph --oneline --decorate
  --all`) in a read-only pane.
- **Diff Revisions** — a unified **diff between any two revisions** (`git diff <a> <b>`), branches /
  tags / hashes, optionally scoped to one path; both revisions are flag-guarded.
- **Compare Files** — a unified **diff** between any two files picked from the tree
  (`git diff --no-index`, so it works outside a repo too).
- **Git Branches** — the local branches (most-recently-committed first, current flagged); click to
  **checkout** (confirmed), or **New Branch** to create and switch (`checkout -b`). Ref names are
  flag-guarded.
- **Git Stash** — the stash list; click an entry for its **patch** (`stash show -p`), **Pop**
  (apply + drop, confirmed) or **Drop** (confirmed) per entry, and **Stash Changes** to save the
  working tree (including untracked) with an optional message.
- **Project Stats** — a read-only report of file / line / byte counts across the tree, broken down by
  extension (binary and oversized files skipped for line counting).

All surfaces are modal overlays (like the Open dialog) built from zgui-core widgets — no docked pane,
so the embedded terminal is never reflowed.

### Documents are searchable

Ordinary project search skips any file whose first bytes contain a NUL, which is every office
package and PDF — they are zip or binary containers. zmax-gui links
[`zoffice-core`](https://github.com/MenkeTechnologies/zoffice-core) and
[`zpdf-core`](https://github.com/MenkeTechnologies/zpdf-core) as rlibs into the Tauri host, so the
walker gets a second branch: a file whose extension names a supported format is **parsed in-process**
and contributes hits to the same result list as the source files around it. One query returns hits
from `main.rs` and from `spec.docx` and from `budget.xlsx` together. There is no subprocess spawn and
no IPC per file, and document parsing fans out across a thread pool (the text branch stays
single-threaded and unchanged).

| Format | Search hit locates | Replace |
|---|---|---|
| `.docx`, `.odt` | paragraph (`¶12`) | lossless package rewrite |
| `.xlsx`, `.ods` | sheet + A1 cell (`Sheet1!B14`) | lossless package rewrite |
| `.pptx`, `.odp` | slide (`slide 4`) | lossless package rewrite |
| `.pdf` | page + on-page rect (`p. 7`) | whole text runs only — see below |

*Lossless* is the literal claim and it is pinned by a test: after a replace, every zip entry other
than the edited XML part is byte-identical, so styles, images, themes and revision marks survive a
rewrite that a parse-and-re-serialize round trip would silently drop.

Four behaviours differ from the text branch, and each is surfaced in the UI rather than hidden:

- **Literal only.** Every engine `find` is a substring scan, so the document branch is skipped when
  **regex** or **whole-word** is on, and the standalone document command rejects a regex query
  outright instead of matching it literally.
- **PDF replace matches whole runs, not substrings.** Searching `getUserName` in a PDF whose text run
  reads `getUserNameFromDb` finds the hit and *cannot* rewrite it. Those rows are still listed, with
  an honest `0` and the reason, rather than dropped as a silent no-match.
- **Replace is case-sensitive** even when the search that found the hit was not, because the package
  rewrite edits raw XML text nodes. A case-insensitive replace says so in the preview.
- **Parse failures are reported**, not swallowed: a corrupt package appears as its own row with the
  engine's message, so it never looks like "no matches".

### Documents are blamable

`git blame` answers "who last changed this line". A `.xlsx` or a `.pdf` has no lines — git sees one
binary blob and reports `Binary files differ`. The prevailing workaround is a `textconv` diff driver
that shells out to `pandoc` or `unoconv` once per file per revision and flattens the document to a
throwaway line stream, so the number that comes back is a line of the *rendering*, not an address in
the document.

**Document Blame** (`⇧⌘Y`, or the `⌘K` palette) answers it in the document's own coordinate system:

```
Sheet1!B14   a3f91c2e  2026-03-04  <author>  quarterly figures
p. 7         5d10ba71  2026-01-19  <author>  redraft the appendix
```

The walk reuses what the search branch already built. `git log --follow` yields the revisions that
touched the document (and the path it had at each, so a rename does not break the history);
`git show <rev>:<path>` materializes each blob; each revision is parsed **in-process** by the same
`zoffice-core` / `zpdf-core` rlibs described above — no `pandoc`, no `unoconv`, no converter
subprocess per revision. Each address is then attributed to the newest revision whose content at
that address differs from its predecessor's, which is why editing one cell does not re-blame the
cells beside it even though their bytes inside the zip moved too. Rows carry the same `DocLocator`
the search rows do, so an address renders identically in both panels.

| Format | Blame address |
|---|---|
| `.xlsx`, `.ods` | sheet + A1 cell (`Sheet1!B14`) |
| `.pdf` | page (`p. 7`) |

Two limits, both surfaced in the panel rather than hidden:

- **Only stable addresses are blamed.** Paragraph and slide indices *shift* when content is inserted
  above them, so index-keyed attribution would mis-blame every unit below an insertion — wrong in a
  way that looks right. `.docx` / `.odt` / `.pptx` / `.odp` are refused with that reason stated,
  pending content-hash alignment between adjacent revisions. Cells and pages do not move.
- **The revision walk is capped** (the panel reports how many revisions it walked out of how many
  exist). The oldest revision in the window has no predecessor to compare against, so addresses that
  did not change inside the window are marked `≤` — *changed at or before* that commit, not by it.
  Revisions that fail to parse are listed as skipped, because a revision that could not be read is a
  gap in the attribution rather than a non-event.

**On prior art**, since the surrounding claims here are narrow on purpose. Address-granular *diff*
for spreadsheets is not new: [ExcelCompare](https://github.com/na-ka-na/ExcelCompare) emits
`DIFF Cell at Sheet1!A3`, [Git XL](https://www.xltrail.com/git-xl) and JetBrains'
[ExcelDiffer](https://plugins.jetbrains.com/plugin/14847-exceldiffer) do cell-by-cell workbook
comparison. Address-granular *authorship* is not new either — [xltrail](https://www.xltrail.com/)
answers "who changed this value, when and why?" per cell. What is unclaimed elsewhere is the
combination this panel occupies: **git-backed** (a real repository, not a proprietary cloud store),
**multi-format** (spreadsheets *and* PDF, not Excel-only), and **in-editor** (a panel in the editor,
not a web SaaS). No editor in the category does authorship on a binary document at all: VS Code
renders `.docx` as [`Binary file not shown`](https://github.com/orgs/community/discussions/27893),
JetBrains' [Diff Viewer](https://www.jetbrains.com/help/idea/differences-viewer.html) treats Office
files as binary, and Zed, Neovim, Emacs and Sublime have no office/PDF blame path.

Dry run is measured, not predicted: each document is genuinely re-serialized into a temp file beside
itself and the count is taken from what the engine actually did, then the temp is discarded. On
apply, that temp is renamed over the source — same directory, so the replace is atomic and a failure
part-way through can never leave a half-written document on disk.

**Transform by Example** and **Reshape by Example** carry an *Apply to* selector with the same reach:
a synthesized rule can run over the buffer (the `:%s` bridge, unchanged) or over the project's
documents. Only a literal *replace* rule can cross that boundary — the other rules emit whole-line
patterns, and a paragraph or a cell is not a line — so the rest are refused with that reason stated,
never silently applied to nothing.

## MacVim-style GUI

The GUI wraps the modal core the way MacVim wraps Vim. Every surface is a **zgui-core widget**; each
action is bridged to the editor by writing an ex-command into the PTY (the GUI never edits files
itself, it drives `zmax`). zmax (a Helix fork) has **both** buffers and a real vim **tabpage**
family, so the GUI drives each with its own menu — the **Buffers** menu cycles/closes open buffers,
the **Tabs** menu manages tabpages (each holds its own split layout).

- **Menu bar** (`ZGui.menubar`) — File / Edit / Search / Text / Extract / Align / Structure / View / Buffers / Window / Tabs / Folds / Marks / Bookmarks / Macros / Snippets / Code / Spell / Abbrev / Git / Help.
- **Search menu** — in-buffer engine commands (distinct from the file-based Find-in-Files workbench):
  whole-buffer regex Replace (`:%s`, delimiter auto-chosen so a `/` in the pattern is safe),
  case-preserving Replace (vim-abolish `:%S` — `foo/Foo/FOO` → `bar/Bar/BAR`), Count Matches
  (`:count-matches`), and Clear Search Highlight (`:nohlsearch`).
- **Text menu** — in-buffer, live-selection line transforms bridged into the PTY (distinct from the
  file-based align-columns / whitespace panels in the project workbench, which act on a picked file):
  comment / uncomment the selected lines (`SPC c c` → `toggle_comments`); sort lines, with
  reverse / numeric / unique variants (`:sort-lines [--reverse|--numeric|--unique]`); sort the ranges
  in the selection (`:sort`); sort paragraphs (`:sort-paragraphs`); hard-wrap the selection to the
  configured width (`:reflow`); and reindent / dedent by a shiftwidth (`:indent-lines` / `:dedent-lines`).
- **Extract menu** — regex extraction over the selection, bridged into the PTY: replace the selection
  with the http(s) URLs / email addresses / IPv4 addresses / numbers / double-quoted strings it contains,
  one per line (`:extract-urls` / `:extract-emails` / `:extract-ips` / `:extract-numbers` /
  `:extract-quoted`); and extract every substring between a start / end delimiter pair from a prompt
  (`:extract-between <start> <end>`, each delimiter shellword-quoted).
- **Align menu** — the vim `SPC x a` column-alignment family bridged into the PTY, acting on the
  primary selection's rows (distinct from the workbench's file-based Align-Columns panel, which aligns a
  picked file on disk — the same in-buffer-vs-file split as the Text menu): align the selection's cursor
  columns (`align_selections`); align each row at a fixed target character — `=` / `:` / `,` / `;` / `&` /
  `.` (numeric), the paired brackets `(` `)` `[` `]` `{` `}`, or the arithmetic operators
  (`align_at_equals` … `align_at_arithmetic`); and align at a prompted single character
  (left / right, `align_left_at_char` / `align_right_at_char`) or a prompted regexp (`align_at_regex`).
- **Structure menu** — the vim `SPC k` paredit/sexp structural-editing family bridged into the PTY
  (plus the split verb on `SPC j s`): sexp navigation — beginning / end of sexp, up to parent, next /
  previous sexp, forward / backward to the enclosing paren, matching paren, copy sexp; slurp / barf
  forward and backward (`paredit_slurp_forward` … `paredit_barf_backward`); wrap with parens, unwrap
  (splice), raise, transpose, split, join, convolute, absorb (`wrap_sexp` / `paredit_splice` /
  `paredit_raise` / `paredit_transpose` / `paredit_split` / `join_selections` / `paredit_convolute` /
  `paredit_absorb`); splice-killing forward / backward and insert-sexp before / after
  (`paredit_splice_kill_forward` / `…_backward`, `paredit_insert_sexp_before` / `…_after`); and delete
  sexp / symbol forward and backward. The submap's generic vim reuses (visual select, undo/redo, mode
  switches, paste) are omitted — they already live on the Edit menu and are not structural ops.
- **Code menu** — language-server actions bridged into the PTY: go to definition / references /
  type definition, hover docs, peek definition, signature help, document / workspace symbol pickers
  (`SPC s j` / `SPC s S`), the refactor set — rename symbol, code action, organize imports, implement /
  override members, generate code (`SPC l r/a/O/i/v/g`) — next/previous diagnostic, format document,
  restart language server.
- **Spell menu** — vim's spell-check family bridged into the PTY: suggest corrections for the word under
  the cursor (`z=`), jump to the previous / next misspelling (`[s` / `]s`), add a word to the dictionary
  or mark it misspelled and undo that (`zg` / `zw` / `zug`), edit the wordlists by typing words
  (`:spellwrong` / `:spellrare` / `:spellundo`), and list the known-good words / show wordlist info
  (`:spelldump` / `:spellinfo`).
- **Abbrev menu** — vim/emacs abbreviation-table commands bridged into the PTY: list every defined
  abbreviation (`:list-abbrevs`); define a global / both-mode / insert-mode / command-mode abbreviation
  from a lhs + expansion prompt pair (`:define-global-abbrev`, `:abbreviate`, `:iabbrev`, `:cabbrev`);
  remove one for both / insert / command mode (`:unabbreviate` / `:iunabbreviate` / `:cunabbreviate`);
  expand every abbrev in the region (`:expand-region-abbrevs`); clear all (`:abclear`) or kill every
  table (`:kill-all-abbrevs`); and load / save the table to a file (`:read-abbrev-file`, reusing the
  Open browser / `:write-abbrev-file`, reusing the Save-As path prompt).
- **Git menu** — zmax-vcs actions bridged into the PTY: Magit status, stage / unstage file, line
  blame, buffer-vs-HEAD diff, next/previous/reset hunk, stash / pop, and merge-conflict resolution
  (3-pane resolve, keep ours / theirs, next conflict).
- **Window menu** — vim's `C-w` split-window family bridged into the PTY (each key backed by a real
  editor command): split horizontally / vertically, focus the split to the left / down / up / right
  (`C-w h/j/k/l`), move the current split to an edge (`C-w H/J/K/L`), rotate splits forward / reverse
  and exchange with the next (`C-w w/R/x`), grow / shrink height and width and equalize
  (`C-w +/-/>/<` / `C-w =`), maximize by closing the others (`C-w o`), close the split (`C-w q`), and
  undo the last layout change (`C-w u`, winner-undo).
- **Tabs menu** — vim's tabpage family bridged into the PTY (real tabpages, distinct from buffers —
  each carries its own split layout): new tab / new tab with file (`:tabnew`, the latter reusing the
  Open file-browser), close / close-others (`:tabclose` / `:tabonly`), next / previous / first / last
  (`:tabnext` / `:tabprevious` / `:tabfirst` / `:tablast`), move to end / to position (`:tabmove`),
  run an ex-command in every tab (`:tabdo`), and the visual list / switch picker (`:tabs`).
- **Folds menu** — vim's `z`-family fold ops bridged into the PTY: toggle / open / close the fold at
  the cursor (`za` / `zo` / `zc`), open / close all folds (`zR` / `zM`), create a fold over the
  selection (`:fold`), delete one / all folds (`zd` / `zE`), and walk to the next / previous fold
  (`zj` / `zk`).
- **Marks menu** — vim's position-and-register family bridged into the PTY: set / go-to / list /
  delete marks (`:mark`, `` `{x} `` goto, `:marks`, `:delmarks[!]`); jumplist back / forward
  (C-o / C-i), list / clear jumps (`:jumps`, `:clearjumps`), recent-files picker (`:oldfiles`); and
  registers show / set / clear / clear-all (`:registers`, `:set-register`, `:clear-register`).
- **Bookmarks menu** — zmax's persistent-bookmark family bridged into the PTY (distinct from the
  transient marks above): JetBrains-style line bookmarks — toggle at point, next / previous, jump via a
  picker (`SPC r t/n/N/j`); focus the Bookmarks tool window (`SPC W b`); and the emacs bookmark file I/O
  — save / load the bookmark store to a path (`:bookmark-write` / `:bookmark-load`, via the Save-As
  prompt and the Open file browser).
- **Macros menu** — vim's keyboard-macro family plus the Spacemacs `SPC K` kmacro tree bridged into
  the PTY: record into a register / stop (`q{reg}` / `q`), replay a register / the last one /
  re-run the last ex-command (`@{reg}` / `Q` / `@:`); the macro ring — cycle next / previous, view /
  swap / delete the head macro (`SPC K r n/p/L/s/d`); the macro counter — increment / insert-and-increment
  (`SPC K c a/c`); and save the last macro to a register (`SPC K e r`).
- **Snippets menu** — the PTY-native snippet library bridged into the PTY (distinct from the workbench
  Snippets panel): insert a snippet via the fuzzy picker (`:Snippets`) and open the library editor to
  create / edit / delete snippets (`:snippets`).
- **Toolbar** (`ZGui.buttonBar`) — new / open / save / buffer nav / find / replace / go-to-def / format / git status / list marks / replay macro / toggle fold / comment lines / list tabs / split / full screen.
- **Command palette** (`⌘K`) — every menu action, fuzzy-searchable.
- **Cmd-shortcuts** — ⌘S save, ⇧⌘S Save As, ⌘O open, ⌘W close buffer, ⌘N new, ⌘Z/⇧⌘Z undo/redo,
  ⌘F find, ⌘G/⇧⌘G next/prev, ⌘{ ⌘} buffer cycle, ⌃⌘F full screen.
- **Open / Save As / Help** dialogs (`ZGui.modal` + `ZGui.tree` file browser).
- **Right-click context menu** in the editor (`ZGui.contextMenu`).
- **Drag-and-drop** files to open (`ZGui.fileDrag`).
- **Full screen** + **translucent background** (window-vibrancy); **Preferences** panel.
- **Open from the terminal / Finder / `mvim://` URL**, forwarded into the running window
  (single-instance + deep-link). Use `scripts/mvim file…`.

Out of scope (no surface in a PTY/WebView host — they need a native text view): native font rendering
(ligatures, thin strokes, antialias), Touch Bar, macOS Services, Force Click / dictionary lookup,
trackpad gesture pseudo-keys, find-pasteboard sharing. A passive always-on **tabline** strip is
omitted on purpose — a faithful one needs editor↔GUI introspection the raw PTY doesn't expose, and a
drifting strip would lie about state; the Tabs menu + the on-demand `:tabs` picker (rendered by the
editor itself) cover switching without that risk.

## Bundled binaries (self-contained)

The app **bundles** both the `zmax` editor and the `stryke` runtime as Tauri `externalBin` sidecars —
it never depends on either being on the user's `PATH`. Before each dev/build,
`scripts/prepare-{zmax,stryke}-sidecar.mjs` stage the binaries into
`app/src-tauri/binaries/<name>-<target-triple>` (the name `externalBin` requires); at runtime
`sidecar.rs` resolves the sidecar beside the executable (or the dev staging dir) and the editor is
launched by absolute path, with `STRYKE_BIN` exported to the bundled stryke. The staged binaries are
gitignored build artifacts.

- **zmax** — vendored as the **`crates/zmax` submodule** and built by the prep script
  (`cargo build --bin zmax`); override with `ZMAX_SIDECAR_BIN`.
- **stryke** — pulled from the **latest [strykelang](https://github.com/MenkeTechnologies/strykelang)
  GitHub release** for the host triple (cached by release tag); falls back to a local stryke offline;
  override with `STRYKE_SIDECAR_BIN`.

## Build

```sh
git submodule update --init --recursive   # zgui-core, zpwr-embed-terminal, zpwr-file-browser, zpwr-i18n, zmax
pnpm install
pnpm tauri dev      # or: pnpm tauri build
```

The first run builds `crates/zmax` (Helix-fork workspace — a few minutes) and downloads the stryke
release; both are cached afterward.

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`, which builds the macOS app on Apple-silicon
(`aarch64`) and Intel (`x86_64`) runners and attaches the per-arch `.dmg` + zipped `.app` to the
GitHub release. The bundled zmax (release build of the submodule) and stryke (latest release) sidecars
are staged automatically by `beforeBuildCommand`, so each `.app` is self-contained.

```sh
git tag v0.1.0 && git push --tags
```

## Links

- **Core editor** — [`zmax`](https://github.com/MenkeTechnologies/zmax)
- **App store** — https://menketechnologies.github.io/app-store/

## License

Free / OSS — MPL-2.0 (zmax / Helix lineage). See [LICENSE](LICENSE).
