mod jobs;

use jobs::{JobManager, JobSnapshot};
use sync_core::{
    ImportReport, OperationJournal, ScanDashboardReport, SnapshotSummary, SnapshotValidationReport,
    create_local_snapshot, create_local_snapshot_with_control, default_codex_home,
    default_repository_root, detect_codex_processes, import_local_snapshot,
    import_local_snapshot_with_control, recover_incomplete_operation, scan_codex_home,
    scan_codex_home_with_control, validate_local_snapshot, validate_local_snapshot_with_control,
};
use tauri::State;

#[tauri::command]
fn get_default_codex_home() -> String {
    default_codex_home().to_string_lossy().into_owned()
}

#[tauri::command]
fn get_default_repository_root() -> String {
    default_repository_root().to_string_lossy().into_owned()
}

#[tauri::command]
async fn scan_local_codex(codex_home: Option<String>) -> Result<ScanDashboardReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let home = codex_home
            .filter(|value| !value.trim().is_empty())
            .map(Into::into)
            .unwrap_or_else(default_codex_home);
        scan_codex_home(home)
            .map(|report| ScanDashboardReport::from(&report))
            .map_err(|error| error.to_string())
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

#[tauri::command]
fn list_codex_processes() -> Vec<sync_core::CodexProcess> {
    detect_codex_processes()
}

#[tauri::command]
fn start_scan_job(jobs: State<'_, JobManager>, codex_home: Option<String>) -> JobSnapshot {
    let home = resolve_codex_home(codex_home);
    jobs.start("scan", true, move |control| {
        scan_codex_home_with_control(home, &control)
            .map(|report| ScanDashboardReport::from(&report))
    })
}

#[tauri::command]
fn start_snapshot_job(
    jobs: State<'_, JobManager>,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    Ok(jobs.start("snapshot", true, move |control| {
        create_local_snapshot_with_control(home, repository, confirmed_codex_closed, &control)
    }))
}

#[tauri::command]
fn start_validation_job(
    jobs: State<'_, JobManager>,
    manifest_path: String,
    repository_root: Option<String>,
) -> JobSnapshot {
    let repository = resolve_repository_root(repository_root);
    jobs.start("validate", true, move |control| {
        validate_local_snapshot_with_control(manifest_path, repository, &control)
    })
}

#[tauri::command]
fn start_import_job(
    jobs: State<'_, JobManager>,
    manifest_path: String,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    Ok(jobs.start("import", true, move |control| {
        import_local_snapshot_with_control(
            manifest_path,
            home,
            repository,
            confirmed_codex_closed,
            &control,
        )
    }))
}

#[tauri::command]
fn start_recovery_job(
    jobs: State<'_, JobManager>,
    journal_path: String,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    ensure_codex_closed()?;
    Ok(jobs.start("recovery", false, move |_control| {
        recover_incomplete_operation(journal_path, confirmed_codex_closed)
    }))
}

#[tauri::command]
fn get_job(jobs: State<'_, JobManager>, job_id: String) -> Result<JobSnapshot, String> {
    jobs.get(&job_id).ok_or_else(|| "找不到任务".to_string())
}

#[tauri::command]
fn cancel_job(jobs: State<'_, JobManager>, job_id: String) -> Result<JobSnapshot, String> {
    jobs.cancel(&job_id)
}

#[tauri::command]
fn take_job_result(
    jobs: State<'_, JobManager>,
    job_id: String,
) -> Result<serde_json::Value, String> {
    jobs.take_result(&job_id)
}

fn ensure_codex_closed() -> Result<(), String> {
    let processes = detect_codex_processes();
    if processes.is_empty() {
        return Ok(());
    }
    let details = processes
        .iter()
        .map(|process| format!("{} (PID {})", process.name, process.pid))
        .collect::<Vec<_>>()
        .join("，");
    Err(format!(
        "检测到 Codex 仍在运行：{details}。请完全退出后重试。"
    ))
}

fn resolve_codex_home(value: Option<String>) -> std::path::PathBuf {
    value
        .filter(|value| !value.trim().is_empty())
        .map(Into::into)
        .unwrap_or_else(default_codex_home)
}

fn resolve_repository_root(value: Option<String>) -> std::path::PathBuf {
    value
        .filter(|value| !value.trim().is_empty())
        .map(Into::into)
        .unwrap_or_else(default_repository_root)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(JobManager::default())
        .invoke_handler(tauri::generate_handler![
            get_default_codex_home,
            get_default_repository_root,
            scan_local_codex,
            create_snapshot,
            validate_snapshot,
            import_snapshot,
            recover_operation,
            list_codex_processes,
            start_scan_job,
            start_snapshot_job,
            start_validation_job,
            start_import_job,
            start_recovery_job,
            get_job,
            cancel_job,
            take_job_result
        ])
        .run(tauri::generate_context!())
        .expect("error while running Codex Session Sync");
}
