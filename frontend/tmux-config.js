// zemacs-gui — drive the shared ZGui.tmux (zgui-core) with an INDEPENDENT zemacs editor per pane.
// Unlike the DOM-view consumers (zemail/zphoto), the "view" here is a terminal: each pane gets its
// own xterm + its own backend PTY session (term_session_* commands), and execs the editor into it,
// exactly like main.js's fullscreen editor. Split the window (C-b %/") to run several editors side by
// side. App-local wiring; loaded after xterm.js + terminal.js + main.js + tmux.js.
(function () {
  "use strict";
  function ready(fn) { if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", fn); else fn(); }

  // Self-inject the per-pane host sizing (a <style> sheet, NOT an inline style — WKWebView release
  // strips inline styles). Idempotent.
  (function () {
    try {
      if (typeof document === "undefined" || document.getElementById("zemacs-tmux-css")) return;
      var s = document.createElement("style");
      s.id = "zemacs-tmux-css";
      s.textContent = ".zemacs-tmux-host { width: 100%; height: 100%; min-height: 0; overflow: auto; position: relative; }"
        // The primary editor terminal (#terminalPane .terminal-pane) is position:fixed
        // z-index:9998 — ABOVE the tmux overlay (#zg-tmux, z-index 8500) — so it covers
        // the tiled panes. Hide it whenever the overlay is open so the tmux-hosted
        // views are visible; it returns when tmux closes.
        + "\nbody:has(#zg-tmux.on) #terminalPane { display: none !important; }";
      (document.head || document.documentElement).appendChild(s);
    } catch (e) {}
  })();

  function core() { return (typeof window !== "undefined" && window.__TAURI__ && window.__TAURI__.core) || null; }
  function events() { return (typeof window !== "undefined" && window.__TAURI__ && window.__TAURI__.event) || null; }

  // Same xterm theme/font as terminal.js's editor terminal, so tiled editors match the fullscreen one.
  var TERM_OPTS = {
    cursorBlink: true, cursorStyle: "block", fontSize: 13,
    fontFamily: "'Hack Nerd Font', 'Hack Nerd Font Mono', 'Hack', 'Share Tech Mono', 'Menlo', monospace",
    theme: {
      background: "rgba(0, 0, 0, 0)", foreground: "#e0e0e0", cursor: "#00e5ff", cursorAccent: "#0a0a12",
      selectionBackground: "rgba(0,229,255,0.25)", black: "#1a1a2e", red: "#ff3860", green: "#23d160",
      yellow: "#ffdd57", blue: "#3273dc", magenta: "#b86bff", cyan: "#00e5ff", white: "#e0e0e0",
      brightBlack: "#4a4a6a", brightRed: "#ff6b8a", brightGreen: "#5dfc8a", brightYellow: "#ffe27a",
      brightBlue: "#5a9cff", brightMagenta: "#d19cff", brightCyan: "#4df0ff", brightWhite: "#ffffff",
    },
    allowProposedApi: true, allowTransparency: true, scrollback: 10000,
  };

  // WebKit release ignores setAttribute("style",…) on dynamically-created elements under
  // tauri://localhost; xterm's DOM renderer uses it for truecolor. Same monkey-patch terminal.js does.
  function patchAddStyle(term) {
    try {
      var c = term._core, rf = null;
      var cand = [c && c._renderService && c._renderService._rowFactory,
                  c && c._renderService && c._renderService._renderer && c._renderService._renderer._rowFactory];
      if (!cand.some(function (x) { return x; })) {
        var scan = function (o, d) {
          if (!o || d > 4 || rf) return;
          if (typeof o._addStyle === "function") { rf = o; return; }
          for (var k in o) { if (k.charAt(0) === "_" && o[k] && typeof o[k] === "object") scan(o[k], d + 1); }
        };
        scan(c, 0);
      } else { rf = cand.filter(function (x) { return x; })[0]; }
      if (rf && typeof rf._addStyle === "function") {
        rf._addStyle = function (el, styleStr) {
          var m = styleStr.match(/^color:(#[0-9a-fA-F]{3,8})/); if (m) { el.style.color = m[1]; return; }
          var b = styleStr.match(/^background-color:(#[0-9a-fA-F]{3,8})/); if (b) { el.style.backgroundColor = b[1]; return; }
          var p = styleStr.split(":");
          if (p.length === 2) { el.style[p[0].trim().replace(/-([a-z])/g, function (_, ch) { return ch.toUpperCase(); })] = p[1].trim().replace(/;$/, ""); }
        };
      }
    } catch (e) {}
  }

  // Compute rows/cols from the host size using xterm's measured cell metrics (mirrors terminal.js _termFit).
  function fit(term, host) {
    try {
      var dims = term._core && term._core._renderService && term._core._renderService.dimensions;
      if (!dims || !dims.css || !dims.css.cell || !dims.css.cell.width || !dims.css.cell.height) return { rows: term.rows, cols: term.cols };
      var cw = dims.css.cell.width, ch = dims.css.cell.height, aw = host.clientWidth, ah = host.clientHeight;
      if (aw <= 0 || ah <= 0) return { rows: term.rows, cols: term.cols };
      var cols = Math.max(2, Math.floor(aw / cw)), rows = Math.max(1, Math.floor(ah / ch));
      if (cols !== term.cols || rows !== term.rows) term.resize(cols, rows);
      return { rows: rows, cols: cols };
    } catch (e) { return { rows: term.rows, cols: term.cols }; }
  }

  // Live pane instances, pruned when their host leaves the DOM (ZGui.tmux's dropPane just removes the
  // pane wrap — there's no CFG close hook — so we watch for disconnection and kill the PTY ourselves).
  var live = [];
  var observer = null;
  function ensureObserver() {
    if (observer || typeof MutationObserver !== "function" || !document.body) return;
    observer = new MutationObserver(function () {
      for (var i = live.length - 1; i >= 0; i--) {
        if (!live[i].host.isConnected) { try { live[i].dispose(); } catch (e) {} }
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
  }

  var seq = 0;
  function mountInto(bodyEl) {
    bodyEl.textContent = "";
    var host = document.createElement("div");
    host.className = "zemacs-tmux-host";
    bodyEl.appendChild(host);

    var C = core(), E = events();
    if (typeof window.Terminal !== "function" || !C) {
      host.textContent = "terminal unavailable";
      return { id: ++seq };
    }

    var term = new window.Terminal(TERM_OPTS);
    term.open(host);
    patchAddStyle(term);
    requestAnimationFrame(function () { try { term.resize(term.cols, term.rows); term.refresh(0, term.rows - 1); } catch (e) {} });

    var sessionId = 0, alive = false, unlisten = null, disposed = false, ro = null;
    var inst = { host: host, dispose: dispose };
    live.push(inst);
    ensureObserver();

    var dims = fit(term, host);

    // Subscribe BEFORE spawn so nothing is lost; filter for OUR session id.
    Promise.resolve(E && E.listen("term-session-output", function (e) {
      if (!e || !e.payload || e.payload.id !== sessionId) return;
      if (disposed) return;
      if (String(e.payload.data).indexOf("\x1b[2J") >= 0) term.clear();
      term.write(e.payload.data);
    })).then(function (un) { unlisten = un; if (disposed && un) un(); });

    // Spawn the PTY, then exec the editor into it (mirrors main.js startEditor).
    C.invoke("term_session_spawn", { rows: dims.rows, cols: dims.cols }).then(function (id) {
      sessionId = id; alive = true;
      setTimeout(function () {
        if (disposed) return;
        C.invoke("zemacs_exec_command").then(function (cmd) {
          C.invoke("term_session_write", { id: sessionId, data: "exec " + (cmd || "zemacs") + " --ide\n" }).catch(function () {});
        }).catch(function () {
          C.invoke("term_session_write", { id: sessionId, data: "exec zemacs --ide\n" }).catch(function () {});
        });
      }, 800);
    }).catch(function (err) { try { term.write("\x1b[31mspawn failed: " + err + "\x1b[0m\r\n"); } catch (e) {} });

    // Keystrokes → PTY.
    term.onData(function (data) {
      if (disposed || !alive || !sessionId) return;
      C.invoke("term_session_write", { id: sessionId, data: data }).catch(function () {});
    });

    // Session died.
    if (E) Promise.resolve(E.listen("term-session-exit", function (e) {
      if (!e || e.payload !== sessionId) return;
      alive = false;
      if (!disposed) { try { term.write("\r\n\x1b[90m[editor exited]\x1b[0m\r\n"); } catch (x) {} }
    }));

    // Debounced fit → backend resize on host size change.
    var deb = null;
    if (typeof ResizeObserver === "function") {
      ro = new ResizeObserver(function () {
        clearTimeout(deb);
        deb = setTimeout(function () {
          if (disposed || !sessionId) return;
          var d = fit(term, host);
          C.invoke("term_session_resize", { id: sessionId, rows: d.rows, cols: d.cols }).catch(function () {});
        }, 50);
      });
      ro.observe(host);
    }

    setTimeout(function () { try { term.focus(); } catch (e) {} }, 0);

    function dispose() {
      if (disposed) return;
      disposed = true;
      var ix = live.indexOf(inst); if (ix >= 0) live.splice(ix, 1);
      if (ro) { try { ro.disconnect(); } catch (e) {} ro = null; }
      if (unlisten) { try { unlisten(); } catch (e) {} unlisten = null; }
      if (sessionId && C) C.invoke("term_session_kill", { id: sessionId }).catch(function () {});
      try { term.dispose(); } catch (e) {}
    }

    return { id: ++seq, dispose: dispose };
  }

  function boot() {
    if (!window.ZGui || !window.ZGui.tmux) return;
    window.ZGui.tmux.init({
      prefs: {
        load: function () { try { return JSON.parse(localStorage.getItem("zemacs.tmux") || "{}"); } catch (e) { return {}; } },
        save: function (o) { try { localStorage.setItem("zemacs.tmux", JSON.stringify(o)); } catch (e) {} }
      },
      // zemacs runs in an xterm textarea (an editable), so without this tmux.js would let every
      // C-b pass through to the editor (its detached-editable "Ctrl-B is Bold" guard). Opt in so
      // C-b is the tmux prefix globally — C-b c/%/" reach tmux even while the editor is focused.
      prefixInEditable: true,
      openEmptyPane: function (bodyEl) { return Promise.resolve(mountInto(bodyEl)); },
      renderPane: function (bodyEl, ref) { mountInto(bodyEl); },
      paneLabel: function (ref) { return "zemacs " + ((ref && ref.id) || ""); }
    });
  }
  ready(boot);
})();
