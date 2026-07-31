use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, MAIN_DB, OpenFlags, TransactionBehavior, params_from_iter};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::codex::{scan_codex_home_metadata_with_control, scan_codex_home_with_control};
use crate::models::{
    ImportReport, JournalRollout, LOCAL_SNAPSHOT_SCHEMA_VERSION, LocalSnapshot,
    OPERATION_JOURNAL_SCHEMA_VERSION, OperationJournal, OperationStatus, SnapshotSummary,
    SnapshotValidationReport, ThreadBundle,
};
use crate::operation::{OperationControl, OperationProgress};
use crate::protocol::{ObjectDescriptor, validate_sha256};
use crate::storage_v2::{
    ContentStore, FilesystemContentStore, StorageRef, load_v2_snapshot, write_v2_snapshot,
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    operation_id: String,
    created_at: String,
    target_database: PathBuf,
    database_backup: PathBuf,
}

#[derive(Debug)]
struct PreparedThread {
    bundle: ThreadBundle,
    content: crate::storage_v2::ContentRef,
    repository_root: PathBuf,
    target_path: PathBuf,
    temporary_path: PathBuf,
}

const SOURCE_OBJECT_INDEX_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceObjectIndex {
    schema_version: u32,
    entries: BTreeMap<String, SourceObjectIndexEntry>,
}

