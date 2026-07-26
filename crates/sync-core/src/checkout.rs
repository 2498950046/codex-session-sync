use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
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
use crate::sync::semantic_thread_hash;

pub const CHECKOUT_JOURNAL_SCHEMA_VERSION: u32 = 1;

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
pub struct CheckoutJournal {
    pub schema_version: u32,
    pub operation_id: String,
    pub snapshot_id: String,
    pub target_codex_home: PathBuf,
    pub repository_backup_dir: PathBuf,
    pub status: CheckoutStatus,
    pub started_at: String,
    pub updated_at: String,
    pub database_backups: Vec<CheckoutDatabaseBackup>,
    pub directory_swaps: Vec<CheckoutDirectorySwap>,
    pub expected_thread_hashes: BTreeMap<String, String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutReport {
    pub operation_id: String,
    pub snapshot_id: String,
    pub thread_count: usize,
    pub backup_dir: PathBuf,
    pub local_backup_dir: PathBuf,
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
    if !confirmed_codex_closed {
        bail!("checkout requires confirmation that Codex is fully closed");
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
    control.check_cancelled()?;

    ensure_repository_layout(&repository_root)?;
    let operation_id = Uuid::now_v7().to_string();
    let repository_backup_dir = repository_root.join("backups").join(&operation_id);
    let journal_path = repository_root
        .join("journal")
        .join(format!("checkout-{operation_id}.json"));
    let private_root = target_codex_home.join(".codex-session-sync");
    let staging_root = private_root.join("staging").join(&operation_id);
    let local_backup_dir = private_root.join("backups").join(&operation_id);
    fs::create_dir_all(&repository_backup_dir)?;
    fs::create_dir_all(&staging_root)?;
    fs::create_dir_all(&local_backup_dir)?;

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
        fs::create_dir_all(&swap.staged)?;
    }

    let expected_thread_hashes = snapshot
        .threads
        .iter()
        .map(|thread| Ok((thread.thread_id.clone(), semantic_thread_hash(thread)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let now = Utc::now().to_rfc3339();
    let mut journal = CheckoutJournal {
        schema_version: CHECKOUT_JOURNAL_SCHEMA_VERSION,
        operation_id: operation_id.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
        target_codex_home: target_codex_home.clone(),
        repository_backup_dir: repository_backup_dir.clone(),
        status: CheckoutStatus::Preparing,
        started_at: now.clone(),
        updated_at: now,
        database_backups: Vec::new(),
        directory_swaps,
        expected_thread_hashes,
        error: None,
    };
    write_checkout_journal(&journal_path, &journal)?;

    let result = (|| -> Result<()> {
        stage_rollouts(&snapshot, &repository_root, &staging_root, control)?;
        control.check_cancelled()?;
        control.report(OperationProgress::indeterminate(
            "checkout_backup",
            "Creating recoverable database backups",
        ));
        let database_dir = repository_backup_dir.join("databases");
        fs::create_dir_all(&database_dir)?;
        for (index, database) in current.database_paths.iter().enumerate() {
            let backup = database_dir.join(format!("{index}.sqlite"));
            backup_database(database, &backup)?;
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
        apply_directory_swaps(&journal.directory_swaps)?;
        replace_databases(
            &current.database_paths,
            &snapshot.threads,
            &target_codex_home,
        )?;

        journal.status = CheckoutStatus::Validating;
        journal.updated_at = Utc::now().to_rfc3339();
        write_checkout_journal(&journal_path, &journal)?;
        let non_cancellable = control.non_cancellable();
        validate_checkout_result(&snapshot, &target_codex_home, &non_cancellable)?;
        journal.status = CheckoutStatus::LocalApplied;
        journal.updated_at = Utc::now().to_rfc3339();
        write_checkout_journal(&journal_path, &journal)?;
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
    Ok(CheckoutReport {
        operation_id,
        snapshot_id: validation.snapshot_id,
        thread_count: snapshot.threads.len(),
        backup_dir: repository_backup_dir,
        local_backup_dir,
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
    rollback_checkout(
        journal_path,
        &mut journal,
        Some("Recovered an incomplete checkout operation".to_string()),
    )?;
    Ok(journal)
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
            fs::create_dir_all(parent)?;
        }
        let source = repository_object_path(repository_root, &thread.rollout.sha256)?;
        copy_verified_object(&source, &target, &thread.rollout.sha256, Some(control))?;
        if fs::metadata(&target)?.len() != thread.rollout.byte_length {
            bail!("staged rollout has an unexpected byte length");
        }
    }
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
            fs::rename(&swap.live, &swap.backup).with_context(|| {
                format!(
                    "failed to move current session directory {} to backup",
                    swap.live.display()
                )
            })?;
        }
        fs::rename(&swap.staged, &swap.live).with_context(|| {
            format!(
                "failed to install staged session directory {}",
                swap.live.display()
            )
        })?;
    }
    Ok(())
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

    let mut connection = Connection::open(&primary)?;
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
    let expected = snapshot
        .threads
        .iter()
        .map(|thread| (thread.thread_id.as_str(), thread.rollout.sha256.as_str()))
        .collect::<BTreeMap<_, _>>();
    let actual = actual
        .threads
        .iter()
        .map(|thread| (thread.thread_id.as_str(), thread.rollout.sha256.as_str()))
        .collect::<BTreeMap<_, _>>();
    if actual != expected {
        let expected_ids = expected.keys().copied().collect::<BTreeSet<_>>();
        let actual_ids = actual.keys().copied().collect::<BTreeSet<_>>();
        bail!(
            "post-checkout validation mismatch: expected {} threads, found {}; missing {:?}; unexpected {:?}",
            expected.len(),
            actual.len(),
            expected_ids.difference(&actual_ids).collect::<Vec<_>>(),
            actual_ids.difference(&expected_ids).collect::<Vec<_>>()
        );
    }
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
            } else if let Err(error) = fs::rename(&swap.live, &failed) {
                failures.push(format!(
                    "failed to preserve checkout directory {}: {error}",
                    swap.live.display()
                ));
            }
        }
        if swap_started
            && swap.backup.exists()
            && let Err(error) = fs::rename(&swap.backup, &swap.live)
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
    fn recovery_restores_original_directories_and_database() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let repository = temp.path().join("repository");
        create_fixture_home(&codex_home, "old", b"old rollout");
        let target = snapshot_with_object(&repository, "new", b"new rollout");
        let manifest = store_local_snapshot(&target, &repository).unwrap();
        let report = checkout_local_snapshot(&manifest, &codex_home, &repository, true).unwrap();

        let mut journal: CheckoutJournal = read_json(&report.journal_path).unwrap();
        journal.status = CheckoutStatus::Applying;
        write_checkout_journal(&report.journal_path, &journal).unwrap();
        let recovered = recover_checkout_operation(&report.journal_path, true).unwrap();
        assert_eq!(recovered.status, CheckoutStatus::RolledBack);
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "old");
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
            expected_thread_hashes: BTreeMap::new(),
            error: None,
        };
        write_checkout_journal(&journal_path, &journal).unwrap();

        recover_checkout_operation(&journal_path, true).unwrap();
        let scanned = crate::scan_codex_home(&codex_home).unwrap();
        assert_eq!(scanned.threads[0].thread_id, "old");
        assert!(codex_home.join("sessions").is_dir());
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
        let descriptor = crate::ObjectDescriptor {
            sha256: sha256.clone(),
            byte_length: rollout.len() as u64,
        };
        crate::install_repository_object(
            repository,
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
                    sha256,
                    byte_length: rollout.len() as u64,
                    media_type: "application/x-ndjson".to_string(),
                    logical_path: Some(format!("sessions/2026/07/26/rollout-{thread_id}.jsonl")),
                    source_path: None,
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
