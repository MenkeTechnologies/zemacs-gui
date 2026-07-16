//! Bridge to the stryke language server (`stryke --lsp`) for the Hooks code editor.
//!
//! A single long-lived `stryke --lsp` child speaks LSP JSON-RPC over stdio with `Content-Length`
//! framing. The frontend's in-editor LSP adapter (Monaco, window.HooksEditor) exchanges **unframed**
//! JSON strings, so this module adds framing on the way to the server and strips it on the way back:
//!
//! - `stryke_lsp_send` frames a JSON string and writes it to the child's stdin.
//! - a reader thread parses framed messages from stdout and emits each raw JSON payload as a
//!   `stryke-lsp-rx` event; the webview feeds those to `HooksEditor.receive(msg)`.
//!
//! Ported from Audio-Haxor's stryke_lsp.rs. Uses the `stryke` binary on PATH (swap for a bundled
//! Tauri sidecar when packaging).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};

/// Managed state: the running language server and its stdin handle.
#[derive(Default)]
pub struct StrykeLspState {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
}

fn kill_inner(state: &StrykeLspState) {
    *state.stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Some(mut c) = state.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Resolve the `stryke` binary. A GUI-launched app inherits only a minimal PATH (no shell rc), so
/// `Command::new("stryke")` alone can fail even when it's installed. Prefer an explicit path in the
/// usual install dirs; fall back to `stryke` on PATH (which works under `tauri dev`).
fn resolve_stryke() -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        for rel in [".cargo/bin/stryke", ".local/bin/stryke"] {
            let p = home.join(rel);
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    for p in ["/opt/homebrew/bin/stryke", "/usr/local/bin/stryke", "/usr/bin/stryke"] {
        if std::path::Path::new(p).exists() {
            return p.to_string();
        }
    }
    "stryke".to_string()
}

/// Start (or restart) the stryke language server.
#[tauri::command]
pub async fn stryke_lsp_start(
    app: AppHandle,
    state: State<'_, StrykeLspState>,
) -> Result<(), String> {
    kill_inner(&state);

    let mut child = Command::new(resolve_stryke())
        .arg("--lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn stryke --lsp: {e} (install the stryke binary / sidecar)"))?;

    let stdin = child.stdin.take().ok_or("no stdin on stryke --lsp")?;
    let stdout = child.stdout.take().ok_or("no stdout on stryke --lsp")?;

    *state.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(stdin);
    *state.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    // Reader thread: parse Content-Length frames → emit raw JSON payloads.
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("stryke-lsp-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut content_length: usize = 0;
                // Read headers until the blank line.
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            let _ = app2.emit("stryke-lsp-exit", ());
                            return;
                        }
                        Ok(_) => {}
                    }
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_length == 0 {
                    continue;
                }
                let mut buf = vec![0u8; content_length];
                if reader.read_exact(&mut buf).is_err() {
                    let _ = app2.emit("stryke-lsp-exit", ());
                    return;
                }
                let payload = String::from_utf8_lossy(&buf).into_owned();
                let _ = app2.emit("stryke-lsp-rx", payload);
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Send one unframed LSP JSON-RPC message to the server (adds Content-Length framing).
#[tauri::command]
pub async fn stryke_lsp_send(
    message: String,
    state: State<'_, StrykeLspState>,
) -> Result<(), String> {
    let mut guard = state.stdin.lock().unwrap_or_else(|e| e.into_inner());
    let stdin = guard.as_mut().ok_or("stryke language server not running")?;
    let header = format!("Content-Length: {}\r\n\r\n", message.len());
    stdin
        .write_all(header.as_bytes())
        .and_then(|_| stdin.write_all(message.as_bytes()))
        .and_then(|_| stdin.flush())
        .map_err(|e| format!("write to stryke --lsp: {e}"))
}

/// Stop the language server.
#[tauri::command]
pub async fn stryke_lsp_stop(state: State<'_, StrykeLspState>) -> Result<(), String> {
    kill_inner(&state);
    Ok(())
}

/// Run a hook's stryke script on a BACKGROUND thread (non-blocking — the UI shows a spinner while it
/// runs). Writes the script to a temp `.stk` and runs `stryke run <file>` with the event context as
/// JSON on stdin. Returns `{ ok, code, stdout, stderr }`. Kept off the engine mutex so a long hook
/// never blocks other commands.
#[tauri::command]
pub async fn run_stryke_hook(
    script: String,
    ctx: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let ctx_json = serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string());
    tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let mut path = std::env::temp_dir();
        path.push(format!("zmax-gui-hook-{}.stk", std::process::id()));
        std::fs::write(&path, &script).map_err(|e| format!("write temp hook: {e}"))?;
        let mut child = Command::new(resolve_stryke())
            .arg("run")
            .arg(&path)
            // Bus identity: lets `App::here()` in the hook resolve to this app's socket.
            .env("ZGUI_APP", "zmax-gui")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot launch stryke ({e}) — install the stryke binary / sidecar"))?;
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(ctx_json.as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&path);
        Ok(serde_json::json!({
            "ok": out.status.success(),
            "code": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Fork `bash -lc <cmd>` and return its exit status + captured output. Backs the shared zgui-core
/// custom-command wizard's "bash" step (ZGui.userCommands) — a desktop app runs shell directly.
#[tauri::command]
pub async fn run_bash(cmd: String) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let out = std::process::Command::new("bash")
            .arg("-lc")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| format!("cannot launch bash: {e}"))?;
        Ok(serde_json::json!({
            "ok": out.status.success(),
            "code": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}
