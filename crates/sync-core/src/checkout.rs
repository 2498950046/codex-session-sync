use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, FileTimes, OpenOptions};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::codex::scan_codex_home_with_control;
use crate::local::{
    atomic_write_json, backup_database, copy_verified_object, ensure_repository_layout,
    insert_related_records, insert_thread_row, load_local_snapshot, read_json,
    repository_object_path, restore_database, safe_rollout_path, select_primary_database,
    thread_table_columns, validate_local_snapshot_with_control,
};
use crate::models::{LocalSnapshot, ThreadBundle};
use crate::operation::{OperationControl, OperationProgress};
use crate::protocol::validate_sha256;
use crate::storage_v3::{ContentRef, ContentStore, FilesystemContentStore};
use crate::sync::{TrackingStore, semantic_thread_hash};

pub const CHECKOUT_JOURNAL_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Preparing,
    BackedUp,
    Applying,
    Validating,
    LocalApplied,
    Completed,
    RolledBack,
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDatabaseBackup {
    pub target: PathBuf,
    pub backup: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDirectorySwap {
    pub live: PathBuf,
    pub staged: PathBuf,
    pub backup: PathBuf,
    pub original_existed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutFileSwap {
    pub live: PathBuf,
    pub staged: PathBuf,
    pub backup: PathBuf,
    pub original_existed: bool,
    pub expected_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutTrackingUpdate {
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub expected_generation: Option<u64>,
    pub integrated_head: Option<String>,
    pub activate_namespace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutJournal {
    pub schema_version: u32,
    pub operation_id: String,
    pub snapshot_id: String,
    pub target_codex_home: PathBuf,
    #[serde(default)]
    pub repository_root: PathBuf,
    pub repository_backup_dir: PathBuf,
    pub status: CheckoutStatus,
    pub started_at: String,
    pub updated_at: String,
    pub database_backups: Vec<CheckoutDatabaseBackup>,
    pub directory_swaps: Vec<CheckoutDirectorySwap>,
    #[serde(default)]
    pub file_swaps: Vec<CheckoutFileSwap>,
    pub expected_thread_hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_update: Option<CheckoutTrackingUpdate>,
    /// Temporary backups are always retained until a checkout reaches a
    /// terminal state so a failed write can be rolled back.  When false they
    /// are removed after a successful checkout and are not a recovery point.
    #[serde(default = "default_retain_recovery_backup")]
    pub retain_recovery_backup: bool,
    pub error: Option<String>,
}

const fn default_retain_recovery_backup() -> bool {
    true
}

/// v4 names the checkout journal explicitly. The existing field layout is
/// intentionally retained as the wire-compatible implementation: directory
/// swaps are FilePlans, expected thread hashes are semantic materialized
/// hashes, and `repository_backup_dir` is the pre-operation recovery point.
pub type OperationJournalV4 = CheckoutJournal;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutReport {
    pub operation_id: String,
    pub snapshot_id: String,
    pub thread_count: usize,
    pub backup_dir: PathBuf,
    pub local_backup_dir: PathBuf,
    pub backup_retained: bool,
    pub journal_path: PathBuf,
}

pub fn checkout_local_snapshot(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_with_control(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        &OperationControl::default(),
    )
}

pub fn checkout_local_snapshot_with_control(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    control: &OperationControl,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_with_control_with_backup_retention(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        true,
        control,
    )
}

pub fn checkout_local_snapshot_with_control_with_backup_retention(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    retain_recovery_backup: bool,
    control: &OperationControl,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_internal(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        None,
        &[],
        retain_recovery_backup,
        control,
    )
}

pub fn checkout_local_snapshot_with_tracking(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    tracking_update: CheckoutTrackingUpdate,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_internal(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        Some(tracking_update),
        &[],
        true,
        &OperationControl::default(),
    )
}

pub fn checkout_local_snapshot_with_tracking_control(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    tracking_update: CheckoutTrackingUpdate,
    control: &OperationControl,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_internal(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        Some(tracking_update),
        &[],
        true,
        control,
    )
}

pub fn checkout_local_snapshot_with_tracking_and_projects_control(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    tracking_update: CheckoutTrackingUpdate,
    workspace_project_roots: &[String],
    control: &OperationControl,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_with_tracking_and_projects_control_with_backup_retention(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        tracking_update,
        workspace_project_roots,
        true,
        control,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn checkout_local_snapshot_with_tracking_and_projects_control_with_backup_retention(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    tracking_update: CheckoutTrackingUpdate,
    workspace_project_roots: &[String],
    retain_recovery_backup: bool,
    control: &OperationControl,
) -> Result<CheckoutReport> {
    checkout_local_snapshot_internal(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        Some(tracking_update),
        workspace_project_roots,
        retain_recovery_backup,
        control,
    )
}

#[allow(clippy::too_many_arguments)]
fn checkout_local_snapshot_internal(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    tracking_update: Option<CheckoutTrackingUpdate>,
    workspace_project_roots: &[String],
    retain_recovery_backup: bool,
    control: &OperationControl,
) -> Result<CheckoutReport> {
    if !confirmed_codex_closed {
        bail!("checkout requires confirmation that Codex is fully closed");
    }
    if let Some(update) = &tracking_update {
        validate_tracking_update(update)?;
    }
    let manifest_path = manifest_path.as_ref();
    let target_codex_home = target_codex_home.as_ref().to_path_buf();
    let repository_root = repository_root.as_ref().to_path_buf();
    control.report(OperationProgress::indeterminate(
        "checkout_preflight",
        "Validating checkout snapshot and local state",
    ));
    let validation =
        validate_local_snapshot_with_control(manifest_path, &repository_root, control)?;
    let snapshot = load_local_snapshot(manifest_path)?;
    let current = scan_codex_home_with_control(&target_codex_home, control)?;
    if !current.warnings.is_empty() {
        bail!(
            "checkout is blocked because the local scan returned {} warning(s)",
            current.warnings.len()
        );
    }
    if current.database_paths.is_empty() {
        bail!("target Codex home has no writable threads database");
    }
    let catalog_databases = discover_local_thread_catalog_databases(&target_codex_home)?;
    control.check_cancelled()?;

    ensure_repository_layout(&repository_root)?;
    ensure_no_incomplete_checkout_journal(&repository_root, &target_codex_home)?;
    let operation_id = Uuid::now_v7().to_string();
    let repository_backup_dir = repository_root.join("backups").join(&operation_id);
    let journal_path = repository_root
        .join("journal")
        .join(format!("checkout-{operation_id}.json"));
    let private_root = target_codex_home.join(".codex-session-sync");
    let staging_root = private_root.join("staging").join(&operation_id);
    let local_backup_dir = private_root.join("backups").join(&operation_id);
    fs::create_dir_all(&repository_backup_dir).with_context(|| {
        format!(
            "failed to create checkout repository backup directory {}",
            repository_backup_dir.display()
        )
    })?;
    fs::create_dir_all(&staging_root).with_context(|| {
        format!(
            "failed to create checkout staging directory {}",
            staging_root.display()
        )
    })?;
    fs::create_dir_all(&local_backup_dir).with_context(|| {
        format!(
            "failed to create checkout local backup directory {}",
            local_backup_dir.display()
        )
    })?;

    let directory_swaps = ["sessions", "archived_sessions"]
        .into_iter()
        .map(|name| CheckoutDirectorySwap {
            live: target_codex_home.join(name),
            staged: staging_root.join(name),
            backup: local_backup_dir.join(name),
            original_existed: target_codex_home.join(name).exists(),
        })
        .collect::<Vec<_>>();
    for swap in &directory_swaps {
        fs::create_dir_all(&swap.staged).with_context(|| {
            format!(
                "failed to create staged session directory {}",
                swap.staged.display()
            )
        })?;
    }
    let file_swaps = prepare_project_state_swap(
        &target_codex_home,
        &snapshot.threads,
        workspace_project_roots,
        &staging_root,
        &local_backup_dir,
    )?;

    let expected_thread_hashes = semantic_thread_hashes(&snapshot.threads)?;
    let now = Utc::now().to_rfc3339();
    let mut journal = CheckoutJournal {
        schema_version: CHECKOUT_JOURNAL_SCHEMA_VERSION,
        operation_id: operation_id.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
        target_codex_home: target_codex_home.clone(),
        repository_root: repository_root.clone(),
        repository_backup_dir: repository_backup_dir.clone(),
        status: CheckoutStatus::Preparing,
        started_at: now.clone(),
        updated_at: now,
        database_backups: Vec::new(),
        directory_swaps,
        file_swaps,
        expected_thread_hashes,
        tracking_update,
        retain_recovery_backup,
        error: None,
    };
    write_checkout_journal(&journal_path, &journal).with_context(|| {
        format!(
            "failed to write checkout journal {}",
            journal_path.display()
        )
    })?;

    let result = (|| -> Result<()> {
        stage_rollouts(&snapshot, &repository_root, &staging_root, control)
            .context("failed to stage merged conversations for local checkout")?;
        control.check_cancelled()?;
        control.report(OperationProgress::indeterminate(
            "checkout_backup",
            "Creating recoverable database backups",
        ));
        let database_dir = repository_backup_dir.join("databases");
        fs::create_dir_all(&database_dir).with_context(|| {
            format!(
                "failed to create checkout database backup directory {}",
                database_dir.display()
            )
        })?;
        let mut databases_to_backup = current.database_paths.clone();
        for database in &catalog_databases {
            if !databases_to_backup.contains(database) {
                databases_to_backup.push(database.clone());
            }
        }
        for (index, database) in databases_to_backup.iter().enumerate() {
            let backup = database_dir.join(format!("{index}.sqlite"));
            backup_database(database, &backup).with_context(|| {
                format!(
                    "failed to back up target database {} to {}",
                    database.display(),
                    backup.display()
                )
            })?;
            journal.database_backups.push(CheckoutDatabaseBackup {
                target: database.clone(),
                backup,
            });
            journal.updated_at = Utc::now().to_rfc3339();
            write_checkout_journal(&journal_path, &journal)?;
        }
        journal.status = CheckoutStatus::BackedUp;
        journal.updated_at = Utc::now().to_rfc3339();
        write_checkout_journal(&journal_path, &journal)?;
        control.check_cancelled()?;

        control.report(OperationProgress {
            phase: "checkout_apply".to_string(),
            message: "Applying the selected namespace".to_string(),
            completed: 0,
            total: None,
            unit: "steps".to_string(),
            cancellable: false,
        });
        journal.status = CheckoutStatus::Applying;
        journal.updated_at = Utc::now().to_rfc3339();
        write_checkout_journal(&journal_path, &journal)?;
        apply_directory_swaps(&journal.directory_swaps)
            .context("failed to replace Codex session directories")?;
        apply_file_swaps(&journal.file_swaps).context("failed to update Codex project state")?;
        replace_databases(
            &current.database_paths,
            &snapshot.threads,
            &target_codex_home,
        )
        .context("failed to update Codex conversation database records")?;
        invalidate_local_thread_catalogs(&catalog_databases)
            .context("failed to invalidate Codex local conversation catalog")?;

        journal.status = CheckoutStatus::Validating;
        journal.updated_at = Utc::now().to_rfc3339();
        write_checkout_journal(&journal_path, &journal)?;
        let non_cancellable = control.non_cancellable();
        validate_checkout_result(&snapshot, &target_codex_home, &non_cancellable)?;
        validate_file_swaps(&journal.file_swaps)?;
        journal.status = CheckoutStatus::LocalApplied;
        journal.updated_at = Utc::now().to_rfc3339();
        write_checkout_journal(&journal_path, &journal)?;
        apply_tracking_update(&journal)?;
        Ok(())
    })();

    if let Err(error) = result {
        let message = error.to_string();
        let rollback = rollback_checkout(&journal_path, &mut journal, Some(message.clone()));
        if let Err(rollback_error) = rollback {
            return Err(error.context(format!("checkout rollback also failed: {rollback_error}")));
        }
        return Err(error);
    }

    journal.status = CheckoutStatus::Completed;
    journal.updated_at = Utc::now().to_rfc3339();
    write_checkout_journal(&journal_path, &journal)?;
    let _ = fs::remove_dir_all(&staging_root);
    if !retain_recovery_backup {
        let _ = fs::remove_dir_all(&repository_backup_dir);
        let _ = fs::remove_dir_all(&local_backup_dir);
    }
    Ok(CheckoutReport {
        operation_id,
        snapshot_id: validation.snapshot_id,
        thread_count: snapshot.threads.len(),
        backup_dir: repository_backup_dir,
        local_backup_dir,
        backup_retained: retain_recovery_backup,
        journal_path,
    })
}

pub fn recover_checkout_operation(
    journal_path: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<CheckoutJournal> {
    if !confirmed_codex_closed {
        bail!("checkout recovery requires confirmation that Codex is fully closed");
    }
    let journal_path = journal_path.as_ref();
    let mut journal: CheckoutJournal = read_json(journal_path)?;
    validate_checkout_journal(&journal)?;
    if matches!(
        journal.status,
        CheckoutStatus::Completed | CheckoutStatus::RolledBack
    ) {
        return Ok(journal);
    }
    if journal.status == CheckoutStatus::LocalApplied {
        if let Err(error) = validate_checkout_journal_result(&journal) {
            return reject_local_applied_recovery(
                journal_path,
                &mut journal,
                error.context(
                    "live Codex Home does not match the journal target; refusing recovery without modifying local data",
                ),
            );
        }
        if journal.tracking_update.is_some()
            && let Err(error) = apply_tracking_update(&journal)
        {
            return reject_local_applied_recovery(
                journal_path,
                &mut journal,
                error.context(
                    "failed to reconcile applied checkout tracking; refusing recovery without rolling back local data",
                ),
            );
        }
        journal.status = CheckoutStatus::Completed;
        journal.updated_at = Utc::now().to_rfc3339();
        journal.error = None;
        write_checkout_journal(journal_path, &journal)?;
        return Ok(journal);
    }
    rollback_checkout(
        journal_path,
        &mut journal,
        Some("Recovered an incomplete checkout operation".to_string()),
    )?;
    Ok(journal)
}

fn ensure_no_incomplete_checkout_journal(
    repository_root: &Path,
    target_codex_home: &Path,
) -> Result<()> {
    let journal_dir = repository_root.join("journal");
    for entry in fs::read_dir(&journal_dir).with_context(|| {
        format!(
            "failed to scan checkout journals in {}",
            journal_dir.display()
        )
    })? {
        let path = entry?.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file_name.starts_with("checkout-") || !file_name.ends_with(".json") {
            continue;
        }
        let journal: CheckoutJournal = read_json(&path).with_context(|| {
            format!(
                "cannot verify whether checkout journal {} is complete",
                path.display()
            )
        })?;
        validate_checkout_journal(&journal)?;
        if matches!(
            journal.status,
            CheckoutStatus::Completed | CheckoutStatus::RolledBack
        ) {
            continue;
        }
        if checkout_targets_same_home(target_codex_home, &journal.target_codex_home)? {
            bail!(
                "checkout is blocked by unfinished checkout operation {} at {}; recover it before starting another checkout for this Codex Home",
                journal.operation_id,
                path.display()
            );
        }
    }
    Ok(())
}

fn checkout_targets_same_home(left: &Path, right: &Path) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    let left = fs::canonicalize(left)
        .with_context(|| format!("failed to normalize Codex Home {}", left.display()))?;
    let right = match fs::canonicalize(right) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to normalize Codex Home {}", right.display()));
        }
    };
    let left = normalized_path_identity(&left);
    let right = normalized_path_identity(&right);
    Ok(left == right)
}

fn normalized_path_identity(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();
    normalized
}

fn reject_local_applied_recovery(
    journal_path: &Path,
    journal: &mut CheckoutJournal,
    error: anyhow::Error,
) -> Result<CheckoutJournal> {
    journal.updated_at = Utc::now().to_rfc3339();
    journal.error = Some(format!("{error:#}"));
    write_checkout_journal(journal_path, journal)
        .context("failed to persist rejected checkout recovery state")?;
    Err(error)
}

fn stage_rollouts(
    snapshot: &LocalSnapshot,
    repository_root: &Path,
    staging_root: &Path,
    control: &OperationControl,
) -> Result<()> {
    for (index, thread) in snapshot.threads.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "checkout_stage".to_string(),
            message: thread.title.clone(),
            completed: index as u64,
            total: Some(snapshot.threads.len() as u64),
            unit: "threads".to_string(),
            cancellable: true,
        });
        let relative = safe_rollout_path(thread)?;
        let target = staging_root.join(relative);
        if target.exists() {
            bail!(
                "checkout contains duplicate rollout path {}",
                target.display()
            );
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create staged rollout parent directory {} for thread {}",
                    parent.display(),
                    thread.thread_id
                )
            })?;
        }
        if let Some(storage) = thread.rollout.storage.clone() {
            if repository_root.join("format.json").is_file() {
                let normalized = target.with_extension("normalized.tmp");
                let v4_storage = match storage {
                    crate::storage_v3::StorageRef::Whole { object_sha256 } => {
                        crate::storage_v4::StorageRefV4::Whole { object_sha256 }
                    }
                    crate::storage_v3::StorageRef::Chunked { manifest_sha256 } => {
                        crate::storage_v4::StorageRefV4::Chunked { manifest_sha256 }
                    }
                };
                crate::storage_v4::FilesystemContentStoreV4::open(repository_root.to_path_buf())?
                    .materialize(
                    &crate::storage_v4::ContentRefV4 {
                        logical_sha256: thread.rollout.sha256.clone(),
                        byte_length: thread.rollout.byte_length,
                        storage: v4_storage,
                        media_type: thread.rollout.media_type.clone(),
                        logical_path: thread.rollout.logical_path.clone(),
                    },
                    &normalized,
                )?;
                let context = crate::storage_v4::MachineContextV4 {
                    configured_provider: thread
                        .model_provider
                        .clone()
                        .unwrap_or_else(|| "openai".to_string()),
                    workspace_path: thread.workspace.source_path.clone(),
                    target_codex_home: staging_root.to_path_buf(),
                };
                let result = crate::storage_v4::materialize_rollout_file(
                    &normalized,
                    &target,
                    &thread.thread_id,
                    &context,
                    control,
                );
                let _ = fs::remove_file(normalized);
                result?;
            } else {
                FilesystemContentStore::open(repository_root.to_path_buf())?.materialize(
                    &ContentRef {
                        logical_sha256: thread.rollout.sha256.clone(),
                        byte_length: thread.rollout.byte_length,
                        storage,
                        media_type: Some(thread.rollout.media_type.clone()),
                        logical_path: thread.rollout.logical_path.clone(),
                    },
                    &target,
                    control,
                )?;
            }
        } else {
            let source = repository_object_path(repository_root, &thread.rollout.sha256)?;
            copy_verified_object(&source, &target, &thread.rollout.sha256, Some(control))?;
        }
        if !repository_root.join("format.json").is_file()
            && fs::metadata(&target)
                .with_context(|| format!("staged rollout is missing: {}", target.display()))?
                .len()
                != thread.rollout.byte_length
        {
            bail!("staged rollout has an unexpected byte length");
        }
        restore_rollout_modified_time(&target, thread_modified_at_ms(thread))?;
    }
    Ok(())
}

