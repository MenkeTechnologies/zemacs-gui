// Thin Tauri v2 host — the builder lives in lib.rs::run() (mobile-ready [lib] crate). See lib.rs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    zmax_gui_lib::run();
}
