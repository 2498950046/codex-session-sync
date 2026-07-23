use sync_core::{ScanReport, default_codex_home, scan_codex_home};

#[tauri::command]
fn get_default_codex_home() -> String {
    default_codex_home().to_string_lossy().into_owned()
}

#[tauri::command]
async fn scan_local_codex(codex_home: Option<String>) -> Result<ScanReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let home = codex_home
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_codex_home);
        scan_codex_home(home).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_default_codex_home,
            scan_local_codex
        ])
        .run(tauri::generate_context!())
        .expect("error while running Codex Session Sync");
}