fn thread_modified_at_ms(thread: &ThreadBundle) -> Option<i64> {
    thread.updated_at_ms.or(thread.created_at_ms).or_else(|| {
        let row = thread
            .related_records
            .tables
            .get("threads")?
            .first()?
            .as_object()?;
        row.get("updated_at_ms")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| {
                row.get("updated_at")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|value| value.checked_mul(1_000))
            })
            .or_else(|| row.get("created_at_ms").and_then(serde_json::Value::as_i64))
            .or_else(|| {
                row.get("created_at")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|value| value.checked_mul(1_000))
            })
    })
}

fn restore_rollout_modified_time(path: &Path, timestamp_ms: Option<i64>) -> Result<()> {
    let Some(timestamp_ms) = timestamp_ms.filter(|timestamp| *timestamp >= 0) else {
        return Ok(());
    };
    let timestamp = std::time::UNIX_EPOCH
        .checked_add(Duration::from_millis(timestamp_ms as u64))
        .context("rollout timestamp is outside the supported filesystem range")?;
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open staged rollout {}", path.display()))?;
    file.set_times(FileTimes::new().set_modified(timestamp))
        .with_context(|| format!("failed to restore rollout timestamp {}", path.display()))?;
    Ok(())
}

fn apply_directory_swaps(swaps: &[CheckoutDirectorySwap]) -> Result<()> {
    for swap in swaps {
        if swap.backup.exists() {
            bail!(
                "checkout backup path already exists: {}",
                swap.backup.display()
            );
        }
        if swap.live.exists() {
            rename_checkout_directory(&swap.live, &swap.backup).with_context(|| {
                format!(
                    "failed to move current session directory {} to backup",
                    swap.live.display()
                )
            })?;
        }
        rename_checkout_directory(&swap.staged, &swap.live).with_context(|| {
            format!(
                "failed to install staged session directory {}",
                swap.live.display()
            )
        })?;
    }
    Ok(())
}

