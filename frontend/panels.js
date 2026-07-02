// zemacs-gui — the project workbench: quick-open, find-in-files, recent files, a project-tree file
// manager and a git panel. App-local (this repo owns it), layered ON TOP of the shared MacVim surface
// in menu.js (which stays the single source for the menu bar / basic dialogs). Like everything else in
// the GUI, nothing edits buffers directly: a pick is opened by writing `:open <path>:<line>:<col>` into
// the PTY, so the zemacs editor remains the source of truth. The OS-side work (walk / grep / fs / git)
// lives in the Rust `project.rs` commands; this file is the UI + the PTY bridge.
//
// Surfaces are modal overlays (like the Open dialog), never a docked pane — a docked sidebar would
// have to reflow the embedded terminal, which resolves differently in release WebKit. Entry points:
// the ⌘K command palette (every action) plus ⌘P quick-open, ⌘E recent, ⌘⇧J find-in-files,
// ⌘⇧E project files, and git via the palette.
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
      actions: opts.actions || [{ label: T("zemacs.dialog.cancel", "Cancel"), close: true }],
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
      count.textContent = opts.countFmt ? opts.countFmt(rows.length) : (rows.length + " " + T("zemacs.panel.results", "results"));
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
        title: T("zemacs.panel.quick_open", "Quick Open"),
        placeholder: T("zemacs.panel.quick_open_ph", "Fuzzy file name…"),
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
      var ci = optToggle("Aa", T("zemacs.panel.case", "Match case"));
      var ww = optToggle("\\b", T("zemacs.panel.word", "Whole word"));
      controls.appendChild(ci.el);
      controls.appendChild(ww.el);

      var pm = pickerModal({
        title: T("zemacs.panel.find_in_files", "Find in Files"),
        placeholder: T("zemacs.panel.find_ph", "Search text or /regex/…"),
        regex: true,
        controls: controls,
        debounce: 200,
        countFmt: function (n) { return n + " " + T("zemacs.panel.matches", "matches"); },
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
            return res.hits.map(function (h) {
              return {
                primary: h.text || "(match)",
                secondary: h.rel + ":" + h.line,
                onPick: function () { openInEditor(h.path, h.line, h.col); },
                action: {
                  label: "★",
                  title: T("zemacs.panel.bookmark_line", "Bookmark this line"),
                  run: function () {
                    var lbl = (h.text || "").slice(0, 60) + " — " + h.rel + ":" + h.line;
                    invoke("bookmark_add", { path: h.path, line: h.line, label: lbl }).then(function () { toast(T("zemacs.panel.bookmarked", "Bookmarked")); });
                  },
                },
              };
            });
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
        title: T("zemacs.panel.recent", "Recent Files"),
        placeholder: T("zemacs.panel.filter", "Filter…"),
        eager: true,
        actions: [
          { label: T("zemacs.panel.clear", "Clear"), close: true, onClick: function () { invoke("recent_clear").then(function () { toast(T("zemacs.panel.recent_cleared", "Recent files cleared")); }); } },
          { label: T("zemacs.dialog.cancel", "Cancel"), close: true },
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
    }, function () { toast(T("zemacs.panel.no_recent", "No recent files")); });
  }

  // ── ⌘⇧E project files (tree + new / rename / delete / copy / stats) ──────────────────────────────
  function projectBrowser() {
    if (!window.ZGui || !ZGui.modal || !ZGui.tree) return;
    var body = document.createElement("div");
    body.className = "zemacs-fb zp-browser";
    var pathBar = document.createElement("div");
    pathBar.className = "zemacs-fb-path";
    var treeHost = document.createElement("div");
    treeHost.className = "zemacs-fb-tree";
    body.appendChild(pathBar);
    body.appendChild(treeHost);

    var curDir = null;
    var dlg = ZGui.modal.open({
      title: T("zemacs.panel.project_files", "Project Files"),
      body: body,
      className: "zemacs-fb-modal zp-browser-modal",
      actions: [
        { label: "＋ " + T("zemacs.panel.new_file", "New File"), close: false, onClick: function () { newEntry(false); } },
        { label: "\u{1F4C1} " + T("zemacs.panel.new_folder", "New Folder"), close: false, onClick: function () { newEntry(true); } },
        { label: T("zemacs.dialog.cancel", "Cancel"), close: true },
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
          { label: T("zemacs.file.open", "Open"), icon: "\u{1F4C2}", action: function () { if (!d.dir) { openInEditor(d.path); dlg.close(); } else load(d.path); } },
          "---",
          { label: T("zemacs.panel.rename", "Rename…"), icon: "✏", action: function () { renameEntry(d.path); } },
          { label: T("zemacs.panel.duplicate", "Duplicate…"), icon: "⎘", action: function () { copyEntry(d.path); } },
          { label: T("zemacs.panel.delete", "Delete…"), icon: "\u{1F5D1}", action: function () { deleteEntry(d.path, d.dir); } },
          "---",
          { label: T("zemacs.panel.stats", "File Info"), icon: "ℹ", action: function () { showStats(d.path); } },
        ]);
      });
    }

    function newEntry(isDir) {
      ZGui.modal.prompt({
        title: isDir ? T("zemacs.panel.new_folder", "New Folder") : T("zemacs.panel.new_file", "New File"),
        message: T("zemacs.panel.new_in", "Create in") + " " + curDir + ":",
        placeholder: isDir ? "folder-name" : "file-name.txt",
      }).then(function (name) {
        if (!name) return;
        var full = curDir.replace(/\/$/, "") + "/" + name;
        invoke("create_path", { path: full, isDir: isDir }).then(function () {
          toast(T("zemacs.panel.created", "Created") + " " + name);
          load(curDir);
          if (!isDir) { openInEditor(full); dlg.close(); }
        }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    function renameEntry(path) {
      ZGui.modal.prompt({ title: T("zemacs.panel.rename", "Rename…"), message: T("zemacs.panel.rename_to", "Rename to:"), value: path }).then(function (to) {
        if (!to || to === path) return;
        invoke("rename_path", { from: path, to: to }).then(function () { toast(T("zemacs.panel.renamed", "Renamed")); load(curDir); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    function copyEntry(path) {
      ZGui.modal.prompt({ title: T("zemacs.panel.duplicate", "Duplicate…"), message: T("zemacs.panel.copy_to", "Copy to:"), value: path + ".copy" }).then(function (to) {
        if (!to || to === path) return;
        invoke("copy_path", { from: path, to: to }).then(function () { toast(T("zemacs.panel.copied", "Copied")); load(curDir); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    function deleteEntry(path, isDir) {
      ZGui.modal.confirm({
        title: T("zemacs.panel.delete", "Delete…"),
        message: (isDir ? T("zemacs.panel.delete_dir_msg", "Delete this folder and everything in it?") : T("zemacs.panel.delete_msg", "Delete this file?")) + "\n" + path,
      }).then(function (ok) {
        if (!ok) return;
        invoke("delete_path", { path: path }).then(function () { toast(T("zemacs.panel.deleted", "Deleted")); load(curDir); }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }
    load(null);
  }

  function showStats(path) {
    invoke("file_stats", { path: path }).then(function (s) {
      var name = path.slice(path.lastIndexOf("/") + 1);
      var msg = s.is_dir
        ? (T("zemacs.panel.folder", "Folder") + " · " + s.chars + " " + T("zemacs.panel.items", "items") + " · " + fmtBytes(s.bytes))
        : (s.lines + " " + T("zemacs.panel.lines", "lines") + " · " + s.words + " " + T("zemacs.panel.words", "words") + " · " + s.chars + " " + T("zemacs.panel.chars", "chars") + " · " + fmtBytes(s.bytes));
      ZGui.modal.open({
        title: name,
        body: (function () { var d = document.createElement("div"); d.className = "zp-stats"; d.textContent = msg; return d; })(),
        actions: [{ label: T("zemacs.dialog.ok", "OK"), close: true }],
      });
    }, function (err) { toast(String(err), "error"); });
  }
  function fmtBytes(n) {
    if (n < 1024) return n + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
    return (n / (1024 * 1024)).toFixed(1) + " MB";
  }

  // ── git panel (branch + changed files + per-file diff) ───────────────────────────────────────────
  function gitPanel() {
    getRoot().then(function (root) {
      invoke("git_status", { root: root }).then(function (entries) {
        var body = document.createElement("div");
        body.className = "zp-git";
        var head = document.createElement("div");
        head.className = "zp-git-head";
        invoke("git_branch", { root: root }).then(function (br) { head.textContent = "⌥ " + (br || "?"); });
        body.appendChild(head);

        var list = document.createElement("div");
        list.className = "zp-list";
        body.appendChild(list);

        var diffPre = document.createElement("pre");
        diffPre.className = "zp-diff";
        body.appendChild(diffPre);

        if (!entries.length) {
          var clean = document.createElement("div");
          clean.className = "zp-count";
          clean.textContent = T("zemacs.panel.clean", "Working tree clean");
          list.appendChild(clean);
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
            invoke("git_file_diff", { path: en.path }).then(function (d) { diffPre.textContent = d || T("zemacs.panel.no_diff", "(no diff)"); }, function () {});
          });
          var open = document.createElement("button");
          open.type = "button";
          open.className = "zp-open-btn";
          open.textContent = T("zemacs.file.open", "Open");
          open.addEventListener("click", function (e) { e.stopPropagation(); openInEditor(en.path); dlgRef.close(); });
          row.appendChild(open);
          list.appendChild(row);
        });

        var dlgRef = ZGui.modal.open({
          title: T("zemacs.panel.git_changes", "Git Changes"),
          body: body,
          className: "zp-modal zp-git-modal",
          actions: [{ label: T("zemacs.dialog.close", "Close"), close: true }],
        });
      }, function (err) { toast(T("zemacs.panel.not_git", "Not a git repository") + (err ? ": " + err : ""), "error"); });
    });
  }

  // ── ⇧⌘H project-wide search & replace (regex, preview then apply on disk) ─────────────────────────
  function searchReplace() {
    if (!window.ZGui || !ZGui.modal) return;
    getRoot().then(function (root) {
      var body = document.createElement("div");
      body.className = "zp-picker zp-replace";

      var find = document.createElement("input");
      find.type = "text"; find.className = "zp-input"; find.placeholder = T("zemacs.panel.find_ph", "Search text or /regex/…");
      find.autocomplete = "off"; find.autocapitalize = "off"; find.spellcheck = false; find.setAttribute("autocorrect", "off");
      var repl = document.createElement("input");
      repl.type = "text"; repl.className = "zp-input"; repl.placeholder = T("zemacs.panel.replace_ph", "Replace with… ($1 for capture groups)");
      repl.autocomplete = "off"; repl.autocapitalize = "off"; repl.spellcheck = false; repl.setAttribute("autocorrect", "off");
      body.appendChild(find);
      body.appendChild(repl);

      var controls = document.createElement("div");
      controls.className = "zp-opts";
      var rx = optToggle(".*", T("zemacs.panel.regex", "Regex"));
      var ci = optToggle("Aa", T("zemacs.panel.case", "Match case"));
      var ww = optToggle("\\b", T("zemacs.panel.word", "Whole word"));
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
        var summary = res.files + " " + T("zemacs.panel.files", "files") + " · " + res.total + " " + T("zemacs.panel.matches", "matches");
        if (res.truncated) summary += " · " + T("zemacs.panel.preview_capped", "preview capped");
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
        if (!query || !lastResult || !lastResult.total) { toast(T("zemacs.panel.nothing_to_replace", "Nothing to replace")); return; }
        ZGui.modal.confirm({
          title: T("zemacs.panel.replace_all", "Replace All"),
          message: T("zemacs.panel.replace_confirm", "Rewrite") + " " + lastResult.total + " " +
            T("zemacs.panel.matches", "matches") + " " + T("zemacs.panel.in", "in") + " " + lastResult.files + " " +
            T("zemacs.panel.files", "files") + "?",
        }).then(function (ok) {
          if (!ok) return;
          invoke("replace_project", { root: root, query: query, replacement: repl.value, opts: opts(true) }).then(function (res) {
            toast(T("zemacs.panel.replaced", "Replaced") + " " + res.total + " " + T("zemacs.panel.in", "in") + " " + res.files + " " + T("zemacs.panel.files", "files"));
            dlg.close();
            act.focusEditor();
          }, function (err) { toast(String(err), "error"); });
        });
      }

      var dlg = ZGui.modal.open({
        title: T("zemacs.panel.search_replace", "Search & Replace"),
        body: body,
        className: "zp-modal zp-replace-modal",
        actions: [
          { label: T("zemacs.panel.replace_all", "Replace All"), close: false, onClick: applyAll },
          { label: T("zemacs.dialog.close", "Close"), close: true },
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
        title: T("zemacs.panel.goto_symbol", "Go to Symbol"),
        placeholder: T("zemacs.panel.symbol_ph", "Symbol name…"),
        eager: true,
        countFmt: function (n) { return n + " " + T("zemacs.panel.symbols", "symbols"); },
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
        title: T("zemacs.panel.markers", "TODO / Markers"),
        placeholder: T("zemacs.panel.marker_ph", "Filter markers…"),
        eager: true,
        countFmt: function (n) { return n + " " + T("zemacs.panel.markers_n", "markers"); },
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
    ZGui.modal.prompt({ title: T("zemacs.panel.bookmark", "Bookmark"), message: T("zemacs.panel.line", "Line:"), value: "1" }).then(function (lineStr) {
      var line = parseInt(lineStr, 10); if (!line || line < 1) line = 1;
      ZGui.modal.prompt({ title: T("zemacs.panel.bookmark", "Bookmark"), message: T("zemacs.panel.label", "Label:"), value: base + ":" + line }).then(function (label) {
        invoke("bookmark_add", { path: path, line: line, label: label || "" }).then(function () {
          toast(T("zemacs.panel.bookmarked", "Bookmarked"));
          if (typeof onDone === "function") onDone();
        }, function (err) { toast(String(err), "error"); });
      }).catch(function () {});
    }).catch(function () {});
  }
  function addBookmarkFlow(onDone) {
    getRoot().then(function (root) {
      pickerModal({
        title: T("zemacs.panel.bookmark_file", "Bookmark a File"),
        placeholder: T("zemacs.panel.quick_open_ph", "Fuzzy file name…"),
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
      title: T("zemacs.panel.bookmarks", "Bookmarks"),
      placeholder: T("zemacs.panel.filter", "Filter…"),
      eager: true,
      countFmt: function (n) { return n + " " + T("zemacs.panel.bookmarks_n", "bookmarks"); },
      actions: [
        { label: "＋ " + T("zemacs.panel.add_bookmark", "Add"), close: false, onClick: function () { addBookmarkFlow(function () { if (pm && pm.refresh) pm.refresh(); }); } },
        { label: T("zemacs.panel.clear", "Clear"), close: true, onClick: function () { invoke("bookmark_clear").then(function () { toast(T("zemacs.panel.bookmarks_cleared", "Bookmarks cleared")); }); } },
        { label: T("zemacs.dialog.cancel", "Cancel"), close: true },
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
                title: T("zemacs.panel.remove", "Remove"),
                run: function () { invoke("bookmark_remove", { path: b.path, line: b.line }).then(function () { if (pm && pm.refresh) pm.refresh(); }); },
              },
            };
          });
        }, function () { return []; });
      },
    });
  }

  // ── palette + shortcuts wiring ──────────────────────────────────────────────────────────────────
  function myPaletteItems() {
    return [
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.quick_open", "Quick Open") + "  ⌘P", run: quickOpen },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.find_in_files", "Find in Files") + "  ⇧⌘J", run: findInFiles },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.search_replace", "Search & Replace") + "  ⇧⌘H", run: searchReplace },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.goto_symbol", "Go to Symbol") + "  ⇧⌘O", run: gotoSymbol },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.markers", "TODO / Markers") + "  ⇧⌘T", run: markers },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.bookmarks", "Bookmarks") + "  ⌘B", run: bookmarks },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.recent", "Recent Files") + "  ⌘E", run: recentFiles },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.project_files", "Project Files") + "  ⇧⌘E", run: projectBrowser },
      { label: T("zemacs.menu.project", "Project") + " ▸ " + T("zemacs.panel.git_changes", "Git Changes"), run: gitPanel },
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
    else if (k === "t" && e.shiftKey && !e.ctrlKey) markers();
    else if (k === "b" && !e.ctrlKey) bookmarks();
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
  }

  window.ZemacsPanels = { mount: mount };
})();
