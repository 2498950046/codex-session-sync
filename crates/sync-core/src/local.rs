use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, MAIN_DB, OpenFlags, TransactionBehavior, params_from_iter};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::codex::scan_codex_home;
use crate::models::{
    ImportReport, JournalRollout, LOCAL_SNAPSHOT_SCHEMA_VERSION, LocalSnapshot,
    OPERATION_JOURNAL_SCHEMA_VERSION, OperationJournal, OperationStatus, SnapshotSummary,
    SnapshotValidationReport, ThreadBundle,
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
    object_path: PathBuf,
    target_path: PathBuf,
    temporary_path: PathBuf,
}

pub fn default_repository_root() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".codex-session-sync"))
        .unwrap_or_else(|| PathBuf::from(".codex-session-sync"))
}

pub fn create_local_snapshot(
    codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<SnapshotSummary> {
    if !confirmed_codex_closed {
        bail!("snapshot creation requires confirmation that Codex is fully closed");
    }
    let repository_root = repository_root.as_ref();
    let report = scan_codex_home(codex_home)?;
    ensure_repository_layout(repository_root)?;

    let mut unique_objects = BTreeSet::new();
    for thread in &report.threads {
        let source_path = thread.rollout.source_path.as_deref().with_context(|| {
            format!(
                "thread {} has no local rollout source path",
                thread.thread_id
            )
        })?;
        let object_path = object_path(repository_root, &thread.rollout.sha256)?;
        store_object(source_path, &object_path, &thread.rollout.sha256)?;
        unique_objects.insert(thread.rollout.sha256.clone());
    }

    let snapshot_id = Uuid::now_v7().to_string();
    let snapshot = LocalSnapshot {
        schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: snapshot_id.clone(),
        created_at: Utc::now().to_rfc3339(),
        threads: report.threads,
        warning_count: report.warnings.len(),
    };
    let manifest_path = repository_root
        .join("snapshots")
        .join(format!("{snapshot_id}.json"));
    atomic_write_json(&manifest_path, &snapshot)?;

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
    let manifest_path = manifest_path.as_ref();
    let repository_root = repository_root.as_ref();
    let snapshot: LocalSnapshot = read_json(manifest_path)?;
    validate_snapshot_structure(&snapshot)?;

    let mut unique_objects = BTreeSet::new();
    for thread in &snapshot.threads {
        let object_path = object_path(repository_root, &thread.rollout.sha256)?;
        validate_object(
            &object_path,
            &thread.rollout.sha256,
            thread.rollout.byte_length,
        )?;
        unique_objects.insert(thread.rollout.sha256.clone());
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
    if !confirmed_codex_closed {
        bail!("import requires confirmation that Codex is fully closed");
    }

    let manifest_path = manifest_path.as_ref();
    let target_codex_home = target_codex_home.as_ref().to_path_buf();
    let repository_root = repository_root.as_ref().to_path_buf();
    let validation = validate_local_snapshot(manifest_path, &repository_root)?;
    let snapshot: LocalSnapshot = read_json(manifest_path)?;
    let target_report = scan_codex_home(&target_codex_home)?;
    let existing = target_report
        .threads
        .iter()
        .map(|thread| (thread.thread_id.as_str(), thread.rollout.sha256.as_str()))
        .collect::<HashMap<_, _>>();

    let mut skipped_count = 0_usize;
    let mut prepared = Vec::new();
    let mut target_paths = HashSet::new();
    for thread in &snapshot.threads {
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
            object_path: object_path(&repository_root, &thread.rollout.sha256)?,
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
    );
    if let Err(error) = import_result {
        return rollback_and_fail(
            error,
            &prepared,
            &target_database,
            &database_backup,
            &mut journal,
            &journal_path,
        );
    }

    journal.status = OperationStatus::Validating;
    journal.updated_at = Utc::now().to_rfc3339();
    write_journal(&journal_path, &journal)?;
    let post_import = scan_codex_home(&target_codex_home);
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
) -> Result<()> {
    journal.status = OperationStatus::Applying;
    journal.updated_at = Utc::now().to_rfc3339();
    write_journal(journal_path, journal)?;

    for thread in prepared {
        if let Some(parent) = thread.temporary_path.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_verified_object(
            &thread.object_path,
            &thread.temporary_path,
            &thread.bundle.rollout.sha256,
        )?;
    }

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
    transaction.commit()?;

    for thread in prepared {
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
) -> Result<ImportReport> {
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

fn insert_thread_row(
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

    let mut columns = Vec::new();
    let mut values = Vec::new();
    for column in target_columns {
        let value = if column == "rollout_path" {
            Some(Value::String(target_rollout.to_string_lossy().into_owned()))
        } else if column == "codex_home" {
            Some(Value::String(
                target_codex_home.to_string_lossy().into_owned(),
            ))
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

fn thread_table_columns(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA table_info(threads)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if columns.is_empty() {
        bail!("target database has no threads table");
    }
    Ok(columns)
}

fn select_primary_database(paths: &[PathBuf]) -> Result<PathBuf> {
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

fn backup_database(source: &Path, destination: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection
        .backup(MAIN_DB, destination, None)
        .with_context(|| format!("failed to back up database {}", source.display()))
}

fn restore_database(target: &Path, source_backup: &Path) -> Result<()> {
    let mut connection = Connection::open(target)?;
    connection
        .restore(
            MAIN_DB,
            source_backup,
            None::<fn(rusqlite::backup::Progress)>,
        )
        .with_context(|| format!("failed to restore database {}", target.display()))
}

fn validate_snapshot_structure(snapshot: &LocalSnapshot) -> Result<()> {
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

fn safe_rollout_path(thread: &ThreadBundle) -> Result<PathBuf> {
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
    let expected_root = if thread.archived {
        "archived_sessions"
    } else {
        "sessions"
    };
    if path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        != Some(expected_root)
    {
        bail!(
            "thread {} rollout path must be below {expected_root}",
            thread.thread_id
        );
    }
    Ok(path)
}

fn ensure_repository_layout(root: &Path) -> Result<()> {
    for directory in ["objects/sha256", "snapshots", "backups", "journal"] {
        fs::create_dir_all(root.join(directory))?;
    }
    Ok(())
}

fn object_path(root: &Path, sha256: &str) -> Result<PathBuf> {
    let digest = sha256
        .strip_prefix("sha256:")
        .context("content hash must start with sha256:")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest {sha256}");
    }
    Ok(root
        .join("objects")
        .join("sha256")
        .join(&digest[..2])
        .join(&digest[2..]))
}

fn store_object(source: &Path, destination: &Path, expected_hash: &str) -> Result<()> {
    if destination.exists() {
        validate_object(destination, expected_hash, fs::metadata(source)?.len())?;
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_sibling(destination, "object");
    copy_verified_object(source, &temporary, expected_hash)?;
    match fs::rename(&temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination.exists() => {
            let _ = fs::remove_file(&temporary);
            validate_object(destination, expected_hash, fs::metadata(source)?.len()).context(error)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to install content object {}", destination.display())),
    }
}

fn copy_verified_object(source: &Path, destination: &Path, expected_hash: &str) -> Result<()> {
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

fn sha256_file(path: &Path) -> Result<String> {
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

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
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

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
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
    use serde_json::json;
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
        let manifest: LocalSnapshot = read_json(&snapshot.manifest_path).unwrap();
        let object = object_path(repository.path(), &manifest.threads[0].rollout.sha256).unwrap();
        fs::write(object, "corrupt").unwrap();

        let error = import_local_snapshot(
            &snapshot.manifest_path,
            target.path(),
            repository.path(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("length mismatch"));
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
        let mut manifest: LocalSnapshot = read_json(&snapshot.manifest_path).unwrap();
        manifest.threads[0].title = "blocked".to_string();
        manifest.threads[0]
            .related_records
            .tables
            .get_mut("threads")
            .unwrap()[0]["title"] = Value::String("blocked".to_string());
        atomic_write_json(&snapshot.manifest_path, &manifest).unwrap();

        let error = import_local_snapshot(
            &snapshot.manifest_path,
            target.path(),
            repository.path(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed to insert thread"));
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
        let manifest: LocalSnapshot = read_json(&snapshot.manifest_path).unwrap();
        let thread = &manifest.threads[0];
        let object = object_path(repository.path(), &thread.rollout.sha256).unwrap();
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
}