fn rename_checkout_directory(source: &Path, destination: &Path) -> io::Result<()> {
    const WINDOWS_RENAME_ATTEMPTS: usize = 20;
    const WINDOWS_RENAME_RETRY_DELAY: Duration = Duration::from_millis(100);

    let mut attempt = 1;
    loop {
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if cfg!(windows)
                    && attempt < WINDOWS_RENAME_ATTEMPTS
                    && is_transient_windows_rename_error(&error) =>
            {
                thread::sleep(WINDOWS_RENAME_RETRY_DELAY);
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

const MAX_CODEX_GLOBAL_STATE_BYTES: u64 = 16 * 1024 * 1024;

fn prepare_project_state_swap(
    codex_home: &Path,
    threads: &[ThreadBundle],
    workspace_project_roots: &[String],
    staging_root: &Path,
    local_backup_dir: &Path,
) -> Result<Vec<CheckoutFileSwap>> {
    let live = codex_home.join(".codex-global-state.json");
    if !live.is_file() {
        return Ok(Vec::new());
    }
    let metadata = fs::metadata(&live)?;
    if metadata.len() > MAX_CODEX_GLOBAL_STATE_BYTES {
        bail!(
            "Codex global state is too large to update safely: {} bytes",
            metadata.len()
        );
    }
    let bytes = fs::read(&live)?;
    let mut state: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse Codex global state {}", live.display()))?;
    if !apply_project_assignments(&mut state, threads, workspace_project_roots)? {
        return Ok(Vec::new());
    }
    let output = serde_json::to_vec(&state)?;
    let expected_sha256 = format!("sha256:{}", hex::encode(Sha256::digest(&output)));
    let staged = staging_root.join("project-state").join("global-state.json");
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staged)?;
    use std::io::Write as _;
    file.write_all(&output)?;
    file.sync_all()?;
    drop(file);
    Ok(vec![CheckoutFileSwap {
        live,
        staged,
        backup: local_backup_dir
            .join("project-state")
            .join("global-state.json"),
        original_existed: true,
        expected_sha256,
    }])
}

#[derive(Debug)]
struct WorkspaceProjectRoot {
    display: String,
    identity: String,
    project_id: String,
}

fn apply_project_assignments(
    state: &mut Value,
    threads: &[ThreadBundle],
    workspace_project_roots: &[String],
) -> Result<bool> {
    let state = state
        .as_object_mut()
        .context("Codex global state is not a JSON object")?;
    // A workspace mapping describes how paths are translated between
    // machines; it is not necessarily a Codex project root.  A broad mapping
    // such as `D:/yaxin -> F:/history/yaxin` can contain several projects
    // (`yxzsApplet`, `yxzsJava`, ...).  Keep mapping roots as fallbacks, and
    // promote a concrete child path when multiple threads use it.  A
    // singleton child path is commonly just a task-specific subdirectory and
    // must stay in its mapped parent project (for example `workspace/new`).
    let mut root_candidates = workspace_project_roots.to_vec();
    let mapping_identities = workspace_project_roots
        .iter()
        .map(|root| workspace_path_identity(&native_workspace_path(root)))
        .filter(|identity| !identity.is_empty())
        .collect::<Vec<_>>();
    let mut concrete_paths = BTreeMap::<String, (String, usize)>::new();
    for path in threads.iter().filter_map(checkout_thread_workspace_path) {
        let display = native_workspace_path(path);
        let identity = workspace_path_identity(&display);
        if identity.is_empty() {
            continue;
        }
        concrete_paths
            .entry(identity)
            .and_modify(|(_, count)| *count += 1)
            .or_insert((display, 1));
    }
    for (identity, (display, count)) in concrete_paths {
        let covered_by_mapping = mapping_identities
            .iter()
            .any(|prefix| workspace_path_matches(&identity, prefix));
        if count >= 2 || !covered_by_mapping {
            root_candidates.push(display);
        }
    }
    let mut roots = root_candidates
        .iter()
        .filter_map(|root| {
            let display = native_workspace_path(root);
            let identity = workspace_path_identity(&display);
            (!identity.is_empty()).then_some(WorkspaceProjectRoot {
                project_id: format!(
                    "local-{}",
                    &hex::encode(Sha256::digest(display.as_bytes()))[..32]
                ),
                display,
                identity,
            })
        })
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| left.identity.cmp(&right.identity));
    roots.dedup_by(|left, right| left.identity == right.identity);

    if let Some(projects) = state.get("local-projects").and_then(Value::as_object) {
        for root in &mut roots {
            if let Some((project_id, _)) = projects.iter().find(|(_, project)| {
                project
                    .get("rootPaths")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|path| workspace_path_identity(path) == root.identity)
            }) {
                root.project_id.clone_from(project_id);
            }
        }
    }

    let assignments = threads
        .iter()
        .filter_map(|thread| {
            let cwd = checkout_thread_workspace_path(thread)?;
            let cwd_display = native_workspace_path(cwd);
            let cwd_identity = workspace_path_identity(&cwd_display);
            let root = roots
                .iter()
                .filter(|root| workspace_path_matches(&cwd_identity, &root.identity))
                .max_by_key(|root| root.identity.len())?;
            Some((
                thread.thread_id.clone(),
                cwd_display,
                root.project_id.clone(),
                root.identity.clone(),
            ))
        })
        .collect::<Vec<_>>();
    if assignments.is_empty() {
        return Ok(false);
    }
    let used_root_ids = assignments
        .iter()
        .map(|(_, _, _, identity)| identity.as_str())
        .collect::<BTreeSet<_>>();
    let now = Utc::now().timestamp_millis();
    let mut changed = false;

    let projects = json_object_field(state, "local-projects")?;
    for root in roots
        .iter()
        .filter(|root| used_root_ids.contains(root.identity.as_str()))
    {
        if let Some(existing) = projects.get(&root.project_id) {
            let same_root = existing
                .get("rootPaths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|path| workspace_path_identity(path) == root.identity);
            if !same_root {
                bail!("Codex project ID collision for {}", root.display);
            }
        } else {
            let name = Path::new(&root.display)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(&root.display);
            projects.insert(
                root.project_id.clone(),
                json!({
                    "id": root.project_id,
                    "name": name,
                    "rootPaths": [root.display],
                    "createdAt": now,
                    "updatedAt": now,
                }),
            );
            changed = true;
        }
    }

    let order = json_array_field(state, "project-order")?;
    for root in roots
        .iter()
        .filter(|root| used_root_ids.contains(root.identity.as_str()))
    {
        if !order
            .iter()
            .any(|value| value.as_str() == Some(&root.project_id))
        {
            order.push(Value::String(root.project_id.clone()));
            changed = true;
        }
    }

    let saved_roots = json_array_field(state, "electron-saved-workspace-roots")?;
    for root in roots
        .iter()
        .filter(|root| used_root_ids.contains(root.identity.as_str()))
    {
        if !saved_roots
            .iter()
            .filter_map(Value::as_str)
            .any(|path| workspace_path_identity(path) == root.identity)
        {
            saved_roots.push(Value::String(root.display.clone()));
            changed = true;
        }
    }

    let thread_assignments = json_object_field(state, "thread-project-assignments")?;
    for (thread_id, cwd, project_id, _) in &assignments {
        let assignment = json!({
            "projectKind": "local",
            "projectId": project_id,
            "cwd": cwd,
            "pendingCoreUpdate": false,
        });
        if thread_assignments.get(thread_id) != Some(&assignment) {
            thread_assignments.insert(thread_id.clone(), assignment);
            changed = true;
        }
    }

    if let Some(projectless) = state
        .get_mut("projectless-thread-ids")
        .and_then(Value::as_array_mut)
    {
        let before = projectless.len();
        projectless.retain(|value| {
            value.as_str().is_none_or(|thread_id| {
                !assignments
                    .iter()
                    .any(|(assigned, _, _, _)| assigned == thread_id)
            })
        });
        changed |= before != projectless.len();
    }
    Ok(changed)
}

fn json_object_field<'a>(
    state: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    if !state.contains_key(key) {
        state.insert(key.to_string(), Value::Object(Map::new()));
    }
    state
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .with_context(|| format!("Codex global state field {key} is not an object"))
}

