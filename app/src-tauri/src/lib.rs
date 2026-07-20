// zmax-gui — the thin Tauri host for the zmax editor (Helix fork). The editor runs in an embedded
// PTY terminal (zpwr-embed-terminal crate); this binary registers the terminal commands, the
// MacVim-style GUI helpers (fs/window/open-intake), and wires the PTY's output/exit to the webview.

mod bus;
mod doc_blame;
mod doc_search;
mod edit_ops;
mod editor_tools;
mod encoding_ops;
mod fs_ops;
mod git_ext;
mod git_more;
mod git_tools;
mod open_intake;
mod project;
mod sidecar;
mod stryke_lsp;
mod terminal;
mod text_tools;
mod window_ops;
mod workbench_ext;

// ---- shared powerline status bar backend (ZGui.powerline) ------------------
// Live system stats via the zwire-host lib linked in-process (no native-messaging
// worker) — the powerline polls the `sys_stats` command. Mirrors zphoto/zoffice.
struct SysMon {
    sys: sysinfo::System,
    nets: sysinfo::Networks,
    disks: sysinfo::Disks,
    last: std::time::Instant,
}
impl Default for SysMon {
    fn default() -> Self {
        SysMon {
            sys: sysinfo::System::new(),
            nets: sysinfo::Networks::new_with_refreshed_list(),
            disks: sysinfo::Disks::new_with_refreshed_list(),
            last: std::time::Instant::now(),
        }
    }
}
/// System stats for the powerline (real per-second deltas via persistent handles).
#[tauri::command]
fn sys_stats(state: tauri::State<'_, std::sync::Mutex<SysMon>>) -> serde_json::Value {
    let mut m = state.lock().unwrap();
    m.sys.refresh_cpu_usage();
    m.sys.refresh_memory();
    m.nets.refresh(true);
    m.disks.refresh(true);
    let dt = m.last.elapsed().as_secs_f64().max(0.1);
    m.last = std::time::Instant::now();
    zwire_host::sysmon::snapshot(dt, &m.nets, &m.disks, &m.sys)
}

// macOS: hold-to-repeat instead of the press-and-hold accent-character popup. The
// editor runs in an xterm textarea; without this, holding e.g. `j` in zmax pops
// é/è/ê… instead of repeating. Registered in the (non-persistent) registration
// domain, which AppKit reads live, so it takes effect this launch. Must run before
// the webview/AppKit text input is created.
#[cfg(target_os = "macos")]
fn disable_press_and_hold() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let key: *mut AnyObject =
            msg_send![class!(NSString), stringWithUTF8String: c"ApplePressAndHoldEnabled".as_ptr()];
        let val: *mut AnyObject = msg_send![class!(NSNumber), numberWithBool: false];
        let dict: *mut AnyObject =
            msg_send![class!(NSDictionary), dictionaryWithObject: val, forKey: key];
        let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let _: () = msg_send![defaults, registerDefaults: dict];
    }
}

