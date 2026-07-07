// zemacs-gui shell — mounts the ZGui.appShell baseline and runs the zemacs editor (Helix fork) in a
// fullscreen embedded terminal (shared zpwr-embed-terminal frontend). The PTY spawns the login shell;
// we then `exec zemacs` so the editor replaces it and fills the window. See GUI_APP_ARCHITECTURE.md.
(function () {
  "use strict";
  // Translate via the shared zpwr-i18n runtime (window.t), falling back to the English literal.
  function T(key, english) {
    var s = (typeof window.t === "function") ? window.t(key) : null;
    return (s && s !== key) ? s : english;
  }
  function boot() {
    if (!window.ZGui || typeof ZGui.appShell !== "function") return;
    var shell = ZGui.appShell(document.getElementById("app"), {
      brand: { glyph: "✎", title: "ZEMACS", subtitle: T("zemacs.shell.subtitle", "editor") },
      filterPlaceholder: T("zemacs.shell.filter", "Filter…"),
      palette: [
        { label: T("zemacs.shell.restart_editor", "Restart editor"), run: restart },
        { label: T("zemacs.shell.focus_editor", "Focus editor"), run: function () { var c = document.getElementById("terminalContainer"); if (c) { var ta = c.querySelector("textarea"); if (ta) ta.focus(); } } },
        // Embedded Stryke hooks editor (zpwr-hooks-editor) — opens the in-app #hooksOverlay defined
        // in index.html; window.openHooksEditor mounts the ZGui.hooks chooser + Monaco editor once.
        { label: T("zemacs.shell.hooks_editor", "Hooks editor"), run: function () { if (typeof window.openHooksEditor === "function") window.openHooksEditor(); } },
      ],
      // Extend the real Settings panel (⚙ / ⌘,) with the editor's language picker + toggles.
      settingsExtra: function (b) { if (window.ZemacsMenu && typeof window.ZemacsMenu.settingsExtra === "function") window.ZemacsMenu.settingsExtra(b); },
    });

    // A fullscreen terminal pane inside shell.body — provided so zpwr-embed-terminal uses it instead
    // of injecting its floating dock pane (its _ensureTerminalDom is a no-op when #terminalPane exists).
    var pane = document.createElement("div");
    pane.id = "terminalPane";
    pane.className = "terminal-pane zemacs-fill active";
    var container = document.createElement("div");
    container.id = "terminalContainer";
    container.className = "term-body";
    pane.appendChild(container);
    shell.body.appendChild(pane);

    // MacVim-style menu bar + Cmd-shortcuts + dialogs + drag-drop (all zgui widgets), bridged to the PTY
    if (window.ZemacsMenu && typeof window.ZemacsMenu.mount === "function") window.ZemacsMenu.mount(shell);

    // App-local project workbench: quick-open (⌘P), find-in-files (⇧⌘J), recent (⌘E), project files
    // (⇧⌘E) and a git panel — all in the ⌘K palette. Mounts after menu.js so its palette items append.
    if (window.ZemacsPanels && typeof window.ZemacsPanels.mount === "function") window.ZemacsPanels.mount(shell);

    // Exposed so the Preferences language picker can re-render the whole UI after switching locale.
    window.zemacsRetranslate = function () { retranslate(shell); };

    // show + spawn the PTY, then exec the editor over the shell once it's up
    if (typeof window.showTerminal === "function") window.showTerminal();
    startEditor();

    // ── Floating shell terminal (⌘K "Terminal") ──
    // #terminalPane is the always-on IDE. The appShell's "Terminal" command calls
    // window.toggleTerminalPopup — point it at a SEPARATE floating shell that pops up ON TOP of the IDE.
    // It's an independent PTY (shell_term_* commands / shell-term-output event), so it never disturbs the
    // editor's terminal_* PTY. Reuses the shared .terminal-pane chrome + the bundled xterm.
    (function () {
      var T = window.__TAURI__;
      if (!T || !T.core || !T.event || typeof window.Terminal !== "function") return;
      var pane = null, term = null, spawned = false, listening = false;
      function ensure() {
        if (pane) return;
        pane = document.createElement("div");
        pane.className = "terminal-pane zshell-float";
        pane.style.cssText = "top:auto;left:auto;right:24px;bottom:24px;";
        var head = document.createElement("div");
        head.className = "term-toolbar";
        head.innerHTML = '<span class="term-toolbar-title">shell</span>' +
          '<div class="term-toolbar-actions">' +
          '<button class="term-btn" data-a="hide" title="Hide">—</button>' +
          '<button class="term-btn term-btn-close" data-a="close" title="Close">✕</button></div>';
        var body = document.createElement("div");
        body.className = "term-body";
        pane.append(head, body);
        document.body.appendChild(pane);
        head.addEventListener("click", function (e) {
          var a = e.target && e.target.getAttribute && e.target.getAttribute("data-a");
          if (a === "hide") { pane.classList.remove("active"); }
          else if (a === "close") { try { T.core.invoke("shell_term_kill"); } catch (x) {} spawned = false; pane.classList.remove("active"); }
        });
        term = new window.Terminal({ fontFamily: "'Hack Nerd Font', Menlo, monospace", fontSize: 13, cursorBlink: true, theme: { background: "rgba(0,0,0,0)" } });
        term.open(body);
        term.onData(function (d) { try { T.core.invoke("shell_term_write", { data: d }); } catch (x) {} });
        if (!listening) { listening = true; T.event.listen("shell-term-output", function (ev) { if (term) term.write(ev.payload); }); }
      }
      window.toggleTerminalPopup = function () {
        ensure();
        if (pane.classList.contains("active")) { pane.classList.remove("active"); return; }
        pane.classList.add("active");
        if (!spawned) { spawned = true; T.core.invoke("shell_term_spawn", { rows: term.rows || 24, cols: term.cols || 80 }).catch(function () {}); }
        setTimeout(function () { try { term.focus(); } catch (x) {} }, 40);
      };

      // Both terminals are .terminal-pane (z-index 9998, fixed) and so render ABOVE the full-screen
      // hooks/file-browser overlays (z-index 9000). Keep them off the overlays:
      //   • the floating shell (.zshell-float) — drop its .active when an overlay opens (manual re-show).
      //   • the always-on IDE (#terminalPane) — hide via .ze-overlay-hidden while ANY overlay is open,
      //     then restore when both close (it's the editor, so it must come back). visibility:hidden keeps
      //     layout so xterm needs no reflow on restore.
      function overlayOpen() {
        return ["hooksOverlay", "fbOverlay"].some(function (id) { var o = document.getElementById(id); return o && !o.hidden; });
      }
      function syncOverlays() {
        var open = overlayOpen();
        if (open && pane && pane.classList.contains("active")) pane.classList.remove("active");
        var ide = document.getElementById("terminalPane");
        if (ide) ide.classList.toggle("ze-overlay-hidden", open);
      }
      if (typeof MutationObserver === "function") {
        ["hooksOverlay", "fbOverlay"].forEach(function (id) {
          var ov = document.getElementById(id);
          if (!ov) return;
          new MutationObserver(syncOverlays).observe(ov, { attributes: true, attributeFilter: ["hidden"] });
        });
      }
    })();

    // i18n: the UI above was built synchronously (English fallbacks) to preserve the #terminalPane
    // creation timing; the locale catalog loads async, so re-translate the menu/toolbar/palette/shell
    // strings in place once it's ready.
    if (typeof window.loadLocale === "function") {
      var loc = (typeof window.savedLocale === "function" && window.savedLocale()) ||
                (typeof window.detectLocale === "function" && window.detectLocale()) || "en";
      window.loadLocale(loc).then(function () { retranslate(shell); }, function () {});
    }
  }

  function retranslate(shell) {
    if (window.ZemacsMenu && typeof window.ZemacsMenu.retranslate === "function") window.ZemacsMenu.retranslate();
    if (shell && shell.filterInput) shell.filterInput.placeholder = T("zemacs.shell.filter", "Filter…");
    var sub = document.querySelector(".zg-shell-sub");
    if (sub) sub.textContent = T("zemacs.shell.subtitle", "editor");
  }

  function tauri() { return window.__TAURI__ && window.__TAURI__.core; }
  function startEditor() {
    var T = tauri();
    if (!T) return; // in-browser preview: no PTY backend
    // give the login shell a moment to come up, then replace it with the BUNDLED editor (sidecar path,
    // with the sidecar dir prepended to PATH so the bundled stryke is reachable too) — never a bare
    // `zemacs` off the user's PATH, so the shipped .app is self-contained.
    // `--ide` so the GUI boots straight into the workbench (toolbar + tool windows visible),
    // since the windowed app IS the IDE. (F2 still toggles it.)
    setTimeout(function () {
      T.invoke("zemacs_exec_command").then(function (cmd) {
        T.invoke("terminal_write", { data: "exec " + (cmd || "zemacs") + " --ide\n" }).catch(function () {});
      }).catch(function () {
        T.invoke("terminal_write", { data: "exec zemacs --ide\n" }).catch(function () {});
      });
      // once the editor is up, sync its theme to the saved zgui-core colorscheme (unified palette)
      setTimeout(function () { if (typeof window.zemacsSyncTheme === "function") window.zemacsSyncTheme(); }, 2500);
    }, 800);
  }
  function restart() {
    var T = tauri();
    if (!T) return;
    T.invoke("terminal_kill").then(function () {
      if (typeof window.showTerminal === "function") window.showTerminal();
      startEditor();
    }).catch(function () {});
  }

  // Run immediately (scripts are at body end, so #app + the terminal globals already exist) — this
  // creates #terminalPane before terminal.js's DOMContentLoaded wire, so it adopts ours. i18n is
  // applied afterward (see boot) without disturbing this timing.
  boot();
})();