impl Default for SourceObjectIndex {
    fn default() -> Self {
        Self {
            schema_version: SOURCE_OBJECT_INDEX_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SourceObjectIndexEntry {
    byte_length: u64,
    modified_unix_nanos: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage: Option<StorageRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceFingerprint {
    byte_length: u64,
    modified_unix_nanos: u64,
}

pub fn default_repository_root() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".codex-session-sync"))
        .unwrap_or_else(|| PathBuf::from(".codex-session-sync"))
}

pub fn load_local_snapshot(manifest_path: impl AsRef<Path>) -> Result<LocalSnapshot> {
    let manifest_path = manifest_path.as_ref();
    let repository_root = snapshot_repository_root(manifest_path)?;
    let snapshot = load_v2_snapshot(manifest_path, &repository_root)?.0;
    validate_snapshot_structure(&snapshot)?;
    Ok(snapshot)
}

pub fn store_local_snapshot(
    snapshot: &LocalSnapshot,
    repository_root: impl AsRef<Path>,
) -> Result<PathBuf> {
    validate_snapshot_structure(snapshot)?;
    let repository_root = repository_root.as_ref();
    ensure_repository_layout(repository_root)?;
    let contents = snapshot
        .threads
        .iter()
        .map(|thread| {
            Ok((
                thread.thread_id.clone(),
                content_ref_from_object(&thread.rollout)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    write_v2_snapshot(snapshot, &contents, repository_root)
}

pub fn collect_object_descriptors(threads: &[ThreadBundle]) -> Result<Vec<ObjectDescriptor>> {
    let mut objects = BTreeMap::new();
    for object in threads
        .iter()
        .flat_map(|thread| std::iter::once(&thread.rollout).chain(thread.attachments.iter()))
    {
        validate_sha256(&object.sha256)
            .map_err(|_| anyhow::anyhow!("invalid content object hash {}", object.sha256))?;
        match objects.insert(object.sha256.clone(), object.byte_length) {
            Some(existing) if existing != object.byte_length => bail!(
                "content object {} has conflicting lengths {} and {}",
                object.sha256,
                existing,
                object.byte_length
            ),
            _ => {}
        }
    }
    Ok(objects
        .into_iter()
        .map(|(sha256, byte_length)| ObjectDescriptor {
            sha256,
            byte_length,
        })
        .collect())
}

pub fn repository_object_path(repository_root: impl AsRef<Path>, sha256: &str) -> Result<PathBuf> {
    crate::storage_v2::typed_object_path(
        repository_root.as_ref(),
        crate::storage_v2::StorageObjectKind::Whole,
        sha256,
    )
}

pub fn validate_repository_object(
    repository_root: impl AsRef<Path>,
    descriptor: &ObjectDescriptor,
) -> Result<()> {
    let path = repository_object_path(repository_root.as_ref(), &descriptor.sha256)?;
    validate_object(&path, &descriptor.sha256, descriptor.byte_length)
}

pub fn install_repository_object<R: Read>(
    repository_root: impl AsRef<Path>,
    descriptor: &ObjectDescriptor,
    mut reader: R,
    control: &OperationControl,
) -> Result<bool> {
    validate_sha256(&descriptor.sha256)
        .map_err(|_| anyhow::anyhow!("invalid content object hash {}", descriptor.sha256))?;
    let repository_root = repository_root.as_ref();
    ensure_repository_layout(repository_root)?;
    let destination = crate::storage_v2::typed_object_path(
        repository_root,
        crate::storage_v2::StorageObjectKind::Whole,
        &descriptor.sha256,
    )?;
    if destination.exists() {
        validate_object(&destination, &descriptor.sha256, descriptor.byte_length)?;
        return Ok(false);
    }

    let temporary = repository_root
        .join("objects")
        .join("tmp")
        .join(format!("{}.download.tmp", Uuid::now_v7()));
    let install_result = (|| -> Result<()> {
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(output);
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            control.check_cancelled()?;
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .context("downloaded object length overflow")?;
            if total > descriptor.byte_length {
                bail!(
                    "content object length mismatch for {}: expected {}, received more",
                    descriptor.sha256,
                    descriptor.byte_length
                );
            }
            hasher.update(&buffer[..count]);
            writer.write_all(&buffer[..count])?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        if total != descriptor.byte_length {
            bail!(
                "content object length mismatch for {}: expected {}, got {}",
                descriptor.sha256,
                descriptor.byte_length,
                total
            );
        }
        let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
        if actual != descriptor.sha256 {
            bail!(
                "content object hash mismatch: expected {}, got {}",
                descriptor.sha256,
                actual
            );
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::rename(&temporary, &destination) {
            Ok(()) => Ok(()),
            Err(error) if destination.exists() => {
                fs::remove_file(&temporary)?;
                validate_object(&destination, &descriptor.sha256, descriptor.byte_length)
                    .context(error)
            }
            Err(error) => Err(error).with_context(|| {
                format!("failed to install content object {}", destination.display())
            }),
        }
    })();
    if install_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    install_result?;
    Ok(true)
}

pub(crate) fn install_prepared_repository_object(
    repository_root: &Path,
    temporary: &Path,
    descriptor: &ObjectDescriptor,
) -> Result<bool> {
    validate_sha256(&descriptor.sha256)
        .map_err(|_| anyhow::anyhow!("invalid content object hash {}", descriptor.sha256))?;
    ensure_repository_layout(repository_root)?;
    let metadata = fs::metadata(temporary).with_context(|| {
        format!(
            "failed to inspect prepared content object {}",
            temporary.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() != descriptor.byte_length {
        bail!(
            "prepared content object length mismatch for {}: expected {}, got {}",
            descriptor.sha256,
            descriptor.byte_length,
            metadata.len()
        );
    }
    let destination = crate::storage_v2::typed_object_path(
        repository_root,
        crate::storage_v2::StorageObjectKind::Whole,
        &descriptor.sha256,
    )?;
    if destination.exists() {
        validate_object(&destination, &descriptor.sha256, descriptor.byte_length)?;
        fs::remove_file(temporary)?;
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(temporary, &destination) {
        Ok(()) => Ok(true),
        Err(error) if destination.exists() => {
            fs::remove_file(temporary)?;
            validate_object(&destination, &descriptor.sha256, descriptor.byte_length)
                .context(error)?;
            Ok(false)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to install content object {}", destination.display())),
    }
}

pub fn create_local_snapshot(
    codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<SnapshotSummary> {
    create_local_snapshot_with_control(
        codex_home,
        repository_root,
        confirmed_codex_closed,
        &OperationControl::default(),
    )
}

pub fn create_local_snapshot_with_control(
    codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    control: &OperationControl,
) -> Result<SnapshotSummary> {
    if !confirmed_codex_closed {
        bail!("snapshot creation requires confirmation that Codex is fully closed");
    }
    let repository_root = repository_root.as_ref();
    let report = scan_codex_home_metadata_with_control(codex_home, control)?;
    ensure_repository_layout(repository_root)?;
    let index_path = source_object_index_path(repository_root);
    let mut source_index = load_source_object_index(&index_path);

    let mut unique_objects = BTreeSet::new();
    let thread_count = report.threads.len();
    let mut threads = Vec::with_capacity(thread_count);
    for (index, thread) in report.threads.into_iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "snapshot_objects".to_string(),
            message: thread.title.clone(),
            completed: index as u64,
            total: Some(thread_count as u64),
            unit: "threads".to_string(),
            cancellable: true,
        });
        let source_path = thread.rollout.source_path.clone();
        let (sha256, storage) = store_snapshot_object(
            &source_path,
            thread.rollout.byte_length,
            repository_root,
            &mut source_index,
            control,
        )?;
        unique_objects.insert(sha256.clone());
        let mut bundle = thread.into_bundle(sha256);
        bundle.rollout.storage = Some(storage);
        threads.push(bundle);
    }

    let snapshot_id = Uuid::now_v7().to_string();
    let snapshot = LocalSnapshot {
        schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: snapshot_id.clone(),
        created_at: Utc::now().to_rfc3339(),
        threads,
        warning_count: report.warnings.len(),
    };
    let contents = snapshot
        .threads
        .iter()
        .map(|thread| {
            Ok((
                thread.thread_id.clone(),
                crate::storage_v2::ContentRef {
                    logical_sha256: thread.rollout.sha256.clone(),
                    byte_length: thread.rollout.byte_length,
                    storage: thread
                        .rollout
                        .storage
                        .clone()
                        .context("snapshot rollout has no v2 storage reference")?,
                    media_type: Some(thread.rollout.media_type.clone()),
                    logical_path: thread.rollout.logical_path.clone(),
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    control.check_cancelled()?;
    control.report(OperationProgress::indeterminate(
        "snapshot_manifest",
        "正在写入快照清单",
    ));
    atomic_write_json(&index_path, &source_index)?;
    let manifest_path = write_v2_snapshot(&snapshot, &contents, repository_root)?;

    Ok(SnapshotSummary {
        snapshot_id,
        manifest_path,
        thread_count: snapshot.threads.len(),
        object_count: unique_objects.len(),
        total_bytes: snapshot
            .threads
            .iter()
            .map(|thread| thread.rollout.byte_length)
            .sum(),
        warning_count: snapshot.warning_count,
    })
}

pub fn validate_local_snapshot(
    manifest_path: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
) -> Result<SnapshotValidationReport> {
    validate_local_snapshot_with_control(
        manifest_path,
        repository_root,
        &OperationControl::default(),
    )
}

pub fn validate_local_snapshot_with_control(
    manifest_path: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    control: &OperationControl,
) -> Result<SnapshotValidationReport> {
    let manifest_path = manifest_path.as_ref();
    let repository_root = repository_root.as_ref();
    let snapshot = load_local_snapshot(manifest_path)?;
    validate_snapshot_structure(&snapshot)?;

    let mut unique_objects = BTreeSet::new();
    let descriptors = collect_object_descriptors(&snapshot.threads)?;
    for (index, descriptor) in descriptors.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "validate_objects".to_string(),
            message: descriptor.sha256.clone(),
            completed: index as u64,
            total: Some(descriptors.len() as u64),
            unit: "objects".to_string(),
            cancellable: true,
        });
        if let Some(storage) = snapshot
            .threads
            .iter()
            .find(|thread| thread.rollout.sha256 == descriptor.sha256)
            .and_then(|thread| thread.rollout.storage.clone())
        {
            FilesystemContentStore::open(repository_root.to_path_buf())?.validate(
                &crate::storage_v2::ContentRef {
                    logical_sha256: descriptor.sha256.clone(),
                    byte_length: descriptor.byte_length,
                    storage,
                    media_type: None,
                    logical_path: None,
                },
                control,
            )?;
        } else {
            let object_path = repository_object_path(repository_root, &descriptor.sha256)?;
            validate_object(&object_path, &descriptor.sha256, descriptor.byte_length)?;
        }
        unique_objects.insert(descriptor.sha256.clone());
    }

    Ok(SnapshotValidationReport {
        snapshot_id: snapshot.snapshot_id,
        manifest_path: manifest_path.to_path_buf(),
        thread_count: snapshot.threads.len(),
        object_count: unique_objects.len(),
        total_bytes: snapshot
            .threads
            .iter()
            .map(|thread| thread.rollout.byte_length)
            .sum(),
        valid: true,
    })
}

pub fn import_local_snapshot(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<ImportReport> {
    import_local_snapshot_with_control(
        manifest_path,
        target_codex_home,
        repository_root,
        confirmed_codex_closed,
        &OperationControl::default(),
    )
}

pub fn import_local_snapshot_with_control(
    manifest_path: impl AsRef<Path>,
    target_codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    control: &OperationControl,
) -> Result<ImportReport> {
    if !confirmed_codex_closed {
        bail!("import requires confirmation that Codex is fully closed");
    }

    let manifest_path = manifest_path.as_ref();
    let target_codex_home = target_codex_home.as_ref().to_path_buf();
    let repository_root = repository_root.as_ref().to_path_buf();
    control.report(OperationProgress::indeterminate(
        "import_preflight",
        "正在校验快照对象",
    ));
    let validation =
        validate_local_snapshot_with_control(manifest_path, &repository_root, control)?;
    let snapshot = load_local_snapshot(manifest_path)?;
    control.check_cancelled()?;
    let target_report = scan_codex_home_with_control(&target_codex_home, control)?;
    let existing = target_report
        .threads
        .iter()
        .map(|thread| (thread.thread_id.as_str(), thread.rollout.sha256.as_str()))
        .collect::<HashMap<_, _>>();

    let mut skipped_count = 0_usize;
    let mut prepared = Vec::new();
    let mut target_paths = HashSet::new();
    for thread in &snapshot.threads {
        control.check_cancelled()?;
        if let Some(existing_hash) = existing.get(thread.thread_id.as_str()) {
            if *existing_hash == thread.rollout.sha256 {
                skipped_count += 1;
                continue;
            }
            bail!(
                "thread {} conflicts with the target: snapshot hash {}, target hash {}",
                thread.thread_id,
                thread.rollout.sha256,
                existing_hash
            );
        }

        let relative_path = safe_rollout_path(thread)?;
        let target_path = target_codex_home.join(&relative_path);
        if target_path.exists() {
            bail!(
                "target rollout path already exists for thread {}: {}",
                thread.thread_id,
                target_path.display()
            );
        }
        if !target_paths.insert(relative_path) {
            bail!("snapshot contains duplicate rollout target paths");
        }
        let temporary_path = temporary_sibling(&target_path, "import");
        ensure_importable_thread(thread)?;
        prepared.push(PreparedThread {
            bundle: thread.clone(),
            content: content_ref_from_object(&thread.rollout)?,
            repository_root: repository_root.clone(),
            target_path,
            temporary_path,
        });
    }

    let operation_id = Uuid::now_v7().to_string();
    let backup_dir = repository_root.join("backups").join(&operation_id);
    let journal_path = repository_root
        .join("journal")
        .join(format!("{operation_id}.json"));
    fs::create_dir_all(&backup_dir)
        .with_context(|| format!("failed to create backup directory {}", backup_dir.display()))?;
    fs::create_dir_all(repository_root.join("journal"))?;
    let now = Utc::now().to_rfc3339();
    let mut journal = OperationJournal {
        schema_version: OPERATION_JOURNAL_SCHEMA_VERSION,
        operation_id: operation_id.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
        target_codex_home: target_codex_home.clone(),
        backup_dir: backup_dir.clone(),
        status: OperationStatus::Preparing,
        started_at: now.clone(),
        updated_at: now,
        planned_rollouts: prepared
            .iter()
            .map(|thread| JournalRollout {
                target_path: thread.target_path.clone(),
                temporary_path: thread.temporary_path.clone(),
                sha256: thread.bundle.rollout.sha256.clone(),
            })
            .collect(),
        imported_thread_ids: Vec::new(),
        error: None,
    };
    write_journal(&journal_path, &journal)?;

    if prepared.is_empty() {
        journal.status = OperationStatus::Completed;
        journal.updated_at = Utc::now().to_rfc3339();
        write_journal(&journal_path, &journal)?;
        return Ok(ImportReport {
            operation_id,
            snapshot_id: snapshot.snapshot_id,
            imported_count: 0,
            skipped_count,
            backup_dir,
            journal_path,
        });
    }

    let target_database = select_primary_database(&target_report.database_paths)?;
    let database_backup = backup_dir.join("threads.sqlite");
    control.check_cancelled()?;
    control.report(OperationProgress::indeterminate(
        "import_backup",
        "正在创建 SQLite 安全备份",
    ));
    backup_database(&target_database, &database_backup)?;
    atomic_write_json(
        &backup_dir.join("manifest.json"),
        &BackupManifest {
            operation_id: operation_id.clone(),
            created_at: Utc::now().to_rfc3339(),
            target_database: target_database.clone(),
            database_backup: database_backup.clone(),
        },
    )?;
    journal.status = OperationStatus::BackedUp;
    journal.updated_at = Utc::now().to_rfc3339();
    write_journal(&journal_path, &journal)?;

    let import_result = apply_import(
        &prepared,
        &target_database,
        &target_codex_home,
        &mut journal,
        &journal_path,
        control,
    );
    if let Err(error) = import_result {
        return rollback_and_fail(
            error,
            &prepared,
            &target_database,
            &database_backup,
            &mut journal,
            &journal_path,
            control,
        );
    }

    journal.status = OperationStatus::Validating;
    journal.updated_at = Utc::now().to_rfc3339();
    write_journal(&journal_path, &journal)?;
    if control.is_cancelled() {
        return rollback_and_fail(
            anyhow::anyhow!("operation cancelled"),
            &prepared,
            &target_database,
            &database_backup,
            &mut journal,
            &journal_path,
            control,
        );
    }
    let post_import = scan_codex_home_with_control(&target_codex_home, control);
    let validation_result = post_import.and_then(|report| {
        let imported = report
            .threads
            .iter()
            .map(|thread| (thread.thread_id.as_str(), thread.rollout.sha256.as_str()))
            .collect::<HashMap<_, _>>();
        for thread in &prepared {
            if imported.get(thread.bundle.thread_id.as_str())
                != Some(&thread.bundle.rollout.sha256.as_str())
            {
                bail!(
                    "post-import validation failed for thread {}",
                    thread.bundle.thread_id
                );
            }
        }
        Ok(())
    });
    if let Err(error) = validation_result {
        return rollback_and_fail(
            error,
            &prepared,
            &target_database,
            &database_backup,
            &mut journal,
            &journal_path,
            control,
        );
    }

    journal.status = OperationStatus::Completed;
    journal.updated_at = Utc::now().to_rfc3339();
    journal.imported_thread_ids = prepared
        .iter()
        .map(|thread| thread.bundle.thread_id.clone())
        .collect();
    write_journal(&journal_path, &journal)?;

    Ok(ImportReport {
        operation_id,
        snapshot_id: validation.snapshot_id,
        imported_count: prepared.len(),
        skipped_count,
        backup_dir,
        journal_path,
    })
}

pub fn recover_incomplete_operation(
    journal_path: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<OperationJournal> {
    if !confirmed_codex_closed {
        bail!("recovery requires confirmation that Codex is fully closed");
    }
    let journal_path = journal_path.as_ref();
    let mut journal: OperationJournal = read_json(journal_path)?;
    if matches!(
        journal.status,
        OperationStatus::Completed | OperationStatus::RolledBack
    ) {
        return Ok(journal);
    }

    let backup_manifest_path = journal.backup_dir.join("manifest.json");
    if !backup_manifest_path.is_file() && journal.status == OperationStatus::Preparing {
        for rollout in &journal.planned_rollouts {
            let _ = fs::remove_file(&rollout.temporary_path);
        }
        journal.status = OperationStatus::RolledBack;
        journal.updated_at = Utc::now().to_rfc3339();
        journal.error = Some("Recovered before any Codex data write began".to_string());
        write_journal(journal_path, &journal)?;
        return Ok(journal);
    }

    let backup_manifest: BackupManifest = read_json(&backup_manifest_path)?;
    if backup_manifest.operation_id != journal.operation_id {
        bail!(
            "backup manifest does not belong to operation {}",
            journal.operation_id
        );
    }

    let mut preserved_paths = Vec::new();
    for rollout in &journal.planned_rollouts {
        let _ = fs::remove_file(&rollout.temporary_path);
        if rollout.target_path.is_file() {
            if sha256_file(&rollout.target_path).ok().as_deref() == Some(rollout.sha256.as_str()) {
                fs::remove_file(&rollout.target_path)?;
            } else {
                preserved_paths.push(rollout.target_path.display().to_string());
            }
        }
    }
    restore_database(
        &backup_manifest.target_database,
        &backup_manifest.database_backup,
    )?;
    journal.status = OperationStatus::RolledBack;
    journal.updated_at = Utc::now().to_rfc3339();
    journal.error = Some(if preserved_paths.is_empty() {
        "Recovered an incomplete operation from its backup".to_string()
    } else {
        format!(
            "Recovered database; preserved files whose hashes no longer matched the operation: {}",
            preserved_paths.join(", ")
        )
    });
    write_journal(journal_path, &journal)?;
    Ok(journal)
}

fn apply_import(
    prepared: &[PreparedThread],
    target_database: &Path,
    target_codex_home: &Path,
    journal: &mut OperationJournal,
    journal_path: &Path,
    control: &OperationControl,
) -> Result<()> {
    journal.status = OperationStatus::Applying;
    journal.updated_at = Utc::now().to_rfc3339();
    write_journal(journal_path, journal)?;

    for (index, thread) in prepared.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "import_rollouts".to_string(),
            message: thread.bundle.title.clone(),
            completed: index as u64,
            total: Some(prepared.len() as u64),
            unit: "threads".to_string(),
            cancellable: true,
        });
        if let Some(parent) = thread.temporary_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if thread.bundle.rollout.storage.is_some() {
            FilesystemContentStore::open(thread.repository_root.clone())?.materialize(
                &thread.content,
                &thread.temporary_path,
                control,
            )?;
        } else {
            copy_verified_object(
                &repository_object_path(&thread.repository_root, &thread.bundle.rollout.sha256)?,
                &thread.temporary_path,
                &thread.bundle.rollout.sha256,
                Some(control),
            )?;
        }
    }

    control.check_cancelled()?;
    control.report(OperationProgress::indeterminate(
        "import_database",
        "正在写入 SQLite 事务",
    ));

    let mut connection = Connection::open(target_database).with_context(|| {
        format!(
            "failed to open target database {}",
            target_database.display()
        )
    })?;
    connection.busy_timeout(Duration::from_millis(250))?;
    let columns = thread_table_columns(&connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("cannot acquire SQLite write lock; make sure Codex is fully closed")?;
    for thread in prepared {
        insert_thread_row(
            &transaction,
            &columns,
            &thread.bundle,
            &thread.target_path,
            target_codex_home,
        )?;
    }
    for thread in prepared {
        insert_related_records(&transaction, &thread.bundle)?;
    }
    transaction.commit()?;

    for thread in prepared {
        control.check_cancelled()?;
        fs::rename(&thread.temporary_path, &thread.target_path).with_context(|| {
            format!("failed to install rollout {}", thread.target_path.display())
        })?;
    }
    Ok(())
}

fn rollback_and_fail(
    error: anyhow::Error,
    prepared: &[PreparedThread],
    target_database: &Path,
    database_backup: &Path,
    journal: &mut OperationJournal,
    journal_path: &Path,
    control: &OperationControl,
) -> Result<ImportReport> {
    control.report(OperationProgress {
        phase: "rollback".to_string(),
        message: "正在恢复 SQLite 备份并清理导入文件".to_string(),
        completed: 0,
        total: None,
        unit: "steps".to_string(),
        cancellable: false,
    });
    for thread in prepared {
        let _ = fs::remove_file(&thread.temporary_path);
        if thread.target_path.is_file()
            && sha256_file(&thread.target_path).ok().as_deref()
                == Some(thread.bundle.rollout.sha256.as_str())
        {
            let _ = fs::remove_file(&thread.target_path);
        }
    }
    let rollback_result = restore_database(target_database, database_backup);
    journal.status = if rollback_result.is_ok() {
        OperationStatus::RolledBack
    } else {
        OperationStatus::RecoveryRequired
    };
    journal.updated_at = Utc::now().to_rfc3339();
    journal.error = Some(match &rollback_result {
        Ok(()) => error.to_string(),
        Err(rollback_error) => format!("{error}; database rollback also failed: {rollback_error}"),
    });
    let _ = write_journal(journal_path, journal);
    rollback_result?;
    Err(error)
}

pub(crate) fn insert_thread_row(
    connection: &Connection,
    target_columns: &[String],
    thread: &ThreadBundle,
    target_rollout: &Path,
    target_codex_home: &Path,
) -> Result<()> {
    let rows = thread
        .related_records
        .tables
        .get("threads")
        .context("snapshot thread has no threads-table record")?;
    let row = rows
        .first()
        .and_then(Value::as_object)
        .context("snapshot thread record is not a JSON object")?;
    let created_at_ms = thread.created_at_ms.or_else(|| {
        row.get("created_at_ms")
            .and_then(Value::as_i64)
            .or_else(|| {
                row.get("created_at")
                    .and_then(Value::as_i64)
                    .and_then(|value| value.checked_mul(1_000))
            })
    });
    let updated_at_ms = thread.updated_at_ms.or_else(|| {
        row.get("updated_at_ms")
            .and_then(Value::as_i64)
            .or_else(|| {
                row.get("updated_at")
                    .and_then(Value::as_i64)
                    .and_then(|value| value.checked_mul(1_000))
            })
    });
    let recency_at_ms = updated_at_ms.or(created_at_ms);

    let mut columns = Vec::new();
    let mut values = Vec::new();
    for column in target_columns {
        let value = if column == "rollout_path" {
            Some(Value::String(target_rollout.to_string_lossy().into_owned()))
        } else if column == "codex_home" {
            Some(Value::String(
                target_codex_home.to_string_lossy().into_owned(),
            ))
        } else if column == "created_at_ms" {
            row.get(column)
                .cloned()
                .or_else(|| created_at_ms.map(Value::from))
        } else if column == "updated_at_ms" {
            row.get(column)
                .cloned()
                .or_else(|| updated_at_ms.map(Value::from))
        } else if column == "created_at" {
            row.get(column)
                .cloned()
                .or_else(|| created_at_ms.map(|value| Value::from(value / 1_000)))
        } else if column == "updated_at" {
            row.get(column)
                .cloned()
                .or_else(|| updated_at_ms.map(|value| Value::from(value / 1_000)))
        } else if column == "recency_at_ms" {
            row.get(column)
                .cloned()
                .or_else(|| recency_at_ms.map(Value::from))
        } else if column == "recency_at" {
            row.get(column)
                .cloned()
                .or_else(|| recency_at_ms.map(|value| Value::from(value / 1_000)))
        } else {
            row.get(column).cloned()
        };
        if let Some(value) = value {
            columns.push(column.clone());
            values.push(json_to_sql_value(&value)?);
        }
    }
    if !columns.iter().any(|column| column == "id") {
        bail!("target threads table or snapshot row has no id column");
    }

    let column_sql = columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT INTO \"threads\" ({column_sql}) VALUES ({placeholders})");
    connection
        .execute(&sql, params_from_iter(values.iter()))
        .with_context(|| format!("failed to insert thread {}", thread.thread_id))?;
    Ok(())
}

pub(crate) fn insert_related_records(connection: &Connection, thread: &ThreadBundle) -> Result<()> {
    for (table, rows) in &thread.related_records.tables {
        if table == "threads" {
            continue;
        }
        let columns = table_columns(connection, table)?;
        if columns.is_empty() {
            bail!(
                "target database has no related table {table} required by thread {}",
                thread.thread_id
            );
        }
        for row in rows {
            let row = row.as_object().with_context(|| {
                format!(
                    "thread {} contains a non-object record for table {table}",
                    thread.thread_id
                )
            })?;
            let mut selected_columns = Vec::new();
            let mut values = Vec::new();
            for column in &columns {
                if let Some(value) = row.get(column) {
                    selected_columns.push(column.clone());
                    values.push(json_to_sql_value(value)?);
                }
            }
            if selected_columns.is_empty() {
                bail!(
                    "thread {} record for table {table} has no compatible columns",
                    thread.thread_id
                );
            }
            let column_sql = selected_columns
                .iter()
                .map(|column| quote_identifier(column))
                .collect::<Vec<_>>()
                .join(", ");
            let placeholders = (1..=values.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {} ({column_sql}) VALUES ({placeholders})",
                quote_identifier(table)
            );
            connection
                .execute(&sql, params_from_iter(values.iter()))
                .with_context(|| {
                    format!(
                        "failed to insert related {table} record for thread {}",
                        thread.thread_id
                    )
                })?;
        }
    }
    Ok(())
}

pub(crate) fn thread_table_columns(connection: &Connection) -> Result<Vec<String>> {
    let columns = table_columns(connection, "threads")?;
    if columns.is_empty() {
        bail!("target database has no threads table");
    }
    Ok(columns)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare(&format!("PRAGMA table_info({})", quote_identifier(table)))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

pub(crate) fn select_primary_database(paths: &[PathBuf]) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    for path in paths {
        let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(connection) => connection,
            Err(_) => continue,
        };
        let count = connection
            .query_row("SELECT COUNT(*) FROM threads", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(-1);
        if count >= 0 {
            candidates.push((count, path.clone()));
        }
    }
    candidates
        .into_iter()
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)))
        .map(|(_, path)| path)
        .context("target Codex home has no writable threads database")
}

pub(crate) fn backup_database(source: &Path, destination: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection
        .backup(MAIN_DB, destination, None)
        .with_context(|| format!("failed to back up database {}", source.display()))
}

pub(crate) fn restore_database(target: &Path, source_backup: &Path) -> Result<()> {
    let mut connection = Connection::open(target)?;
    connection
        .restore(
            MAIN_DB,
            source_backup,
            None::<fn(rusqlite::backup::Progress)>,
        )
        .with_context(|| format!("failed to restore database {}", target.display()))
}

pub(crate) fn validate_snapshot_structure(snapshot: &LocalSnapshot) -> Result<()> {
    if snapshot.schema_version != LOCAL_SNAPSHOT_SCHEMA_VERSION {
        bail!(
            "unsupported snapshot schema version {}; expected {}",
            snapshot.schema_version,
            LOCAL_SNAPSHOT_SCHEMA_VERSION
        );
    }
    let mut thread_ids = HashSet::new();
    for thread in &snapshot.threads {
        if !thread_ids.insert(thread.thread_id.as_str()) {
            bail!("snapshot contains duplicate thread id {}", thread.thread_id);
        }
        safe_rollout_path(thread)?;
    }
    Ok(())
}

fn ensure_importable_thread(thread: &ThreadBundle) -> Result<()> {
    let rows = thread
        .related_records
        .tables
        .get("threads")
        .with_context(|| format!("thread {} has no database record", thread.thread_id))?;
    if rows.len() != 1 || !rows[0].is_object() {
        bail!(
            "thread {} must contain exactly one threads-table record",
            thread.thread_id
        );
    }
    Ok(())
}

pub(crate) fn safe_rollout_path(thread: &ThreadBundle) -> Result<PathBuf> {
    let logical_path = thread
        .rollout
        .logical_path
        .as_deref()
        .with_context(|| format!("thread {} has no rollout logical path", thread.thread_id))?;
    let path = PathBuf::from(logical_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe rollout path in snapshot: {logical_path}");
    }
    let root = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str());
    if !matches!(root, Some("sessions" | "archived_sessions")) {
        bail!(
            "thread {} rollout path must be below sessions or archived_sessions",
            thread.thread_id
        );
    }
    Ok(path)
}

pub(crate) fn ensure_repository_layout(root: &Path) -> Result<()> {
    for directory in [
        "objects/tmp",
        "index",
        "snapshots",
        "backups",
        "journal",
        "objects/whole/sha256",
        "objects/chunks/sha256",
        "objects/chunk-manifests/sha256",
        "objects/threads/sha256",
        "objects/revision-roots/sha256",
        "trash",
        "quarantine",
    ] {
        fs::create_dir_all(root.join(directory))?;
    }
    Ok(())
}

fn source_object_index_path(root: &Path) -> PathBuf {
    root.join("index").join("source-objects-v2.json")
}

fn load_source_object_index(path: &Path) -> SourceObjectIndex {
    let Ok(file) = File::open(path) else {
        return SourceObjectIndex::default();
    };
    let Ok(index) = serde_json::from_reader::<_, SourceObjectIndex>(BufReader::new(file)) else {
        return SourceObjectIndex::default();
    };
    if index.schema_version != SOURCE_OBJECT_INDEX_SCHEMA_VERSION {
        return SourceObjectIndex::default();
    }
    index
}

fn source_index_key(source: &Path) -> String {
    fs::canonicalize(source)
        .unwrap_or_else(|_| source.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

fn source_fingerprint(source: &Path) -> Result<SourceFingerprint> {
    let metadata = fs::metadata(source)
        .with_context(|| format!("failed to stat rollout {}", source.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modification time for {}", source.display()))?;
    let modified_unix_nanos = modified
        .duration_since(UNIX_EPOCH)
        .with_context(|| format!("invalid modification time for {}", source.display()))?
        .as_nanos()
        .try_into()
        .with_context(|| format!("modification time is out of range for {}", source.display()))?;
    Ok(SourceFingerprint {
        byte_length: metadata.len(),
        modified_unix_nanos,
    })
}

fn store_snapshot_object(
    source: &Path,
    expected_length: u64,
    repository_root: &Path,
    source_index: &mut SourceObjectIndex,
    control: &OperationControl,
) -> Result<(String, StorageRef)> {
    let before = source_fingerprint(source)?;
    if before.byte_length != expected_length {
        bail!(
            "rollout changed after metadata scan: {} expected {} bytes, got {}",
            source.display(),
            expected_length,
            before.byte_length
        );
    }
    let source_key = source_index_key(source);
    if let Some(entry) = source_index.entries.get(&source_key)
        && entry.byte_length == before.byte_length
        && entry.modified_unix_nanos == before.modified_unix_nanos
        && let Some(storage) = entry.storage.clone()
    {
        let content = crate::storage_v2::ContentRef {
            logical_sha256: entry.sha256.clone(),
            byte_length: entry.byte_length,
            storage: storage.clone(),
            media_type: None,
            logical_path: None,
        };
        if FilesystemContentStore::open(repository_root.to_path_buf())?
            .content_objects(&content)
            .is_ok()
        {
            return Ok((entry.sha256.clone(), storage));
        }
    }

    let content =
        FilesystemContentStore::open(repository_root.to_path_buf())?.ingest(source, control)?;
    let after = source_fingerprint(source)?;
    if after != before || content.byte_length != before.byte_length {
        bail!(
            "rollout changed while creating snapshot: {}",
            source.display()
        );
    }
    let sha256 = content.logical_sha256;
    let storage = content.storage;
    source_index.entries.insert(
        source_key,
        SourceObjectIndexEntry {
            byte_length: before.byte_length,
            modified_unix_nanos: before.modified_unix_nanos,
            sha256: sha256.clone(),
            storage: Some(storage.clone()),
        },
    );
    Ok((sha256, storage))
}

fn content_ref_from_object(object: &crate::ContentObject) -> Result<crate::storage_v2::ContentRef> {
    Ok(crate::storage_v2::ContentRef {
        logical_sha256: object.sha256.clone(),
        byte_length: object.byte_length,
        storage: object
            .storage
            .clone()
            .context("v2 content object has no physical storage reference")?,
        media_type: Some(object.media_type.clone()),
        logical_path: object.logical_path.clone(),
    })
}

fn snapshot_repository_root(path: &Path) -> Result<PathBuf> {
    path.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("v2 snapshot is not below the repository snapshots directory")
}

pub(crate) fn copy_verified_object(
    source: &Path,
    destination: &Path,
    expected_hash: &str,
    control: Option<&OperationControl>,
) -> Result<()> {
    let input = File::open(source)?;
    let mut reader = BufReader::new(input);
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut writer = BufWriter::new(output);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if let Some(control) = control
            && control.is_cancelled()
        {
            drop(writer);
            let _ = fs::remove_file(destination);
            bail!("operation cancelled");
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        writer.write_all(&buffer[..count])?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let actual_hash = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual_hash != expected_hash {
        drop(writer);
        let _ = fs::remove_file(destination);
        bail!("content hash mismatch: expected {expected_hash}, got {actual_hash}");
    }
    Ok(())
}

fn validate_object(path: &Path, expected_hash: &str, expected_length: u64) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("missing content object {}", path.display()))?;
    if metadata.len() != expected_length {
        bail!(
            "content object length mismatch for {}: expected {}, got {}",
            path.display(),
            expected_length,
            metadata.len()
        );
    }
    let actual_hash = sha256_file(path)?;
    if actual_hash != expected_hash {
        bail!(
            "content object hash mismatch for {}: expected {}, got {}",
            path.display(),
            expected_hash,
            actual_hash
        );
    }
    Ok(())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn json_to_sql_value(value: &Value) -> Result<SqlValue> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer(i64::from(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                SqlValue::Integer(value)
            } else if let Some(value) = value.as_f64() {
                SqlValue::Real(value)
            } else {
                bail!("unsupported SQLite numeric value {value}");
            }
        }
        Value::String(value) => SqlValue::Text(value.clone()),
        Value::Object(value) if value.get("$type").and_then(Value::as_str) == Some("blob") => {
            let encoded = value
                .get("base64")
                .and_then(Value::as_str)
                .context("blob value has no base64 field")?;
            SqlValue::Blob(base64::engine::general_purpose::STANDARD.decode(encoded)?)
        }
        Value::Array(_) | Value::Object(_) => SqlValue::Text(serde_json::to_string(value)?),
    })
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn temporary_sibling(path: &Path, purpose: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(".{file_name}.{purpose}-{}.tmp", Uuid::now_v7()))
}

pub(crate) fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_sibling(path, "write");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn write_journal(path: &Path, journal: &OperationJournal) -> Result<()> {
    atomic_write_json(path, journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::scan_codex_home;
    use crate::models::{
        ContentObject, RelatedRecords, THREAD_BUNDLE_SCHEMA_VERSION, WorkspaceRef,
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

    fn create_codex_home(root: &Path, threads: &[(&str, &str, &str)]) {
        fs::create_dir_all(root.join("sessions/2026/07/24")).unwrap();
        let database = root.join("state_5.sqlite");
        let connection = Connection::open(database).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, archived INTEGER NOT NULL, rollout_path TEXT, cwd TEXT, model_provider TEXT)",
                [],
            )
            .unwrap();
        for (id, title, body) in threads {
            let rollout = root
                .join("sessions/2026/07/24")
                .join(format!("rollout-{id}.jsonl"));
            let content = format!(
                "{}\n{}",
                json!({
                    "type": "session_meta",
                    "payload": {"id": id, "cwd": "/tmp/demo", "model_provider": "openai"}
                }),
                body
            );
            fs::write(&rollout, content).unwrap();
            connection
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2, 0, ?3, '/tmp/demo', 'openai')",
                    (id, title, rollout.to_string_lossy().as_ref()),
                )
                .unwrap();
        }
    }

    #[test]
    fn insert_thread_row_synthesizes_recency_from_bundle_timestamp() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    updated_at_ms INTEGER,
                    recency_at INTEGER NOT NULL DEFAULT 0,
                    recency_at_ms INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        let timestamp_ms = 1_700_000_100_123_i64;
        let thread = ThreadBundle {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: "thread".to_string(),
            title: "Thread".to_string(),
            archived: false,
            created_at_ms: None,
            updated_at_ms: Some(timestamp_ms),
            model_provider: Some("openai".to_string()),
            workspace: WorkspaceRef {
                logical_id: None,
                source_path: Some("C:/work".to_string()),
            },
            rollout: ContentObject {
                sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
                byte_length: 0,
                media_type: "application/x-ndjson".to_string(),
                logical_path: Some("sessions/rollout-thread.jsonl".to_string()),
                source_path: None,
                storage: None,
            },
            related_records: RelatedRecords {
                source_database: None,
                tables: BTreeMap::from([(
                    "threads".to_string(),
                    vec![json!({"id": "thread", "cwd": "C:/work"})],
                )]),
            },
            attachments: Vec::new(),
        };
        let columns = thread_table_columns(&connection).unwrap();

        insert_thread_row(
            &connection,
            &columns,
            &thread,
            Path::new("C:/codex/sessions/rollout-thread.jsonl"),
            Path::new("C:/codex"),
        )
        .unwrap();

        let values: (i64, i64, i64) = connection
            .query_row(
                "SELECT updated_at_ms, recency_at, recency_at_ms FROM threads WHERE id = 'thread'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(values, (timestamp_ms, timestamp_ms / 1_000, timestamp_ms));
    }

    #[test]
    fn creates_and_validates_content_addressed_snapshot() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );

        let summary = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        assert_eq!(summary.thread_count, 1);
        assert_eq!(summary.object_count, 1);
        let validation =
            validate_local_snapshot(&summary.manifest_path, repository.path()).unwrap();
        assert!(validation.valid);
        assert_eq!(validation.snapshot_id, summary.snapshot_id);
    }

    #[test]
    fn snapshot_hashes_while_copying_without_a_separate_hash_phase() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", &"x".repeat(1024 * 1024))],
        );
        let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reporter_phases = phases.clone();
        let control = OperationControl::new(Arc::new(AtomicBool::new(false)), move |progress| {
            reporter_phases.lock().unwrap().push(progress.phase);
        });

        create_local_snapshot_with_control(source.path(), repository.path(), true, &control)
            .unwrap();

        assert!(
            !phases
                .lock()
                .unwrap()
                .iter()
                .any(|phase| phase == "hash_rollouts")
        );
    }

    #[test]
    fn unchanged_source_reuses_trusted_index_but_full_validation_still_detects_corruption() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(source.path(), &[("thread-1", "Demo", "abcdefgh")]);
        let first = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let manifest = load_local_snapshot(&first.manifest_path).unwrap();
        let object =
            repository_object_path(repository.path(), &manifest.threads[0].rollout.sha256).unwrap();
        let mut bytes = fs::read(&object).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&object, bytes).unwrap();