fn json_array_field<'a>(
    state: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Vec<Value>> {
    if !state.contains_key(key) {
        state.insert(key.to_string(), Value::Array(Vec::new()));
    }
    state
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .with_context(|| format!("Codex global state field {key} is not an array"))
}

fn checkout_thread_workspace_path(thread: &ThreadBundle) -> Option<&str> {
    thread.workspace.source_path.as_deref().or_else(|| {
        thread
            .related_records
            .tables
            .get("threads")?
            .first()?
            .get("cwd")?
            .as_str()
    })
}

fn native_workspace_path(path: &str) -> String {
    #[cfg(windows)]
    let path = path.replace('/', "\\");
    #[cfg(not(windows))]
    let path = path.replace('\\', "/");
    path
}

fn workspace_path_identity(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.strip_prefix("//?/").unwrap_or(&normalized);
    let normalized = normalized.trim_end_matches('/');
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();
    normalized.to_string()
}

fn workspace_path_matches(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn apply_file_swaps(swaps: &[CheckoutFileSwap]) -> Result<()> {
    for swap in swaps {
        if swap.backup.exists() {
            bail!(
                "checkout backup path already exists: {}",
                swap.backup.display()
            );
        }
        if let Some(parent) = swap.backup.parent() {
            fs::create_dir_all(parent)?;
        }
        if swap.live.exists() {
            rename_checkout_directory(&swap.live, &swap.backup).with_context(|| {
                format!(
                    "failed to back up local Codex state {}",
                    swap.live.display()
                )
            })?;
        }
        rename_checkout_directory(&swap.staged, &swap.live).with_context(|| {
            format!(
                "failed to install local Codex state {}",
                swap.live.display()
            )
        })?;
    }
    Ok(())
}

fn validate_file_swaps(swaps: &[CheckoutFileSwap]) -> Result<()> {
    for swap in swaps {
        let actual = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(fs::read(&swap.live)?))
        );
        if actual != swap.expected_sha256 {
            bail!(
                "local Codex state hash mismatch for {}: expected {}, got {}",
                swap.live.display(),
                swap.expected_sha256,
                actual
            );
        }
    }
    Ok(())
}

fn is_transient_windows_rename_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::WouldBlock
        || matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

fn replace_databases(
    database_paths: &[PathBuf],
    threads: &[ThreadBundle],
    target_codex_home: &Path,
) -> Result<()> {
    let primary = select_primary_database(database_paths)?;
    for database in database_paths {
        let mut connection = Connection::open(database)
            .with_context(|| format!("failed to open target database {}", database.display()))?;
        connection.busy_timeout(std::time::Duration::from_millis(250))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("cannot acquire SQLite write lock; make sure Codex is fully closed")?;
        transaction.execute("DELETE FROM threads", [])?;
        transaction.commit()?;
    }

    let mut connection = Connection::open(&primary).with_context(|| {
        format!(
            "failed to reopen primary target database {}",
            primary.display()
        )
    })?;
    connection.busy_timeout(std::time::Duration::from_millis(250))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    let columns = thread_table_columns(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for thread in threads {
        let relative = safe_rollout_path(thread)?;
        let rollout_path = target_codex_home.join(relative);
        insert_thread_row(
            &transaction,
            &columns,
            thread,
            &rollout_path,
            target_codex_home,
        )?;
    }
    for thread in threads {
        insert_related_records(&transaction, thread)?;
    }
    transaction.commit()?;
    Ok(())
}

/// Clear Codex Desktop's rebuildable local sidebar catalogue for one Codex Home.
///
/// The catalogue is derived from rollout and thread metadata. It must be
/// invalidated whenever those authoritative records are changed outside of
/// Codex Desktop so the next launch does not retain stale sidebar entries.
pub fn invalidate_codex_local_thread_catalog(codex_home: impl AsRef<Path>) -> Result<()> {
    let databases = discover_local_thread_catalog_databases(codex_home.as_ref())?;
    invalidate_local_thread_catalogs(&databases)
}

pub(crate) fn discover_local_thread_catalog_databases(codex_home: &Path) -> Result<Vec<PathBuf>> {
    let sqlite_home = std::env::var_os("CODEX_SQLITE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| codex_home.to_path_buf());
    let candidates = [
        sqlite_home.join("sqlite").join("codex-dev.db"),
        sqlite_home.join("codex-dev.db"),
    ];
    let mut databases = Vec::new();
    for path in candidates {
        if !path.is_file() || databases.contains(&path) {
            continue;
        }
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| {
                format!("failed to inspect Codex thread catalog {}", path.display())
            })?;
        let table_count: u32 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN (
                 'local_thread_catalog',
                 'local_thread_catalog_sync_state',
                 'local_thread_catalog_metadata'
             )",
            [],
            |row| row.get(0),
        )?;
        if table_count == 3 {
            databases.push(path);
        }
    }
    Ok(databases)
}

