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

    // Exposed so the Preferences language picker can re-render the whole UI after switching locale.
    window.zemacsRetranslate = function () { retranslate(shell); };

    // show + spawn the PTY, then exec the editor over the shell once it's up
    if (typeof window.showTerminal === "function") window.showTerminal();
    startEditor();

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
