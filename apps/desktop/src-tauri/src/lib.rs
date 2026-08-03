mod jobs;
mod namespace_mapping;
mod remote;
mod remote_config;
mod remote_sync;
mod workspace_mapping;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use jobs::{JobManager, JobSnapshot};
use namespace_mapping::{
    NamespaceMappingState, NamespaceMappingStore, build_mapping_state, detect_local_identity,
};
use remote::{RemoteClient, SecretToken};
use remote_config::{
    CredentialStore, RemoteProfile, RemoteProfileStore, RemoteProfileSummary, SystemCredentialStore,
};
use remote_sync::{
    LocalSyncContext, download_remote_revision_as_snapshot, download_revision_graph,
    pull_namespace, push_namespace, reapply_workspace_mappings, resolve_pull_conflicts,
    restore_remote_revision_and_publish, restore_remote_revision_locally, switch_namespace,
};
use serde::Deserialize;
use serde::Serialize;
use sync_core::{
    CheckoutJournal, GcPlan, ImportReport, LocalSnapshotListItem, OperationJournal,
    ProviderSyncJournal, QuarantinedRollout, RepositoryStorageSummary, ScanDashboardReport,
    SnapshotDeletionPlan, SnapshotDiff, SnapshotMetadata, SnapshotSummary, SnapshotTrashEntry,
    SnapshotValidationReport, ThreadConflictResolution, TrackingStore, create_local_snapshot,
    create_local_snapshot_with_control, default_codex_home, default_repository_root,
    detect_codex_processes, import_local_snapshot, import_local_snapshot_with_control,
    preview_provider_sync, quarantine_empty_rollout, recover_checkout_operation,
    recover_incomplete_operation, recover_provider_sync, scan_codex_home_dashboard,
    scan_codex_home_dashboard_with_control, synchronize_local_provider, validate_local_snapshot,
    validate_local_snapshot_with_control,
};
use tauri::State;
use uuid::Uuid;
use workspace_mapping::{
    AutomaticWorkspaceMappingResult, WorkspaceCleanupReport, WorkspaceCleanupResult,
    WorkspaceMappingState, WorkspaceMappingStore, WorkspacePathSelection, WorkspacePullPlan,
    collect_workspace_paths,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteConnectionStatus {
    profile: RemoteProfileSummary,
    protocol: sync_core::ProtocolInfoResponse,
    namespaces: Vec<sync_core::Namespace>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteNamespaceStatus {
    remote_id: Uuid,
    namespace_id: Uuid,
    active: bool,
    active_remote_id: Option<Uuid>,
    active_namespace_id: Option<Uuid>,
    integrated_head: Option<String>,
    remote_head: Option<String>,
    generation: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateNamespaceMappingRequest {
    remote_id: String,
    namespace_id: String,
    label: String,
    match_api_key: bool,
    match_provider: bool,
    match_codex_home: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkspaceMappingRequest {
    remote_id: String,
    namespace_id: String,
    remote_prefix: String,
    local_prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAutomaticWorkspaceMappingsRequest {
    remote_id: String,
    namespace_id: String,
    expected_head: Option<String>,
    mappings: Vec<WorkspacePathSelection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CleanupWorkspaceDirectoriesRequest {
    remote_id: String,
    namespace_id: String,
    paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum RecoveredOperationJournal {
    Import(OperationJournal),
    Checkout(CheckoutJournal),
    ProviderSync(ProviderSyncJournal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryJournalKind {
    Import,
    Checkout,
    ProviderSync,
}

#[derive(Debug, Clone)]
struct RecoveryJournalDescriptor {
    kind: RecoveryJournalKind,
    target_codex_home: PathBuf,
    repository_root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryPoint {
    operation_id: String,
    kind: String,
    status: String,
    journal_path: PathBuf,
    target_codex_home: PathBuf,
    started_at: Option<String>,
    updated_at: Option<String>,
    requires_attention: bool,
}

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
        scan_codex_home_dashboard(home).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn quarantine_empty_rollout_file(
    jobs: State<'_, JobManager>,
    codex_home: Option<String>,
    repository_root: Option<String>,
    rollout_path: String,
    confirmed_codex_closed: bool,
) -> Result<QuarantinedRollout, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    let lease = jobs.try_acquire_codex_home(&home)?;
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        let _repository_lease = repository_lease;
        quarantine_empty_rollout(home, repository, rollout_path, confirmed_codex_closed)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn create_snapshot(
    jobs: State<'_, JobManager>,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<SnapshotSummary, String> {
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    let lease = jobs.try_acquire_codex_home(&home)?;
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        let _repository_lease = repository_lease;
        create_local_snapshot(home, repository, confirmed_codex_closed)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn validate_snapshot(
    jobs: State<'_, JobManager>,
    manifest_path: String,
    repository_root: Option<String>,
) -> Result<SnapshotValidationReport, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        validate_local_snapshot(manifest_path, repository).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn list_local_snapshots(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
) -> Result<Vec<LocalSnapshotListItem>, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::list_local_snapshots(&repository)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn compare_local_snapshots(
    jobs: State<'_, JobManager>,
    left_manifest: String,
    right_manifest: String,
) -> Result<SnapshotDiff, String> {
    let repository = Path::new(&left_manifest)
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            "snapshot manifest is not inside a repository snapshots directory".to_string()
        })?
        .to_path_buf();
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::compare_local_snapshots(Path::new(&left_manifest), Path::new(&right_manifest))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn update_snapshot_metadata(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    snapshot_id: String,
    metadata: SnapshotMetadata,
) -> Result<SnapshotMetadata, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_exclusive(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::update_snapshot_metadata(&repository, &snapshot_id, metadata)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn plan_snapshot_deletion(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    snapshot_id: String,
) -> Result<SnapshotDeletionPlan, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::plan_snapshot_deletion(&repository, &snapshot_id)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn trash_local_snapshot(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    plan: SnapshotDeletionPlan,
) -> Result<SnapshotTrashEntry, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_exclusive(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::trash_local_snapshot(&repository, &plan)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn list_local_snapshot_trash(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
) -> Result<Vec<SnapshotTrashEntry>, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::list_local_snapshot_trash(&repository)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn restore_trashed_snapshot(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    operation_id: String,
) -> Result<String, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_exclusive(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::restore_trashed_snapshot(&repository, &operation_id)
    })
    .await
    .map_err(|error| error.to_string())?
    .map(|path| path.to_string_lossy().into_owned())
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn plan_local_gc(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
) -> Result<GcPlan, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::plan_local_gc(&repository)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_repository_storage_summary(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
) -> Result<RepositoryStorageSummary, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::repository_storage_summary(&repository)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn list_recovery_points(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
) -> Result<Vec<RecoveryPoint>, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        discover_recovery_points(&repository)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn quarantine_local_gc(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    plan: GcPlan,
) -> Result<String, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_exclusive(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        sync_core::quarantine_local_gc_plan(&repository, &plan)
    })
    .await
    .map_err(|error| error.to_string())?
    .map(|path| path.to_string_lossy().into_owned())
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn start_snapshot_restore_job(
    jobs: State<'_, JobManager>,
    manifest_path: String,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    let lock_home = home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "restore",
        false,
        move |control| {
            sync_core::checkout_local_snapshot_with_control(
                manifest_path,
                home,
                repository,
                confirmed_codex_closed,
                &control,
            )
        },
    )
}

#[tauri::command]
async fn list_remote_revisions(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<Vec<sync_core::RevisionSummary>, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        let (_, client) = load_remote_client(&repository, parse_uuid(&remote_id)?)?;
        client.list_revisions(parse_uuid(&namespace_id)?)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn list_remote_history_trash(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<Vec<sync_core::HistoryTrashOperation>, String> {
    let repository = resolve_repository_root(repository_root);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        let (_, client) = load_remote_client(&repository, parse_uuid(&remote_id)?)?;
        client.list_history_trash(parse_uuid(&namespace_id)?)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn truncate_remote_history(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    new_head: Option<String>,
) -> Result<sync_core::HistoryTrashOperation, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        let remote_id = parse_uuid(&remote_id)?;
        let (_, client) = load_remote_client(&repository, remote_id)?;
        let namespace_id = parse_uuid(&namespace_id)?;
        let state = client.namespace_head_state(namespace_id)?;
        let operation = client.truncate_history(
            namespace_id,
            &sync_core::TruncateHistoryRequest {
                expected_head: state.head,
                expected_namespace_epoch: state.namespace_epoch,
                new_head,
            },
        )?;
        let tracking = TrackingStore::open(&repository)?;
        if let Some(record) = tracking.load(&codex_home, remote_id, namespace_id)? {
            tracking.update_remote_epoch(
                &codex_home,
                remote_id,
                namespace_id,
                record.generation,
                operation.epoch_after,
            )?;
        }
        Ok::<_, anyhow::Error>(operation)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn restore_remote_history_trash(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    operation_id: String,
) -> Result<sync_core::HistoryTrashOperation, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _repository_lease = repository_lease;
        let remote_id = parse_uuid(&remote_id)?;
        let (_, client) = load_remote_client(&repository, remote_id)?;
        let namespace_id = parse_uuid(&namespace_id)?;
        let state = client.namespace_head_state(namespace_id)?;
        let operation = client.restore_history_trash(
            namespace_id,
            parse_uuid(&operation_id)?,
            &sync_core::RestoreHistoryRequest {
                expected_head: state.head,
                expected_namespace_epoch: state.namespace_epoch,
            },
        )?;
        let current = client.namespace_head_state(namespace_id)?;
        let tracking = TrackingStore::open(&repository)?;
        if let Some(record) = tracking.load(&codex_home, remote_id, namespace_id)? {
            tracking.update_remote_epoch(
                &codex_home,
                remote_id,
                namespace_id,
                record.generation,
                current.namespace_epoch,
            )?;
        }
        Ok::<_, anyhow::Error>(operation)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn start_remote_revision_download_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    remote_id: String,
    namespace_id: String,
    revision_id: String,
) -> Result<JobSnapshot, String> {
    let repository = resolve_repository_root(repository_root);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    jobs.start_repository_shared(
        &repository.clone(),
        "revision-download",
        true,
        move |control| {
            download_remote_revision_as_snapshot(
                namespace_id,
                &revision_id,
                &client,
                &repository,
                &control,
            )
        },
    )
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn start_remote_revision_restore_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    revision_id: String,
    publish: bool,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    let mapper = WorkspaceMappingStore::new(&repository)
        .mapper(&home, remote_id, namespace_id)
        .map_err(|error| error.to_string())?;
    let lock_home = home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        if publish {
            "revision-publish"
        } else {
            "revision-restore"
        },
        false,
        move |control| {
            let context = LocalSyncContext::new(&home, &repository, &mapper, &control);
            if publish {
                serde_json::to_value(restore_remote_revision_and_publish(
                    remote_id,
                    namespace_id,
                    &revision_id,
                    &client,
                    context,
                )?)
                .map_err(Into::into)
            } else {
                serde_json::to_value(restore_remote_revision_locally(
                    remote_id,
                    namespace_id,
                    &revision_id,
                    &client,
                    context,
                )?)
                .map_err(Into::into)
            }
        },
    )
}

#[tauri::command]
async fn import_snapshot(
    jobs: State<'_, JobManager>,
    manifest_path: String,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<ImportReport, String> {
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    let lease = jobs.try_acquire_codex_home(&home)?;
    let repository_lease = jobs.try_acquire_repository_shared(&repository)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        let _repository_lease = repository_lease;
        import_local_snapshot(manifest_path, home, repository, confirmed_codex_closed)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn recover_operation(
    jobs: State<'_, JobManager>,
    journal_path: String,
    confirmed_codex_closed: bool,
) -> Result<RecoveredOperationJournal, String> {
    ensure_codex_closed()?;
    let journal_path = PathBuf::from(journal_path);
    let descriptor = inspect_recovery_journal(&journal_path).map_err(|error| error.to_string())?;
    let lease = jobs.try_acquire_codex_home(&descriptor.target_codex_home)?;
    let recovery_kind = descriptor.kind;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        recover_local_operation_as(journal_path, confirmed_codex_closed, recovery_kind)
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
        scan_codex_home_dashboard_with_control(home, &control)
    })
}

#[tauri::command]
fn start_provider_sync_preview_job(
    jobs: State<'_, JobManager>,
    codex_home: Option<String>,
    repository_root: Option<String>,
) -> Result<JobSnapshot, String> {
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    jobs.start_repository_shared(
        &repository.clone(),
        "provider_sync_preview",
        true,
        move |control| preview_provider_sync(home, &control),
    )
}

#[tauri::command]
fn start_provider_sync_job(
    jobs: State<'_, JobManager>,
    codex_home: Option<String>,
    repository_root: Option<String>,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let home = resolve_codex_home(codex_home);
    let repository = resolve_repository_root(repository_root);
    let lock_home = home.clone();
    jobs.start_home_repository_exclusive(
        &lock_home,
        &repository.clone(),
        "provider_sync",
        true,
        move |control| {
            synchronize_local_provider(home, repository, confirmed_codex_closed, &control)
        },
    )
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
    let lock_home = home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "snapshot",
        true,
        move |control| {
            create_local_snapshot_with_control(home, repository, confirmed_codex_closed, &control)
        },
    )
}

#[tauri::command]
fn start_validation_job(
    jobs: State<'_, JobManager>,
    manifest_path: String,
    repository_root: Option<String>,
) -> Result<JobSnapshot, String> {
    let repository = resolve_repository_root(repository_root);
    jobs.start_repository_shared(&repository.clone(), "validate", true, move |control| {
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
    let lock_home = home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "import",
        true,
        move |control| {
            import_local_snapshot_with_control(
                manifest_path,
                home,
                repository,
                confirmed_codex_closed,
                &control,
            )
        },
    )
}

#[tauri::command]
fn start_recovery_job(
    jobs: State<'_, JobManager>,
    journal_path: String,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    ensure_codex_closed()?;
    let journal_path = PathBuf::from(journal_path);
    let descriptor = inspect_recovery_journal(&journal_path).map_err(|error| error.to_string())?;
    let recovery_kind = descriptor.kind;
    jobs.start_home_repository_exclusive(
        &descriptor.target_codex_home,
        &descriptor.repository_root,
        "recovery",
        false,
        move |_control| {
            recover_local_operation_as(journal_path, confirmed_codex_closed, recovery_kind)
        },
    )
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

#[tauri::command]
fn list_remote_profiles(
    repository_root: Option<String>,
) -> Result<Vec<RemoteProfileSummary>, String> {
    let repository = resolve_repository_root(repository_root);
    RemoteProfileStore::new(repository)
        .list(&SystemCredentialStore)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn save_remote_profile(
    repository_root: Option<String>,
    remote_id: Option<String>,
    display_name: String,
    server_url: String,
    token: Option<String>,
) -> Result<RemoteConnectionStatus, String> {
    let repository = resolve_repository_root(repository_root);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = remote_id
            .as_deref()
            .map(parse_uuid)
            .transpose()?
            .unwrap_or_else(Uuid::now_v7);
        let credentials = SystemCredentialStore;
        let token = match token.filter(|value| !value.trim().is_empty()) {
            Some(value) => SecretToken::new(value)?,
            None => credentials
                .get(&remote_id)
                .with_context(|| "a server token is required when creating a remote profile")?,
        };
        let client = RemoteClient::new(&server_url, token.clone())?;
        let protocol = client.info()?;
        let namespaces = client.list_namespaces()?;
        credentials.set(&remote_id, &token)?;
        let store = RemoteProfileStore::new(&repository);
        let profile = store.upsert(remote_id, display_name, server_url)?;
        let profile = store
            .list(&credentials)?
            .into_iter()
            .find(|candidate| candidate.profile.id == profile.id)
            .context("saved remote profile disappeared")?;
        Ok::<_, anyhow::Error>(RemoteConnectionStatus {
            profile,
            protocol,
            namespaces,
        })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn test_remote_connection(
    repository_root: Option<String>,
    remote_id: String,
) -> Result<RemoteConnectionStatus, String> {
    let repository = resolve_repository_root(repository_root);
    tauri::async_runtime::spawn_blocking(move || {
        connection_status(&repository, parse_uuid(&remote_id)?)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn list_remote_namespaces(
    repository_root: Option<String>,
    remote_id: String,
) -> Result<Vec<sync_core::Namespace>, String> {
    let repository = resolve_repository_root(repository_root);
    tauri::async_runtime::spawn_blocking(move || {
        let (_, client) = load_remote_client(&repository, parse_uuid(&remote_id)?)?;
        client.info()?;
        client.list_namespaces()
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn create_remote_namespace(
    repository_root: Option<String>,
    remote_id: String,
    display_name: String,
) -> Result<sync_core::Namespace, String> {
    let repository = resolve_repository_root(repository_root);
    tauri::async_runtime::spawn_blocking(move || {
        let (_, client) = load_remote_client(&repository, parse_uuid(&remote_id)?)?;
        client.create_namespace(display_name)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn rename_remote_namespace(
    repository_root: Option<String>,
    remote_id: String,
    namespace_id: String,
    display_name: String,
) -> Result<sync_core::Namespace, String> {
    let repository = resolve_repository_root(repository_root);
    tauri::async_runtime::spawn_blocking(move || {
        let (_, client) = load_remote_client(&repository, parse_uuid(&remote_id)?)?;
        client.rename_namespace(parse_uuid(&namespace_id)?, display_name)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn select_remote_namespace(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<RemoteProfile, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let store = RemoteProfileStore::new(&repository);
    let current = store.get(remote_id).map_err(|error| error.to_string())?;
    if current.automatic_namespace_selection {
        let home_key = sync_core::codex_home_key(&codex_home).map_err(|error| error.to_string())?;
        NamespaceMappingStore::new(&repository)
            .set_manual_override(remote_id, namespace_id, home_key)
            .map_err(|error| error.to_string())?;
    }
    store
        .select_namespace(remote_id, namespace_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_namespace_mapping_state(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
) -> Result<NamespaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        namespace_mapping_state(&repository, &codex_home, parse_uuid(&remote_id)?)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn create_namespace_mapping(
    repository_root: Option<String>,
    codex_home: Option<String>,
    request: CreateNamespaceMappingRequest,
) -> Result<NamespaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&request.remote_id)?;
        let namespace_id = parse_uuid(&request.namespace_id)?;
        let profile = RemoteProfileStore::new(&repository).get(remote_id)?;
        let identity = detect_local_identity(&codex_home, &profile.server_url, None)?;
        let api_key_fingerprint = if request.match_api_key {
            Some(
                identity
                    .api_key_fingerprint
                    .clone()
                    .context("the current API key could not be detected safely")?,
            )
        } else {
            None
        };
        let provider = if request.match_provider {
            Some(
                identity
                    .provider
                    .clone()
                    .context("the current model provider could not be detected")?,
            )
        } else {
            None
        };
        let codex_home_key = request
            .match_codex_home
            .then(|| identity.codex_home_key.clone());
        NamespaceMappingStore::new(&repository).create(
            remote_id,
            namespace_id,
            request.label,
            api_key_fingerprint,
            provider,
            codex_home_key,
        )?;
        namespace_mapping_state(&repository, &codex_home, remote_id)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn delete_namespace_mapping(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    mapping_id: String,
) -> Result<NamespaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&remote_id)?;
        NamespaceMappingStore::new(&repository).delete(remote_id, parse_uuid(&mapping_id)?)?;
        namespace_mapping_state(&repository, &codex_home, remote_id)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_automatic_namespace_selection(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    enabled: bool,
) -> Result<NamespaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&remote_id)?;
        let home_key = sync_core::codex_home_key(&codex_home)?;
        let mappings = NamespaceMappingStore::new(&repository);
        if enabled {
            mappings.clear_manual_override(remote_id, &home_key)?;
        }
        RemoteProfileStore::new(&repository)
            .set_automatic_namespace_selection(remote_id, enabled)?;
        namespace_mapping_state(&repository, &codex_home, remote_id)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn clear_manual_namespace_override(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
) -> Result<NamespaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&remote_id)?;
        let home_key = sync_core::codex_home_key(&codex_home)?;
        NamespaceMappingStore::new(&repository).clear_manual_override(remote_id, &home_key)?;
        namespace_mapping_state(&repository, &codex_home, remote_id)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_workspace_mapping_state(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<WorkspaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        WorkspaceMappingStore::new(repository).state(
            &codex_home,
            parse_uuid(&remote_id)?,
            parse_uuid(&namespace_id)?,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_workspace_cleanup_report(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<WorkspaceCleanupReport, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        WorkspaceMappingStore::new(repository).cleanup_report(
            &codex_home,
            parse_uuid(&remote_id)?,
            parse_uuid(&namespace_id)?,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn quarantine_workspace_directories(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    request: CleanupWorkspaceDirectoriesRequest,
    confirmed_codex_closed: bool,
) -> Result<WorkspaceCleanupResult, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&request.remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&request.namespace_id).map_err(|error| error.to_string())?;
    let lease = jobs.try_acquire_codex_home(&codex_home)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        WorkspaceMappingStore::new(repository)
            .quarantine_empty_directories(&codex_home, remote_id, namespace_id, request.paths)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn get_workspace_pull_plan(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<WorkspacePullPlan, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&remote_id)?;
        let namespace_id = parse_uuid(&namespace_id)?;
        let (_, client) = load_remote_client(&repository, remote_id)?;
        client.info()?;
        let remote_head = client.namespace_head(namespace_id)?;
        let remote_paths = match remote_head.as_deref() {
            Some(head) => {
                let (revision, threads, _) = download_revision_graph(
                    &client,
                    head,
                    &repository,
                    &sync_core::OperationControl::default(),
                )?;
                if revision.namespace_id != namespace_id {
                    bail!("remote revision belongs to a different namespace");
                }
                collect_workspace_paths(&threads)
            }
            None => Vec::new(),
        };
        WorkspaceMappingStore::new(repository).pull_plan(
            &codex_home,
            remote_id,
            namespace_id,
            remote_head,
            remote_paths,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn create_automatic_workspace_mappings(
    repository_root: Option<String>,
    codex_home: Option<String>,
    request: CreateAutomaticWorkspaceMappingsRequest,
) -> Result<AutomaticWorkspaceMappingResult, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&request.remote_id)?;
        let namespace_id = parse_uuid(&request.namespace_id)?;
        let (_, client) = load_remote_client(&repository, remote_id)?;
        client.info()?;
        let current_head = client.namespace_head(namespace_id)?;
        if current_head != request.expected_head {
            bail!(
                "remote namespace changed while workspace paths were being prepared; retry the sync"
            );
        }
        let remote_paths = match current_head.as_deref() {
            Some(head) => {
                let (revision, threads, _) = download_revision_graph(
                    &client,
                    head,
                    &repository,
                    &sync_core::OperationControl::default(),
                )?;
                if revision.namespace_id != namespace_id {
                    bail!("remote revision belongs to a different namespace");
                }
                collect_workspace_paths(&threads)
            }
            None => Vec::new(),
        };
        let store = WorkspaceMappingStore::new(repository);
        let plan = store.pull_plan(
            &codex_home,
            remote_id,
            namespace_id,
            current_head,
            remote_paths,
        )?;
        let expected = plan
            .unmapped_paths
            .iter()
            .map(|candidate| candidate.remote_path.as_str())
            .collect::<BTreeSet<_>>();
        let provided = request
            .mappings
            .iter()
            .map(|mapping| mapping.remote_path.trim())
            .collect::<BTreeSet<_>>();
        if expected != provided || request.mappings.len() != expected.len() {
            bail!("workspace path set changed while mappings were being edited; inspect again");
        }
        store.create_automatic(&codex_home, remote_id, namespace_id, request.mappings)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn create_workspace_mapping(
    repository_root: Option<String>,
    codex_home: Option<String>,
    request: CreateWorkspaceMappingRequest,
) -> Result<WorkspaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        WorkspaceMappingStore::new(repository).create(
            &codex_home,
            parse_uuid(&request.remote_id)?,
            parse_uuid(&request.namespace_id)?,
            request.remote_prefix,
            request.local_prefix,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn delete_workspace_mapping(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    mapping_id: String,
) -> Result<WorkspaceMappingState, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        WorkspaceMappingStore::new(repository).delete(
            &codex_home,
            parse_uuid(&remote_id)?,
            parse_uuid(&namespace_id)?,
            parse_uuid(&mapping_id)?,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_remote_namespace_status(
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
) -> Result<RemoteNamespaceStatus, String> {
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    tauri::async_runtime::spawn_blocking(move || {
        let remote_id = parse_uuid(&remote_id)?;
        let namespace_id = parse_uuid(&namespace_id)?;
        let (_, client) = load_remote_client(&repository, remote_id)?;
        let remote_head = client.namespace_head(namespace_id)?;
        let tracking = TrackingStore::open(&repository)?;
        let record = tracking.load(&codex_home, remote_id, namespace_id)?;
        let active_binding = tracking.active(&codex_home)?;
        let active = active_binding.as_ref().is_some_and(|active| {
            active.remote_id == remote_id && active.namespace_id == namespace_id
        });
        Ok::<_, anyhow::Error>(RemoteNamespaceStatus {
            remote_id,
            namespace_id,
            active,
            active_remote_id: active_binding.as_ref().map(|active| active.remote_id),
            active_namespace_id: active_binding.map(|active| active.namespace_id),
            integrated_head: record
                .as_ref()
                .and_then(|record| record.integrated_head.clone()),
            remote_head,
            generation: record.map(|record| record.generation),
        })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn start_push_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    let workspace_mapper = WorkspaceMappingStore::new(&repository)
        .mapper(&codex_home, remote_id, namespace_id)
        .map_err(|error| error.to_string())?;
    let lock_home = codex_home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "push",
        true,
        move |control| {
            push_namespace(
                remote_id,
                namespace_id,
                &client,
                LocalSyncContext::new(&codex_home, &repository, &workspace_mapper, &control),
            )
        },
    )
}

#[tauri::command]
fn start_pull_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    let workspace_mapper = WorkspaceMappingStore::new(&repository)
        .mapper(&codex_home, remote_id, namespace_id)
        .map_err(|error| error.to_string())?;
    let lock_home = codex_home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "pull",
        true,
        move |control| {
            pull_namespace(
                remote_id,
                namespace_id,
                &client,
                LocalSyncContext::new(&codex_home, &repository, &workspace_mapper, &control),
            )
        },
    )
}

#[tauri::command]
fn start_conflict_resolution_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    resolutions: Vec<ThreadConflictResolution>,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    let workspace_mapper = WorkspaceMappingStore::new(&repository)
        .mapper(&codex_home, remote_id, namespace_id)
        .map_err(|error| error.to_string())?;
    let lock_home = codex_home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "resolve",
        true,
        move |control| {
            resolve_pull_conflicts(
                remote_id,
                namespace_id,
                &resolutions,
                &client,
                LocalSyncContext::new(&codex_home, &repository, &workspace_mapper, &control),
            )
        },
    )
}

#[tauri::command]
fn start_namespace_switch_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    confirmed_codex_closed: bool,
    confirmed_replace_local: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    if !confirmed_replace_local {
        return Err(
            "namespace switch requires confirmation that local sessions will be replaced"
                .to_string(),
        );
    }
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, target_client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    let target_workspace_mapper = WorkspaceMappingStore::new(&repository)
        .mapper(&codex_home, remote_id, namespace_id)
        .map_err(|error| error.to_string())?;
    let active = TrackingStore::open(&repository)
        .and_then(|tracking| tracking.active(&codex_home))
        .map_err(|error| error.to_string())?;
    let current_client = active
        .as_ref()
        .map(|active| load_remote_client(&repository, active.remote_id).map(|(_, client)| client))
        .transpose()
        .map_err(|error| error.to_string())?;
    let current_workspace_mapper = active
        .as_ref()
        .map(|active| {
            WorkspaceMappingStore::new(&repository).mapper(
                &codex_home,
                active.remote_id,
                active.namespace_id,
            )
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let lock_home = codex_home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "switch",
        true,
        move |control| {
            switch_namespace(
                remote_id,
                namespace_id,
                &target_client,
                current_client.as_ref(),
                current_workspace_mapper.as_ref(),
                LocalSyncContext::new(&codex_home, &repository, &target_workspace_mapper, &control),
            )
        },
    )
}

#[tauri::command]
fn start_workspace_remap_job(
    jobs: State<'_, JobManager>,
    repository_root: Option<String>,
    codex_home: Option<String>,
    remote_id: String,
    namespace_id: String,
    confirmed_codex_closed: bool,
) -> Result<JobSnapshot, String> {
    require_closed_confirmation(confirmed_codex_closed)?;
    ensure_codex_closed()?;
    let repository = resolve_repository_root(repository_root);
    let codex_home = resolve_codex_home(codex_home);
    let remote_id = parse_uuid(&remote_id).map_err(|error| error.to_string())?;
    let namespace_id = parse_uuid(&namespace_id).map_err(|error| error.to_string())?;
    let (_, client) =
        load_remote_client(&repository, remote_id).map_err(|error| error.to_string())?;
    let workspace_mapper = WorkspaceMappingStore::new(&repository)
        .mapper(&codex_home, remote_id, namespace_id)
        .map_err(|error| error.to_string())?;
    if workspace_mapper.is_empty() {
        return Err("at least one workspace path mapping is required".to_string());
    }
    let lock_home = codex_home.clone();
    jobs.start_home_repository_shared(
        &lock_home,
        &repository.clone(),
        "remap",
        true,
        move |control| {
            reapply_workspace_mappings(
                remote_id,
                namespace_id,
                &client,
                LocalSyncContext::new(&codex_home, &repository, &workspace_mapper, &control),
            )
        },
    )
}

fn inspect_recovery_journal(journal_path: &Path) -> anyhow::Result<RecoveryJournalDescriptor> {
    let file = File::open(journal_path)
        .with_context(|| format!("failed to open recovery journal {}", journal_path.display()))?;
    let value: serde_json::Value =
        serde_json::from_reader(BufReader::new(file)).with_context(|| {
            format!(
                "failed to parse recovery journal {}",
                journal_path.display()
            )
        })?;
    let object = value
        .as_object()
        .context("recovery journal must be a JSON object")?;
    let import_markers = ["backupDir", "plannedRollouts", "importedThreadIds"];
    let checkout_markers = [
        "repositoryBackupDir",
        "databaseBackups",
        "directorySwaps",
        "expectedThreadHashes",
    ];
    let provider_markers = ["targetProvider", "files", "databaseBackups"];
    let has_import_marker = import_markers
        .iter()
        .any(|field| object.contains_key(*field));
    let has_checkout_marker = checkout_markers
        .iter()
        .any(|field| object.contains_key(*field));
    let has_provider_marker = provider_markers
        .iter()
        .all(|field| object.contains_key(*field));
    let has_checkout_marker = has_checkout_marker && !has_provider_marker;
    if [has_import_marker, has_checkout_marker, has_provider_marker]
        .into_iter()
        .filter(|marker| *marker)
        .count()
        > 1
    {
        bail!("ambiguous recovery journal contains markers for multiple operation types");
    }

    if has_import_marker {
        let journal: OperationJournal =
            serde_json::from_value(value).context("invalid import operation recovery journal")?;
        return Ok(RecoveryJournalDescriptor {
            kind: RecoveryJournalKind::Import,
            target_codex_home: journal.target_codex_home,
            repository_root: journal_path
                .parent()
                .and_then(Path::parent)
                .context("import journal is not inside a repository journal directory")?
                .to_path_buf(),
        });
    }
    if has_checkout_marker {
        let journal: CheckoutJournal =
            serde_json::from_value(value).context("invalid checkout operation recovery journal")?;
        return Ok(RecoveryJournalDescriptor {
            kind: RecoveryJournalKind::Checkout,
            target_codex_home: journal.target_codex_home,
            repository_root: journal.repository_root,
        });
    }
    if has_provider_marker {
        let journal: ProviderSyncJournal = serde_json::from_value(value)
            .context("invalid provider synchronization recovery journal")?;
        return Ok(RecoveryJournalDescriptor {
            kind: RecoveryJournalKind::ProviderSync,
            target_codex_home: journal.target_codex_home,
            repository_root: journal.repository_root,
        });
    }
    bail!("unsupported recovery journal type")
}

fn discover_recovery_points(repository_root: &Path) -> anyhow::Result<Vec<RecoveryPoint>> {
    let journal_directory = repository_root.join("journal");
    if !journal_directory.exists() {
        return Ok(Vec::new());
    }
    let mut points = Vec::new();
    for entry in std::fs::read_dir(journal_directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let metadata = std::fs::metadata(&path)?;
        if !metadata.is_file() || metadata.len() > sync_core::MAX_STRUCTURED_OBJECT_BYTES {
            continue;
        }
        let value: serde_json::Value = serde_json::from_reader(BufReader::new(File::open(&path)?))?;
        let Some(object) = value.as_object() else {
            continue;
        };
        let kind = if object.contains_key("targetProvider") && object.contains_key("files") {
            "provider_sync"
        } else if ["backupDir", "plannedRollouts", "importedThreadIds"]
            .iter()
            .any(|field| object.contains_key(*field))
        {
            "import"
        } else if [
            "repositoryBackupDir",
            "databaseBackups",
            "directorySwaps",
            "expectedThreadHashes",
        ]
        .iter()
        .any(|field| object.contains_key(*field))
        {
            "checkout"
        } else {
            continue;
        };
        let string_field = |name: &str| {
            object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let status = string_field("status").unwrap_or_else(|| "unknown".to_string());
        let target_codex_home = string_field("targetCodexHome")
            .map(PathBuf::from)
            .unwrap_or_default();
        let normalized_status = status.to_ascii_lowercase();
        let requires_attention = !matches!(
            normalized_status.as_str(),
            "completed" | "rolled_back" | "rolledback"
        );
        points.push(RecoveryPoint {
            operation_id: string_field("operationId").unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            kind: kind.to_string(),
            status,
            journal_path: path,
            target_codex_home,
            started_at: string_field("startedAt"),
            updated_at: string_field("updatedAt"),
            requires_attention,
        });
    }
    points.sort_by(|left, right| {
        right
            .requires_attention
            .cmp(&left.requires_attention)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.operation_id.cmp(&left.operation_id))
    });
    Ok(points)
}

#[cfg(test)]
fn recover_local_operation(
    journal_path: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> anyhow::Result<RecoveredOperationJournal> {
    let journal_path = journal_path.as_ref();
    let kind = inspect_recovery_journal(journal_path)?.kind;
    recover_local_operation_as(journal_path, confirmed_codex_closed, kind)
}

fn recover_local_operation_as(
    journal_path: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    kind: RecoveryJournalKind,
) -> anyhow::Result<RecoveredOperationJournal> {
    let journal_path = journal_path.as_ref();
    match kind {
        RecoveryJournalKind::Import => Ok(RecoveredOperationJournal::Import(
            recover_incomplete_operation(journal_path, confirmed_codex_closed)?,
        )),
        RecoveryJournalKind::Checkout => Ok(RecoveredOperationJournal::Checkout(
            recover_checkout_operation(journal_path, confirmed_codex_closed)?,
        )),
        RecoveryJournalKind::ProviderSync => Ok(RecoveredOperationJournal::ProviderSync(
            recover_provider_sync(journal_path, confirmed_codex_closed)?,
        )),
    }
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

fn require_closed_confirmation(confirmed: bool) -> Result<(), String> {
    if confirmed {
        Ok(())
    } else {
        Err("operation requires confirmation that Codex is fully closed".to_string())
    }
}

fn parse_uuid(value: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("invalid UUID {value}"))
}

fn load_remote_client(
    repository_root: &std::path::Path,
    remote_id: Uuid,
) -> anyhow::Result<(RemoteProfile, RemoteClient)> {
    let profile = RemoteProfileStore::new(repository_root).get(remote_id)?;
    let token = SystemCredentialStore.get(&remote_id)?;
    let client = RemoteClient::new(&profile.server_url, token)?;
    Ok((profile, client))
}

fn connection_status(
    repository_root: &std::path::Path,
    remote_id: Uuid,
) -> anyhow::Result<RemoteConnectionStatus> {
    let credentials = SystemCredentialStore;
    let store = RemoteProfileStore::new(repository_root);
    let (_, client) = load_remote_client(repository_root, remote_id)?;
    let protocol = client.info()?;
    let namespaces = client.list_namespaces()?;
    let profile = store
        .list(&credentials)?
        .into_iter()
        .find(|candidate| candidate.profile.id == remote_id)
        .context("remote profile disappeared")?;
    Ok(RemoteConnectionStatus {
        profile,
        protocol,
        namespaces,
    })
}

fn namespace_mapping_state(
    repository_root: &std::path::Path,
    codex_home: &std::path::Path,
    remote_id: Uuid,
) -> anyhow::Result<NamespaceMappingState> {
    let profile = RemoteProfileStore::new(repository_root).get(remote_id)?;
    let identity = detect_local_identity(codex_home, &profile.server_url, None)?;
    let mappings = NamespaceMappingStore::new(repository_root);
    let rules = mappings.list(remote_id)?;
    let manual_override = mappings.manual_override(remote_id, &identity.codex_home_key)?;
    Ok(build_mapping_state(
        remote_id,
        profile.automatic_namespace_selection,
        profile.selected_namespace_id,
        &identity,
        &rules,
        manual_override,
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
        .plugin(tauri_plugin_dialog::init())
        .manage(JobManager::default())
        .invoke_handler(tauri::generate_handler![
            get_default_codex_home,
            get_default_repository_root,
            scan_local_codex,
            quarantine_empty_rollout_file,
            create_snapshot,
            validate_snapshot,
            list_local_snapshots,
            compare_local_snapshots,
            update_snapshot_metadata,
            plan_snapshot_deletion,
            trash_local_snapshot,
            list_local_snapshot_trash,
            restore_trashed_snapshot,
            plan_local_gc,
            get_repository_storage_summary,
            list_recovery_points,
            quarantine_local_gc,
            start_snapshot_restore_job,
            list_remote_revisions,
            list_remote_history_trash,
            truncate_remote_history,
            restore_remote_history_trash,
            start_remote_revision_download_job,
            start_remote_revision_restore_job,
            import_snapshot,
            recover_operation,
            list_codex_processes,
            start_scan_job,
            start_provider_sync_preview_job,
            start_provider_sync_job,
            start_snapshot_job,
            start_validation_job,
            start_import_job,
            start_recovery_job,
            get_job,
            cancel_job,
            take_job_result,
            list_remote_profiles,
            save_remote_profile,
            test_remote_connection,
            list_remote_namespaces,
            create_remote_namespace,
            rename_remote_namespace,
            select_remote_namespace,
            get_namespace_mapping_state,
            create_namespace_mapping,
            delete_namespace_mapping,
            set_automatic_namespace_selection,
            clear_manual_namespace_override,
            get_workspace_mapping_state,
            get_workspace_cleanup_report,
            quarantine_workspace_directories,
            get_workspace_pull_plan,
            create_workspace_mapping,
            create_automatic_workspace_mappings,
            delete_workspace_mapping,
            get_remote_namespace_status,
            start_push_job,
            start_pull_job,
            start_conflict_resolution_job,
            start_namespace_switch_job,
            start_workspace_remap_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Codex Session Sync");
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sync_core::{
        CHECKOUT_JOURNAL_SCHEMA_VERSION, CheckoutJournal, CheckoutStatus,
        OPERATION_JOURNAL_SCHEMA_VERSION, OperationStatus, PROVIDER_SYNC_JOURNAL_SCHEMA_VERSION,
        ProviderSyncStatus,
    };
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn checkout_journal_is_dispatched_to_checkout_recovery() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let journal_path = directory.path().join("checkout-operation.json");
        let journal = CheckoutJournal {
            schema_version: CHECKOUT_JOURNAL_SCHEMA_VERSION,
            operation_id: Uuid::now_v7().to_string(),
            snapshot_id: Uuid::now_v7().to_string(),
            target_codex_home: codex_home.clone(),
            repository_root: directory.path().to_path_buf(),
            repository_backup_dir: directory.path().join("backup"),
            status: CheckoutStatus::Completed,
            started_at: "2026-07-26T10:00:00Z".to_string(),
            updated_at: "2026-07-26T10:00:01Z".to_string(),
            database_backups: Vec::new(),
            directory_swaps: Vec::new(),
            file_swaps: Vec::new(),
            expected_thread_hashes: BTreeMap::new(),
            tracking_update: None,
            error: None,
        };
        std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let recovered = recover_local_operation(&journal_path, true).unwrap();

        assert!(matches!(
            recovered,
            RecoveredOperationJournal::Checkout(recovered)
                if recovered.status == CheckoutStatus::Completed
                    && recovered.target_codex_home == codex_home
        ));
    }

    #[test]
    fn import_journal_still_uses_the_existing_import_recovery() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let journal_path = directory.path().join("import-operation.json");
        let journal = OperationJournal {
            schema_version: OPERATION_JOURNAL_SCHEMA_VERSION,
            operation_id: Uuid::now_v7().to_string(),
            snapshot_id: Uuid::now_v7().to_string(),
            target_codex_home: codex_home.clone(),
            backup_dir: directory.path().join("backup"),
            status: OperationStatus::Completed,
            started_at: "2026-07-26T10:00:00Z".to_string(),
            updated_at: "2026-07-26T10:00:01Z".to_string(),
            planned_rollouts: Vec::new(),
            imported_thread_ids: Vec::new(),
            error: None,
        };
        std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let recovered = recover_local_operation(&journal_path, true).unwrap();

        assert!(matches!(
            recovered,
            RecoveredOperationJournal::Import(recovered)
                if recovered.status == OperationStatus::Completed
                    && recovered.target_codex_home == codex_home
        ));
    }

    #[test]
    fn provider_sync_journal_is_dispatched_to_provider_recovery() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let repository = directory.path().join("repository");
        std::fs::create_dir_all(&codex_home).unwrap();
        let journal_path = directory.path().join("provider-operation.json");
        let journal = ProviderSyncJournal {
            schema_version: PROVIDER_SYNC_JOURNAL_SCHEMA_VERSION,
            operation_id: Uuid::now_v7().to_string(),
            target_codex_home: codex_home.clone(),
            repository_root: repository,
            target_provider: "custom".to_string(),
            status: ProviderSyncStatus::Completed,
            started_at: "2026-08-03T10:00:00Z".to_string(),
            updated_at: "2026-08-03T10:00:01Z".to_string(),
            files: Vec::new(),
            database_backups: Vec::new(),
            error: None,
        };
        std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let recovered = recover_local_operation(&journal_path, true).unwrap();

        assert!(matches!(
            recovered,
            RecoveredOperationJournal::ProviderSync(recovered)
                if recovered.status == ProviderSyncStatus::Completed
                    && recovered.target_codex_home == codex_home
        ));
    }

    #[test]
    fn ambiguous_recovery_journal_is_rejected_without_dispatch() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let journal_path = directory.path().join("ambiguous-operation.json");
        let mut value = serde_json::json!({
            "schemaVersion": CHECKOUT_JOURNAL_SCHEMA_VERSION,
            "operationId": Uuid::now_v7().to_string(),
            "snapshotId": Uuid::now_v7().to_string(),
            "targetCodexHome": codex_home,
            "repositoryBackupDir": directory.path().join("checkout-backup"),
            "status": "completed",
            "startedAt": "2026-07-26T10:00:00Z",
            "updatedAt": "2026-07-26T10:00:01Z",
            "databaseBackups": [],
            "directorySwaps": [],
            "expectedThreadHashes": {},
            "error": null
        });
        let object = value.as_object_mut().unwrap();
        object.insert(
            "backupDir".to_string(),
            serde_json::json!(directory.path().join("import-backup")),
        );
        object.insert("plannedRollouts".to_string(), serde_json::json!([]));
        object.insert("importedThreadIds".to_string(), serde_json::json!([]));
        std::fs::write(&journal_path, serde_json::to_vec(&value).unwrap()).unwrap();

        let error = recover_local_operation(&journal_path, true).unwrap_err();

        assert!(error.to_string().contains("ambiguous recovery journal"));
    }

    #[test]
    fn recovery_point_discovery_pins_incomplete_journals_first() {
        let repository = tempdir().unwrap();
        let journal_directory = repository.path().join("journal");
        std::fs::create_dir_all(&journal_directory).unwrap();
        let incomplete = serde_json::json!({
            "operationId": "01900000-0000-7000-8000-000000000001",
            "targetCodexHome": "C:/temp/codex",
            "status": "recovery_required",
            "startedAt": "2026-07-31T10:00:00Z",
            "updatedAt": "2026-07-31T10:02:00Z",
            "repositoryBackupDir": "C:/temp/backup",
            "databaseBackups": [],
            "directorySwaps": [],
            "expectedThreadHashes": {}
        });
        let completed = serde_json::json!({
            "operationId": "01900000-0000-7000-8000-000000000002",
            "targetCodexHome": "C:/temp/codex",
            "status": "completed",
            "startedAt": "2026-07-31T11:00:00Z",
            "updatedAt": "2026-07-31T11:02:00Z",
            "backupDir": "C:/temp/backup",
            "plannedRollouts": [],
            "importedThreadIds": []
        });
        std::fs::write(
            journal_directory.join("checkout.json"),
            serde_json::to_vec(&incomplete).unwrap(),
        )
        .unwrap();
        std::fs::write(
            journal_directory.join("import.json"),
            serde_json::to_vec(&completed).unwrap(),
        )
        .unwrap();

        let points = discover_recovery_points(repository.path()).unwrap();
        assert_eq!(points.len(), 2);
        assert!(points[0].requires_attention);
        assert_eq!(points[0].kind, "checkout");
        assert!(!points[1].requires_attention);
    }

    #[tokio::test]
    async fn namespace_mapping_commands_restore_automatic_selection_after_manual_override() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            "model_provider = \"openai\"\n",
        )
        .unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"integration-secret"}"#,
        )
        .unwrap();

        let remote_id = Uuid::now_v7();
        let mapped_namespace = Uuid::now_v7();
        let manual_namespace = Uuid::now_v7();
        let profiles = RemoteProfileStore::new(&repository);
        let profile = profiles
            .upsert(
                remote_id,
                "Personal".to_string(),
                "https://sync.example.test".to_string(),
            )
            .unwrap();
        let identity = detect_local_identity(&codex_home, &profile.server_url, None).unwrap();
        NamespaceMappingStore::new(&repository)
            .create(
                remote_id,
                mapped_namespace,
                "API key mapping".to_string(),
                identity.api_key_fingerprint.clone(),
                None,
                None,
            )
            .unwrap();
        profiles
            .set_automatic_namespace_selection(remote_id, true)
            .unwrap();

        let automatic = namespace_mapping_state(&repository, &codex_home, remote_id).unwrap();
        assert_eq!(
            automatic.selection.source,
            namespace_mapping::NamespaceSelectionSource::Mapping
        );
        assert_eq!(
            automatic.selection.selected_namespace_id,
            Some(mapped_namespace)
        );

        select_remote_namespace(
            Some(repository.to_string_lossy().into_owned()),
            Some(codex_home.to_string_lossy().into_owned()),
            remote_id.to_string(),
            manual_namespace.to_string(),
        )
        .unwrap();
        let overridden = namespace_mapping_state(&repository, &codex_home, remote_id).unwrap();
        assert_eq!(
            overridden.selection.source,
            namespace_mapping::NamespaceSelectionSource::ManualOverride
        );
        assert_eq!(
            overridden.selection.selected_namespace_id,
            Some(manual_namespace)
        );

        let restored = clear_manual_namespace_override(
            Some(repository.to_string_lossy().into_owned()),
            Some(codex_home.to_string_lossy().into_owned()),
            remote_id.to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            restored.selection.source,
            namespace_mapping::NamespaceSelectionSource::Mapping
        );
        assert_eq!(
            restored.selection.selected_namespace_id,
            Some(mapped_namespace)
        );

        let disabled = set_automatic_namespace_selection(
            Some(repository.to_string_lossy().into_owned()),
            Some(codex_home.to_string_lossy().into_owned()),
            remote_id.to_string(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            disabled.selection.source,
            namespace_mapping::NamespaceSelectionSource::ProfileDefault
        );
        assert_eq!(
            disabled.selection.selected_namespace_id,
            Some(manual_namespace)
        );

        let reenabled = set_automatic_namespace_selection(
            Some(repository.to_string_lossy().into_owned()),
            Some(codex_home.to_string_lossy().into_owned()),
            remote_id.to_string(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            reenabled.selection.source,
            namespace_mapping::NamespaceSelectionSource::Mapping
        );
        assert_eq!(
            reenabled.selection.selected_namespace_id,
            Some(mapped_namespace)
        );
    }
}