pub(crate) fn invalidate_local_thread_catalogs(databases: &[PathBuf]) -> Result<()> {
    for database in databases {
        let mut connection = Connection::open(database).with_context(|| {
            format!("failed to open Codex thread catalog {}", database.display())
        })?;
        connection.busy_timeout(std::time::Duration::from_millis(250))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("cannot lock the Codex thread catalog; make sure Codex is fully closed")?;
        let sync_state_columns = table_columns(&transaction, "local_thread_catalog_sync_state")?;
        for required in ["host_id", "watermark_updated_at", "initial_build_complete"] {
            if !sync_state_columns.contains(required) {
                bail!(
                    "Codex thread catalog {} is missing required column {required}",
                    database.display()
                );
            }
        }
        transaction.execute(
            "DELETE FROM local_thread_catalog WHERE host_id = 'local'",
            [],
        )?;
        if sync_state_columns.contains("last_full_reconciled_at") {
            transaction.execute(
                "UPDATE local_thread_catalog_sync_state
                 SET watermark_updated_at = NULL,
                     initial_build_complete = 0,
                     last_full_reconciled_at = NULL
                 WHERE host_id = 'local'",
                [],
            )?;
        } else {
            transaction.execute(
                "UPDATE local_thread_catalog_sync_state
                 SET watermark_updated_at = NULL,
                     initial_build_complete = 0
                 WHERE host_id = 'local'",
                [],
            )?;
        }
        transaction.execute(
            "INSERT INTO local_thread_catalog_metadata (id, catalog_revision)
             VALUES (1, 1)
             ON CONFLICT(id) DO UPDATE SET
                 catalog_revision = local_thread_catalog_metadata.catalog_revision + 1",
            [],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn table_columns(transaction: &rusqlite::Transaction<'_>, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(columns)
}

fn validate_checkout_result(
    snapshot: &LocalSnapshot,
    target_codex_home: &Path,
    control: &OperationControl,
) -> Result<()> {
    let actual = scan_codex_home_with_control(target_codex_home, control)?;
    if !actual.warnings.is_empty() {
        bail!(
            "post-checkout scan returned {} warning(s)",
            actual.warnings.len()
        );
    }
    let expected = semantic_thread_hashes(&snapshot.threads)?;
    let actual = semantic_thread_hashes(&actual.threads)?;
    validate_thread_hashes(&expected, &actual)
}

fn validate_checkout_journal_result(journal: &CheckoutJournal) -> Result<()> {
    let actual =
        scan_codex_home_with_control(&journal.target_codex_home, &OperationControl::default())?;
    if !actual.warnings.is_empty() {
        bail!(
            "recovered checkout scan returned {} warning(s)",
            actual.warnings.len()
        );
    }
    let actual = semantic_thread_hashes(&actual.threads)?;
    validate_thread_hashes(&journal.expected_thread_hashes, &actual)?;
    validate_file_swaps(&journal.file_swaps)
}

fn semantic_thread_hashes(threads: &[ThreadBundle]) -> Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for thread in threads {
        let mut semantic = thread.clone();
        if let Some(source) = semantic.rollout.source_path.as_deref() {
            let file = std::fs::File::open(source)?;
            let normalized = crate::storage_v4::normalize_rollout_reader(
                BufReader::new(file),
                &semantic.thread_id,
                crate::storage_v4::CHUNK_SIZE_V4 as usize,
                |_| Ok(()),
                &OperationControl::default(),
            )?;
            semantic.rollout.sha256 = normalized.logical_sha256;
            semantic.rollout.byte_length = normalized.byte_length;
        }
        semantic.model_provider = None;
        // The scanner can observe the materialized machine path, but it cannot
        // reconstruct the remote logical workspace identity stored in a v4
        // descriptor. Checkout validation therefore compares the normalized
        // rollout and portable thread data, while workspace materialization is
        // validated by the staged file/database apply paths themselves.
        semantic.workspace.logical_id = None;
        semantic.workspace.source_path = None;
        semantic.rollout.source_path = None;
        semantic.rollout.storage = None;
        semantic.rollout.logical_path = None;
        for attachment in &mut semantic.attachments {
            attachment.source_path = None;
            attachment.storage = None;
            attachment.logical_path = None;
        }
        semantic.related_records.source_database = None;
        for rows in semantic.related_records.tables.values_mut() {
            for row in rows {
                strip_machine_fields(row);
            }
        }
        if hashes
            .insert(thread.thread_id.clone(), semantic_thread_hash(&semantic)?)
            .is_some()
        {
            bail!("checkout contains duplicate thread ID {}", thread.thread_id);
        }
    }
    Ok(hashes)
}

fn strip_machine_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in ["model_provider", "rollout_path", "codex_home", "cwd"] {
                object.remove(key);
            }
            for value in object.values_mut() {
                strip_machine_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_machine_fields(value);
            }
        }
        _ => {}
    }
}

fn validate_thread_hashes(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> Result<()> {
    if actual != expected {
        let expected_ids = expected.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let actual_ids = actual.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let changed = expected
            .iter()
            .filter_map(|(thread_id, expected_hash)| {
                actual
                    .get(thread_id)
                    .filter(|actual_hash| *actual_hash != expected_hash)
                    .map(|_| thread_id.as_str())
            })
            .collect::<Vec<_>>();
        bail!(
            "post-checkout validation mismatch: expected {} threads, found {}; missing {:?}; unexpected {:?}; changed {:?}",
            expected.len(),
            actual.len(),
            expected_ids.difference(&actual_ids).collect::<Vec<_>>(),
            actual_ids.difference(&expected_ids).collect::<Vec<_>>(),
            changed
        );
    }
    Ok(())
}

fn apply_tracking_update(journal: &CheckoutJournal) -> Result<()> {
    let Some(update) = &journal.tracking_update else {
        return Ok(());
    };
    let tracking = TrackingStore::open(&journal.repository_root)?;
    tracking
        .reconcile_checkout(
            &journal.target_codex_home,
            update.remote_id,
            update.namespace_id,
            update.expected_generation,
            update.integrated_head.as_deref(),
            update.activate_namespace,
        )
        .context("failed to reconcile checkout tracking state")?;
    Ok(())
}

fn rollback_checkout(
    journal_path: &Path,
    journal: &mut CheckoutJournal,
    error: Option<String>,
) -> Result<()> {
    let local_write_started = matches!(
        journal.status,
        CheckoutStatus::Applying
            | CheckoutStatus::Validating
            | CheckoutStatus::LocalApplied
            | CheckoutStatus::RecoveryRequired
    );
    let failed_root = journal
        .target_codex_home
        .join(".codex-session-sync")
        .join("failed")
        .join(&journal.operation_id);
    fs::create_dir_all(&failed_root)?;
    let mut failures = Vec::new();
    for swap in journal.file_swaps.iter().rev() {
        let swap_started = local_write_started
            && (swap.backup.exists() || (!swap.original_existed && swap.live.exists()));
        if swap_started && swap.live.exists() {
            let failed = failed_root.join(
                swap.live
                    .file_name()
                    .context("checkout live file has no file name")?,
            );
            if failed.exists() {
                failures.push(format!(
                    "failed-state path already exists: {}",
                    failed.display()
                ));
            } else if let Err(error) = rename_checkout_directory(&swap.live, &failed) {
                failures.push(format!(
                    "failed to preserve checkout file {}: {error}",
                    swap.live.display()
                ));
            }
        }
        if swap_started
            && swap.backup.exists()
            && !swap.live.exists()
            && let Err(error) = rename_checkout_directory(&swap.backup, &swap.live)
        {
            failures.push(format!(
                "failed to restore local Codex state {}: {error}",
                swap.live.display()
            ));
        }
        let _ = fs::remove_file(&swap.staged);
    }
    for swap in journal.directory_swaps.iter().rev() {
        let swap_started = local_write_started
            && (swap.backup.exists() || (!swap.original_existed && swap.live.exists()));
        if swap_started && swap.live.exists() {
            let failed = failed_root.join(
                swap.live
                    .file_name()
                    .context("checkout live directory has no file name")?,
            );
            if failed.exists() {
                failures.push(format!(
                    "failed-state path already exists: {}",
                    failed.display()
                ));
            } else if let Err(error) = rename_checkout_directory(&swap.live, &failed) {
                failures.push(format!(
                    "failed to preserve checkout directory {}: {error}",
                    swap.live.display()
                ));
            }
        }
        if swap_started
            && swap.backup.exists()
            && !swap.live.exists()
            && let Err(error) = rename_checkout_directory(&swap.backup, &swap.live)
        {
            failures.push(format!(
                "failed to restore session directory {}: {error}",
                swap.live.display()
            ));
        }
        let _ = fs::remove_dir_all(&swap.staged);
    }
    if local_write_started {
        for backup in &journal.database_backups {
            if let Err(error) = restore_database(&backup.target, &backup.backup) {
                failures.push(format!(
                    "failed to restore database {}: {error}",
                    backup.target.display()
                ));
            }
        }
    }
    journal.status = if failures.is_empty() {
        CheckoutStatus::RolledBack
    } else {
        CheckoutStatus::RecoveryRequired
    };
    journal.updated_at = Utc::now().to_rfc3339();
    journal.error = Some(match (error, failures.is_empty()) {
        (Some(error), true) => error,
        (Some(error), false) => format!("{error}; {}", failures.join("; ")),
        (None, true) => "Checkout was rolled back".to_string(),
        (None, false) => failures.join("; "),
    });
    write_checkout_journal(journal_path, journal)?;
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "checkout recovery requires manual attention: {}",
            failures.join("; ")
        )
    }
}

