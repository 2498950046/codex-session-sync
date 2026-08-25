// Use the Windows GUI subsystem in release builds so the app does not spawn a
// console window alongside the Tauri window. Debug builds keep the console for
// easier `println!` debugging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    codex_session_sync_desktop_lib::run();
}
