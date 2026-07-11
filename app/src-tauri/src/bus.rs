//! GUI Automation Bus wiring for zemacs-gui (see GUI_AUTOMATION_BUS.md). Opens the per-app
//! `zgui-bridge` Unix socket so a stryke script can drive the whole app by name —
//! `App::open("zemacs-gui")->call(...)`, or `App::here()` from a hook running inside the app.
//!
//! zemacs-gui has no `-core` engine of its own, so EVERY verb/state/`verbs()` query is forwarded to
//! the webview's `ZGui.automation` surface (the appShell actions + the zemacs menu flattened into ⌘K)
//! via the automation-host.js dispatcher, which runs the registered verb and reports back through
//! `zgui_bridge_reply`. So the entire appShell surface — not an engine — is what a script sees.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use zgui_bridge::{serve, Bridge, Handler};

static BRIDGE: OnceLock<Arc<Bridge>> = OnceLock::new();

/// Per-request reply channels for webview-forwarded calls, keyed by request id.
type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

struct ZemacsBus {
    app: AppHandle,
    pending: Pending,
    next_id: AtomicU64,
}

impl ZemacsBus {
    /// Forward one request to the webview's `ZGui.automation` (via automation-host.js) and block the
    /// socket thread until `zgui_bridge_reply` fulfills it (or a timeout). `kind` is "call"|"get"|"verbs".
    fn forward(&self, kind: &str, name: &str, args: Value) -> Result<Value, String> {
        let win = self
            .app
            .get_webview_window("main")
            .ok_or_else(|| "no main webview".to_string())?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let js = format!(
            "window.__zguiBridgeDispatch&&window.__zguiBridgeDispatch({id},{kind},{name},{args})",
            kind = serde_json::to_string(kind).unwrap_or_else(|_| "\"call\"".into()),
            name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into()),
            args = args,
        );
        if let Err(e) = win.eval(&js) {
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("eval failed: {e}"));
        }
        let out = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "webview did not reply".to_string());
        self.pending.lock().unwrap().remove(&id);
        out?
    }
}

impl Handler for ZemacsBus {
    fn call(&self, verb: &str, args: Value) -> Result<Value, String> {
        self.forward("call", verb, args)
    }

    fn get(&self, state: &str) -> Result<Value, String> {
        self.forward("get", state, json!({}))
    }

    /// The whole surface: whatever the webview registered in `ZGui.automation` (appShell actions +
    /// the zemacs menu). Best-effort — an empty webview reply yields an empty (but valid) manifest.
    fn surface(&self) -> Value {
        let mut verbs: Vec<Value> = Vec::new();
        let mut state: Vec<Value> = Vec::new();
        let mut events: Vec<Value> = Vec::new();
        if let Ok(web) = self.forward("verbs", "", json!({})) {
            if let Some(v) = web.get("verbs").and_then(|x| x.as_array()) {
                verbs.extend(v.iter().cloned());
            }
            if let Some(s) = web.get("state").and_then(|x| x.as_array()) {
                state.extend(s.iter().cloned());
            }
            if let Some(e) = web.get("events").and_then(|x| x.as_array()) {
                events.extend(e.iter().cloned());
            }
        }
        json!({ "app": "zemacs-gui", "verbs": verbs, "state": state, "events": events })
    }
}

/// The webview calls this (from automation-host.js) to report a forwarded request's result.
#[tauri::command]
pub fn zgui_bridge_reply(
    id: u64,
    ok: bool,
    value: Option<Value>,
    error: Option<String>,
    pending: tauri::State<'_, Pending>,
) {
    if let Some(tx) = pending.lock().unwrap().remove(&id) {
        let _ = tx.send(if ok {
            Ok(value.unwrap_or(Value::Null))
        } else {
            Err(error.unwrap_or_else(|| "webview verb failed".into()))
        });
    }
}

/// The webview calls this to push an emitted automation event; we forward it to bus subscribers.
#[tauri::command]
pub fn zgui_bridge_event(event: String, payload: Value) {
    if let Some(b) = BRIDGE.get() {
        b.emit(&event, payload);
    }
}

/// Open the GUI-scripts directory (`<config>/zgui/scripts`) in the OS file manager.
#[tauri::command]
pub fn zgui_reveal_scripts(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = app
        .path()
        .config_dir()
        .map_err(|e| e.to_string())?
        .join("zgui")
        .join("scripts");
    let _ = std::fs::create_dir_all(&dir);
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// The shared pending-map, created once so both `start` and the `zgui_bridge_reply` command state use it.
pub fn pending_state() -> Pending {
    static P: OnceLock<Pending> = OnceLock::new();
    P.get_or_init(|| Arc::new(Mutex::new(HashMap::new()))).clone()
}

/// Open zemacs-gui's bus socket. Called once from `setup()`.
pub fn start(app: &AppHandle) {
    let handler = ZemacsBus {
        app: app.clone(),
        pending: pending_state(),
        next_id: AtomicU64::new(1),
    };
    match serve("zemacs-gui", handler) {
        Ok(bridge) => {
            let _ = BRIDGE.set(bridge);
        }
        Err(e) => eprintln!("zemacs-gui: could not open automation-bus socket: {e}"),
    }
}
