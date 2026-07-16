// fb-backend.js — wires the shared zpwr-file-browser front end (file-browser.js) into zmax-gui.
// Classic script, loaded BEFORE file-browser.js so the host globals it reads (window.zfbHost +
// escapeHtml/showToast/prefs/fzfMatch/shortcutTip) exist on init.
//
// zfbHost maps each method file-browser.js calls to the matching Tauri command. The fs
// method→command→arg lines are ported verbatim from the sibling apps (zemail/ztranslator), which took
// them from Audio-Haxor frontend/js/ipc.js; only the fs_* commands zmax-gui's src-tauri registers
// (35 of them, from zpwr_file_browser::commands) are real. Because zmax-gui IS an editor, "open a
// file" routes into the zmax buffer (window.zmaxOpenPath, panels.js) instead of the OS opener.
(function () {
    'use strict';
    const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
    const reject = (name) => Promise.reject(new Error(`${name}: not available in zmax-gui`));

    window.zfbHost = {
        // ── Registered fs_* commands (verbatim arg mapping from the sibling apps) ──
        listDirectory: (dirPath, includeHidden) => invoke('fs_list_dir', {dirPath, includeHidden: !!includeHidden}),
        fsListSubdirs: (dirPath, includeHidden) =>
            invoke('fs_list_subdirs', {dirPath, includeHidden: !!includeHidden}),
        fsFolderSize: (folderPath, timeoutMs) => invoke('fs_folder_size', {folderPath, timeoutMs}),
        fsSecureDelete: (filePath) => invoke('fs_secure_delete', {filePath}),
        fsCreateDir: (dirPath) => invoke('fs_create_dir', {dirPath}),
        fsCreateFile: (filePath) => invoke('fs_create_file', {filePath}),
        fsDuplicate: (path) => invoke('fs_duplicate', {path}),
        fsCompress: (paths, archivePath) => invoke('fs_compress', {paths, archivePath}),
        fsGetInfo: (path) => invoke('fs_get_info', {path}),
        fsMakeAlias: (path) => invoke('fs_make_alias', {path}),
        fsCopyPath: (src, dest) => invoke('fs_copy_path', {src, dest}),
        fsExtract: (archivePath, destDir) => invoke('fs_extract', {archivePath, destDir}),
        fsRunProgram: (filePath) => invoke('fs_run_program', {filePath}),
        fsHash: (path, algos) => invoke('fs_hash', {path, algos: algos || null}),
        fsChmod: (path, modeOctal) => invoke('fs_chmod', {path, modeOctal}),
        fsGrep: (root, needle, caseInsensitive, maxResults) =>
            invoke('fs_grep', {root, needle, caseInsensitive: !!caseInsensitive, maxResults: maxResults || null}),
        fsGitStatus: (dirPath) => invoke('fs_git_status', {dirPath}),
        fsXattrs: (path) => invoke('fs_xattrs', {path}),
        fsOpenInEditor: (filePath, editorOverride) =>
            invoke('fs_open_in_editor', {filePath, editorOverride: editorOverride || null}),
        fsFindDuplicates: (dir, recursive, minSizeBytes) =>
            invoke('fs_find_duplicates', {dir, recursive: !!recursive, minSizeBytes: minSizeBytes || null}),
        fsDiff: (pathA, pathB) => invoke('fs_diff', {pathA, pathB}),
        fsTouch: (filePath) => invoke('fs_touch', {filePath}),
        fsCompareDirs: (dirA, dirB) => invoke('fs_compare_dirs', {dirA, dirB}),
        fsReadHeadBytes: (filePath, maxBytes) =>
            invoke('fs_read_head_bytes', {filePath, maxBytes: maxBytes || null}),
        fsSymlinkRetarget: (path, newTarget) =>
            invoke('fs_symlink_retarget', {path, newTarget}),
        fsDiskUsage: (path) => invoke('fs_disk_usage', {path}),
        fsOpenTerminal: (folderPath) => invoke('fs_open_terminal', {folderPath}),
        fsReadFileBase64: (filePath, maxBytes) => invoke('fs_read_file_base64', {filePath, maxBytes}),
        fsReadHead: (filePath, maxBytes) => invoke('fs_read_head', {filePath, maxBytes}),
        fsReadFileBytes: (filePath, maxBytes) => invoke('fs_read_file_bytes', {filePath, maxBytes}),

        // ── "Open a file" in an editor host = load it into the zmax buffer. Drive the editor via
        //    panels.js's PTY bridge (window.zmaxOpenPath); fall back to the OS opener if unavailable. ──
        openFileDefault: (path) => {
            if (typeof window.zmaxOpenPath === 'function') { window.zmaxOpenPath(path); return Promise.resolve(); }
            const op = window.__TAURI__ && window.__TAURI__.opener;
            if (op && typeof op.openPath === 'function') return op.openPath(path);
            return reject('openFileDefault');
        },

        // ── Registered (crate ports delete_file/move_to_trash/rename_file) ──
        deleteFile: (filePath) => invoke('delete_file', {filePath}),
        moveToTrash: (filePath) => invoke('move_to_trash', {filePath}),
        renameFile: (oldPath, newPath) => invoke('rename_file', {oldPath, newPath}),

        // ── Home dir: zmax-gui already registers `home_dir` (fs_ops.rs) for the Open dialog. ──
        getHomeDir: () => invoke('home_dir'),

        // ── Directory watcher (registered; the lib live-updates on file-browser-change) ──
        fbWatcherSet: (dirPath) => invoke('fb_watcher_set', {dir: dirPath}),
    };

    // ── Host-util shims required by file-browser.js. escapeHtml/prefs come from zgui-core (fzf.js);
    //    appFmt/t/toastFmt ship from i18n.js — do NOT redefine any of those. Only fill genuine gaps. ──

    // escapeHtml: fallback only if zgui-core hasn't provided one.
    if (typeof window.escapeHtml !== 'function') {
        const _escDiv = document.createElement('div');
        window.escapeHtml = function escapeHtml(str) {
            _escDiv.textContent = str == null ? '' : String(str);
            return _escDiv.innerHTML;
        };
    }

    // showToast: prefer the shared zgui-core toast; else a minimal self-dismissing toast; else console.
    if (typeof window.showToast !== 'function') {
        window.showToast = function showToast(message, duration = 2500, type = '') {
            if (window.ZGui && window.ZGui.toast && typeof window.ZGui.toast.show === 'function') {
                window.ZGui.toast.show(message, Math.max(800, duration | 0), type === 'error' ? 'error' : '');
                return;
            }
            let host = document.getElementById('fbToastHost');
            if (!host) {
                host = document.createElement('div');
                host.id = 'fbToastHost';
                host.style.cssText = 'position:fixed;bottom:16px;left:50%;transform:translateX(-50%);z-index:2147483647;display:flex;flex-direction:column;gap:6px;align-items:center;pointer-events:none';
                document.body.appendChild(host);
            }
            const el = document.createElement('div');
            el.textContent = message;
            el.style.cssText = `padding:8px 14px;border-radius:6px;font:13px/1.3 'Share Tech Mono',monospace;color:#e0f0ff;background:${type === 'error' ? 'rgba(180,40,40,.95)' : 'rgba(20,30,50,.95)'};border:1px solid rgba(120,180,255,.4);max-width:60vw;box-shadow:0 4px 12px rgba(0,0,0,.5)`;
            host.appendChild(el);
            setTimeout(() => el.remove(), Math.max(800, duration | 0));
            (type === 'error' ? console.error : console.log)('[fb]', message);
        };
    }

    // prefs: localStorage-backed store matching the {getItem,setItem,getObject,removeItem} contract.
    // Installed only if zgui-core hasn't already provided window.prefs.
    if (!window.prefs || typeof window.prefs.getItem !== 'function') {
        window.prefs = {
            getItem(key) { try { return localStorage.getItem(key); } catch (_) { return null; } },
            setItem(key, value) { try { localStorage.setItem(key, typeof value === 'string' ? value : JSON.stringify(value)); } catch (_) { /* ignore */ } },
            removeItem(key) { try { localStorage.removeItem(key); } catch (_) { /* ignore */ } },
            getObject(key, fallback) {
                try { const v = localStorage.getItem(key); return v == null ? fallback : JSON.parse(v); }
                catch (_) { return fallback; }
            },
        };
    }

    // fzfMatch: simple subsequence matcher returning {score, indices} (the shape file-browser.js's
    // fuzzy filter highlight expects). Installed only if the host hasn't already provided one.
    if (typeof window.fzfMatch !== 'function') {
        window.fzfMatch = function fzfMatch(needle, haystack) {
            const n = String(needle || ''), h = String(haystack || '');
            if (n.length === 0) return {score: 0, indices: []};
            if (n.length > h.length) return null;
            const nl = n.toLowerCase(), hl = h.toLowerCase();
            const indices = [];
            let hi = 0;
            for (let ni = 0; ni < nl.length; ni++) {
                const found = hl.indexOf(nl[ni], hi);
                if (found < 0) return null;
                indices.push(found);
                hi = found + 1;
            }
            const span = indices[indices.length - 1] - indices[0];
            return {score: -span, indices};
        };
    }

    // shortcutTip: zmax-gui has no per-id tip lookup matching file-browser.js's contract; return {}
    // (file-browser.js treats {} as "no tip").
    if (typeof window.shortcutTip !== 'function') {
        window.shortcutTip = function shortcutTip() { return {}; };
    }
})();