fn validate_checkout_journal(journal: &CheckoutJournal) -> Result<()> {
    if journal.schema_version != CHECKOUT_JOURNAL_SCHEMA_VERSION {
        bail!(
            "unsupported checkout journal schema version {}",
            journal.schema_version
        );
    }
    if journal.operation_id.trim().is_empty() {
        bail!("checkout journal has no operation ID");
    }
    if let Some(update) = &journal.tracking_update {
        if journal.repository_root.as_os_str().is_empty() {
            bail!("checkout tracking update has no repository root");
        }
        validate_tracking_update(update)?;
    }
    for swap in &journal.file_swaps {
        validate_sha256(&swap.expected_sha256).map_err(|_| {
            anyhow::anyhow!(
                "invalid checkout file hash {} for {}",
                swap.expected_sha256,
                swap.live.display()
            )
        })?;
    }
    Ok(())
}

fn validate_tracking_update(update: &CheckoutTrackingUpdate) -> Result<()> {
    if update.remote_id.get_version_num() != 7 {
        bail!("checkout tracking remote ID must be a UUIDv7");
    }
    if update.namespace_id.get_version_num() != 7 {
        bail!("checkout tracking namespace ID must be a UUIDv7");
    }
    if let Some(head) = &update.integrated_head {
        validate_sha256(head)
            .map_err(|_| anyhow::anyhow!("invalid checkout tracking head {head}"))?;
    }
    if update.expected_generation == Some(u64::MAX) {
        bail!("checkout tracking generation cannot be incremented");
    }
    Ok(())
}

