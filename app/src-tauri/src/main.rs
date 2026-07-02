// zemacs-gui — the thin Tauri host for the zemacs editor (Helix fork). The editor runs in an embedded
// PTY terminal (zpwr-embed-terminal crate); this binary registers the terminal commands, the
// MacVim-style GUI helpers (fs/window/open-intake), and wires the PTY's output/exit to the webview.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod editor_tools;
mod fs_ops;
mod open_intake;
mod project;
mod sidecar;
mod terminal;
mod window_ops;

fn main() {
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
        .manage(open_intake::OpenQueue::default())
        .invoke_handler(tauri::generate_handler![
            terminal::terminal_spawn,
            terminal::terminal_write,
            terminal::terminal_resize,
            terminal::terminal_kill,
            fs_ops::list_dir,
            fs_ops::home_dir,
            window_ops::toggle_fullscreen,
            window_ops::set_blur,
            window_ops::focus_window,
            open_intake::take_pending_opens,
            sidecar::zemacs_exec_command,
            sidecar::stryke_bin_path,
            // Project workbench (panels.js): quick-open, find-in-files, tree file ops, recent files,
            // file stats, git.
            project::find_files,
            project::search_project,
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
            editor_tools::project_symbols,
            editor_tools::scan_markers,
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
                        .open(d.join("zemacs.log"));
                }
            }

            // Cold launch with file args (`zemacs-gui file…`) — queue them for the frontend to drain.
            let handle = app.handle().clone();
            open_intake::ingest(
                &handle,
                open_intake::paths_from_argv(&std::env::args().collect::<Vec<_>>()),
            );

            // mvim:// / zemacs:// URLs delivered while running.
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let h = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    let urls = event.urls().iter().map(|u| u.to_string()).collect();
                    open_intake::ingest(&h, urls);
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building zemacs-gui")
        .run(|app, event| {
            // Finder double-click / `open file` arrives as a macOS Apple-event, not argv.
            if let tauri::RunEvent::Opened { urls } = event {
                open_intake::ingest(app, urls.iter().map(|u| u.to_string()).collect());
            }
        });
}
