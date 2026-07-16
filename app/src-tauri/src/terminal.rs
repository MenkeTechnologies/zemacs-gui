//! Embedded PTY terminal — thin Tauri adapter over the shared `zpwr-embed-terminal` crate (same as
//! ztunnel/Audio-Haxor). The editor (zmax) runs inside this PTY: the frontend execs `zmax` once
//! the session is up. Forwards the session's `on_output`/`on_exit` callbacks to webview events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};
use zpwr_embed_terminal::TerminalSession;

/// Managed state for the embedded terminal.
#[derive(Default)]
pub struct TerminalState {
    session: TerminalSession,
}

/// Spawn a new PTY session (login shell). Kills any existing session first. The frontend then runs
/// `exec zmax` so the editor replaces the shell and fills the window.
#[tauri::command]
pub async fn terminal_spawn(
    rows: Option<u16>,
    cols: Option<u16>,
    app: AppHandle,
    state: State<'_, TerminalState>,
) -> Result<(), String> {
    let app_out = app.clone();
    let app_exit = app;
    state.session.spawn(
        rows.unwrap_or(40),
        cols.unwrap_or(120),
        move |text| {
            let _ = app_out.emit("terminal-output", text);
        },
        move || {
            let _ = app_exit.emit("terminal-exit", ());
        },
    )
}

/// Write raw bytes (user keystrokes) into the PTY.
#[tauri::command]
pub fn terminal_write(data: String, state: State<'_, TerminalState>) -> Result<(), String> {
    state.session.write(&data)
}

/// Notify the PTY of a viewport resize.
#[tauri::command]
pub fn terminal_resize(rows: u16, cols: u16, state: State<'_, TerminalState>) -> Result<(), String> {
    state.session.resize(rows, cols)
}

/// Kill the terminal session.
#[tauri::command]
pub fn terminal_kill(state: State<'_, TerminalState>) -> Result<(), String> {
    state.session.kill();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// Second, INDEPENDENT PTY: the floating shell terminal the user pops open on top of the IDE (⌘K
// "Terminal"). It runs a plain login shell — NOT `zmax --ide` — so it's a real scratch terminal,
// separate from the editor's PTY above. Its own state + `shell-term-output`/`shell-term-exit` events.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

/// Managed state for the floating shell terminal (independent of the IDE's [`TerminalState`]).
#[derive(Default)]
pub struct ShellTermState {
    session: TerminalSession,
}

/// Spawn (or respawn) the floating shell terminal's login shell.
#[tauri::command]
pub async fn shell_term_spawn(
    rows: Option<u16>,
    cols: Option<u16>,
    app: AppHandle,
    state: State<'_, ShellTermState>,
) -> Result<(), String> {
    let app_out = app.clone();
    let app_exit = app;
    state.session.spawn(
        rows.unwrap_or(24),
        cols.unwrap_or(80),
        move |text| {
            let _ = app_out.emit("shell-term-output", text);
        },
        move || {
            let _ = app_exit.emit("shell-term-exit", ());
        },
    )
}

/// Write raw bytes into the floating shell terminal's PTY.
#[tauri::command]
pub fn shell_term_write(data: String, state: State<'_, ShellTermState>) -> Result<(), String> {
    state.session.write(&data)
}

/// Notify the floating shell terminal's PTY of a viewport resize.
#[tauri::command]
pub fn shell_term_resize(rows: u16, cols: u16, state: State<'_, ShellTermState>) -> Result<(), String> {
    state.session.resize(rows, cols)
}

/// Kill the floating shell terminal session.
#[tauri::command]
pub fn shell_term_kill(state: State<'_, ShellTermState>) -> Result<(), String> {
    state.session.kill();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// tmux tiling: arbitrary-N INDEPENDENT PTY sessions, one per ZGui.tmux pane. Each pane runs its own
// zmax editor in its own PTY (the frontend `exec`s the editor into it, exactly like the singleton
// editor PTY above). Addressed by a u32 id so several editors tile side by side. Output is forwarded
// to the "term-session-output" event as a { id, data } payload (a pane's xterm filters for its own
// id); exit fires "term-session-exit" with the id. Independent of the singleton terminal_*/shell_term_*
// PTYs, which are left untouched.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

/// Managed state for the tmux per-pane PTY sessions.
#[derive(Default)]
pub struct SessionTermState {
    sessions: Mutex<HashMap<u32, TerminalSession>>,
    next_id: AtomicU32,
}

/// Payload for the per-session output event — tags each chunk with its session id so a pane's
/// xterm can ignore output belonging to the other panes' sessions.
#[derive(Clone, serde::Serialize)]
struct SessionOutput {
    id: u32,
    data: String,
}

/// Spawn a new per-pane PTY session (login shell); returns its id. Ids start at 1 (0 is never
/// handed out), so the frontend can treat 0 as "no session".
#[tauri::command]
pub async fn term_session_spawn(
    rows: Option<u16>,
    cols: Option<u16>,
    app: AppHandle,
    state: State<'_, SessionTermState>,
) -> Result<u32, String> {
    let id = state.next_id.fetch_add(1, Ordering::SeqCst) + 1;
    let session = TerminalSession::new();
    let app_out = app.clone();
    let app_exit = app;
    session.spawn(
        rows.unwrap_or(40),
        cols.unwrap_or(120),
        move |text| {
            let _ = app_out.emit(
                "term-session-output",
                SessionOutput { id, data: text.to_string() },
            );
        },
        move || {
            let _ = app_exit.emit("term-session-exit", id);
        },
    )?;
    state
        .sessions
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id, session);
    Ok(id)
}

/// Write raw bytes (user keystrokes) into pane session `id`'s PTY.
#[tauri::command]
pub fn term_session_write(
    id: u32,
    data: String,
    state: State<'_, SessionTermState>,
) -> Result<(), String> {
    let guard = state.sessions.lock().map_err(|e| e.to_string())?;
    match guard.get(&id) {
        Some(s) => s.write(&data),
        None => Err(format!("no terminal session {id}")),
    }
}

/// Notify pane session `id`'s PTY of a viewport resize.
#[tauri::command]
pub fn term_session_resize(
    id: u32,
    rows: u16,
    cols: u16,
    state: State<'_, SessionTermState>,
) -> Result<(), String> {
    let guard = state.sessions.lock().map_err(|e| e.to_string())?;
    match guard.get(&id) {
        Some(s) => s.resize(rows, cols),
        None => Err(format!("no terminal session {id}")),
    }
}

/// Kill pane session `id` and drop it from the map.
#[tauri::command]
pub fn term_session_kill(id: u32, state: State<'_, SessionTermState>) -> Result<(), String> {
    if let Some(s) = state.sessions.lock().map_err(|e| e.to_string())?.remove(&id) {
        s.kill();
    }
    Ok(())
}
