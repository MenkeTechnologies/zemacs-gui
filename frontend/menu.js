// zemacs-gui — the MacVim-style GUI surface, built ENTIRELY from zgui-core widgets (no native menus
// or dialogs). Every action is bridged to the editor by writing into the embedded PTY:
//   * a native-looking menu bar      → ZGui.menubar + ZGui.contextMenu
//   * Cmd-key shortcuts              → window keydown (zemacs-gui is the standalone host; macOS ⌘ keys
//                                       never reach the shell/tmux inside the PTY, so nothing is masked)
//   * Open / Save-As / Help dialogs  → ZGui.modal + ZGui.tree (file browser) / ZGui.modal.prompt
//   * drag-and-drop files            → ZGui.fileDrag.initDrop
// The OS-access primitives (list_dir, fullscreen, blur) are thin Tauri commands; the UI is all zgui.
(function () {
  "use strict";

  // ── PTY bridge ────────────────────────────────────────────────────────────────────────────────
  function core() { return window.__TAURI__ && window.__TAURI__.core; }
  function invoke(cmd, args) { var T = core(); return T ? T.invoke(cmd, args || {}) : Promise.reject("no tauri"); }
  function ptyWrite(data) { invoke("terminal_write", { data: data }).catch(function () {}); }

  // Esc MUST arrive on its own. A terminal input parser reads ESC immediately followed by a byte as
  // Alt+<byte> (so `\x1b:` = Alt+:, not "Esc then :"), which silently drops the command. So we send
  // ESC first, then the rest after a short delay (longer than the editor's esc-disambiguation window)
  // — that makes the editor treat ESC as a discrete keypress before the command/keys land.
  function afterEsc(rest) {
    ptyWrite("\x1b");
    setTimeout(function () { ptyWrite(rest); }, 50);
  }
  // zemacs is a Helix fork with a vim keymap. Run an ex-command from ANY mode: Esc → `:cmd` → Enter.
  function ex(cmd) { afterEsc(":" + cmd + "\r"); }
  // Send normal-mode keys (Esc first so we're not stuck in insert/command mode).
  function nkeys(keys) { afterEsc(keys); }
  // Quote a path for Helix typed-command shellwords parsing (handles spaces).
  function q(p) { return '"' + String(p).replace(/"/g, '\\"') + '"'; }

  // GUI preferences (zgui-widget Preferences panel toggles these).
  var prefs = { showHidden: false };

  // Translate via the shared zpwr-i18n runtime (window.t). appFmt returns the key itself when a key is
  // missing, so we fall back to the English literal — the UI always reads, with or without i18n loaded.
  function T(key, english) {
    var s = (typeof window.t === "function") ? window.t(key) : null;
    return (s && s !== key) ? s : english;
  }

  // ── editor actions (the bridge table; menu items and shortcuts both call these) ─────────────────
  var act = {
    newBuffer:   function () { ex("new"); },
    open:        function () { openDialog(); },
    save:        function () { ex("write"); },
    saveAs:      function () { saveAsDialog(); },
    reload:      function () { ex("reload"); },
    closeBuffer: function () { ex("buffer-close"); },
    quit:        function () { ex("quit-all"); },

    undo:   function () { nkeys("u"); },
    redo:   function () { nkeys("\x12"); },   // C-r
    copy:   function () { nkeys("y"); },
    cut:    function () { nkeys("d"); },
    paste:  function () { nkeys("p"); },
    find:   function () { nkeys("/"); },        // hands off to the editor's own search line
    findNext: function () { nkeys("n"); },
    findPrev: function () { nkeys("N"); },

    nextBuffer: function () { ex("buffer-next"); },
    prevBuffer: function () { ex("buffer-previous"); },

    // Run the current file / start debugging — the IDE binds F5/F6 globally (mode-independent),
    // so we send the raw function-key sequences straight to the PTY (no Esc dance needed).
    run:   function () { ptyWrite("\x1b[15~"); },  // F5 → IDE: smart-run current file
    debug: function () { ptyWrite("\x1b[17~"); },  // F6 → IDE: launch debugger

    hsplit: function () { ex("hsplit"); },
    vsplit: function () { ex("vsplit"); },
    closeSplit: function () { nkeys("\x17q"); }, // C-w q
    rotate:     function () { nkeys("\x17w"); }, // C-w w

    fullscreen: function () { invoke("toggle_fullscreen").catch(function () {}); },
    blurOn:  function () { invoke("set_blur", { on: true }).catch(function () {}); document.body.classList.add("zemacs-translucent"); },
    blurOff: function () { invoke("set_blur", { on: false }).catch(function () {}); document.body.classList.remove("zemacs-translucent"); },

    help: function () { helpDialog(); },
    tutor: function () { nkeys(":tutor\r"); },

    focusEditor: function () { var c = document.getElementById("terminalContainer"); if (c) { var ta = c.querySelector("textarea"); if (ta) ta.focus(); } },
    restartEditor: function () { invoke("terminal_kill").then(function () { if (typeof window.showTerminal === "function") window.showTerminal(); setTimeout(function () { invoke("zemacs_exec_command").then(function (cmd) { ptyWrite("exec " + (cmd || "zemacs") + " --ide\r"); }).catch(function () { ptyWrite("exec zemacs --ide\r"); }); }, 800); }).catch(function () {}); },
  };

  // ── menu bar (ZGui.menubar → ZGui.contextMenu) ──────────────────────────────────────────────────
  function item(label, hint, run) { return { label: hint ? label + "  " + hint : label, action: run }; }
  var SEP = "---";

  function menus() {
    return [
      { label: T("zemacs.menu.file", "File"), items: [
        item(T("zemacs.file.new", "New Buffer"), "⌘N", act.newBuffer),
        item(T("zemacs.file.open", "Open…"), "⌘O", act.open),
        SEP,
        item(T("zemacs.file.save", "Save"), "⌘S", act.save),
        item(T("zemacs.file.save_as", "Save As…"), "⇧⌘S", act.saveAs),
        item(T("zemacs.file.reload", "Reload"), "", act.reload),
        SEP,
        item(T("zemacs.file.close", "Close Buffer"), "⌘W", act.closeBuffer),
        item(T("zemacs.file.quit", "Quit"), "⌘Q", act.quit),
      ] },
      { label: T("zemacs.menu.edit", "Edit"), items: [
        item(T("zemacs.edit.undo", "Undo"), "⌘Z", act.undo),
        item(T("zemacs.edit.redo", "Redo"), "⇧⌘Z", act.redo),
        SEP,
        item(T("zemacs.edit.cut", "Cut"), "", act.cut),
        item(T("zemacs.edit.copy", "Copy"), "", act.copy),
        item(T("zemacs.edit.paste", "Paste"), "", act.paste),
        SEP,
        item(T("zemacs.edit.find", "Find"), "⌘F", act.find),
        item(T("zemacs.edit.find_next", "Find Next"), "⌘G", act.findNext),
        item(T("zemacs.edit.find_prev", "Find Previous"), "⇧⌘G", act.findPrev),
      ] },
      { label: T("zemacs.menu.view", "View"), items: [
        item(T("zemacs.view.fullscreen", "Toggle Full Screen"), "⌃⌘F", act.fullscreen),
        SEP,
        item(T("zemacs.view.translucent", "Translucent Background"), "", act.blurOn),
        item(T("zemacs.view.opaque", "Opaque Background"), "", act.blurOff),
      ] },
      { label: T("zemacs.menu.buffers", "Buffers"), items: [
        item(T("zemacs.buffers.next", "Next Buffer"), "⌘}", act.nextBuffer),
        item(T("zemacs.buffers.prev", "Previous Buffer"), "⌘{", act.prevBuffer),
        SEP,
        item(T("zemacs.file.close", "Close Buffer"), "⌘W", act.closeBuffer),
      ] },
      { label: T("zemacs.menu.window", "Window"), items: [
        item(T("zemacs.window.split_h", "Split Horizontally"), "", act.hsplit),
        item(T("zemacs.window.split_v", "Split Vertically"), "", act.vsplit),
        SEP,
        item(T("zemacs.window.rotate", "Rotate Splits"), "", act.rotate),
        item(T("zemacs.window.close_split", "Close Split"), "", act.closeSplit),
      ] },
      { label: T("zemacs.menu.help", "Help"), items: [
        item(T("zemacs.help.search", "Search Help…"), "", act.help),
        item(T("zemacs.help.tutor", "Open Tutor"), "", act.tutor),
      ] },
    ];
  }

  // Flatten the menu tree into ⌘K command-palette entries so the palette mirrors the full menu (not
  // just the 2 seed items). Labels are "Menu ▸ Item" so they group + fuzzy-search cleanly.
  function paletteItems() {
    var out = [];
    menus().forEach(function (m) {
      m.items.forEach(function (it) {
        if (it === SEP) return;
        out.push({ label: m.label + " ▸ " + it.label, run: it.action });
      });
    });
    out.push({ label: "Editor ▸ " + T("zemacs.editor.focus", "Focus"), run: act.focusEditor });
    out.push({ label: "Editor ▸ " + T("zemacs.editor.restart", "Restart"), run: act.restartEditor });
    return out;
  }

  // ── Cmd-key shortcuts (host owns the global keymap; ⌘ keys don't reach the PTY shell) ────────────
  function onKey(e) {
    if (!e.metaKey) return;
    var k = e.key.toLowerCase();
    var shift = e.shiftKey, ctrl = e.ctrlKey;
    var handled = true;
    if (ctrl && k === "f") act.fullscreen();
    else if (k === "n" && !shift) act.newBuffer();
    else if (k === "o" && !shift) act.open();
    else if (k === "s" && shift) act.saveAs();
    else if (k === "s") act.save();
    else if (k === "w") act.closeBuffer();
    else if (k === "z" && shift) act.redo();
    else if (k === "z") act.undo();
    else if (k === "f") act.find();
    else if (k === "g" && shift) act.findPrev();
    else if (k === "g") act.findNext();
    else if (k === "}" || (k === "]" && shift)) act.nextBuffer();
    else if (k === "{" || (k === "[" && shift)) act.prevBuffer();
    else if (k === "r" && !shift && !ctrl) act.run();     // ⌘R → run current file
    else if (k === "d" && !shift && !ctrl) act.debug();   // ⌘D → debug
    else handled = false;
    if (handled) { e.preventDefault(); e.stopPropagation(); }
  }

  // ── Open dialog: ZGui.modal + ZGui.tree file browser ────────────────────────────────────────────
  function openDialog() {
    if (!window.ZGui || !ZGui.modal || !ZGui.tree) return;
    var bodyWrap = document.createElement("div");
    bodyWrap.className = "zemacs-fb";
    var pathBar = document.createElement("div");
    pathBar.className = "zemacs-fb-path";
    var treeHost = document.createElement("div");
    treeHost.className = "zemacs-fb-tree";
    bodyWrap.appendChild(pathBar);
    bodyWrap.appendChild(treeHost);

    var dlg = ZGui.modal.open({
      title: T("zemacs.dialog.open_title", "Open File"),
      body: bodyWrap,
      className: "zemacs-fb-modal",
      actions: [{ label: T("zemacs.dialog.cancel", "Cancel"), close: true }],
    });

    function load(dir) {
      invoke("list_dir", { path: dir || null, showHidden: prefs.showHidden }).then(function (listing) {
        pathBar.textContent = listing.dir;
        var nodes = [];
        if (listing.parent) nodes.push({ label: "..", icon: "↑", data: { path: listing.parent, dir: true } });
        listing.entries.forEach(function (en) {
          nodes.push({ label: en.name, icon: en.is_dir ? "\u{1F4C1}" : "\u{1F4C4}", data: { path: en.path, dir: en.is_dir } });
        });
        ZGui.tree.render(treeHost, nodes, {
          defaultExpanded: false,
          onSelect: function (node) {
            var d = node.data || {};
            if (d.dir) load(d.path);
            else { ex("open " + q(d.path)); dlg.close(); }
          },
        });
      }).catch(function () {});
    }
    load(null);
  }

  // ── Save As: ZGui.modal.prompt ──────────────────────────────────────────────────────────────────
  function saveAsDialog() {
    if (!window.ZGui || !ZGui.modal || !ZGui.modal.prompt) { act.save(); return; }
    invoke("home_dir").then(function (home) {
      return ZGui.modal.prompt({ title: T("zemacs.dialog.save_as_title", "Save As"), message: T("zemacs.dialog.save_as_msg", "Write the current buffer to:"), value: home + "/", placeholder: T("zemacs.dialog.save_as_ph", "/path/to/file") });
    }).then(function (path) {
      if (path) ex("write " + q(path));
    }).catch(function () {});
  }

  function helpDialog() {
    if (!window.ZGui || !ZGui.modal || !ZGui.modal.prompt) { nkeys(":help\r"); return; }
    ZGui.modal.prompt({ title: T("zemacs.dialog.help_title", "Search Help"), message: T("zemacs.dialog.help_msg", "Topic:"), placeholder: T("zemacs.dialog.help_ph", "e.g. registers") }).then(function (topic) {
      if (topic) ex("help " + topic);
    }).catch(function () {});
  }

  // ── file-open intake: CLI / Finder / mvim:// (Tauri open_intake.rs → :open) ──────────────────────
  function openPaths(paths) { (paths || []).forEach(function (p) { if (p) ex("open " + q(p)); }); }
  function initOpenIntake() {
    var T = window.__TAURI__;
    // Live opens while running (2nd `mvim file`, Finder, deep-link) — editor is already up.
    if (T && T.event && T.event.listen) {
      T.event.listen("open-files", function (e) { openPaths(e && e.payload); }).catch(function () {});
    }
    // Cold-launch file args were queued before the webview existed; drain once the editor has replaced
    // the login shell (exec happens at ~800ms in main.js).
    setTimeout(function () { invoke("take_pending_opens").then(openPaths).catch(function () {}); }, 1500);
  }

  // ── Settings extension: appended to the appShell ⚙/⌘, Settings panel via settingsExtra (NOT a
  //    separate modal — the app already has one Settings panel; this just adds the editor's rows) ──
  function shellRow(label, control) {
    var row = document.createElement("div"); row.className = "zg-shell-row";
    var lab = document.createElement("span"); lab.className = "zg-shell-row-label"; lab.textContent = label;
    row.appendChild(lab); if (control) row.appendChild(control);
    return row;
  }
  function toggleControl(on, onChange) {
    var btn = document.createElement("button"); btn.type = "button";
    btn.className = "zg-shell-toggle" + (on ? " on" : ""); btn.textContent = on ? "ON" : "OFF";
    btn.addEventListener("click", function () {
      var n = !btn.classList.contains("on");
      btn.classList.toggle("on", n); btn.textContent = n ? "ON" : "OFF"; onChange(n);
    });
    return btn;
  }
  // The translated locales shipped in zpwr-i18n (code → native name), for the language picker.
  var LOCALES = [
    ["en", "English"], ["de", "Deutsch"], ["es", "Español"], ["es_419", "Español (LatAm)"],
    ["fr", "Français"], ["it", "Italiano"], ["pt", "Português"], ["pt_br", "Português (BR)"],
    ["nl", "Nederlands"], ["sv", "Svenska"], ["da", "Dansk"], ["nb", "Norsk Bokmål"],
    ["fi", "Suomi"], ["pl", "Polski"], ["cs", "Čeština"], ["hu", "Magyar"], ["ro", "Română"],
    ["el", "Ελληνικά"], ["ru", "Русский"], ["uk", "Українська"], ["tr", "Türkçe"], ["zh", "中文"],
    ["ja", "日本語"], ["ko", "한국어"], ["hi", "हिन्दी"], ["vi", "Tiếng Việt"], ["id", "Bahasa Indonesia"],
  ];
  function languageControl() {
    var sel = document.createElement("select"); sel.className = "zemacs-lang-select";
    var cur = (typeof window.savedLocale === "function" && window.savedLocale()) ||
              (typeof window.detectLocale === "function" && window.detectLocale()) || "en";
    LOCALES.forEach(function (l) {
      var o = document.createElement("option"); o.value = l[0]; o.textContent = l[1];
      if (l[0] === cur) o.selected = true;
      sel.appendChild(o);
    });
    // Switch locale live: loadLocale persists the choice, then re-render the whole UI in place.
    sel.addEventListener("change", function () {
      if (typeof window.loadLocale !== "function") return;
      window.loadLocale(sel.value).then(function () {
        if (typeof window.zemacsRetranslate === "function") window.zemacsRetranslate();
      }, function () {});
    });
    return sel;
  }
  // Called by the appShell Settings panel (main.js passes this as settingsExtra). Adds an editor
  // section with the language picker + the translucency / hidden-files toggles.
  function settingsExtra(b) {
    var sec = document.createElement("div"); sec.className = "zg-shell-section";
    sec.textContent = T("zemacs.settings.section", "Editor");
    b.appendChild(sec);
    b.appendChild(shellRow(T("zemacs.prefs.language", "Language"), languageControl()));
    b.appendChild(shellRow(T("zemacs.prefs.translucent", "Translucent background"),
      toggleControl(document.body.classList.contains("zemacs-translucent"),
        function (on) { if (on) act.blurOn(); else act.blurOff(); })));
    b.appendChild(shellRow(T("zemacs.prefs.show_hidden", "Show hidden files in Open dialog"),
      toggleControl(prefs.showHidden, function (on) { prefs.showHidden = on; })));
  }

  // ── Toolbar (ZGui.buttonBar) ────────────────────────────────────────────────────────────────────
  function toolbar(host) {
    if (!window.ZGui || !ZGui.buttonBar) return;
    var bar = ZGui.buttonBar(host, { className: "zemacs-toolbar" });
    bar.add("⊕", T("zemacs.tb.new", "New buffer"), act.newBuffer);
    bar.add("\u{1F4C2}", T("zemacs.file.open", "Open…"), act.open);
    bar.add("\u{1F4BE}", T("zemacs.file.save", "Save"), act.save);
    bar.sep();
    bar.add("◀", T("zemacs.buffers.prev", "Previous Buffer"), act.prevBuffer);
    bar.add("▶", T("zemacs.buffers.next", "Next Buffer"), act.nextBuffer);
    bar.sep();
    bar.add("\u{1F50D}", T("zemacs.edit.find", "Find"), act.find);
    bar.add("▃", T("zemacs.tb.split_h", "Split horizontally"), act.hsplit);
    bar.add("⛶", T("zemacs.view.fullscreen", "Toggle Full Screen"), act.fullscreen);
  }

  // ── Editor right-click context menu (ZGui.contextMenu) ───────────────────────────────────────────
  function editorContextMenu() {
    if (!window.ZGui || !ZGui.contextMenu || !ZGui.contextMenu.bind) return;
    var pane = document.getElementById("terminalPane");
    if (!pane) return;
    ZGui.contextMenu.bind(pane, function () {
      return [
        { label: T("zemacs.edit.copy", "Copy"), action: act.copy },
        { label: T("zemacs.edit.paste", "Paste"), action: act.paste },
        "---",
        { label: T("zemacs.file.open", "Open…"), action: act.open },
        { label: T("zemacs.file.save", "Save"), action: act.save },
        "---",
        { label: T("zemacs.edit.find", "Find"), action: act.find },
      ];
    });
  }

  // ── drag-and-drop files → open as buffers (ZGui.fileDrag) ────────────────────────────────────────
  function initDrop() {
    if (window.ZGui && ZGui.fileDrag && ZGui.fileDrag.initDrop) {
      ZGui.fileDrag.initDrop({
        overlayText: T("zemacs.drop_overlay", "Drop to open in zemacs"),
        onDrop: function (paths) { (paths || []).forEach(function (p) { ex("open " + q(p)); }); },
      });
    }
  }

  // Build (or rebuild) the menubar into `bar` from the current locale, with the stopPropagation fix:
  // menubar opens the dropdown on a left click; that same click otherwise bubbles to ZGui.contextMenu's
  // document-level "click outside closes" handler and shuts it instantly. Registered after menubar's own
  // handler (same button, bubble phase), so the dropdown is already open when we stop the click.
  function buildMenubar(bar) {
    bar.innerHTML = "";
    ZGui.menubar(bar, menus());
    bar.querySelectorAll(".zg-menubar-item").forEach(function (btn) {
      btn.addEventListener("click", function (e) { e.stopPropagation(); });
    });
  }

  // ── mount ───────────────────────────────────────────────────────────────────────────────────────
  var _bar = null, _tb = null, _shell = null;

  function mount(shell) {
    if (!window.ZGui || !ZGui.menubar) return;
    _shell = shell;
    _bar = document.createElement("div");
    _bar.id = "zemacsMenubar";
    // Insert between the appShell bar and the terminal body so the menu sits at the top of the window.
    if (shell && shell.body && shell.body.parentNode) shell.body.parentNode.insertBefore(_bar, shell.body);
    else document.body.insertBefore(_bar, document.body.firstChild);
    buildMenubar(_bar);
    // Toolbar (icon buttons) directly under the menu bar.
    _tb = document.createElement("div");
    _tb.id = "zemacsToolbar";
    if (shell && shell.body && shell.body.parentNode) shell.body.parentNode.insertBefore(_tb, shell.body);
    toolbar(_tb);

    // Populate ⌘K with every menu action (the appShell seed had only Restart/Focus).
    if (shell && typeof shell.setPaletteItems === "function") shell.setPaletteItems(paletteItems());
    window.addEventListener("keydown", onKey, true);
    editorContextMenu();
    initDrop();
    initOpenIntake();
  }

  // Re-render the locale-dependent chrome in place (called after the i18n catalog loads). Dialogs and
  // the editor context menu read T() on open, so they need no retranslation.
  function retranslate() {
    if (_bar) buildMenubar(_bar);
    if (_tb) { _tb.innerHTML = ""; toolbar(_tb); }
    if (_shell && typeof _shell.setPaletteItems === "function") _shell.setPaletteItems(paletteItems());
  }

  window.ZemacsMenu = { mount: mount, retranslate: retranslate, settingsExtra: settingsExtra, actions: act };
})();
