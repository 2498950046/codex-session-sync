use sync_core::{
    ImportReport, OperationJournal, ScanReport, SnapshotSummary, SnapshotValidationReport,
    create_local_snapshot, default_codex_home, default_repository_root, import_local_snapshot,
    recover_incomplete_operation, scan_codex_home, validate_local_snapshot,
};

#[tauri::command]
fn get_default_codex_home() -> String {
    default_codex_home().to_string_lossy().into_owned()
}

#[tauri::command]
fn get_default_repository_root() -> String {
    default_repository_root().to_string_lossy().into_owned()
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

#[tauri::command]
async fn create_snapshot(
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<SnapshotSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let home = codex_home
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_codex_home);
        let repository = repository_root
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_repository_root);
        create_local_snapshot(home, repository, confirmed_codex_closed)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn validate_snapshot(
    manifest_path: String,
    repository_root: Option<String>,
) -> Result<SnapshotValidationReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let repository = repository_root
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_repository_root);
        validate_local_snapshot(manifest_path, repository).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn import_snapshot(
    manifest_path: String,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<ImportReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let home = codex_home
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_codex_home);
        let repository = repository_root
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_repository_root);
        import_local_snapshot(manifest_path, home, repository, confirmed_codex_closed)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn recover_operation(
    journal_path: String,
    confirmed_codex_closed: bool,
) -> Result<OperationJournal, String> {
    tauri::async_runtime::spawn_blocking(move || {
        recover_incomplete_operation(journal_path, confirmed_codex_closed)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_default_codex_home,
            get_default_repository_root,
            scan_local_codex,
            create_snapshot,
            validate_snapshot,
            import_snapshot,
            recover_operation
        ])
        .run(tauri::generate_context!())
        .expect("error while running Codex Session Sync");
}
