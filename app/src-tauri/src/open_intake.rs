//! File-open intake — the MacVim "open from Finder / `mvim` CLI / `mvim://` URL" surface. Every entry
//! point (initial argv, a second instance's argv, a Finder Apple-event, a deep-link URL) funnels file
//! paths here. Paths are both queued (drained by the frontend on boot via `take_pending_opens`, so a
//! cold launch with file args isn't lost before the webview is ready) and emitted live as an
//! `open-files` event (so an already-running window opens them immediately). The frontend then writes
//! `:open <path>` into the PTY — the GUI never edits files itself, it drives the editor.

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Pending file paths captured before the frontend was ready to receive them.
#[derive(Default)]
pub struct OpenQueue(pub Mutex<Vec<String>>);

/// Turn a `file://` / `mvim://` / `zmax://` URL or a bare path into a filesystem path.
fn url_to_path(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix("file://") {
        // Strip an optional host segment, keep the absolute path.
        let path = rest.splitn(2, '/').nth(1).map(|p| format!("/{p}")).unwrap_or_else(|| rest.to_string());
        return Some(percent_decode(&path));
    }
    // TextMate-style mvim://open?url=file:///path&line=N — pull the url= argument.
    for scheme in ["mvim://", "zmax://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            if let Some(q) = rest.split_once("url=") {
                let enc = q.1.split('&').next().unwrap_or("");
                return url_to_path(&percent_decode(enc));
            }
            // mvim://open/<path> fallback
            let tail = rest.trim_start_matches("open/").trim_start_matches('/');
            if !tail.is_empty() {
                return Some(percent_decode(tail));
            }
            return None;
        }
    }
    Some(s.to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract openable file paths from a process argv (skips flags and the program name).
pub fn paths_from_argv(argv: &[String]) -> Vec<String> {
    argv.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .filter_map(|a| url_to_path(a))
        .filter(|p| std::path::Path::new(p).exists())
        .collect()
}

/// Queue + broadcast a batch of paths (already filesystem paths or URLs).
pub fn ingest(app: &AppHandle, raw: Vec<String>) {
    let paths: Vec<String> = raw.iter().filter_map(|s| url_to_path(s)).collect();
    if paths.is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<OpenQueue>() {
        if let Ok(mut q) = state.0.lock() {
            q.extend(paths.iter().cloned());
        }
    }
    let _ = app.emit("open-files", paths);
    // Surface the window so a Finder/CLI open feels native.
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Drain the cold-launch queue — the frontend calls this once on boot.
#[tauri::command]
pub fn take_pending_opens(state: tauri::State<'_, OpenQueue>) -> Vec<String> {
    state.0.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_becomes_absolute_path() {
        assert_eq!(url_to_path("file:///Users/x/y.txt").as_deref(), Some("/Users/x/y.txt"));
    }

    #[test]
    fn textmate_style_mvim_url_extracts_inner_file() {
        assert_eq!(
            url_to_path("mvim://open?url=file:///a/b.txt&line=5").as_deref(),
            Some("/a/b.txt")
        );
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(url_to_path("file:///a%20b/c%2Bd.txt").as_deref(), Some("/a b/c+d.txt"));
    }

    #[test]
    fn bare_path_passes_through() {
        assert_eq!(url_to_path("/plain/path").as_deref(), Some("/plain/path"));
    }

    #[test]
    fn argv_keeps_only_existing_non_flag_files() {
        // /etc/hosts exists on macOS + Linux CI; the flag and the missing path are dropped.
        let argv = vec![
            "zmax-gui".to_string(),
            "-v".to_string(),
            "/etc/hosts".to_string(),
            "/no/such/file/zzz".to_string(),
        ];
        assert_eq!(paths_from_argv(&argv), vec!["/etc/hosts".to_string()]);
    }
}