pub fn run() {
    #[cfg(target_os = "macos")]
    disable_press_and_hold();
    tauri::Builder::default()
        // Single-instance MUST be the first plugin: a 2nd launch (e.g. `mvim file`) forwards its file
        // args into the already-running window instead of opening a second one.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            open_intake::ingest(app, open_intake::paths_from_argv(&argv));
        }))
        .plugin(tauri_plugin_deep_link::init())
        // Opens the log file / data dir in the OS default handler (appShell Diagnostics buttons).
        .plugin(tauri_plugin_opener::init())
        .manage(terminal::TerminalState::default())
        .manage(terminal::ShellTermState::default())
        .manage(terminal::SessionTermState::default())
        .manage(open_intake::OpenQueue::default())
        // Shared file-browser directory watcher state (zpwr-file-browser crate).
        .manage(zpwr_file_browser::commands::watcher_state())
        // Stryke language server bridge (Hooks editor LSP completion/hover/diagnostics).
        .manage(stryke_lsp::StrykeLspState::default())
        .manage(std::sync::Mutex::new(SysMon::default()))
        // GUI Automation Bus: per-request reply channels for webview-forwarded verbs (bus::forward).
        .manage(bus::pending_state())
        .invoke_handler(tauri::generate_handler![
            sys_stats,
            terminal::terminal_spawn,
            terminal::terminal_write,
            terminal::terminal_resize,
            terminal::terminal_kill,
            terminal::shell_term_spawn,
            terminal::shell_term_write,
            terminal::shell_term_resize,
            terminal::shell_term_kill,
            terminal::term_session_spawn,
            terminal::term_session_write,
            terminal::term_session_resize,
            terminal::term_session_kill,
            fs_ops::list_dir,
            fs_ops::home_dir,
            window_ops::toggle_fullscreen,
            window_ops::set_blur,
            window_ops::focus_window,
            open_intake::take_pending_opens,
            sidecar::zmax_exec_command,
            sidecar::stryke_bin_path,
            // Project workbench (panels.js): quick-open, find-in-files, tree file ops, recent files,
            // file stats, git.
            project::find_files,
            project::search_project,
            doc_search::search_documents,
            // Document blame (panels.js): git blame at document-address granularity (cell / page).
            doc_blame::doc_blame,
            project::create_path,
            project::rename_path,
            project::delete_path,
            project::copy_path,
            project::recent_add,
            project::recent_list,
            project::recent_clear,
            project::file_stats,
            project::git_branch,
            project::git_status,
            project::git_file_diff,
            // Editor tools (panels.js): bookmarks, project search & replace, go-to-symbol, markers.
            editor_tools::bookmark_add,
            editor_tools::bookmark_list,
            editor_tools::bookmark_remove,
            editor_tools::bookmark_clear,
            editor_tools::replace_project,
            doc_search::replace_documents,
            editor_tools::project_symbols,
            editor_tools::scan_markers,
            // Git tools (panels.js): per-line blame, per-file history + show-commit, stage / unstage /
            // discard, and a two-file compare.
            git_tools::git_blame,
            git_tools::git_log_file,
            git_tools::git_show,
            git_tools::git_stage,
            git_tools::git_unstage,
            git_tools::git_discard,
            git_tools::diff_files,
            // Workbench extras (panels.js): persisted snippets + a project code-stats report.
            workbench_ext::snippet_add,
            workbench_ext::snippet_list,
            workbench_ext::snippet_remove,
            workbench_ext::snippet_clear,
            workbench_ext::project_stats,
            // Git extras (panels.js): branch list / checkout / create + stash save / list / pop /
            // drop / show.
            git_ext::git_branches,
            git_ext::git_checkout_branch,
            git_ext::git_create_branch,
            git_ext::git_stash_list,
            git_ext::git_stash_save,
            git_ext::git_stash_pop,
            git_ext::git_stash_drop,
            git_ext::git_stash_show,
            // Text tools (panels.js): file cleanup/convert, sort lines, find definition, batch rename.
            text_tools::convert_file,
            text_tools::sort_file_lines,
            text_tools::find_definition,
            text_tools::batch_rename,
            // Edit ops (panels.js): align columns on a delimiter + language-aware comment toggle.
            edit_ops::align_columns,
            edit_ops::comment_toggle,
            // Encoding ops (panels.js): detect + transcode a file's character encoding.
            encoding_ops::detect_encoding,
            encoding_ops::convert_encoding,
            // Git more (panels.js): repo-wide log, show-commit, diff two revisions, commit graph.
            git_more::git_log_repo,
            git_more::git_show_commit,
            git_more::git_diff_revs,
            git_more::git_graph,
            // Shared multi-pane file browser (zpwr-file-browser crate, `tauri` feature) — the fs_*
            // commands its front end (fb-backend.js → file-browser.js) calls, plus the watcher.
            zpwr_file_browser::commands::fs_list_dir,
            zpwr_file_browser::commands::fs_list_subdirs,
            zpwr_file_browser::commands::fs_folder_size,
            zpwr_file_browser::commands::fs_get_info,
            zpwr_file_browser::commands::fs_make_alias,
            zpwr_file_browser::commands::fs_hash,
            zpwr_file_browser::commands::fs_chmod,
            zpwr_file_browser::commands::fs_grep,
            zpwr_file_browser::commands::fs_symlink_retarget,
            zpwr_file_browser::commands::fs_disk_usage,
            zpwr_file_browser::commands::fs_touch,
            zpwr_file_browser::commands::fs_compare_dirs,
            zpwr_file_browser::commands::fs_diff,
            zpwr_file_browser::commands::fs_find_duplicates,
            zpwr_file_browser::commands::fs_git_status,
            zpwr_file_browser::commands::fs_xattrs,
            zpwr_file_browser::commands::fs_compress,
            zpwr_file_browser::commands::fs_extract,
            zpwr_file_browser::commands::fs_secure_delete,
            zpwr_file_browser::commands::fs_duplicate,
            zpwr_file_browser::commands::fs_copy_path,
            zpwr_file_browser::commands::fs_create_dir,
            zpwr_file_browser::commands::fs_create_file,
            zpwr_file_browser::commands::delete_file,
            zpwr_file_browser::commands::move_to_trash,
            zpwr_file_browser::commands::rename_file,
            zpwr_file_browser::commands::fs_read_file_base64,
            zpwr_file_browser::commands::fs_write_file_base64,
            zpwr_file_browser::commands::fs_read_head,
            zpwr_file_browser::commands::fs_read_head_bytes,
            zpwr_file_browser::commands::fs_read_file_bytes,
            zpwr_file_browser::commands::fs_open_terminal,
            zpwr_file_browser::commands::fs_open_in_editor,
            zpwr_file_browser::commands::fs_run_program,
            zpwr_file_browser::commands::fb_watcher_set,
            // Stryke hooks-editor (zpwr-hooks-editor) — LSP bridge + hook runner. The embedded
            // #hooksOverlay (index.html) drives these: stryke_lsp_start/_send/_stop frame LSP
            // JSON-RPC to a `stryke --lsp` child; run_stryke_hook executes a hook's .stk script.
            stryke_lsp::stryke_lsp_start,
            stryke_lsp::stryke_lsp_send,
            stryke_lsp::stryke_lsp_stop,
            stryke_lsp::run_stryke_hook,
            stryke_lsp::run_bash,
            // GUI Automation Bus: webview forwards replies/events here; reveal opens the scripts dir.
            bus::zgui_bridge_reply,
            bus::zgui_bridge_event,
            bus::zgui_reveal_scripts,
            zgui_shell::tauri_prefs::zgui_write_scripts,
        ])
        .setup(|app| {
            // Ensure the app data + log dirs exist and seed the log file, so the appShell
            // Diagnostics buttons (open log / log dir / data dir) always have a target.
            {
                use tauri::Manager;
                if let Ok(d) = app.path().app_data_dir() {
                    let _ = std::fs::create_dir_all(&d);
                }
                if let Ok(d) = app.path().app_log_dir() {
                    let _ = std::fs::create_dir_all(&d);
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(d.join("zmax.log"));
                }
            }

            // Cold launch with file args (`zmax-gui file…`) — queue them for the frontend to drain.
            let handle = app.handle().clone();
            open_intake::ingest(
                &handle,
                open_intake::paths_from_argv(&std::env::args().collect::<Vec<_>>()),
            );

            // mvim:// / zmax:// URLs delivered while running.
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let h = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    let urls = event.urls().iter().map(|u| u.to_string()).collect();
                    open_intake::ingest(&h, urls);
                });
            }

            // Open the GUI Automation Bus socket so `App::open("zmax-gui")` / `App::here()->verbs()`
            // resolve to this app's scriptable appShell surface.
            bus::start(&app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building zmax-gui")
        .run(|app, event| {
            // Finder double-click / `open file` arrives as a macOS Apple-event, not argv.
            if let tauri::RunEvent::Opened { urls } = event {
                open_intake::ingest(app, urls.iter().map(|u| u.to_string()).collect());
            }
        });
}
