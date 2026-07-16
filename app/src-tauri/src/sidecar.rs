//! Resolve the bundled `zmax` / `stryke` sidecars so the app is self-contained — it must NOT depend
//! on either being on the user's PATH. Same resolution as traderview's `resolve_stryke_bin`: an env
//! override, then a sidecar beside the executable (Tauri places externalBin there, sometimes suffixed
//! with the target triple, e.g. `zmax-aarch64-apple-darwin`), then PATH.

use std::path::PathBuf;

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A sidecar in `dir`: exact `prefix`, then `prefix.exe`, then any `prefix-*` (triple-suffixed).
fn sidecar_in_dir(dir: &std::path::Path, prefix: &str) -> Option<PathBuf> {
    for name in [prefix.to_string(), format!("{prefix}.exe")] {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if fname.starts_with(&format!("{prefix}-")) && entry.path().is_file() {
            return Some(entry.path());
        }
    }
    None
}

/// A sidecar beside the current executable (where `tauri build` places externalBin).
fn sidecar_beside_exe(prefix: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    sidecar_in_dir(exe.parent()?, prefix)
}

/// The staging dir the prepare-*-sidecar scripts write to — found during `tauri dev`, where the
/// sidecars are NOT yet copied next to the dev binary. Non-existent in a shipped bundle (harmless).
fn sidecar_in_staging(prefix: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries");
    sidecar_in_dir(&dir, prefix)
}

pub fn resolve_zmax_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ZMAX_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    sidecar_beside_exe("zmax")
        .or_else(|| sidecar_in_staging("zmax"))
        .or_else(|| which_on_path("zmax"))
}

/// Absolute path to the bundled (or system) `stryke`, or empty if none is found. Lets the frontend
/// confirm the language runtime shipped with the app.
#[tauri::command]
pub fn stryke_bin_path() -> String {
    resolve_stryke_bin()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn resolve_stryke_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STRYKE_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(p) = sidecar_beside_exe("stryke").or_else(|| sidecar_in_staging("stryke")) {
        return Some(p);
    }
    for name in ["stryke", "strykec", "stryke-lang"] {
        if let Some(found) = which_on_path(name) {
            return Some(found);
        }
    }
    None
}

/// The shell command the frontend execs in the PTY to launch the editor: the bundled zmax by
/// absolute path, with `STRYKE_BIN` exported to the bundled stryke (the suffixed sidecar isn't callable
/// as a bare `stryke` on PATH, so the env override — which the stack's stryke resolver honors first —
/// is the reliable hand-off). Falls back to a bare `zmax` only if no binary resolves at all.
#[tauri::command]
pub fn zmax_exec_command() -> String {
    let zmax = match resolve_zmax_bin() {
        Some(p) => p.to_string_lossy().into_owned(),
        None => return "zmax".to_string(),
    };
    let mut prefix = String::from("env");
    if let Some(stryke) = resolve_stryke_bin() {
        prefix.push_str(&format!(" STRYKE_BIN=\"{}\"", stryke.to_string_lossy()));
    }
    format!("{prefix} \"{zmax}\"")
}
