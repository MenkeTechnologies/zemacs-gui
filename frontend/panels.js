// zmax-gui — the project workbench: quick-open, find-in-files, recent files, a project-tree file
// manager and a git panel. App-local (this repo owns it), layered ON TOP of the shared MacVim surface
// in menu.js (which stays the single source for the menu bar / basic dialogs). Like everything else in
// the GUI, nothing edits buffers directly: a pick is opened by writing `:open <path>:<line>:<col>` into
// the PTY, so the zmax editor remains the source of truth. The OS-side work (walk / grep / fs / git)
// lives in the Rust `project.rs` commands; this file is the UI + the PTY bridge.
//
// Surfaces are modal overlays (like the Open dialog), never a docked pane — a docked sidebar would
// have to reflow the embedded terminal, which resolves differently in release WebKit. Entry points:
// the ⌘K command palette (every action) plus ⌘P quick-open, ⌘E recent, ⌘⇧J find-in-files,
// ⌘⇧E project files, ⌘⇧I snippets, ⌘⇧B git blame, ⌘⇧Y document blame, and the rest (search &
// replace, go-to-symbol,
// markers, bookmarks, git changes / history, compare files, project stats) via the palette.
// The git tools, snippets and project stats are backed by the Rust `git_tools.rs` / `workbench_ext.rs`
// commands (blame / log / show / stage / unstage / discard / diff, snippet CRUD, code stats), and
// document blame by `doc_blame.rs`.
(function () {
  "use strict";

  // ── PTY bridge (mirrors menu.js: the ex-command channel isn't exported, so it's replicated here
  //    byte-for-byte — ESC must land on its own before the command or the terminal reads it as Alt+x) ──
  function core() { return window.__TAURI__ && window.__TAURI__.core; }
  function invoke(cmd, args) { var T = core(); return T ? T.invoke(cmd, args || {}) : Promise.reject("no tauri"); }
  function ptyWrite(data) { invoke("terminal_write", { data: data }).catch(function () {}); }
  function afterEsc(rest) { ptyWrite("\x1b"); setTimeout(function () { ptyWrite(rest); }, 50); }
  function ex(cmd) { afterEsc(":" + cmd + "\r"); }
  function q(p) { return '"' + String(p).replace(/"/g, '\\"') + '"'; }

  function T(key, english) {
    var s = (typeof window.t === "function") ? window.t(key) : null;
    return (s && s !== key) ? s : english;
  }
  function toast(msg, type) { if (window.ZGui && ZGui.toast) ZGui.toast.show(msg, 2600, type || ""); }

  // Open a file in the editor at an optional 1-based line/col, and record it in the MRU list. The
  // editor's `:open` parses `path:line:col` (see typed.rs open_impl) and centres on the position.
  function openInEditor(path, line, col) {
    if (!path) return;
    var arg = q(path);
    if (line) arg += ":" + line + (col ? ":" + col : "");
    ex("open " + arg);
    invoke("recent_add", { path: path }).catch(function () {});
    act.focusEditor();
  }
  var act = {
    focusEditor: function () { var c = document.getElementById("terminalContainer"); if (c) { var ta = c.querySelector("textarea"); if (ta) ta.focus(); } },
  };

  // ── binary-document hits (docx/odt/xlsx/ods/pptx/odp/pdf) ──────────────────────────────────────
  // The Rust side (doc_search.rs) parses these in-process and returns hits alongside the source
  // hits. They need their own row treatment for two reasons: a paragraph index / cell ref / slide
  // number is not a line:col, and `:open` cannot render a zip package in the PTY editor.

  // Human label for a DocLocator. The enum is internally tagged (`kind`), so switching on `kind`
  // is enough — no probing for the presence of fields.
  function locatorLabel(loc) {
    if (!loc) return "";
    switch (loc.kind) {
      case "paragraph": return "¶" + (loc.index + 1);
      case "cell": return (loc.sheet_name || ("sheet " + (loc.sheet + 1))) + "!" + loc.reference;
      case "slide": return T("zmax.panel.slide", "slide") + " " + (loc.index + 1);
      case "page": return "p. " + loc.page;
      default: return "";
    }
  }

  // Activate a document hit. `:open` is not an option — the editor is a text buffer and these are
  // packages — so the document is handed to the OS default application and the in-document
  // locator goes to the clipboard, which is the part the external app cannot be told about.
  // A row is never left inert.
  function openDocument(h) {
    if (!h || !h.path) return;
    var where = locatorLabel(h.locator);
    var op = window.__TAURI__ && window.__TAURI__.opener;
    if (op && typeof op.openPath === "function") {
      op.openPath(h.path).catch(function (err) { toast(String(err), "error"); });
    } else {
      toast(T("zmax.panel.no_opener", "No handler for this document"), "error");
      return;
    }
    if (where) {
      try {
        if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(where);
      } catch (e) { /* clipboard denied — the toast below still says where the hit is */ }
    }
    invoke("recent_add", { path: h.path }).catch(function () {});
    toast(T("zmax.panel.opened_doc", "Opened") + " " + h.rel + (where ? " · " + where : ""));
  }

  // Picker rows for the `doc_hits` half of a search result. Bookmarks are deliberately absent:
  // `bookmark_add` takes `{path, line}` and a paragraph index is not a line, so offering the ★
  // would file a bookmark that jumps to the wrong place.
  function docRows(res) {
    return ((res && res.doc_hits) || []).map(function (h) {
      return {
        badge: h.format,
        primary: h.text || "(match)",
        secondary: h.rel + " · " + locatorLabel(h.locator),
        onPick: function () { openDocument(h); },
      };
    });
  }

  // The project root = the PTY's working directory (list_dir with no path canonicalises the process
  // cwd, which the shell/editor share). Cached after the first lookup.
  var rootCache = null;
  function getRoot() {
    if (rootCache) return Promise.resolve(rootCache);
    return invoke("list_dir", { path: null }).then(function (l) { rootCache = l.dir; return rootCache; });
  }

  function debounce(fn, ms) {
    var t = null;
    return function () { var a = arguments, self = this; clearTimeout(t); t = setTimeout(function () { fn.apply(self, a); }, ms); };
  }

  // ── a reusable "search input + keyboard-navigable result list" modal ────────────────────────────
  // rowsFor(query) -> Promise<[{ primary, secondary, onPick }]>; the list handles ↑/↓/Enter/click.
  function pickerModal(opts) {
    if (!window.ZGui || !ZGui.modal) return null;
    var body = document.createElement("div");
    body.className = "zp-picker";

    var searchHost = document.createElement("div");
    body.appendChild(searchHost);
    if (opts.controls) body.appendChild(opts.controls);

    var count = document.createElement("div");
    count.className = "zp-count";
    body.appendChild(count);

    var list = document.createElement("div");
    list.className = "zp-list";
    body.appendChild(list);

    var dlg = ZGui.modal.open({
      title: opts.title,
      body: body,
      className: "zp-modal",
      actions: opts.actions || [{ label: T("zmax.dialog.cancel", "Cancel"), close: true }],
    });

    var rows = [];      // [{ el, onPick }]
    var sel = -1;

    function highlight() {
      rows.forEach(function (r, i) { r.el.classList.toggle("active", i === sel); });
      if (sel >= 0 && rows[sel]) rows[sel].el.scrollIntoView({ block: "nearest" });
    }
    function render(items) {
      list.textContent = "";
      rows = [];
      (items || []).forEach(function (it) {
        var row = document.createElement("div");
        row.className = "zp-row";
        var p = document.createElement("span");
        p.className = "zp-row-primary";
        p.textContent = it.primary;
        row.appendChild(p);
        if (it.secondary) {
          var s = document.createElement("span");
          s.className = "zp-row-secondary";
          s.textContent = it.secondary;
          row.appendChild(s);
        }
        if (it.badge) {
          var b = document.createElement("span");
          b.className = "zp-badge";
          b.textContent = it.badge;
          row.insertBefore(b, p);
        }
        // Optional per-row action button (e.g. bookmark / remove) — doesn't trigger the row pick.
        if (it.action) {
          var ab = document.createElement("button");
          ab.type = "button";
          ab.className = "zp-open-btn zp-row-action";
          ab.textContent = it.action.label;
          if (it.action.title) ab.title = it.action.title;
          (function (action) {
            ab.addEventListener("click", function (e) { e.stopPropagation(); if (typeof action.run === "function") action.run(); });
          })(it.action);
          row.appendChild(ab);
        }
        var pick = function () { dlg.close(); if (typeof it.onPick === "function") it.onPick(); };
        row.addEventListener("click", pick);
        list.appendChild(row);
        rows.push({ el: row, onPick: pick });
      });
      sel = rows.length ? 0 : -1;
      highlight();
      count.textContent = opts.countFmt ? opts.countFmt(rows.length) : (rows.length + " " + T("zmax.panel.results", "results"));
    }

    var refresh = debounce(function () {
      var val = box.get ? box.get() : (input ? input.value : "");
      var extra = box.getRegex ? { regex: box.getRegex() } : {};
      Promise.resolve(opts.rowsFor(val, extra)).then(render, function () { render([]); });
    }, opts.debounce != null ? opts.debounce : 120);

    // Search input: reuse the zgui-core searchBox (with its regex toggle) when asked, else a plain box.
    var box = {}, input = null;
    if (opts.regex && ZGui.searchBox) {
      var sb = ZGui.searchBox(searchHost, {
        placeholder: opts.placeholder,
        regex: true,
        onInput: refresh,
        onRegex: refresh,
        onClear: refresh,
      });
      box = sb; input = sb.input;
    } else {
      input = document.createElement("input");
      input.type = "text";
      input.className = "zp-input";
      input.placeholder = opts.placeholder || "";
      input.autocomplete = "off"; input.autocapitalize = "off"; input.spellcheck = false; input.setAttribute("autocorrect", "off");
      searchHost.appendChild(input);
      input.addEventListener("input", refresh);
      box = { get: function () { return input.value; } };
    }

    input.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown") { e.preventDefault(); if (rows.length) { sel = (sel + 1) % rows.length; highlight(); } }
      else if (e.key === "ArrowUp") { e.preventDefault(); if (rows.length) { sel = (sel - 1 + rows.length) % rows.length; highlight(); } }
      else if (e.key === "Enter") { e.preventDefault(); if (sel >= 0 && rows[sel]) rows[sel].onPick(); }
    });

    setTimeout(function () { input.focus(); }, 30);
    if (opts.eager) refresh();
    return { dlg: dlg, refresh: refresh };
  }

  // ── ⌘P quick-open (fuzzy file finder) ───────────────────────────────────────────────────────────
  function quickOpen() {
    getRoot().then(function (root) {
      pickerModal({
        title: T("zmax.panel.quick_open", "Quick Open"),
        placeholder: T("zmax.panel.quick_open_ph", "Fuzzy file name…"),
        eager: true,
        rowsFor: function (query) {
          return invoke("find_files", { root: root, query: query, limit: 300 }).then(function (hits) {
            return hits.map(function (h) {
              var slash = h.rel.lastIndexOf("/");
              return {
                primary: slash >= 0 ? h.rel.slice(slash + 1) : h.rel,
                secondary: slash >= 0 ? h.rel.slice(0, slash) : "",
                onPick: function () { openInEditor(h.path); },
              };
            });
          });
        },
      });
    });
  }

  // ── ⌘⇧J find-in-files (project-wide text / regex search) ─────────────────────────────────────────
  function findInFiles() {
    getRoot().then(function (root) {
      // Case + whole-word toggles (regex toggle comes from the searchBox itself).
      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var ci = optToggle("Aa", T("zmax.panel.case", "Match case"));
      var ww = optToggle("\\b", T("zmax.panel.word", "Whole word"));
      controls.appendChild(ci.el);
      controls.appendChild(ww.el);

      var pm = pickerModal({
        title: T("zmax.panel.find_in_files", "Find in Files"),
        placeholder: T("zmax.panel.find_ph", "Search text or /regex/…"),
        regex: true,
        controls: controls,
        debounce: 200,
        countFmt: function (n) { return n + " " + T("zmax.panel.matches", "matches"); },
        rowsFor: function (query, extra) {
          if (!query) return [];
          return invoke("search_project", {
            root: root,
            query: query,
            opts: {
              regex: !!(extra && extra.regex),
              case_insensitive: !ci.on,   // toggle labelled "Match case" → OFF means case-insensitive
              whole_word: ww.on,
              max_results: 2000,
            },
          }).then(function (res) {
            var srcRows = res.hits.map(function (h) {
              return {
                primary: h.text || "(match)",
                secondary: h.rel + ":" + h.line,
                onPick: function () { openInEditor(h.path, h.line, h.col); },
                action: {
                  label: "★",
                  title: T("zmax.panel.bookmark_line", "Bookmark this line"),
                  run: function () {
                    var lbl = (h.text || "").slice(0, 60) + " — " + h.rel + ":" + h.line;
                    invoke("bookmark_add", { path: h.path, line: h.line, label: lbl }).then(function () { toast(T("zmax.panel.bookmarked", "Bookmarked")); });
                  },
                },
              };
            });
            // One query, one ranked list: `.docx` / `.xlsx` / `.pptx` / ODF / `.pdf` hits land in
            // the same result list as the source hits, after them so the familiar rows stay put.
            // A document parse failure is surfaced as a row rather than dropped, otherwise a
            // corrupt package is indistinguishable from "no matches".
            var errRows = ((res.doc_errors) || []).map(function (pair) {
              return {
                badge: "!",
                primary: String(pair[1]),
                secondary: String(pair[0]),
                onPick: function () { toast(String(pair[1]), "error"); },
              };
            });
            return srcRows.concat(docRows(res), errRows);
          });
        },
      });
      // Re-run the search when a toggle flips.
      ci.onChange = pm.refresh;
      ww.onChange = pm.refresh;
    });
  }

  // A small square on/off toggle button (label flips visual state via .active).
  function optToggle(label, title) {
    var el = document.createElement("button");
    el.type = "button";
    el.className = "zp-opt";
    el.textContent = label;
    el.title = title;
    var api = { el: el, on: false, onChange: null };
    el.addEventListener("click", function () {
      api.on = !api.on;
      el.classList.toggle("active", api.on);
      if (typeof api.onChange === "function") api.onChange();
    });
    return api;
  }

  // ── ⌘E recent files ─────────────────────────────────────────────────────────────────────────────
  function recentFiles() {
    invoke("recent_list").then(function (paths) {
      pickerModal({
        title: T("zmax.panel.recent", "Recent Files"),
        placeholder: T("zmax.panel.filter", "Filter…"),
        eager: true,
        actions: [
          { label: T("zmax.panel.clear", "Clear"), close: true, onClick: function () { invoke("recent_clear").then(function () { toast(T("zmax.panel.recent_cleared", "Recent files cleared")); }); } },
          { label: T("zmax.dialog.cancel", "Cancel"), close: true },
        ],
        rowsFor: function (query) {
          var q2 = (query || "").toLowerCase();
          return (paths || []).filter(function (p) { return !q2 || p.toLowerCase().indexOf(q2) >= 0; }).map(function (p) {
            var slash = p.lastIndexOf("/");
            return {
              primary: slash >= 0 ? p.slice(slash + 1) : p,
              secondary: slash >= 0 ? p.slice(0, slash) : "",
              onPick: function () { openInEditor(p); },
            };
          });
        },
      });
    }, function () { toast(T("zmax.panel.no_recent", "No recent files")); });
  }

  // ── ⌘⇧E project files (tree + new / rename / delete / copy / stats) ──────────────────────────────
  function projectBrowser() {
    if (!window.ZGui || !ZGui.modal || !ZGui.tree) return;
    var body = document.createElement("div");
    body.className = "zmax-fb zp-browser";
    var pathBar = document.createElement("div");
    pathBar.className = "zmax-fb-path";
    var treeHost = document.createElement("div");
    treeHost.className = "zmax-fb-tree";
    body.appendChild(pathBar);
    body.appendChild(treeHost);

    var curDir = null;
    var dlg = ZGui.modal.open({
      title: T("zmax.panel.project_files", "Project Files"),
      body: body,
      className: "zmax-fb-modal zp-browser-modal",
      actions: [
        { label: "＋ " + T("zmax.panel.new_file", "New File"), close: false, onClick: function () { newEntry(false); } },
        { label: "\u{1F4C1} " + T("zmax.panel.new_folder", "New Folder"), close: false, onClick: function () { newEntry(true); } },
        { label: T("zmax.dialog.cancel", "Cancel"), close: true },
      ],
    });

    function load(dir) {
      invoke("list_dir", { path: dir || null }).then(function (listing) {
        curDir = listing.dir;
        pathBar.textContent = listing.dir;
        var nodes = [];
        if (listing.parent) nodes.push({ label: "..", icon: "↑", data: { path: listing.parent, dir: true } });
        listing.entries.forEach(function (en) {
          nodes.push({ label: en.name, icon: en.is_dir ? "\u{1F4C1}" : "\u{1F4C4}", data: { path: en.path, dir: en.is_dir } });
        });
        ZGui.tree.render(treeHost, nodes, {
          onSelect: function (node) {
            var d = node.data || {};
            if (d.dir) load(d.path);
            else { openInEditor(d.path); dlg.close(); }
          },
        });
        // Right-click a row for file operations.
        treeHost.querySelectorAll(".zg-tree-row").forEach(function (rowEl, i) {
          var node = nodes[i];
          if (!node || node.label === "..") return;
          bindRowMenu(rowEl, node.data);
        });
      }).catch(function () {});
    }

    function bindRowMenu(rowEl, d) {
      if (!ZGui.contextMenu) return;
      rowEl.addEventListener("contextmenu", function (e) {
        ZGui.contextMenu.show(e, [
          { label: T("zmax.file.open", "Open"), icon: "\u{1F4C2}", action: function () { if (!d.dir) { openInEditor(d.path); dlg.close(); } else load(d.path); } },
          "---",
          { label: T("zmax.panel.rename", "Rename…"), icon: "✏", action: function () { renameEntry(d.path); } },
          { label: T("zmax.panel.duplicate", "Duplicate…"), icon: "⎘", action: function () { copyEntry(d.path); } },
          { label: T("zmax.panel.delete", "Delete…"), icon: "\u{1F5D1}", action: function () { deleteEntry(d.path, d.dir); } },
          "---",
          { label: T("zmax.panel.stats", "File Info"), icon: "ℹ", action: function () { showStats(d.path); } },
        ]);
      });
    }

    function newEntry(isDir) {
      ZGui.modal.prompt({
        title: isDir ? T("zmax.panel.new_folder", "New Folder") : T("zmax.panel.new_file", "New File"),
        message: T("zmax.panel.new_in", "Create in") + " " + curDir + ":",
        placeholder: isDir ? "folder-name" : "file-name.txt",
      }).then(function (name) {
        if (!name) return;
        var full = curDir.replace(/\/$/, "") + "/" + name;
        invoke("create_path", { path: full, isDir: isDir }).then(function () {
          toast(T("zmax.panel.created", "Created") + " " + name);
          load(curDir);
          if (!isDir) { openInEditor(full); dlg.close(); }
        }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    function renameEntry(path) {
      ZGui.modal.prompt({ title: T("zmax.panel.rename", "Rename…"), message: T("zmax.panel.rename_to", "Rename to:"), value: path }).then(function (to) {
        if (!to || to === path) return;
        invoke("rename_path", { from: path, to: to }).then(function () { toast(T("zmax.panel.renamed", "Renamed")); load(curDir); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    function copyEntry(path) {
      ZGui.modal.prompt({ title: T("zmax.panel.duplicate", "Duplicate…"), message: T("zmax.panel.copy_to", "Copy to:"), value: path + ".copy" }).then(function (to) {
        if (!to || to === path) return;
        invoke("copy_path", { from: path, to: to }).then(function () { toast(T("zmax.panel.copied", "Copied")); load(curDir); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    function deleteEntry(path, isDir) {
      ZGui.modal.confirm({
        title: T("zmax.panel.delete", "Delete…"),
        message: (isDir ? T("zmax.panel.delete_dir_msg", "Delete this folder and everything in it?") : T("zmax.panel.delete_msg", "Delete this file?")) + "\n" + path,
      }).then(function (ok) {
        if (!ok) return;
        invoke("delete_path", { path: path }).then(function () { toast(T("zmax.panel.deleted", "Deleted")); load(curDir); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    load(null);
  }

  function showStats(path) {
    invoke("file_stats", { path: path }).then(function (s) {
      var name = path.slice(path.lastIndexOf("/") + 1);
      var msg = s.is_dir
        ? (T("zmax.panel.folder", "Folder") + " · " + s.chars + " " + T("zmax.panel.items", "items") + " · " + fmtBytes(s.bytes))
        : (s.lines + " " + T("zmax.panel.lines", "lines") + " · " + s.words + " " + T("zmax.panel.words", "words") + " · " + s.chars + " " + T("zmax.panel.chars", "chars") + " · " + fmtBytes(s.bytes));
      ZGui.modal.open({
        title: name,
        body: (function () { var d = document.createElement("div"); d.className = "zp-stats"; d.textContent = msg; return d; })(),
        actions: [{ label: T("zmax.dialog.ok", "OK"), close: true }],
      });
    }, function (err) { toast(String(err), "error"); });
  }
  function fmtBytes(n) {
    if (n < 1024) return n + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
    return (n / (1024 * 1024)).toFixed(1) + " MB";
  }

  // ── git panel (branch + changed files + per-file diff + stage / unstage / discard) ───────────────
  // A small right-aligned action button that doesn't trigger the row's diff click.
  function gitActBtn(label, title, cls, fn) {
    var b = document.createElement("button");
    b.type = "button";
    b.className = "zp-open-btn" + (cls ? " " + cls : "");
    b.textContent = label;
    if (title) b.title = title;
    b.addEventListener("click", function (e) { e.stopPropagation(); fn(); });
    return b;
  }
  function gitPanel() {
    getRoot().then(function (root) {
      var body = document.createElement("div");
      body.className = "zp-git";
      var head = document.createElement("div");
      head.className = "zp-git-head";
      invoke("git_branch", { root: root }).then(function (br) { head.textContent = "⌥ " + (br || "?"); }, function () {});
      body.appendChild(head);

      var list = document.createElement("div");
      list.className = "zp-list";
      body.appendChild(list);

      var diffPre = document.createElement("pre");
      diffPre.className = "zp-diff";
      body.appendChild(diffPre);

      var dlgRef;
      function reload() {
        invoke("git_status", { root: root }).then(render, function (err) { toast(T("zmax.panel.not_git", "Not a git repository") + (err ? ": " + err : ""), "error"); });
      }
      function render(entries) {
        list.textContent = "";
        if (!entries.length) {
          var clean = document.createElement("div");
          clean.className = "zp-count";
          clean.textContent = T("zmax.panel.clean", "Working tree clean");
          list.appendChild(clean);
          return;
        }
        entries.forEach(function (en) {
          var row = document.createElement("div");
          row.className = "zp-row";
          var badge = document.createElement("span");
          badge.className = "zp-badge zp-git-" + en.status.trim().charAt(0);
          badge.textContent = en.status.trim() || en.status;
          var name = document.createElement("span");
          name.className = "zp-row-primary";
          name.textContent = en.rel;
          row.appendChild(badge);
          row.appendChild(name);
          row.addEventListener("click", function () {
            invoke("git_file_diff", { path: en.path }).then(function (d) { diffPre.textContent = d || T("zmax.panel.no_diff", "(no diff)"); }, function () {});
          });
          // Porcelain XY: X = index (staged) status, Y = worktree status; "??" = untracked.
          var index = en.status.charAt(0), work = en.status.charAt(1);
          var untracked = en.status.indexOf("?") >= 0;
          var staged = index !== " " && index !== "?";
          var workDirty = work !== " " && work !== "?";
          if (untracked || workDirty) row.appendChild(gitActBtn("＋", T("zmax.panel.stage", "Stage"), "", function () { invoke("git_stage", { path: en.path }).then(reload, function (err) { toast(String(err), "error"); }); }));
          if (staged) row.appendChild(gitActBtn("−", T("zmax.panel.unstage", "Unstage"), "", function () { invoke("git_unstage", { path: en.path }).then(reload, function (err) { toast(String(err), "error"); }); }));
          if (!untracked && workDirty) row.appendChild(gitActBtn("⟲", T("zmax.panel.discard", "Discard changes"), "zp-danger", function () {
            ZGui.modal.confirm({
              title: T("zmax.panel.discard", "Discard changes"),
              message: T("zmax.panel.discard_msg", "Discard working-tree changes to this file? This cannot be undone.") + "\n" + en.rel,
            }).then(function (ok) { if (ok) invoke("git_discard", { path: en.path }).then(reload, function (err) { toast(String(err), "error"); }); });
          }));
          row.appendChild(gitActBtn("▤", T("zmax.panel.blame", "Blame"), "", function () { gitBlame(en.path); }));
          row.appendChild(gitActBtn(T("zmax.file.open", "Open"), "", "", function () { openInEditor(en.path); dlgRef.close(); }));
          list.appendChild(row);
        });
      }

      dlgRef = ZGui.modal.open({
        title: T("zmax.panel.git_changes", "Git Changes"),
        body: body,
        className: "zp-modal zp-git-modal",
        actions: [
          { label: T("zmax.panel.refresh", "Refresh"), close: false, onClick: reload },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      reload();
    });
  }

  // ── a reusable "pick a project file, then do X with its path" picker (fuzzy quick-open) ───────────
  function pickFileThen(title, onPick) {
    getRoot().then(function (root) {
      pickerModal({
        title: title,
        placeholder: T("zmax.panel.quick_open_ph", "Fuzzy file name…"),
        eager: true,
        rowsFor: function (query) {
          return invoke("find_files", { root: root, query: query, limit: 300 }).then(function (hits) {
            return hits.map(function (h) {
              var slash = h.rel.lastIndexOf("/");
              return {
                primary: slash >= 0 ? h.rel.slice(slash + 1) : h.rel,
                secondary: slash >= 0 ? h.rel.slice(0, slash) : "",
                onPick: function () { onPick(h.path, h.rel); },
              };
            });
          });
        },
      });
    });
  }

  // ── git blame (per-line author / commit / date; click a line to jump there) ──────────────────────
  function gitBlame(path) {
    if (!path) { pickFileThen(T("zmax.panel.blame_file", "Blame a File"), gitBlame); return; }
    invoke("git_blame", { path: path }).then(function (lines) {
      var body = document.createElement("div");
      body.className = "zp-blame";
      var list = document.createElement("div");
      list.className = "zp-list zp-blame-list";
      (lines || []).forEach(function (bl) {
        var row = document.createElement("div");
        row.className = "zp-row zp-blame-row";
        var meta = document.createElement("span");
        meta.className = "zp-blame-meta";
        meta.textContent = bl.commit + " " + bl.date + " " + bl.author;
        var ln = document.createElement("span");
        ln.className = "zp-blame-line";
        ln.textContent = bl.line;
        var sum = document.createElement("span");
        sum.className = "zp-row-primary zp-blame-sum";
        sum.textContent = bl.summary;
        row.appendChild(ln);
        row.appendChild(meta);
        row.appendChild(sum);
        row.addEventListener("click", function () { openInEditor(path, bl.line); });
        list.appendChild(row);
      });
      if (!lines || !lines.length) { var e = document.createElement("div"); e.className = "zp-count"; e.textContent = T("zmax.panel.no_blame", "No blame (untracked or not a repo)"); list.appendChild(e); }
      body.appendChild(list);
      ZGui.modal.open({
        title: T("zmax.panel.blame", "Blame") + " · " + path.slice(path.lastIndexOf("/") + 1),
        body: body,
        className: "zp-modal zp-blame-modal",
        actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
      });
    }, function (err) { toast(String(err), "error"); });
  }

  // ── document blame (git blame at document-address granularity) ───────────────────────────────────
  // The per-line blame above stops at binary documents: git sees one blob and reports
  // "Binary files differ". `doc_blame` (doc_blame.rs) walks the revisions that touched the document,
  // parses each one in-process with the office/PDF engines already linked into the host, and blames
  // each ADDRESS — Sheet1!B14, p. 7 — instead of a line. Rows reuse `locatorLabel`, the same label
  // function the document-search rows use, so one address renders identically everywhere.
  function docBlame(path) {
    if (!path) { pickFileThen(T("zmax.panel.doc_blame_file", "Blame a Document"), docBlame); return; }
    invoke("doc_blame", { path: path }).then(function (res) {
      var entries = (res && res.entries) || [];
      var body = document.createElement("div");
      body.className = "zp-blame zp-docblame";

      // How deep the walk went. Derived from the result, never a literal: a capped walk that
      // silently claimed full history would make every "at or before" row look exact.
      var head = document.createElement("div");
      head.className = "zp-count";
      var scope = res.truncated
        ? T("zmax.panel.doc_blame_capped", "blamed over the newest") + " " + res.revisions_walked +
          " " + T("zmax.panel.doc_blame_of", "of") + " " + res.revisions_total + " " +
          T("zmax.panel.doc_blame_revs", "revisions")
        : res.revisions_walked + " " + T("zmax.panel.doc_blame_revs", "revisions");
      head.textContent = entries.length + " " + T("zmax.panel.doc_blame_addresses", "addresses") + " · " + scope;
      body.appendChild(head);

      var list = document.createElement("div");
      list.className = "zp-list zp-blame-list";
      entries.forEach(function (en) {
        var row = document.createElement("div");
        row.className = "zp-row zp-blame-row";
        var addr = document.createElement("span");
        addr.className = "zp-docblame-addr";
        addr.textContent = locatorLabel(en.locator);
        var meta = document.createElement("span");
        meta.className = "zp-blame-meta";
        // The "≤" prefix is the honest marker for an attribution the capped window could not
        // prove — the change is at or before this commit, not necessarily by it.
        meta.textContent = (en.at_or_before ? "≤ " : "") + en.commit + " " + en.date + " " + en.author;
        if (en.at_or_before) meta.classList.add("zp-docblame-approx");
        var sum = document.createElement("span");
        sum.className = "zp-row-primary zp-blame-sum";
        sum.textContent = en.summary;
        var val = document.createElement("span");
        val.className = "zp-row-secondary zp-docblame-val";
        val.textContent = en.text;
        row.appendChild(addr);
        row.appendChild(meta);
        row.appendChild(sum);
        row.appendChild(val);
        // Same activation as a document search hit: hand the file to the OS default app and put the
        // address on the clipboard, since `:open` cannot render a package in the PTY editor.
        row.addEventListener("click", function () {
          openDocument({ path: path, rel: path.slice(path.lastIndexOf("/") + 1), locator: en.locator });
        });
        list.appendChild(row);
      });
      if (!entries.length) {
        var e = document.createElement("div");
        e.className = "zp-count";
        e.textContent = T("zmax.panel.no_doc_blame", "No blamable addresses in this document");
        list.appendChild(e);
      }
      body.appendChild(list);

      // Revisions that would not parse are a GAP in the attribution, not a non-event — the Rust
      // side reports them rather than swallowing them, so the UI shows them.
      if (res.errors && res.errors.length) {
        var errs = document.createElement("div");
        errs.className = "zp-count zp-docblame-errs";
        errs.textContent = T("zmax.panel.doc_blame_skipped", "Unreadable revisions skipped") + ": " +
          res.errors.map(function (p) { return p[0]; }).join(", ");
        body.appendChild(errs);
      }

      ZGui.modal.open({
        title: T("zmax.panel.doc_blame", "Document Blame") + " · " + path.slice(path.lastIndexOf("/") + 1),
        body: body,
        className: "zp-modal zp-blame-modal",
        actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
      });
    }, function (err) { toast(String(err), "error"); });
  }

  // ── git file history (log) + per-commit diff preview ─────────────────────────────────────────────
  function gitHistory(path) {
    if (!path) { pickFileThen(T("zmax.panel.history_file", "File History"), gitHistory); return; }
    invoke("git_log_file", { path: path, limit: 300 }).then(function (commits) {
      var body = document.createElement("div");
      body.className = "zp-git";
      var list = document.createElement("div");
      list.className = "zp-list";
      var diffPre = document.createElement("pre");
      diffPre.className = "zp-diff";
      if (!commits || !commits.length) { var e = document.createElement("div"); e.className = "zp-count"; e.textContent = T("zmax.panel.no_history", "No history for this file"); list.appendChild(e); }
      (commits || []).forEach(function (c) {
        var row = document.createElement("div");
        row.className = "zp-row";
        var badge = document.createElement("span");
        badge.className = "zp-badge";
        badge.textContent = c.short;
        var name = document.createElement("span");
        name.className = "zp-row-primary";
        name.textContent = c.summary;
        var sec = document.createElement("span");
        sec.className = "zp-row-secondary";
        sec.textContent = c.author + " · " + c.date;
        row.appendChild(badge);
        row.appendChild(name);
        row.appendChild(sec);
        row.addEventListener("click", function () {
          invoke("git_show", { path: path, hash: c.hash }).then(function (d) { diffPre.textContent = d || T("zmax.panel.no_diff", "(no diff)"); }, function (err) { diffPre.textContent = String(err); });
        });
        list.appendChild(row);
      });
      body.appendChild(list);
      body.appendChild(diffPre);
      ZGui.modal.open({
        title: T("zmax.panel.history", "File History") + " · " + path.slice(path.lastIndexOf("/") + 1),
        body: body,
        className: "zp-modal zp-git-modal",
        actions: [
          { label: T("zmax.file.open", "Open"), close: true, onClick: function () { openInEditor(path); } },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
    }, function (err) { toast(String(err), "error"); });
  }

  // ── compare two files (unified diff via git diff --no-index) ──────────────────────────────────────
  function compareFiles() {
    pickFileThen(T("zmax.panel.compare_left", "Compare: pick first file"), function (left) {
      pickFileThen(T("zmax.panel.compare_right", "Compare: pick second file"), function (right) {
        invoke("diff_files", { left: left, right: right }).then(function (d) {
          var body = document.createElement("div");
          body.className = "zp-git";
          var head = document.createElement("div");
          head.className = "zp-git-head";
          head.textContent = left.slice(left.lastIndexOf("/") + 1) + " ↔ " + right.slice(right.lastIndexOf("/") + 1);
          var diffPre = document.createElement("pre");
          diffPre.className = "zp-diff";
          diffPre.textContent = (d && d.trim()) ? d : T("zmax.panel.files_identical", "(files are identical)");
          body.appendChild(head);
          body.appendChild(diffPre);
          ZGui.modal.open({
            title: T("zmax.panel.compare_files", "Compare Files"),
            body: body,
            className: "zp-modal zp-git-modal",
            actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
          });
        }, function (err) { toast(String(err), "error"); });
      });
    });
  }

  // ── project code stats (file / line counts by extension) ─────────────────────────────────────────
  function projectStats() {
    getRoot().then(function (root) {
      invoke("project_stats", { root: root, top: 40 }).then(function (s) {
        var body = document.createElement("div");
        body.className = "zp-stats-panel";
        var summary = document.createElement("div");
        summary.className = "zp-git-head";
        summary.textContent = s.files + " " + T("zmax.panel.files", "files") + " · " + s.total_lines.toLocaleString() + " " + T("zmax.panel.lines", "lines") + " · " + fmtBytes(s.total_bytes);
        body.appendChild(summary);

        var list = document.createElement("div");
        list.className = "zp-list";
        var header = document.createElement("div");
        header.className = "zp-row zp-stats-head";
        ["EXT", "FILES", "LINES", "SIZE"].forEach(function (h, i) {
          var c = document.createElement("span");
          c.className = "zp-stat-col zp-stat-col-" + i;
          c.textContent = h;
          header.appendChild(c);
        });
        list.appendChild(header);
        (s.by_ext || []).forEach(function (e) {
          var row = document.createElement("div");
          row.className = "zp-row zp-stats-row";
          var cells = [e.ext, String(e.files), e.lines.toLocaleString(), fmtBytes(e.bytes)];
          cells.forEach(function (val, i) {
            var c = document.createElement("span");
            c.className = "zp-stat-col zp-stat-col-" + i;
            c.textContent = val;
            row.appendChild(c);
          });
          list.appendChild(row);
        });
        body.appendChild(list);
        ZGui.modal.open({
          title: T("zmax.panel.project_stats", "Project Stats"),
          body: body,
          className: "zp-modal zp-stats-modal",
          actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
        });
      }, function (err) { toast(String(err), "error"); });
    });
  }

  // ── snippets (persisted text library; insert into the editor via bracketed paste) ────────────────
  // Insert literal text into the zmax (Helix-fork) editor: ESC to normal mode, `i` to enter insert
  // before the cursor, then a bracketed-paste block (disables auto-indent so multi-line bodies land
  // verbatim), then ESC back to normal. Mirrors menu.js's afterEsc PTY discipline.
  function insertText(bodyText) {
    if (!bodyText) return;
    ptyWrite("\x1b");
    setTimeout(function () { ptyWrite("i\x1b[200~" + bodyText + "\x1b[201~\x1b"); }, 50);
    act.focusEditor();
  }
  function addSnippetFlow(onDone) {
    ZGui.modal.prompt({ title: T("zmax.panel.snippet_new", "New Snippet"), message: T("zmax.panel.snippet_name", "Name:"), placeholder: "header" }).then(function (name) {
      if (!name) return;
      ZGui.modal.prompt({ title: T("zmax.panel.snippet_new", "New Snippet"), message: T("zmax.panel.snippet_body", "Body (\\n for newlines):"), placeholder: "// …" }).then(function (bodyText) {
        if (bodyText == null) return;
        var expanded = String(bodyText).replace(/\\n/g, "\n").replace(/\\t/g, "\t");
        invoke("snippet_add", { name: name, body: expanded }).then(function () { toast(T("zmax.panel.snippet_saved", "Snippet saved")); if (typeof onDone === "function") onDone(); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }).catch(function () {});
  }
  function snippets() {
    var pm;
    pm = pickerModal({
      title: T("zmax.panel.snippets", "Snippets"),
      placeholder: T("zmax.panel.filter", "Filter…"),
      eager: true,
      countFmt: function (n) { return n + " " + T("zmax.panel.snippets_n", "snippets"); },
      actions: [
        { label: "＋ " + T("zmax.panel.snippet_add_a", "Add"), close: false, onClick: function () { addSnippetFlow(function () { if (pm && pm.refresh) pm.refresh(); }); } },
        { label: T("zmax.panel.clear", "Clear"), close: true, onClick: function () { invoke("snippet_clear").then(function () { toast(T("zmax.panel.snippets_cleared", "Snippets cleared")); }); } },
        { label: T("zmax.dialog.cancel", "Cancel"), close: true },
      ],
      rowsFor: function (query) {
        return invoke("snippet_list").then(function (list) {
          var qq = (query || "").toLowerCase();
          return (list || []).filter(function (s) { return !qq || (s.name + " " + s.body).toLowerCase().indexOf(qq) >= 0; }).map(function (s) {
            return {
              primary: s.name,
              secondary: s.body.replace(/\n/g, "⏎").slice(0, 80),
              onPick: function () { insertText(s.body); },
              action: {
                label: "✕",
                title: T("zmax.panel.remove", "Remove"),
                run: function () { invoke("snippet_remove", { name: s.name }).then(function () { if (pm && pm.refresh) pm.refresh(); }); },
              },
            };
          });
        }, function () { return []; });
      },
    });
  }

  // ── ⇧⌘H project-wide search & replace (regex, preview then apply on disk) ─────────────────────────
  function searchReplace() {
    if (!window.ZGui || !ZGui.modal) return;
    getRoot().then(function (root) {
      var body = document.createElement("div");
      body.className = "zp-picker zp-replace";

      var find = document.createElement("input");
      find.type = "text"; find.className = "zp-input"; find.placeholder = T("zmax.panel.find_ph", "Search text or /regex/…");
      find.autocomplete = "off"; find.autocapitalize = "off"; find.spellcheck = false; find.setAttribute("autocorrect", "off");
      var repl = document.createElement("input");
      repl.type = "text"; repl.className = "zp-input"; repl.placeholder = T("zmax.panel.replace_ph", "Replace with… ($1 for capture groups)");
      repl.autocomplete = "off"; repl.autocapitalize = "off"; repl.spellcheck = false; repl.setAttribute("autocorrect", "off");
      body.appendChild(find);
      body.appendChild(repl);

      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var rx = optToggle(".*", T("zmax.panel.regex", "Regex"));
      var ci = optToggle("Aa", T("zmax.panel.case", "Match case"));
      var ww = optToggle("\\b", T("zmax.panel.word", "Whole word"));
      controls.appendChild(rx.el); controls.appendChild(ci.el); controls.appendChild(ww.el);
      body.appendChild(controls);

      var count = document.createElement("div");
      count.className = "zp-count";
      body.appendChild(count);

      var list = document.createElement("div");
      list.className = "zp-list";
      body.appendChild(list);

      var lastResult = null;
      function opts(apply) {
        return { regex: rx.on, case_insensitive: !ci.on, whole_word: ww.on, apply: apply, max_results: 1000 };
      }
      function renderPreview(res) {
        lastResult = res;
        list.textContent = "";
        (res.hits || []).forEach(function (h) {
          var row = document.createElement("div");
          row.className = "zp-row zp-rep-row";
          var loc = document.createElement("div"); loc.className = "zp-rep-loc"; loc.textContent = h.rel + ":" + h.line;
          var bef = document.createElement("div"); bef.className = "zp-rep-before"; bef.textContent = h.before;
          var aft = document.createElement("div"); aft.className = "zp-rep-after"; aft.textContent = "→ " + h.after;
          row.appendChild(loc); row.appendChild(bef); row.appendChild(aft);
          row.addEventListener("click", function () { openInEditor(h.path, h.line, h.col); });
          list.appendChild(row);
        });
        // Document rows. There is no before/after text to show: the engines rewrite the package
        // and return an occurrence *count*, not per-line text, so the row states what will change
        // rather than pretending to a diff it cannot produce.
        (res.doc_hits || []).forEach(function (h) {
          var row = document.createElement("div");
          row.className = "zp-row zp-rep-row zp-doc-row";
          var loc = document.createElement("div");
          loc.className = "zp-rep-loc";
          loc.textContent = h.rel + " · " + h.format;
          var n = document.createElement("div");
          n.className = "zp-doc-count";
          if (h.replaced > 0) {
            n.textContent = h.replaced + " " + (h.whole_run_only
              ? T("zmax.panel.pages_changed", "pages changed")
              : T("zmax.panel.replacements", "replacements"));
          } else {
            // Found by search, not replaceable by the engine — the PDF whole-run asymmetry.
            // Saying so here is the difference between an understood limitation and a bug report.
            n.className = "zp-doc-count zp-doc-skipped";
            n.textContent = T("zmax.panel.pdf_whole_run",
              "matched, but not replaceable: pdf replace matches whole text runs, not substrings");
          }
          row.appendChild(loc);
          row.appendChild(n);
          row.addEventListener("click", function () { openDocument(h); });
          list.appendChild(row);
        });
        (res.doc_errors || []).forEach(function (pair) {
          var row = document.createElement("div");
          row.className = "zp-row zp-rep-row zp-doc-row";
          var loc = document.createElement("div");
          loc.className = "zp-rep-loc";
          loc.textContent = String(pair[0]);
          var msg = document.createElement("div");
          msg.className = "zp-doc-count zp-doc-error";
          msg.textContent = String(pair[1]);
          row.appendChild(loc);
          row.appendChild(msg);
          list.appendChild(row);
        });
        if (res.doc_case_note) {
          var note = document.createElement("div");
          note.className = "zp-doc-note";
          note.textContent = res.doc_case_note;
          list.appendChild(note);
        }
        // Line counts and document counts are reported separately because they measure different
        // things — `total` counts matched lines, `doc_total` counts occurrences inside packages.
        var summary = res.files + " " + T("zmax.panel.files", "files") + " · " + res.total + " " + T("zmax.panel.matches", "matches");
        if (res.doc_files) {
          summary += " · " + res.doc_files + " " + T("zmax.panel.documents", "documents") +
            " · " + res.doc_total + " " + T("zmax.panel.matches", "matches");
        }
        if (res.truncated) summary += " · " + T("zmax.panel.preview_capped", "preview capped");
        count.textContent = summary;
      }
      var preview = debounce(function () {
        var query = find.value;
        if (!query) { list.textContent = ""; count.textContent = ""; lastResult = null; return; }
        invoke("replace_project", { root: root, query: query, replacement: repl.value, opts: opts(false) })
          .then(renderPreview, function (err) { count.textContent = String(err); list.textContent = ""; });
      }, 220);
      find.addEventListener("input", preview);
      repl.addEventListener("input", preview);
      rx.onChange = preview; ci.onChange = preview; ww.onChange = preview;

      function applyAll() {
        var query = find.value;
        // A document-only result is still work to do, so the guard counts both halves.
        if (!query || !lastResult || (!lastResult.total && !lastResult.doc_total)) { toast(T("zmax.panel.nothing_to_replace", "Nothing to replace")); return; }
        // Documents are named separately in the confirm: they are rewritten in place as whole
        // packages, which is a materially different action from editing lines of text, and the
        // user is about to authorise it.
        var msg = T("zmax.panel.replace_confirm", "Rewrite") + " " + lastResult.total + " " +
          T("zmax.panel.matches", "matches") + " " + T("zmax.panel.in", "in") + " " + lastResult.files + " " +
          T("zmax.panel.files", "files");
        if (lastResult.doc_total) {
          msg += "\n" + T("zmax.panel.replace_confirm_docs", "and rewrite") + " " + lastResult.doc_total + " " +
            T("zmax.panel.matches", "matches") + " " + T("zmax.panel.in", "in") + " " + lastResult.doc_files + " " +
            T("zmax.panel.documents", "documents");
        }
        ZGui.modal.confirm({
          title: T("zmax.panel.replace_all", "Replace All"),
          message: msg + "?",
        }).then(function (ok) {
          if (!ok) return;
          invoke("replace_project", { root: root, query: query, replacement: repl.value, opts: opts(true) }).then(function (res) {
            var done = T("zmax.panel.replaced", "Replaced") + " " + res.total + " " + T("zmax.panel.in", "in") + " " + res.files + " " + T("zmax.panel.files", "files");
            if (res.doc_total) {
              done += " · " + res.doc_total + " " + T("zmax.panel.in", "in") + " " + res.doc_files + " " + T("zmax.panel.documents", "documents");
            }
            toast(done);
            dlg.close();
            act.focusEditor();
          }, function (err) { toast(String(err), "error"); });
        });
      }

      var dlg = ZGui.modal.open({
        title: T("zmax.panel.search_replace", "Search & Replace"),
        body: body,
        className: "zp-modal zp-replace-modal",
        actions: [
          { label: T("zmax.panel.replace_all", "Replace All"), close: false, onClick: applyAll },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      setTimeout(function () { find.focus(); }, 30);
    });
  }

  // ── ⇧⌘O go to symbol (workspace outline: fn / struct / class / heading) ───────────────────────────
  function gotoSymbol() {
    getRoot().then(function (root) {
      var cache = null;
      pickerModal({
        title: T("zmax.panel.goto_symbol", "Go to Symbol"),
        placeholder: T("zmax.panel.symbol_ph", "Symbol name…"),
        eager: true,
        countFmt: function (n) { return n + " " + T("zmax.panel.symbols", "symbols"); },
        rowsFor: function (query) {
          var p = cache ? Promise.resolve(cache) : invoke("project_symbols", { root: root, limit: 5000 }).then(function (s) { cache = s; return s; });
          return p.then(function (syms) {
            var qq = (query || "").toLowerCase();
            return (syms || []).filter(function (s) { return !qq || s.name.toLowerCase().indexOf(qq) >= 0; }).slice(0, 500).map(function (s) {
              return { badge: s.kind, primary: s.name, secondary: s.rel + ":" + s.line, onPick: function () { openInEditor(s.path, s.line, s.col); } };
            });
          });
        },
      });
    });
  }

  // ── ⇧⌘T TODO / markers scan (TODO / FIXME / HACK / … across the tree) ─────────────────────────────
  function markers() {
    getRoot().then(function (root) {
      var cache = null;
      pickerModal({
        title: T("zmax.panel.markers", "TODO / Markers"),
        placeholder: T("zmax.panel.marker_ph", "Filter markers…"),
        eager: true,
        countFmt: function (n) { return n + " " + T("zmax.panel.markers_n", "markers"); },
        rowsFor: function (query) {
          var p = cache ? Promise.resolve(cache) : invoke("scan_markers", { root: root, limit: 5000 }).then(function (m) { cache = m; return m; });
          return p.then(function (ms) {
            var qq = (query || "").toLowerCase();
            return (ms || []).filter(function (m) { return !qq || (m.kind + " " + m.text + " " + m.rel).toLowerCase().indexOf(qq) >= 0; }).slice(0, 800).map(function (m) {
              return { badge: m.kind, primary: m.text || "(" + m.kind + ")", secondary: m.rel + ":" + m.line, onPick: function () { openInEditor(m.path, m.line, m.col); } };
            });
          });
        },
      });
    });
  }

  // ── ⌘B bookmarks (persisted file:line marks) ─────────────────────────────────────────────────────
  function promptBookmarkMeta(path, base, onDone) {
    ZGui.modal.prompt({ title: T("zmax.panel.bookmark", "Bookmark"), message: T("zmax.panel.line", "Line:"), value: "1" }).then(function (lineStr) {
      var line = parseInt(lineStr, 10); if (!line || line < 1) line = 1;
      ZGui.modal.prompt({ title: T("zmax.panel.bookmark", "Bookmark"), message: T("zmax.panel.label", "Label:"), value: base + ":" + line }).then(function (label) {
        invoke("bookmark_add", { path: path, line: line, label: label || "" }).then(function () {
          toast(T("zmax.panel.bookmarked", "Bookmarked"));
          if (typeof onDone === "function") onDone();
        }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }).catch(function () {});
  }
  function addBookmarkFlow(onDone) {
    getRoot().then(function (root) {
      pickerModal({
        title: T("zmax.panel.bookmark_file", "Bookmark a File"),
        placeholder: T("zmax.panel.quick_open_ph", "Fuzzy file name…"),
        eager: true,
        rowsFor: function (query) {
          return invoke("find_files", { root: root, query: query, limit: 300 }).then(function (hits) {
            return hits.map(function (h) {
              var slash = h.rel.lastIndexOf("/");
              var base = slash >= 0 ? h.rel.slice(slash + 1) : h.rel;
              return { primary: base, secondary: slash >= 0 ? h.rel.slice(0, slash) : "", onPick: function () { promptBookmarkMeta(h.path, base, onDone); } };
            });
          });
        },
      });
    });
  }
  function bookmarks() {
    var pm;
    pm = pickerModal({
      title: T("zmax.panel.bookmarks", "Bookmarks"),
      placeholder: T("zmax.panel.filter", "Filter…"),
      eager: true,
      countFmt: function (n) { return n + " " + T("zmax.panel.bookmarks_n", "bookmarks"); },
      actions: [
        { label: "＋ " + T("zmax.panel.add_bookmark", "Add"), close: false, onClick: function () { addBookmarkFlow(function () { if (pm && pm.refresh) pm.refresh(); }); } },
        { label: T("zmax.panel.clear", "Clear"), close: true, onClick: function () { invoke("bookmark_clear").then(function () { toast(T("zmax.panel.bookmarks_cleared", "Bookmarks cleared")); }); } },
        { label: T("zmax.dialog.cancel", "Cancel"), close: true },
      ],
      rowsFor: function (query) {
        return invoke("bookmark_list").then(function (list) {
          var qq = (query || "").toLowerCase();
          return (list || []).filter(function (b) { return !qq || (b.label + " " + b.path).toLowerCase().indexOf(qq) >= 0; }).map(function (b) {
            var slash = b.path.lastIndexOf("/");
            return {
              primary: b.label,
              secondary: (slash >= 0 ? b.path.slice(slash + 1) : b.path) + ":" + b.line,
              onPick: function () { openInEditor(b.path, b.line); },
              action: {
                label: "✕",
                title: T("zmax.panel.remove", "Remove"),
                run: function () { invoke("bookmark_remove", { path: b.path, line: b.line }).then(function () { if (pm && pm.refresh) pm.refresh(); }); },
              },
            };
          });
        }, function () { return []; });
      },
    });
  }

  // ── git branches (list / checkout / create) ──────────────────────────────────────────────────────
  function gitBranches() {
    getRoot().then(function (root) {
      var pm;
      pm = pickerModal({
        title: T("zmax.panel.branches", "Git Branches"),
        placeholder: T("zmax.panel.filter", "Filter branches…"),
        eager: true,
        countFmt: function (n) { return n + " " + T("zmax.panel.branches_n", "branches"); },
        actions: [
          { label: "＋ " + T("zmax.panel.new_branch", "New Branch"), close: false, onClick: function () {
            ZGui.modal.prompt({ title: T("zmax.panel.new_branch", "New Branch"), message: T("zmax.panel.branch_name", "Branch name:"), placeholder: "feature/x" }).then(function (name) {
              if (!name) return;
              invoke("git_create_branch", { root: root, name: name }).then(function () { toast(T("zmax.panel.branch_created", "Branch created") + ": " + name); if (pm && pm.refresh) pm.refresh(); }, function (err) { toast(String(err), "error"); });
            }).catch(function () {});
          } },
          { label: T("zmax.panel.refresh", "Refresh"), close: false, onClick: function () { if (pm && pm.refresh) pm.refresh(); } },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
        rowsFor: function (query) {
          return invoke("git_branches", { root: root }).then(function (list) {
            var qq = (query || "").toLowerCase();
            return (list || []).filter(function (b) { return !qq || (b.name + " " + b.subject).toLowerCase().indexOf(qq) >= 0; }).map(function (b) {
              return {
                badge: b.current ? "●" : "",
                primary: b.name,
                secondary: b.date + " · " + b.subject,
                onPick: function () {
                  if (b.current) { toast(T("zmax.panel.already_on", "Already on") + " " + b.name); return; }
                  ZGui.modal.confirm({ title: T("zmax.panel.checkout", "Checkout"), message: T("zmax.panel.checkout_msg", "Switch to branch?") + "\n" + b.name }).then(function (ok) {
                    if (!ok) return;
                    invoke("git_checkout_branch", { root: root, name: b.name }).then(function () { toast(T("zmax.panel.switched_to", "Switched to") + " " + b.name); }, function (err) { toast(String(err), "error"); });
                  });
                },
              };
            });
          }, function (err) { toast(T("zmax.panel.not_git", "Not a git repository") + (err ? ": " + err : ""), "error"); return []; });
        },
      });
    });
  }

  // ── git stash (save / list / pop / drop / show patch) ─────────────────────────────────────────────
  function gitStash() {
    getRoot().then(function (root) {
      var body = document.createElement("div");
      body.className = "zp-git";
      var list = document.createElement("div");
      list.className = "zp-list";
      body.appendChild(list);
      var diffPre = document.createElement("pre");
      diffPre.className = "zp-diff";
      body.appendChild(diffPre);

      function reload() {
        invoke("git_stash_list", { root: root }).then(render, function (err) { toast(T("zmax.panel.not_git", "Not a git repository") + (err ? ": " + err : ""), "error"); });
      }
      function render(entries) {
        list.textContent = "";
        diffPre.textContent = "";
        if (!entries || !entries.length) {
          var none = document.createElement("div");
          none.className = "zp-count";
          none.textContent = T("zmax.panel.no_stash", "No stash entries");
          list.appendChild(none);
          return;
        }
        entries.forEach(function (en) {
          var row = document.createElement("div");
          row.className = "zp-row";
          var badge = document.createElement("span");
          badge.className = "zp-badge";
          badge.textContent = en.selector;
          var name = document.createElement("span");
          name.className = "zp-row-primary";
          name.textContent = en.message;
          row.appendChild(badge);
          row.appendChild(name);
          row.addEventListener("click", function () {
            invoke("git_stash_show", { root: root, index: en.index }).then(function (d) { diffPre.textContent = d || T("zmax.panel.no_diff", "(no diff)"); }, function (err) { diffPre.textContent = String(err); });
          });
          row.appendChild(gitActBtn("▲", T("zmax.panel.stash_pop", "Pop"), "", function () {
            ZGui.modal.confirm({ title: T("zmax.panel.stash_pop", "Pop"), message: T("zmax.panel.stash_pop_msg", "Apply and remove this stash?") + "\n" + en.selector + ": " + en.message }).then(function (ok) {
              if (!ok) return;
              invoke("git_stash_pop", { root: root, index: en.index }).then(function () { toast(T("zmax.panel.stash_popped", "Stash popped")); reload(); }, function (err) { toast(String(err), "error"); });
            });
          }));
          row.appendChild(gitActBtn("✕", T("zmax.panel.stash_drop", "Drop"), "zp-danger", function () {
            ZGui.modal.confirm({ title: T("zmax.panel.stash_drop", "Drop"), message: T("zmax.panel.stash_drop_msg", "Delete this stash without applying? This cannot be undone.") + "\n" + en.selector + ": " + en.message }).then(function (ok) {
              if (!ok) return;
              invoke("git_stash_drop", { root: root, index: en.index }).then(function () { toast(T("zmax.panel.stash_dropped", "Stash dropped")); reload(); }, function (err) { toast(String(err), "error"); });
            });
          }));
          list.appendChild(row);
        });
      }

      function stashSave() {
        ZGui.modal.prompt({ title: T("zmax.panel.stash_save", "Stash Changes"), message: T("zmax.panel.stash_msg", "Message (optional):"), placeholder: "wip" }).then(function (msg) {
          if (msg == null) return;
          invoke("git_stash_save", { root: root, message: msg, includeUntracked: true }).then(function (out) { toast(out || T("zmax.panel.stashed", "Stashed")); reload(); }, function (err) { toast(String(err), "error"); });
        }).catch(function () {});
      }

      ZGui.modal.open({
        title: T("zmax.panel.stash", "Git Stash"),
        body: body,
        className: "zp-modal zp-git-modal",
        actions: [
          { label: "＋ " + T("zmax.panel.stash_save", "Stash Changes"), close: false, onClick: stashSave },
          { label: T("zmax.panel.refresh", "Refresh"), close: false, onClick: reload },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      reload();
    });
  }

  // ── find definition (jump to where an exact symbol is declared) ───────────────────────────────────
  function findDefinition() {
    getRoot().then(function (root) {
      pickerModal({
        title: T("zmax.panel.find_def", "Find Definition"),
        placeholder: T("zmax.panel.def_ph", "Symbol name (exact)…"),
        debounce: 220,
        countFmt: function (n) { return n + " " + T("zmax.panel.definitions", "definitions"); },
        rowsFor: function (query) {
          var q2 = (query || "").trim();
          if (q2.length < 2) return [];
          return invoke("find_definition", { root: root, name: q2, limit: 200 }).then(function (defs) {
            return (defs || []).map(function (s) {
              return { badge: s.kind, primary: s.name, secondary: s.rel + ":" + s.line, onPick: function () { openInEditor(s.path, s.line, s.col); } };
            });
          }, function () { return []; });
        },
      });
    });
  }

  // ── sort lines (reorder a file's lines on disk) ───────────────────────────────────────────────────
  function sortLines() {
    pickFileThen(T("zmax.panel.sort_file", "Sort Lines: pick a file"), function (path, rel) {
      var body = document.createElement("div");
      body.className = "zp-picker";
      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var rev = optToggle("⇅", T("zmax.panel.reverse", "Reverse"));
      var ci = optToggle("Aa", T("zmax.panel.case_insensitive", "Ignore case"));
      var num = optToggle("#", T("zmax.panel.numeric", "Numeric"));
      var uniq = optToggle("∪", T("zmax.panel.unique", "Unique"));
      [rev, ci, num, uniq].forEach(function (o) { controls.appendChild(o.el); });
      body.appendChild(controls);
      var count = document.createElement("div");
      count.className = "zp-count";
      body.appendChild(count);

      function opts(apply) { return { reverse: rev.on, case_insensitive: ci.on, numeric: num.on, unique: uniq.on, apply: apply }; }
      function preview() {
        invoke("sort_file_lines", { path: path, opts: opts(false) }).then(function (r) {
          count.textContent = r.lines_before + " " + T("zmax.panel.lines", "lines")
            + (r.lines_after !== r.lines_before ? " → " + r.lines_after : "")
            + " · " + (r.differs ? T("zmax.panel.will_change", "will change") : T("zmax.panel.no_change", "no change"));
        }, function (err) { count.textContent = String(err); });
      }
      rev.onChange = ci.onChange = num.onChange = uniq.onChange = preview;

      var dlg = ZGui.modal.open({
        title: T("zmax.panel.sort_lines", "Sort Lines") + " · " + rel,
        body: body,
        className: "zp-modal",
        actions: [
          { label: T("zmax.panel.apply", "Apply"), close: false, onClick: function () {
            invoke("sort_file_lines", { path: path, opts: opts(true) }).then(function (r) {
              toast(r.applied ? T("zmax.panel.sorted", "Sorted") : T("zmax.panel.no_change", "no change"));
              if (r.applied) openInEditor(path);
              dlg.close();
            }, function (err) { toast(String(err), "error"); });
          } },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      preview();
    });
  }

  // ── file cleanup / convert (line endings, trailing ws, tabs, final newline) ───────────────────────
  function fileCleanup() {
    pickFileThen(T("zmax.panel.cleanup_file", "Cleanup: pick a file"), function (path, rel) {
      var body = document.createElement("div");
      body.className = "zp-picker";
      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var trim = optToggle("⌫", T("zmax.panel.trim_ws", "Trim trailing whitespace"));
      var finalNl = optToggle("¶", T("zmax.panel.final_nl", "Ensure final newline"));
      var lf = optToggle("LF", T("zmax.panel.to_lf", "Convert to LF"));
      var crlf = optToggle("CRLF", T("zmax.panel.to_crlf", "Convert to CRLF"));
      var expand = optToggle("→ ", T("zmax.panel.expand_tabs", "Tabs → spaces"));
      var tabify = optToggle("⇥", T("zmax.panel.tabify", "Leading spaces → tabs"));
      [trim, finalNl, lf, crlf, expand, tabify].forEach(function (o) { controls.appendChild(o.el); });
      body.appendChild(controls);
      var count = document.createElement("div");
      count.className = "zp-count";
      body.appendChild(count);

      function opts(apply) {
        var eol = lf.on ? "lf" : (crlf.on ? "crlf" : null);
        var tabs = expand.on ? "expand" : (tabify.on ? "tabify" : null);
        return { eol: eol, trim_trailing: trim.on, tabs: tabs, tab_width: 4, final_newline: finalNl.on ? true : null, apply: apply };
      }
      // LF/CRLF and expand/tabify are mutually exclusive — flip the sibling off.
      lf.onChange = function () { if (lf.on && crlf.on) { crlf.on = false; crlf.el.classList.remove("active"); } preview(); };
      crlf.onChange = function () { if (crlf.on && lf.on) { lf.on = false; lf.el.classList.remove("active"); } preview(); };
      expand.onChange = function () { if (expand.on && tabify.on) { tabify.on = false; tabify.el.classList.remove("active"); } preview(); };
      tabify.onChange = function () { if (tabify.on && expand.on) { expand.on = false; expand.el.classList.remove("active"); } preview(); };
      trim.onChange = finalNl.onChange = preview;

      function preview() {
        invoke("convert_file", { path: path, opts: opts(false) }).then(function (r) {
          count.textContent = (r.differs ? T("zmax.panel.will_change", "will change") : T("zmax.panel.no_change", "no change"))
            + " · " + r.changed_lines + " " + T("zmax.panel.lines", "lines")
            + " · " + fmtBytes(r.bytes_before) + " → " + fmtBytes(r.bytes_after);
        }, function (err) { count.textContent = String(err); });
      }

      var dlg = ZGui.modal.open({
        title: T("zmax.panel.cleanup", "File Cleanup") + " · " + rel,
        body: body,
        className: "zp-modal",
        actions: [
          { label: T("zmax.panel.apply", "Apply"), close: false, onClick: function () {
            invoke("convert_file", { path: path, opts: opts(true) }).then(function (r) {
              toast(r.applied ? T("zmax.panel.cleaned", "File cleaned") : T("zmax.panel.no_change", "no change"));
              if (r.applied) openInEditor(path);
              dlg.close();
            }, function (err) { toast(String(err), "error"); });
          } },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      preview();
    });
  }

  // ── batch rename (find → replace on file base names, preview then apply) ───────────────────────────
  function batchRename() {
    if (!window.ZGui || !ZGui.modal) return;
    getRoot().then(function (root) {
      var body = document.createElement("div");
      body.className = "zp-picker zp-replace";
      var find = document.createElement("input");
      find.type = "text"; find.className = "zp-input"; find.placeholder = T("zmax.panel.rename_find", "Match in file name (text or /regex/)…");
      find.autocomplete = "off"; find.autocapitalize = "off"; find.spellcheck = false; find.setAttribute("autocorrect", "off");
      var repl = document.createElement("input");
      repl.type = "text"; repl.className = "zp-input"; repl.placeholder = T("zmax.panel.rename_to", "Replace with… ($1 for capture groups)");
      repl.autocomplete = "off"; repl.autocapitalize = "off"; repl.spellcheck = false; repl.setAttribute("autocorrect", "off");
      body.appendChild(find);
      body.appendChild(repl);

      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var rx = optToggle(".*", T("zmax.panel.regex", "Regex"));
      var ci = optToggle("Aa", T("zmax.panel.case_insensitive", "Ignore case"));
      controls.appendChild(rx.el); controls.appendChild(ci.el);
      body.appendChild(controls);

      var count = document.createElement("div");
      count.className = "zp-count";
      body.appendChild(count);
      var list = document.createElement("div");
      list.className = "zp-list";
      body.appendChild(list);

      var lastResult = null;
      function opts(apply) { return { regex: rx.on, case_insensitive: ci.on, apply: apply, max_results: 1000 }; }
      function renderPreview(res) {
        lastResult = res;
        list.textContent = "";
        (res.plans || []).forEach(function (p) {
          var row = document.createElement("div");
          row.className = "zp-row zp-rep-row";
          var bef = document.createElement("div"); bef.className = "zp-rep-before"; bef.textContent = p.from_rel;
          var aft = document.createElement("div"); aft.className = "zp-rep-after"; aft.textContent = "→ " + p.to_rel;
          row.appendChild(bef); row.appendChild(aft);
          if (p.skipped) { var sk = document.createElement("div"); sk.className = "zp-rename-skip"; sk.textContent = "⚠ " + p.skipped; row.appendChild(sk); }
          list.appendChild(row);
        });
        var summary = res.matched + " " + T("zmax.panel.matched", "matched");
        if (res.truncated) summary += " · " + T("zmax.panel.preview_capped", "preview capped");
        count.textContent = summary;
      }
      var preview = debounce(function () {
        var query = find.value;
        if (!query) { list.textContent = ""; count.textContent = ""; lastResult = null; return; }
        invoke("batch_rename", { root: root, find: query, replace: repl.value, opts: opts(false) })
          .then(renderPreview, function (err) { count.textContent = String(err); list.textContent = ""; });
      }, 220);
      find.addEventListener("input", preview);
      repl.addEventListener("input", preview);
      rx.onChange = preview; ci.onChange = preview;

      function applyAll() {
        var query = find.value;
        if (!query || !lastResult || !lastResult.matched) { toast(T("zmax.panel.nothing_to_rename", "Nothing to rename")); return; }
        ZGui.modal.confirm({
          title: T("zmax.panel.rename_all", "Rename All"),
          message: T("zmax.panel.rename_confirm", "Rename") + " " + lastResult.matched + " " + T("zmax.panel.files", "files") + "?",
        }).then(function (ok) {
          if (!ok) return;
          invoke("batch_rename", { root: root, find: query, replace: repl.value, opts: opts(true) }).then(function (res) {
            toast(T("zmax.panel.renamed_n", "Renamed") + " " + res.renamed + " / " + res.matched);
            dlg.close();
            act.focusEditor();
          }, function (err) { toast(String(err), "error"); });
        });
      }

      var dlg = ZGui.modal.open({
        title: T("zmax.panel.batch_rename", "Batch Rename"),
        body: body,
        className: "zp-modal zp-replace-modal",
        actions: [
          { label: T("zmax.panel.rename_all", "Rename All"), close: false, onClick: applyAll },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      setTimeout(function () { find.focus(); }, 30);
    });
  }

  // ── align columns (align each line on a delimiter, like Emacs align-regexp) ───────────────────────
  function alignColumns() {
    pickFileThen(T("zmax.panel.align_file", "Align Columns: pick a file"), function (path, rel) {
      var body = document.createElement("div");
      body.className = "zp-picker";
      var sep = document.createElement("input");
      sep.type = "text"; sep.className = "zp-input"; sep.placeholder = T("zmax.panel.align_delim", "Delimiter to align on (e.g. = or : or //)…");
      sep.autocomplete = "off"; sep.autocapitalize = "off"; sep.spellcheck = false; sep.setAttribute("autocorrect", "off");
      body.appendChild(sep);
      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var rx = optToggle(".*", T("zmax.panel.regex", "Regex"));
      controls.appendChild(rx.el);
      body.appendChild(controls);
      var count = document.createElement("div");
      count.className = "zp-count";
      body.appendChild(count);

      function opts(apply) { return { separator: sep.value, regex: rx.on, apply: apply }; }
      var preview = debounce(function () {
        if (!sep.value.trim()) { count.textContent = ""; return; }
        invoke("align_columns", { path: path, opts: opts(false) }).then(function (r) {
          count.textContent = r.matched_lines + " " + T("zmax.panel.lines_match", "lines with delimiter")
            + " · " + r.changed_lines + " " + T("zmax.panel.will_move", "to re-align")
            + " · " + (r.differs ? T("zmax.panel.will_change", "will change") : T("zmax.panel.no_change", "no change"));
        }, function (err) { count.textContent = String(err); });
      }, 200);
      sep.addEventListener("input", preview);
      rx.onChange = preview;

      var dlg = ZGui.modal.open({
        title: T("zmax.panel.align_columns", "Align Columns") + " · " + rel,
        body: body,
        className: "zp-modal",
        actions: [
          { label: T("zmax.panel.apply", "Apply"), close: false, onClick: function () {
            if (!sep.value.trim()) { toast(T("zmax.panel.align_need_delim", "Enter a delimiter")); return; }
            invoke("align_columns", { path: path, opts: opts(true) }).then(function (r) {
              toast(r.applied ? T("zmax.panel.aligned", "Aligned") : T("zmax.panel.no_change", "no change"));
              if (r.applied) openInEditor(path);
              dlg.close();
            }, function (err) { toast(String(err), "error"); });
          } },
          { label: T("zmax.dialog.close", "Close"), close: true },
        ],
      });
      setTimeout(function () { sep.focus(); }, 30);
    });
  }

  // ── comment toggle (comment / uncomment a line range with the language prefix) ─────────────────────
  function commentToggle() {
    pickFileThen(T("zmax.panel.comment_file", "Comment/Uncomment: pick a file"), function (path, rel) {
      invoke("file_stats", { path: path }).then(function (st) {
        var maxLine = (st && st.lines) ? st.lines : 1;
        var body = document.createElement("div");
        body.className = "zp-picker";
        var rowEl = document.createElement("div");
        rowEl.className = "zp-opts";
        var from = document.createElement("input");
        from.type = "text"; from.className = "zp-input zp-line-input"; from.value = "1"; from.title = T("zmax.panel.start_line", "Start line");
        from.autocomplete = "off"; from.spellcheck = false;
        var to = document.createElement("input");
        to.type = "text"; to.className = "zp-input zp-line-input"; to.value = String(maxLine); to.title = T("zmax.panel.end_line", "End line");
        to.autocomplete = "off"; to.spellcheck = false;
        var dash = document.createElement("span"); dash.className = "zp-count"; dash.textContent = "–";
        rowEl.appendChild(from); rowEl.appendChild(dash); rowEl.appendChild(to);
        body.appendChild(rowEl);
        var count = document.createElement("div");
        count.className = "zp-count";
        count.textContent = maxLine + " " + T("zmax.panel.lines", "lines");
        body.appendChild(count);

        function range() { return { start: Math.max(1, parseInt(from.value, 10) || 1), end: Math.max(1, parseInt(to.value, 10) || maxLine) }; }
        var preview = debounce(function () {
          var r = range();
          invoke("comment_toggle", { path: path, startLine: r.start, endLine: r.end, apply: false }).then(function (res) {
            count.textContent = (res.commented ? T("zmax.panel.will_comment", "will comment") : T("zmax.panel.will_uncomment", "will uncomment"))
              + " " + res.changed_lines + " " + T("zmax.panel.lines", "lines") + " · " + res.prefix;
          }, function (err) { count.textContent = String(err); });
        }, 200);
        from.addEventListener("input", preview);
        to.addEventListener("input", preview);

        var dlg = ZGui.modal.open({
          title: T("zmax.panel.comment_toggle", "Comment / Uncomment") + " · " + rel,
          body: body,
          className: "zp-modal",
          actions: [
            { label: T("zmax.panel.apply", "Apply"), close: false, onClick: function () {
              var r = range();
              invoke("comment_toggle", { path: path, startLine: r.start, endLine: r.end, apply: true }).then(function (res) {
                toast(res.applied ? (res.commented ? T("zmax.panel.commented", "Commented") : T("zmax.panel.uncommented", "Uncommented")) : T("zmax.panel.no_change", "no change"));
                if (res.applied) openInEditor(path);
                dlg.close();
              }, function (err) { toast(String(err), "error"); });
            } },
            { label: T("zmax.dialog.close", "Close"), close: true },
          ],
        });
        preview();
        setTimeout(function () { from.focus(); }, 30);
      }, function (err) { toast(String(err), "error"); });
    });
  }

  // ── file encoding (detect + transcode charset) ────────────────────────────────────────────────────
  function fileEncoding() {
    pickFileThen(T("zmax.panel.encoding_file", "File Encoding: pick a file"), function (path, rel) {
      invoke("detect_encoding", { path: path }).then(function (info) {
        var body = document.createElement("div");
        body.className = "zp-picker";
        var head = document.createElement("div");
        head.className = "zp-git-head";
        head.textContent = info.encoding + (info.bom ? " (BOM)" : "") + " · " + info.line_ending + " · " + fmtBytes(info.bytes);
        body.appendChild(head);

        var label = document.createElement("div");
        label.className = "zp-count";
        label.textContent = T("zmax.panel.convert_to", "Convert to:");
        body.appendChild(label);

        var controls = document.createElement("div");
        controls.className = "zp-opts";
        var targets = [["utf-8", "UTF-8"], ["utf-16le", "UTF-16LE"], ["utf-16be", "UTF-16BE"], ["latin1", "Latin-1"]];
        var chosen = { to: "utf-8" };
        var toggles = [];
        targets.forEach(function (t) {
          var o = optToggle(t[1], t[1]);
          if (t[0] === chosen.to) { o.on = true; o.el.classList.add("active"); }
          o.onChange = function () {
            chosen.to = t[0];
            toggles.forEach(function (other) { if (other !== o) { other.on = false; other.el.classList.remove("active"); } });
            o.on = true; o.el.classList.add("active");
            preview();
          };
          toggles.push(o);
          controls.appendChild(o.el);
        });
        body.appendChild(controls);

        var count = document.createElement("div");
        count.className = "zp-count";
        body.appendChild(count);

        function preview() {
          invoke("convert_encoding", { path: path, to: chosen.to, apply: false }).then(function (r) {
            count.textContent = r.from + " → " + r.to + " · " + fmtBytes(r.bytes_before) + " → " + fmtBytes(r.bytes_after)
              + " · " + (r.differs ? T("zmax.panel.will_change", "will change") : T("zmax.panel.no_change", "no change"));
          }, function (err) { count.textContent = String(err); });
        }

        var dlg = ZGui.modal.open({
          title: T("zmax.panel.file_encoding", "File Encoding") + " · " + rel,
          body: body,
          className: "zp-modal",
          actions: [
            { label: T("zmax.panel.apply", "Apply"), close: false, onClick: function () {
              invoke("convert_encoding", { path: path, to: chosen.to, apply: true }).then(function (r) {
                toast(r.applied ? (T("zmax.panel.converted", "Converted") + " " + r.from + " → " + r.to) : T("zmax.panel.no_change", "no change"));
                if (r.applied) openInEditor(path);
                dlg.close();
              }, function (err) { toast(String(err), "error"); });
            } },
            { label: T("zmax.dialog.close", "Close"), close: true },
          ],
        });
        preview();
      }, function (err) { toast(String(err), "error"); });
    });
  }

  // ── repo-wide git log (whole history; click a commit for the diff it introduced) ──────────────────
  function gitLog() {
    getRoot().then(function (root) {
      invoke("git_log_repo", { root: root, limit: 400 }).then(function (commits) {
        var body = document.createElement("div");
        body.className = "zp-git";
        var list = document.createElement("div");
        list.className = "zp-list";
        var diffPre = document.createElement("pre");
        diffPre.className = "zp-diff";
        if (!commits || !commits.length) { var e = document.createElement("div"); e.className = "zp-count"; e.textContent = T("zmax.panel.no_history", "No history for this file"); list.appendChild(e); }
        (commits || []).forEach(function (c) {
          var row = document.createElement("div");
          row.className = "zp-row";
          var badge = document.createElement("span");
          badge.className = "zp-badge";
          badge.textContent = c.short;
          var name = document.createElement("span");
          name.className = "zp-row-primary";
          name.textContent = (c.refs ? "(" + c.refs + ") " : "") + c.subject;
          var sec = document.createElement("span");
          sec.className = "zp-row-secondary";
          sec.textContent = c.author + " · " + c.date;
          row.appendChild(badge);
          row.appendChild(name);
          row.appendChild(sec);
          row.addEventListener("click", function () {
            invoke("git_show_commit", { root: root, hash: c.hash }).then(function (d) { diffPre.textContent = d || T("zmax.panel.no_diff", "(no diff)"); }, function (err) { diffPre.textContent = String(err); });
          });
          list.appendChild(row);
        });
        body.appendChild(list);
        body.appendChild(diffPre);
        ZGui.modal.open({
          title: T("zmax.panel.repo_log", "Repository Log"),
          body: body,
          className: "zp-modal zp-git-modal",
          actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
        });
      }, function (err) { toast(T("zmax.panel.not_git", "Not a git repository") + (err ? ": " + err : ""), "error"); });
    });
  }

  // ── git commit graph (ASCII branch graph across all refs) ─────────────────────────────────────────
  function gitGraph() {
    getRoot().then(function (root) {
      invoke("git_graph", { root: root, limit: 400 }).then(function (txt) {
        var body = document.createElement("div");
        body.className = "zp-git";
        var pre = document.createElement("pre");
        pre.className = "zp-diff";
        pre.textContent = (txt && txt.trim()) ? txt : T("zmax.panel.no_commits", "(no commits)");
        body.appendChild(pre);
        ZGui.modal.open({
          title: T("zmax.panel.commit_graph", "Commit Graph"),
          body: body,
          className: "zp-modal zp-git-modal",
          actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
        });
      }, function (err) { toast(T("zmax.panel.not_git", "Not a git repository") + (err ? ": " + err : ""), "error"); });
    });
  }

  // ── diff two revisions (git diff <a> <b>, optionally scoped to a file) ─────────────────────────────
  function diffRevisions() {
    getRoot().then(function (root) {
      var body = document.createElement("div");
      body.className = "zp-git zp-diffrev";
      var a = document.createElement("input");
      a.type = "text"; a.className = "zp-input"; a.placeholder = T("zmax.panel.rev_a", "Revision A (e.g. HEAD~5, main, v1.0)…"); a.value = "HEAD~1";
      a.autocomplete = "off"; a.spellcheck = false;
      var b = document.createElement("input");
      b.type = "text"; b.className = "zp-input"; b.placeholder = T("zmax.panel.rev_b", "Revision B…"); b.value = "HEAD";
      b.autocomplete = "off"; b.spellcheck = false;
      var pathI = document.createElement("input");
      pathI.type = "text"; pathI.className = "zp-input"; pathI.placeholder = T("zmax.panel.rev_path", "Path (optional, relative)…");
      pathI.autocomplete = "off"; pathI.spellcheck = false;
      body.appendChild(a); body.appendChild(b); body.appendChild(pathI);
      var diffPre = document.createElement("pre");
      diffPre.className = "zp-diff";
      body.appendChild(diffPre);

      function run() {
        if (!a.value.trim() || !b.value.trim()) { diffPre.textContent = T("zmax.panel.rev_need", "Enter both revisions"); return; }
        invoke("git_diff_revs", { root: root, revA: a.value.trim(), revB: b.value.trim(), path: pathI.value.trim() || null })
          .then(function (d) { diffPre.textContent = (d && d.trim()) ? d : T("zmax.panel.no_diff_revs", "(no differences)"); }, function (err) { diffPre.textContent = String(err); });
      }
      var runDeb = debounce(run, 250);
      a.addEventListener("input", runDeb); b.addEventListener("input", runDeb); pathI.addEventListener("input", runDeb);

      ZGui.modal.open({
        title: T("zmax.panel.diff_revs", "Diff Revisions"),
        body: body,
        className: "zp-modal zp-git-modal",
        actions: [{ label: T("zmax.dialog.close", "Close"), close: true }],
      });
      run();
    });
  }

  // ── shared file browser (zpwr-file-browser) — full-screen overlay, inited lazily on first open ────
  // The overlay markup (#fbOverlay/#tabFiles) is inlined in index.html; file-browser.js (loaded via
  // fb-backend.js) drives it. file-browser.js's keyboard nav guards on `.tab-content.active#tabFiles`,
  // so we toggle that class; initFileBrowser() is idempotent but we still gate it to first open.
  var _fbInited = false;
  function openFileBrowser() {
    var ov = document.getElementById("fbOverlay");
    if (!ov) return;
    ov.hidden = false;
    var pane = document.getElementById("tabFiles");
    if (pane) pane.classList.add("active");
    if (!_fbInited && typeof window.initFileBrowser === "function") {
      _fbInited = true;
      try { window.initFileBrowser(); } catch (e) {}
    }
    var search = document.getElementById("fileSearchInput");
    if (search) { try { search.focus(); } catch (e) {} }
  }
  function closeFileBrowser() {
    var ov = document.getElementById("fbOverlay");
    if (!ov || ov.hidden) return;
    ov.hidden = true;
    var pane = document.getElementById("tabFiles");
    if (pane) pane.classList.remove("active");
    act.focusEditor();
  }
  function toggleFileBrowser() {
    var ov = document.getElementById("fbOverlay");
    if (ov && ov.hidden) openFileBrowser(); else closeFileBrowser();
  }
  // Exposed for file-browser.js's own injected close control + the overlay bar's ✕.
  window.toggleFileBrowser = toggleFileBrowser;
  window.closeFileBrowser = closeFileBrowser;
  // fb-backend.js routes the browser's "open a file" (openFileDefault) here: load it into the zmax
  // buffer and hide the overlay so the editor is visible.
  window.zmaxOpenPath = function (path) {
    if (!path) return;
    closeFileBrowser();
    openInEditor(path);
  };

  // ── palette + shortcuts wiring ──────────────────────────────────────────────────────────────────
  function myPaletteItems() {
    return [
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.quick_open", "Quick Open") + "  ⌘P", run: quickOpen },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.find_in_files", "Find in Files") + "  ⇧⌘J", run: findInFiles },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.search_replace", "Search & Replace") + "  ⇧⌘H", run: searchReplace },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.goto_symbol", "Go to Symbol") + "  ⇧⌘O", run: gotoSymbol },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.find_def", "Find Definition") + "  ⇧⌘D", run: findDefinition },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.markers", "TODO / Markers") + "  ⇧⌘T", run: markers },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.bookmarks", "Bookmarks") + "  ⌘B", run: bookmarks },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.recent", "Recent Files") + "  ⌘E", run: recentFiles },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.project_files", "Project Files") + "  ⇧⌘E", run: projectBrowser },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.file_browser", "File Browser"), run: openFileBrowser },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.snippets", "Snippets") + "  ⇧⌘I", run: snippets },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.project_stats", "Project Stats"), run: projectStats },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.compare_files", "Compare Files"), run: compareFiles },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.sort_lines", "Sort Lines"), run: sortLines },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.cleanup", "File Cleanup"), run: fileCleanup },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.batch_rename", "Batch Rename"), run: batchRename },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.align_columns", "Align Columns"), run: alignColumns },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.comment_toggle", "Comment / Uncomment") + "  ⇧⌘/", run: commentToggle },
      { label: T("zmax.menu.project", "Project") + " ▸ " + T("zmax.panel.file_encoding", "File Encoding"), run: fileEncoding },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.git_changes", "Git Changes"), run: gitPanel },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.blame", "Blame") + "  ⇧⌘B", run: function () { gitBlame(); } },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.doc_blame", "Document Blame") + "  ⇧⌘Y", run: function () { docBlame(); } },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.history", "File History"), run: function () { gitHistory(); } },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.repo_log", "Repository Log"), run: gitLog },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.commit_graph", "Commit Graph"), run: gitGraph },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.diff_revs", "Diff Revisions"), run: diffRevisions },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.branches", "Git Branches"), run: gitBranches },
      { label: T("zmax.menu.git", "Git") + " ▸ " + T("zmax.panel.stash", "Git Stash"), run: gitStash },
    ];
  }
  function registerPalette() { if (window.ZGui && ZGui.palette && ZGui.palette.register) ZGui.palette.register(myPaletteItems()); }

  function onKey(e) {
    if (!e.metaKey || e.altKey) return;
    var k = e.key.toLowerCase();
    var handled = true;
    if (k === "p" && !e.shiftKey && !e.ctrlKey) quickOpen();
    else if (k === "e" && e.shiftKey) projectBrowser();
    else if (k === "e" && !e.ctrlKey) recentFiles();
    else if (k === "j" && e.shiftKey) findInFiles();
    else if (k === "h" && e.shiftKey && !e.ctrlKey) searchReplace();
    else if (k === "o" && e.shiftKey && !e.ctrlKey) gotoSymbol();
    else if (k === "d" && e.shiftKey && !e.ctrlKey) findDefinition();
    else if (k === "t" && e.shiftKey && !e.ctrlKey) markers();
    else if (k === "i" && e.shiftKey && !e.ctrlKey) snippets();
    else if (k === "b" && e.shiftKey && !e.ctrlKey) gitBlame();
    else if (k === "y" && e.shiftKey && !e.ctrlKey) docBlame();
    else if (k === "b" && !e.shiftKey && !e.ctrlKey) bookmarks();
    else if ((k === "/" || k === "?") && e.shiftKey && !e.ctrlKey) commentToggle();
    else handled = false;
    if (handled) { e.preventDefault(); e.stopPropagation(); }
  }

  function mount(shell) {
    // Record files opened via any route (menu Open, drag-drop, Finder/CLI) into the MRU list too.
    var TA = window.__TAURI__;
    if (TA && TA.event && TA.event.listen) {
      TA.event.listen("open-files", function (ev) {
        (ev && ev.payload || []).forEach(function (p) { if (p) invoke("recent_add", { path: p }).catch(function () {}); });
      }).catch(function () {});
    }

    // Add our actions to ⌘K. menu.js's retranslate() re-sets the palette after the locale loads, which
    // clears ours — so wrap setPaletteItems to re-append every time it runs.
    if (shell && typeof shell.setPaletteItems === "function") {
      var orig = shell.setPaletteItems.bind(shell);
      shell.setPaletteItems = function (items) { orig(items); registerPalette(); };
    }
    registerPalette();

    // Global ⌘ shortcuts (capture phase; these keys aren't claimed by menu.js/appShell).
    window.addEventListener("keydown", onKey, true);

    // File-browser overlay: the bar's ✕ closes it; Escape closes it too (but not while typing in one
    // of its inputs, so field-local Escapes — cancel rename, clear filter — still reach file-browser.js).
    var closeBtn = document.getElementById("fbOverlayClose");
    if (closeBtn) closeBtn.addEventListener("click", closeFileBrowser);
    window.addEventListener("keydown", function (e) {
      if (e.key !== "Escape") return;
      var ov = document.getElementById("fbOverlay");
      if (!ov || ov.hidden) return;
      var t = e.target;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      e.preventDefault();
      closeFileBrowser();
    }, false);
  }

  window.ZmaxPanels = { mount: mount };
})();