        let second = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let error = validate_local_snapshot(&second.manifest_path, repository.path()).unwrap_err();

        assert!(error.to_string().contains("storage object"));
    }

    #[test]
    fn changed_source_invalidates_trusted_index() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(source.path(), &[("thread-1", "Demo", "abcdefgh")]);
        let first = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let first_manifest = load_local_snapshot(&first.manifest_path).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let rollout = source
            .path()
            .join("sessions/2026/07/24/rollout-thread-1.jsonl");
        let original = fs::read_to_string(&rollout).unwrap();
        fs::write(&rollout, original.replace("abcdefgh", "abcdxfgh")).unwrap();

        let second = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let second_manifest = load_local_snapshot(&second.manifest_path).unwrap();

        assert_ne!(
            first_manifest.threads[0].rollout.sha256,
            second_manifest.threads[0].rollout.sha256
        );
        validate_local_snapshot(&second.manifest_path, repository.path()).unwrap();
    }

    #[test]
    fn accepts_archived_directory_when_database_state_is_not_archived() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        let original = source
            .path()
            .join("sessions/2026/07/24/rollout-thread-1.jsonl");
        let archived = source
            .path()
            .join("archived_sessions/rollout-thread-1.jsonl");
        fs::create_dir_all(archived.parent().unwrap()).unwrap();
        fs::rename(&original, &archived).unwrap();
        let connection = Connection::open(source.path().join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "UPDATE threads SET archived = 0, rollout_path = ?1 WHERE id = 'thread-1'",
                [archived.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);

        let summary = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let validation =
            validate_local_snapshot(&summary.manifest_path, repository.path()).unwrap();
        assert!(validation.valid);
    }

    #[test]
    fn rejects_rollout_paths_outside_codex_session_directories() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        let summary = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let mut manifest = load_local_snapshot(&summary.manifest_path).unwrap();
        manifest.threads[0].rollout.logical_path = Some("other/rollout-thread-1.jsonl".to_string());
        let error = store_local_snapshot(&manifest, repository.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must be below sessions or archived_sessions")
        );
    }

    #[test]
    fn imports_new_thread_with_backup_and_completed_journal() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        create_codex_home(target.path(), &[]);
        let snapshot = create_local_snapshot(source.path(), repository.path(), true).unwrap();

        let report = import_local_snapshot(
            &snapshot.manifest_path,
            target.path(),
            repository.path(),
            true,
        )
        .unwrap();
        assert_eq!(report.imported_count, 1);
        assert!(report.backup_dir.join("threads.sqlite").is_file());
        let journal: OperationJournal = read_json(&report.journal_path).unwrap();
        assert_eq!(journal.status, OperationStatus::Completed);
        let scan = scan_codex_home(target.path()).unwrap();
        assert_eq!(scan.total_count(), 1);
        assert_eq!(scan.threads[0].title, "Demo");
    }

    #[test]
    fn rejects_divergent_thread_before_writing() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(source.path(), &[("thread-1", "Source", "{\"value\":1}")]);
        create_codex_home(target.path(), &[("thread-1", "Target", "{\"value\":2}")]);
        let snapshot = create_local_snapshot(source.path(), repository.path(), true).unwrap();

        let error = import_local_snapshot(
            &snapshot.manifest_path,
            target.path(),
            repository.path(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("conflicts with the target"));
        let scan = scan_codex_home(target.path()).unwrap();
        assert_eq!(scan.threads[0].title, "Target");
    }

    #[test]
    fn rejects_corrupt_object_before_backup() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(source.path(), &[("thread-1", "Demo", "{\"value\":1}")]);
        create_codex_home(target.path(), &[]);
        let snapshot = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let manifest = load_local_snapshot(&snapshot.manifest_path).unwrap();
        let object =
            repository_object_path(repository.path(), &manifest.threads[0].rollout.sha256).unwrap();
        fs::write(object, "corrupt").unwrap();

        let error = import_local_snapshot(
            &snapshot.manifest_path,
            target.path(),
            repository.path(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("storage object"));
        assert_eq!(
            fs::read_dir(repository.path().join("backups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn restores_database_and_removes_rollout_when_apply_fails() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        create_codex_home(target.path(), &[]);
        let target_database = target.path().join("state_5.sqlite");
        let connection = Connection::open(&target_database).unwrap();
        connection.execute("DROP TABLE threads", []).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL CHECK(title != 'blocked'), archived INTEGER NOT NULL, rollout_path TEXT, cwd TEXT, model_provider TEXT)",
                [],
            )
            .unwrap();
        drop(connection);

        let snapshot = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let mut manifest = load_local_snapshot(&snapshot.manifest_path).unwrap();
        manifest.threads[0].title = "blocked".to_string();
        manifest.threads[0]
            .related_records
            .tables
            .get_mut("threads")
            .unwrap()[0]["title"] = Value::String("blocked".to_string());
        manifest.snapshot_id = Uuid::now_v7().to_string();
        let blocked_manifest = store_local_snapshot(&manifest, repository.path()).unwrap();

        let error =
            import_local_snapshot(&blocked_manifest, target.path(), repository.path(), true)
                .unwrap_err();
        assert!(!error.to_string().is_empty());
        let connection = Connection::open(&target_database).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(scan_codex_home(target.path()).unwrap().total_count(), 0);
        let journal_path = fs::read_dir(repository.path().join("journal"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let journal: OperationJournal = read_json(&journal_path).unwrap();
        assert_eq!(journal.status, OperationStatus::RolledBack);
        assert!(journal.error.unwrap().contains("failed to insert thread"));
    }

    #[test]
    fn recovers_an_incomplete_journal_after_restart() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        create_codex_home(target.path(), &[]);
        let snapshot = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let manifest = load_local_snapshot(&snapshot.manifest_path).unwrap();
        let thread = &manifest.threads[0];
        let object = repository_object_path(repository.path(), &thread.rollout.sha256).unwrap();
        let target_rollout = target.path().join(safe_rollout_path(thread).unwrap());
        fs::create_dir_all(target_rollout.parent().unwrap()).unwrap();
        fs::copy(&object, &target_rollout).unwrap();
        let temporary_path = temporary_sibling(&target_rollout, "import");
        fs::write(&temporary_path, "partial").unwrap();

        let target_database = target.path().join("state_5.sqlite");
        let operation_id = Uuid::now_v7().to_string();
        let backup_dir = repository.path().join("backups").join(&operation_id);
        fs::create_dir_all(&backup_dir).unwrap();
        let database_backup = backup_dir.join("threads.sqlite");
        backup_database(&target_database, &database_backup).unwrap();
        atomic_write_json(
            &backup_dir.join("manifest.json"),
            &BackupManifest {
                operation_id: operation_id.clone(),
                created_at: Utc::now().to_rfc3339(),
                target_database: target_database.clone(),
                database_backup,
            },
        )
        .unwrap();
        let connection = Connection::open(&target_database).unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES ('thread-1', 'Demo', 0, ?1, '/tmp/demo', 'openai')",
                [target_rollout.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(connection);

        let journal_path = repository
            .path()
            .join("journal")
            .join(format!("{operation_id}.json"));
        let now = Utc::now().to_rfc3339();
        atomic_write_json(
            &journal_path,
            &OperationJournal {
                schema_version: OPERATION_JOURNAL_SCHEMA_VERSION,
                operation_id,
                snapshot_id: manifest.snapshot_id,
                target_codex_home: target.path().to_path_buf(),
                backup_dir,
                status: OperationStatus::Applying,
                started_at: now.clone(),
                updated_at: now,
                planned_rollouts: vec![JournalRollout {
                    target_path: target_rollout.clone(),
                    temporary_path: temporary_path.clone(),
                    sha256: thread.rollout.sha256.clone(),
                }],
                imported_thread_ids: Vec::new(),
                error: None,
            },
        )
        .unwrap();

        let recovered = recover_incomplete_operation(&journal_path, true).unwrap();
        assert_eq!(recovered.status, OperationStatus::RolledBack);
        assert!(!target_rollout.exists());
        assert!(!temporary_path.exists());
        let connection = Connection::open(target_database).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn closes_preparing_journal_when_no_backup_or_write_exists() {
        let repository = tempdir().unwrap();
        let operation_id = Uuid::now_v7().to_string();
        let backup_dir = repository.path().join("backups").join(&operation_id);
        fs::create_dir_all(&backup_dir).unwrap();
        let temporary_path = repository.path().join("unfinished.tmp");
        fs::write(&temporary_path, "partial").unwrap();
        let journal_path = repository.path().join("journal.json");
        let now = Utc::now().to_rfc3339();
        atomic_write_json(
            &journal_path,
            &OperationJournal {
                schema_version: OPERATION_JOURNAL_SCHEMA_VERSION,
                operation_id,
                snapshot_id: "snapshot-1".to_string(),
                target_codex_home: repository.path().join("codex"),
                backup_dir,
                status: OperationStatus::Preparing,
                started_at: now.clone(),
                updated_at: now,
                planned_rollouts: vec![JournalRollout {
                    target_path: repository.path().join("not-created.jsonl"),
                    temporary_path: temporary_path.clone(),
                    sha256: format!("sha256:{}", "0".repeat(64)),
                }],
                imported_thread_ids: Vec::new(),
                error: None,
            },
        )
        .unwrap();

        let recovered = recover_incomplete_operation(&journal_path, true).unwrap();
        assert_eq!(recovered.status, OperationStatus::RolledBack);
        assert!(!temporary_path.exists());
    }

    #[test]
    fn cancellation_during_import_rolls_back_before_any_target_file_is_installed() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        create_codex_home(target.path(), &[]);
        let snapshot = create_local_snapshot(source.path(), repository.path(), true).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_on_copy = cancelled.clone();
        let control = OperationControl::new(cancelled, move |progress| {
            if progress.phase == "import_rollouts" {
                cancel_on_copy.store(true, Ordering::Relaxed);
            }
        });

        let error = import_local_snapshot_with_control(
            &snapshot.manifest_path,
            target.path(),
            repository.path(),
            true,
            &control,
        )
        .unwrap_err();
        assert!(error.to_string().contains("operation cancelled"));
        assert_eq!(
            scan_codex_home_with_control(target.path(), &OperationControl::default())
                .unwrap()
                .total_count(),
            0
        );
        let connection = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn cancellation_during_snapshot_cleans_temporary_object_file() {
        let source = tempdir().unwrap();
        let repository = tempdir().unwrap();
        create_codex_home(
            source.path(),
            &[("thread-1", "Demo", "{\"type\":\"event\"}")],
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_on_object_copy = cancelled.clone();
        let control = OperationControl::new(cancelled, move |progress| {
            if progress.phase == "snapshot_objects" {
                cancel_on_object_copy.store(true, Ordering::Relaxed);
            }
        });

        let error =
            create_local_snapshot_with_control(source.path(), repository.path(), true, &control)
                .unwrap_err();
        assert!(error.to_string().contains("operation cancelled"));
        let temporary_files = walkdir::WalkDir::new(repository.path())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".object-"))
            .count();
        assert_eq!(temporary_files, 0);
    }
}