fn write_checkout_journal(path: &Path, journal: &CheckoutJournal) -> Result<()> {
    validate_checkout_journal(journal)?;
    atomic_write_json(path, journal)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::Write;

    use rusqlite::params;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;
    use crate::local::{create_local_snapshot, store_local_snapshot};
    use crate::models::{
        ContentObject, LOCAL_SNAPSHOT_SCHEMA_VERSION, RelatedRecords, THREAD_BUNDLE_SCHEMA_VERSION,
        WorkspaceRef,
    };

    #[test]
    fn staged_rollout_modified_time_can_be_restored_from_thread_timestamp() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout-thread.jsonl");
        fs::write(&rollout, b"rollout").unwrap();
        let timestamp_ms = 1_700_000_100_123_i64;

        restore_rollout_modified_time(&rollout, Some(timestamp_ms)).unwrap();

        let actual = fs::metadata(&rollout)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!((actual - timestamp_ms).abs() <= 2_000);
    }

    #[test]
    fn exact_checkout_replaces_sessions_and_keeps_recoverable_backup() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        create_local_snapshot(&codex_home, &repository, true).unwrap();

        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();

        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads.len(), 1);
        assert_eq!(scanned.threads[0].thread_id, "new");
        assert!(report.local_backup_dir.join("sessions").is_dir());
        assert!(report.backup_dir.join("databases/0.sqlite").is_file());
        let journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        assert_eq!(journal.status, CheckoutStatus::Completed);
    }

    #[test]
    fn checkout_can_discard_recovery_backups_after_successful_apply() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();

        let report =
            checkout_local_snapshot_with_tracking_and_projects_control_with_backup_retention(
                &manifest,
                &codex_home,
                &repository,
                true,
                CheckoutTrackingUpdate {
                    remote_id: Uuid::now_v7(),
                    namespace_id: Uuid::now_v7(),
                    expected_generation: None,
                    integrated_head: Some(format!("sha256:{}", "a".repeat(64))),
                    activate_namespace: true,
                },
                &[],
                false,
                &OperationControl::default(),
            )
            .unwrap();

        assert_eq!(
            crate::scan_codex_home(&codex_home).unwrap().threads[0].thread_id,
            "new"
        );
        assert!(!report.local_backup_dir.exists());
        assert!(!report.backup_dir.exists());
        assert!(!report.backup_retained);
        let journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        assert!(!journal.retain_recovery_backup);
        assert_eq!(journal.status, CheckoutStatus::Completed);
    }

    #[test]
    fn checkout_accepts_older_thread_rows_when_newer_schema_adds_defaulted_columns() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let connection = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE threads ADD COLUMN recency_at INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE threads ADD COLUMN recency_at_ms INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE threads ADD COLUMN history_mode TEXT NOT NULL DEFAULT 'legacy';
                 ALTER TABLE threads ADD COLUMN name TEXT;
                 CREATE TRIGGER threads_recency_at_after_insert
                 AFTER INSERT ON threads
                 WHEN NEW.recency_at_ms = 0
                 BEGIN
                     UPDATE threads
                     SET recency_at = NEW.updated_at,
                         recency_at_ms = NEW.updated_at * 1000
                     WHERE id = NEW.id;
                 END;",
            )
            .unwrap();
        drop(connection);

        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();

        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();
        let scanned = crate::scan_codex_home(&codex_home).unwrap();

        assert_eq!(scanned.threads.len(), 1);
        assert_eq!(scanned.threads[0].thread_id, "new");
        let journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        assert_eq!(journal.status, CheckoutStatus::Completed);
    }

    #[test]
    fn checkout_invalidates_only_the_rebuildable_local_thread_catalog() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let catalog_path = create_fixture_thread_catalog(&codex_home);
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();

        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();

        let connection = Connection::open(&catalog_path).unwrap();
        let local_entries: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id = 'local'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let remote_entries: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id = 'ssh-host'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let local_state: (Option<f64>, i64, i64, Option<i64>) = connection
            .query_row(
                "SELECT watermark_updated_at, initial_build_complete,
                        observation_sequence, last_full_reconciled_at
                 FROM local_thread_catalog_sync_state WHERE host_id = 'local'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let revision: u64 = connection
            .query_row(
                "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let timeline_rows: u64 = connection
            .query_row("SELECT COUNT(*) FROM thread_timeline_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        let automation_rows: u64 = connection
            .query_row("SELECT COUNT(*) FROM automations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(local_entries, 0);
        assert_eq!(remote_entries, 1);
        assert_eq!(local_state, (None, 0, 42, None));
        assert_eq!(revision, 8);
        assert_eq!(timeline_rows, 1);
        assert_eq!(automation_rows, 1);

        let journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        let backup = journal
            .database_backups
            .iter()
            .find(|backup| backup.target == catalog_path)
            .expect("thread catalog was not included in the checkout backup");
        assert!(backup.backup.is_file());
    }

    #[test]
    fn tracked_checkout_commits_tracking_and_active_namespace() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let old_head = format!("sha256:{}", "a".repeat(64));
        let target_head = format!("sha256:{}", "b".repeat(64));
        let tracking = TrackingStore::open(&repository).unwrap();
        let previous = tracking
            .compare_and_set(&codex_home, remote_id, namespace_id, None, Some(&old_head))
            .unwrap();

        let report = checkout_local_snapshot_with_tracking(
            &manifest,
            &codex_home,
            &repository,
            true,
            CheckoutTrackingUpdate {
                remote_id,
                namespace_id,
                expected_generation: Some(previous.generation),
                integrated_head: Some(target_head.clone()),
                activate_namespace: true,
            },
        )
        .unwrap();

        let journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        assert_eq!(journal.status, CheckoutStatus::Completed);
        let record = tracking
            .load(&codex_home, remote_id, namespace_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            record.integrated_head.as_deref(),
            Some(target_head.as_str())
        );
        assert_eq!(record.generation, previous.generation + 1);
        let active = tracking.active(&codex_home).unwrap().unwrap();
        assert_eq!(
            (active.remote_id, active.namespace_id),
            (remote_id, namespace_id)
        );
    }

    #[test]
    fn checkout_rejects_nonterminal_journal_for_the_same_home() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let first = snapshot_with_object(&repository, "first", b"first rollout");
        let first_manifest = store_local_snapshot(&first, &repository).unwrap();
        let first_report =
            checkout_local_snapshot(&first_manifest, &codex_home, &repository, true).unwrap();
        let mut first_journal: CheckoutJournal = read_json(&first_report.journal_path).unwrap();
        first_journal.status = CheckoutStatus::LocalApplied;
        write_checkout_journal(&first_report.journal_path, &first_journal).unwrap();

        let second = snapshot_with_object(&repository, "second", b"second rollout");
        let second_manifest = store_local_snapshot(&second, &repository).unwrap();
        let error =
            checkout_local_snapshot(&second_manifest, &codex_home, &repository, true).unwrap_err();

        assert!(error.to_string().contains("unfinished checkout operation"));
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "first");
    }

    #[test]
    fn tracking_conflict_rolls_back_the_thread_catalog_invalidation() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let catalog_path = create_fixture_thread_catalog(&codex_home);
        let original_project_state = serde_json::to_vec(&json!({
            "electron-saved-workspace-roots": ["C:/existing"],
            "project-order": ["local-existing"],
            "local-projects": {
                "local-existing": {
                    "id": "local-existing",
                    "name": "existing",
                    "rootPaths": ["C:/existing"],
                    "createdAt": 1,
                    "updatedAt": 1
                }
            },
            "thread-project-assignments": {},
            "projectless-thread-ids": ["new"]
        }))
        .unwrap();
        fs::write(
            codex_home.join(".codex-global-state.json"),
            &original_project_state,
        )
        .unwrap();
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let old_head = format!("sha256:{}", "a".repeat(64));
        let target_head = format!("sha256:{}", "b".repeat(64));
        TrackingStore::open(&repository)
            .unwrap()
            .compare_and_set(&codex_home, remote_id, namespace_id, None, Some(&old_head))
            .unwrap();

        let error = checkout_local_snapshot_with_tracking_and_projects_control(
            &manifest,
            &codex_home,
            &repository,
            true,
            CheckoutTrackingUpdate {
                remote_id,
                namespace_id,
                expected_generation: None,
                integrated_head: Some(target_head),
                activate_namespace: true,
            },
            &["C:/work".to_string()],
            &OperationControl::default(),
        )
        .unwrap_err();

        assert!(error.chain().any(|cause| {
            cause
                .to_string()
                .contains("tracking state changed concurrently")
        }));
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "old");
        let connection = Connection::open(catalog_path).unwrap();
        let local_entries: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id = 'local'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let local_state: (Option<f64>, i64, i64, Option<i64>) = connection
            .query_row(
                "SELECT watermark_updated_at, initial_build_complete,
                        observation_sequence, last_full_reconciled_at
                 FROM local_thread_catalog_sync_state WHERE host_id = 'local'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let revision: u64 = connection
            .query_row(
                "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(local_entries, 1);
        assert_eq!(local_state, (Some(100.0), 1, 42, Some(100)));
        assert_eq!(revision, 7);
        assert_eq!(
            fs::read(codex_home.join(".codex-global-state.json")).unwrap(),
            original_project_state
        );
    }

    #[test]
    fn recovery_reconciles_tracking_after_local_checkout_was_applied() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let old_head = format!("sha256:{}", "a".repeat(64));
        let target_head = format!("sha256:{}", "b".repeat(64));
        let tracking = TrackingStore::open(&repository).unwrap();
        let previous = tracking
            .compare_and_set(&codex_home, remote_id, namespace_id, None, Some(&old_head))
            .unwrap();

        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();
        let mut journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        journal.status = CheckoutStatus::LocalApplied;
        journal.tracking_update = Some(CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: Some(previous.generation),
            integrated_head: Some(target_head.clone()),
            activate_namespace: true,
        });
        write_checkout_journal(&report.journal_path, &journal).unwrap();

        let recovered = recover_checkout_operation(&report.journal_path, true).unwrap();

        assert_eq!(recovered.status, CheckoutStatus::Completed);
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "new");
        let record = tracking
            .load(&codex_home, remote_id, namespace_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            record.integrated_head.as_deref(),
            Some(target_head.as_str())
        );
        assert_eq!(record.generation, previous.generation + 1);
        let active = tracking.active(&codex_home).unwrap().unwrap();
        assert_eq!(active.remote_id, remote_id);
        assert_eq!(active.namespace_id, namespace_id);
    }

    #[test]
    fn recovery_rejects_stale_local_applied_journal_without_overwriting_newer_checkout() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");

        let first = snapshot_with_object(&repository, "first", b"first rollout");
        let first_manifest = store_local_snapshot(&first, &repository).unwrap();
        let first_report =
            checkout_local_snapshot(&first_manifest, &codex_home, &repository, true).unwrap();

        let newer = snapshot_with_object(&repository, "newer", b"newer rollout");
        let newer_manifest = store_local_snapshot(&newer, &repository).unwrap();
        checkout_local_snapshot(&newer_manifest, &codex_home, &repository, true).unwrap();

        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let target_head = format!("sha256:{}", "b".repeat(64));
        let mut stale_journal: CheckoutJournal = read_json(&first_report.journal_path).unwrap();
        stale_journal.status = CheckoutStatus::LocalApplied;
        stale_journal.tracking_update = Some(CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: None,
            integrated_head: Some(target_head),
            activate_namespace: true,
        });
        stale_journal.error = None;
        write_checkout_journal(&first_report.journal_path, &stale_journal).unwrap();
        let tracking = TrackingStore::open(&repository).unwrap();

        for _ in 0..2 {
            let error = recover_checkout_operation(&first_report.journal_path, true).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("does not match the journal target")
            );

            let scanned = crate::scan_codex_home(&codex_home).unwrap();
            assert_eq!(scanned.threads[0].thread_id, "newer");
            let retained: CheckoutJournal = read_json(&first_report.journal_path).unwrap();
            assert_eq!(retained.status, CheckoutStatus::LocalApplied);
            assert!(
                retained
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("does not match the journal target"))
            );
            assert!(
                tracking
                    .load(&codex_home, remote_id, namespace_id)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn recovery_rejects_tracking_conflict_without_rolling_back_local_state() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let old_head = format!("sha256:{}", "a".repeat(64));
        let target_head = format!("sha256:{}", "b".repeat(64));
        let concurrent_head = format!("sha256:{}", "c".repeat(64));
        let tracking = TrackingStore::open(&repository).unwrap();
        let previous = tracking
            .compare_and_set(&codex_home, remote_id, namespace_id, None, Some(&old_head))
            .unwrap();
        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();
        let mut journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        journal.status = CheckoutStatus::LocalApplied;
        journal.tracking_update = Some(CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: Some(previous.generation),
            integrated_head: Some(target_head),
            activate_namespace: true,
        });
        write_checkout_journal(&report.journal_path, &journal).unwrap();
        let concurrent = tracking
            .compare_and_set(
                &codex_home,
                remote_id,
                namespace_id,
                Some(previous.generation),
                Some(&concurrent_head),
            )
            .unwrap();

        let error = recover_checkout_operation(&report.journal_path, true).unwrap_err();

        assert!(error.chain().any(|cause| {
            cause
                .to_string()
                .contains("tracking state changed concurrently")
        }));
        let retained: CheckoutJournal = read_json(&report.journal_path).unwrap();
        assert_eq!(retained.status, CheckoutStatus::LocalApplied);
        assert!(retained.error.is_some());
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "new");
        let unchanged = tracking
            .load(&codex_home, remote_id, namespace_id)
            .unwrap()
            .unwrap();
        assert_eq!(unchanged, concurrent);
        assert!(tracking.active(&codex_home).unwrap().is_none());
    }

    #[test]
    fn recovery_completes_untracked_local_applied_checkout_without_rollback() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();

        let mut journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        journal.status = CheckoutStatus::LocalApplied;
        write_checkout_journal(&report.journal_path, &journal).unwrap();
        let recovered = recover_checkout_operation(&report.journal_path, true).unwrap();
        assert_eq!(recovered.status, CheckoutStatus::Completed);
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "new");
    }

    #[test]
    fn recovery_before_local_write_leaves_live_directories_untouched() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        fs::create_dir_all(repository.join("journal")).unwrap();
        let operation_id = Uuid::now_v7().to_string();
        let staging_root = codex_home
            .join(".codex-session-sync/staging")
            .join(&operation_id);
        let backup_root = codex_home
            .join(".codex-session-sync/backups")
            .join(&operation_id);
        fs::create_dir_all(staging_root.join("sessions")).unwrap();
        fs::create_dir_all(staging_root.join("archived_sessions")).unwrap();
        let journal_path = repository
            .join("journal")
            .join(format!("checkout-{operation_id}.json"));
        let now = Utc::now().to_rfc3339();
        let journal = CheckoutJournal {
            schema_version: CHECKOUT_JOURNAL_SCHEMA_VERSION,
            operation_id,
            snapshot_id: Uuid::now_v7().to_string(),
            target_codex_home: codex_home.clone(),
            repository_root: repository.clone(),
            repository_backup_dir: repository.join("backups/test"),
            status: CheckoutStatus::Preparing,
            started_at: now.clone(),
            updated_at: now,
            database_backups: Vec::new(),
            directory_swaps: ["sessions", "archived_sessions"]
                .into_iter()
                .map(|name| CheckoutDirectorySwap {
                    live: codex_home.join(name),
                    staged: staging_root.join(name),
                    backup: backup_root.join(name),
                    original_existed: true,
                })
                .collect(),
            file_swaps: Vec::new(),
            expected_thread_hashes: BTreeMap::new(),
            tracking_update: None,
            retain_recovery_backup: true,
            error: None,
        };
        write_checkout_journal(&journal_path, &journal).unwrap();

        recover_checkout_operation(&journal_path, true).unwrap();
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "old");
        assert!(codex_home.join("sessions").is_dir());
    }

    #[test]
    fn project_assignments_preserve_nested_projects_under_a_mapping_root() -> Result<()> {
        let temp = tempdir().unwrap();
        let repository = temp.path().join("repository");
        let mut parent = snapshot_with_object(&repository, "parent", b"parent")
            .threads
            .remove(0);
        let mut applet = parent.clone();
        let mut java = parent.clone();
        let mut technical = parent.clone();
        parent.workspace.source_path = Some("F:/history/yaxin".to_string());
        applet.thread_id = "applet".to_string();
        applet.workspace.source_path = Some("F:/history/yaxin/yxzsApplet".to_string());
        java.thread_id = "java".to_string();
        java.workspace.source_path = Some("F:/history/yaxin/yxzsJava".to_string());
        technical.thread_id = "technical".to_string();
        technical.workspace.source_path = Some("F:/history/yaxin/technical-center".to_string());
        let mut applet_second = applet.clone();
        applet_second.thread_id = "applet-second".to_string();
        let mut java_second = java.clone();
        java_second.thread_id = "java-second".to_string();
        let mut technical_second = technical.clone();
        technical_second.thread_id = "technical-second".to_string();

        let mut state = json!({
            "electron-saved-workspace-roots": [],
            "project-order": [],
            "local-projects": {},
            "thread-project-assignments": {},
            "projectless-thread-ids": [
                "parent", "applet", "applet-second", "java", "java-second",
                "technical", "technical-second"
            ]
        });
        assert!(apply_project_assignments(
            &mut state,
            &[
                parent,
                applet,
                applet_second,
                java,
                java_second,
                technical,
                technical_second,
            ],
            &["F:/history/yaxin".to_string()]
        )?);

        let assignments = state["thread-project-assignments"].as_object().unwrap();
        let projects = state["local-projects"].as_object().unwrap();
        let project_name = |thread_id: &str| {
            let project_id = assignments[thread_id]["projectId"].as_str().unwrap();
            projects[project_id]["name"].as_str().unwrap()
        };
        assert_eq!(project_name("parent"), "yaxin");
        assert_eq!(project_name("applet"), "yxzsApplet");
        assert_eq!(project_name("java"), "yxzsJava");
        assert_eq!(project_name("technical"), "technical-center");
        assert_eq!(
            assignments["applet"]["projectId"],
            assignments["applet-second"]["projectId"]
        );
        assert_ne!(
            assignments["applet"]["projectId"],
            assignments["java"]["projectId"]
        );
        assert!(
            state["projectless-thread-ids"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        Ok(())
    }

    fn create_fixture_home(codex_home: &Path, thread_id: &str, content: &[u8]) {
        let sessions = codex_home.join("sessions/2026/07/26");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(codex_home.join("archived_sessions")).unwrap();
        let rollout = sessions.join(format!("rollout-{thread_id}.jsonl"));
        let mut file = File::create(&rollout).unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "type": "session_meta",
                "payload": {"id": thread_id, "cwd": "C:/work", "model_provider": "openai"}
            })
        )
        .unwrap();
        file.write_all(content).unwrap();

        let database = codex_home.join("state_5.sqlite");
        let connection = Connection::open(database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at, source, model_provider,
                    cwd, title, sandbox_policy, approval_mode, archived
                 ) VALUES (?1, ?2, 1, 1, 'cli', 'openai', 'C:/work', ?1, '{}', 'never', 0)",
                params![thread_id, rollout.to_string_lossy().as_ref()],
            )
            .unwrap();
    }

    fn create_fixture_thread_catalog(codex_home: &Path) -> PathBuf {
        let database = codex_home.join("sqlite/codex-dev.db");
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE local_thread_catalog (
                     host_id TEXT NOT NULL,
                     thread_id TEXT NOT NULL,
                     display_title TEXT NOT NULL,
                     source_updated_at REAL NOT NULL,
                     PRIMARY KEY (host_id, thread_id)
                 );
                 CREATE TABLE local_thread_catalog_sync_state (
                     host_id TEXT PRIMARY KEY,
                     watermark_updated_at REAL,
                     initial_build_complete INTEGER NOT NULL DEFAULT 0,
                     observation_sequence INTEGER NOT NULL DEFAULT 0,
                     last_full_reconciled_at INTEGER
                 );
                 CREATE TABLE local_thread_catalog_hosts (
                     host_id TEXT PRIMARY KEY,
                     host_kind TEXT NOT NULL
                 );
                 CREATE TABLE local_thread_catalog_metadata (
                     id INTEGER PRIMARY KEY,
                     catalog_revision INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE thread_timeline_ledger (
                     host_id TEXT NOT NULL,
                     thread_id TEXT NOT NULL,
                     sequence INTEGER NOT NULL,
                     record_id TEXT NOT NULL,
                     payload_json TEXT NOT NULL,
                     PRIMARY KEY (host_id, thread_id, sequence)
                 );
                 CREATE TABLE automations (id TEXT PRIMARY KEY, name TEXT NOT NULL);
                 INSERT INTO local_thread_catalog
                     (host_id, thread_id, display_title, source_updated_at)
                     VALUES ('local', 'stale-local', 'stale', 100),
                            ('ssh-host', 'remote-thread', 'remote', 100);
                 INSERT INTO local_thread_catalog_sync_state
                     (host_id, watermark_updated_at, initial_build_complete,
                      observation_sequence, last_full_reconciled_at)
                     VALUES ('local', 100, 1, 42, 100),
                            ('ssh-host', 100, 1, 8, 100);
                 INSERT INTO local_thread_catalog_hosts
                     (host_id, host_kind) VALUES ('local', 'local'), ('ssh-host', 'ssh');
                 INSERT INTO local_thread_catalog_metadata (id, catalog_revision) VALUES (1, 7);
                 INSERT INTO thread_timeline_ledger
                     (host_id, thread_id, sequence, record_id, payload_json)
                     VALUES ('local', 'stale-local', 1, 'record', '{}');
                 INSERT INTO automations (id, name) VALUES ('automation', 'preserved');",
            )
            .unwrap();
        database
    }

    fn snapshot_with_object(repository: &Path, thread_id: &str, content: &[u8]) -> LocalSnapshot {
        fs::create_dir_all(repository.join("objects/tmp")).unwrap();
        let mut rollout = format!(
            "{}\n",
            json!({
                "type": "session_meta",
                "payload": {"id": thread_id, "cwd": "C:/work", "model_provider": "openai"}
            })
        )
        .into_bytes();
        rollout.extend_from_slice(content);
        let sha256 = format!("sha256:{}", hex::encode(Sha256::digest(&rollout)));
        let descriptor = crate::StorageObjectRef {
            kind: crate::StorageObjectKind::Whole,
            sha256: sha256.clone(),
            byte_length: rollout.len() as u64,
        };
        crate::FilesystemContentStore::open(repository.to_path_buf())
            .unwrap()
            .install(
                &descriptor,
                rollout.as_slice(),
                &OperationControl::default(),
            )
            .unwrap();
        LocalSnapshot {
            schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: Uuid::now_v7().to_string(),
            created_at: Utc::now().to_rfc3339(),
            threads: vec![ThreadBundle {
                schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
                thread_id: thread_id.to_string(),
                title: thread_id.to_string(),
                archived: false,
                created_at_ms: None,
                updated_at_ms: None,
                model_provider: Some("openai".to_string()),
                workspace: WorkspaceRef {
                    logical_id: None,
                    source_path: Some("C:/work".to_string()),
                },
                rollout: ContentObject {
                    sha256: sha256.clone(),
                    byte_length: rollout.len() as u64,
                    media_type: "application/x-ndjson".to_string(),
                    logical_path: Some(format!("sessions/2026/07/26/rollout-{thread_id}.jsonl")),
                    source_path: None,
                    storage: Some(crate::StorageRef::Whole {
                        object_sha256: sha256.clone(),
                    }),
                },
                related_records: RelatedRecords {
                    source_database: None,
                    tables: BTreeMap::from([(
                        "threads".to_string(),
                        vec![json!({
                            "id": thread_id,
                            "created_at": 1,
                            "updated_at": 1,
                            "source": "cli",
                            "model_provider": "openai",
                            "cwd": "C:/work",
                            "title": thread_id,
                            "sandbox_policy": "{}",
                            "approval_mode": "never",
                            "archived": 0
                        })],
                    )]),
                },
                attachments: Vec::new(),
            }],
            warning_count: 0,
        }
    }
}
