use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::checkout::{discover_local_thread_catalog_databases, invalidate_local_thread_catalogs};
use crate::local::{atomic_write_json, invalidate_source_object_index};
use crate::provider::{detect_configured_provider, transform_rollout_file, validate_provider_id};
use crate::{
    ObjectDescriptor, OperationControl, OperationProgress, ScanWarning,
    scan_codex_home_with_control,
};

pub const PROVIDER_SYNC_JOURNAL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSyncStatus {
    Preparing,
    ApplyingFiles,
    ApplyingDatabases,
    Validating,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncFilePlan {
    pub thread_id: String,
    pub target: PathBuf,
    pub staged: PathBuf,
    pub displaced: PathBuf,
    pub original: ObjectDescriptor,
    pub replacement: ObjectDescriptor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncJournal {
    pub schema_version: u32,
    pub operation_id: String,
    pub target_codex_home: PathBuf,
    pub repository_root: PathBuf,
    pub target_provider: String,
    pub status: ProviderSyncStatus,
    pub started_at: String,
    pub updated_at: String,
    pub files: Vec<ProviderSyncFilePlan>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncPreview {
    pub provider: String,
    pub rollout_count: usize,
    pub rollout_bytes: u64,
    pub database_row_count: usize,
    pub catalog_database_count: usize,
    pub warnings: Vec<ScanWarning>,
    pub no_changes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncReport {
    pub operation_id: String,
    pub provider: String,
    pub rollout_count: usize,
    pub database_row_count: usize,
    pub journal_path: PathBuf,
}

pub fn preview_provider_sync(
    codex_home: impl AsRef<Path>,
    control: &OperationControl,
) -> Result<ProviderSyncPreview> {
    let codex_home = codex_home.as_ref();
    let provider = detect_configured_provider(codex_home)?;
    let report = scan_codex_home_with_control(codex_home, control)?;
    let mut paths = BTreeSet::new();
    let mut rollout_bytes = 0_u64;
    for thread in &report.threads {
        let path =
            thread.rollout.source_path.as_ref().with_context(|| {
                format!("thread {} has no rollout source path", thread.thread_id)
            })?;
        if read_rollout_provider(path)?.as_deref() == Some(provider.as_str()) {
            continue;
        }
        if paths.insert(path.clone()) {
            rollout_bytes = rollout_bytes.saturating_add(thread.rollout.byte_length);
        }
    }
    let database_row_count = count_database_rows(&report.database_paths, &provider)?;
    let catalog_database_count = discover_local_thread_catalog_databases(codex_home)?.len();
    Ok(ProviderSyncPreview {
        provider,
        rollout_count: paths.len(),
        rollout_bytes,
        database_row_count,
        catalog_database_count,
        warnings: report.warnings,
        no_changes: paths.is_empty() && database_row_count == 0,
    })
}

pub fn synchronize_local_provider(
    codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    confirmed_codex_closed: bool,
    control: &OperationControl,
) -> Result<ProviderSyncReport> {
    if !confirmed_codex_closed {
        bail!("provider synchronization requires confirmation that Codex is fully closed");
    }
    let codex_home = codex_home.as_ref().to_path_buf();
    let repository_root = repository_root.as_ref().to_path_buf();
    let provider = detect_configured_provider(&codex_home)?;
    validate_provider_id(&provider)?;
    let scan = scan_codex_home_with_control(&codex_home, control)?;
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for thread in &scan.threads {
        let path =
            thread.rollout.source_path.clone().with_context(|| {
                format!("thread {} has no rollout source path", thread.thread_id)
            })?;
        if read_rollout_provider(&path)?.as_deref() == Some(provider.as_str()) {
            continue;
        }
        if seen.insert(path.clone()) {
            targets.push((thread.thread_id.clone(), path));
        }
    }
    let planned_database_row_count = count_database_rows(&scan.database_paths, &provider)?;

    let operation_id = Uuid::now_v7().to_string();
    let staging_dir = codex_home
        .join(".codex-session-sync")
        .join("provider-sync")
        .join(&operation_id);
    let journal_path = repository_root
        .join("journal")
        .join(format!("provider-sync-{operation_id}.json"));
    fs::create_dir_all(&staging_dir)?;
    fs::create_dir_all(repository_root.join("journal"))?;
    let now = Utc::now().to_rfc3339();
    let mut journal = ProviderSyncJournal {
        schema_version: PROVIDER_SYNC_JOURNAL_SCHEMA_VERSION,
        operation_id: operation_id.clone(),
        target_codex_home: codex_home.clone(),
        repository_root: repository_root.clone(),
        target_provider: provider.clone(),
        status: ProviderSyncStatus::Preparing,
        started_at: now.clone(),
        updated_at: now,
        files: Vec::new(),
        error: None,
    };
    write_journal(&journal_path, &journal)?;

    let result = (|| -> Result<(usize, usize)> {
        if targets.is_empty() && planned_database_row_count == 0 {
            control.report(OperationProgress {
                phase: "provider_sync_no_changes".to_string(),
                message: format!("all sessions already use provider {provider}"),
                completed: 1,
                total: Some(1),
                unit: "checks".to_string(),
                cancellable: true,
            });
            return Ok((0, 0));
        }
        for (index, (thread_id, target)) in targets.iter().enumerate() {
            control.check_cancelled()?;
            control.report(OperationProgress {
                phase: "provider_sync_prepare".to_string(),
                message: thread_id.clone(),
                completed: index as u64,
                total: Some(targets.len() as u64),
                unit: "rollouts".to_string(),
                cancellable: true,
            });
            let staged = staging_dir.join(format!("{index}.jsonl"));
            let replacement =
                stage_provider_rollout(target, &staged, thread_id, &provider, control)?;
            let original = hash_file(target, control)?;
            let displaced = target.with_extension(format!("provider-sync-{operation_id}.old"));
            journal.files.push(ProviderSyncFilePlan {
                thread_id: thread_id.clone(),
                target: target.clone(),
                staged,
                displaced,
                original,
                replacement,
            });
            journal.updated_at = Utc::now().to_rfc3339();
            write_journal(&journal_path, &journal)?;
        }

        control.check_cancelled()?;

        let write_control = control;
        journal.status = ProviderSyncStatus::ApplyingFiles;
        journal.updated_at = Utc::now().to_rfc3339();
        write_journal(&journal_path, &journal)?;
        for (index, file) in journal.files.iter().enumerate() {
            write_control.check_cancelled()?;
            write_control.report(OperationProgress {
                phase: "provider_sync_files".to_string(),
                message: file.thread_id.clone(),
                completed: index as u64,
                total: Some(journal.files.len() as u64),
                unit: "rollouts".to_string(),
                cancellable: true,
            });
            replace_with_staged(file)?;
        }

        journal.status = ProviderSyncStatus::ApplyingDatabases;
        journal.updated_at = Utc::now().to_rfc3339();
        write_journal(&journal_path, &journal)?;
        write_control.check_cancelled()?;
        let database_rows = update_database_providers(&scan.database_paths, &provider)?;
        let catalogs = discover_local_thread_catalog_databases(&codex_home)?;
        invalidate_local_thread_catalogs(&catalogs)?;
        invalidate_source_object_index(&repository_root)?;

        journal.status = ProviderSyncStatus::Validating;
        journal.updated_at = Utc::now().to_rfc3339();
        write_journal(&journal_path, &journal)?;
        validate_provider_sync(&codex_home, &provider, write_control)?;
        Ok((journal.files.len(), database_rows))
    })();

    let (rollout_count, database_row_count) = match result {
        Ok(result) => result,
        Err(error) => {
            journal.status = ProviderSyncStatus::Failed;
            journal.updated_at = Utc::now().to_rfc3339();
            journal.error = Some(error.to_string());
            write_journal(&journal_path, &journal)?;
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(error);
        }
    };
    journal.status = ProviderSyncStatus::Completed;
    journal.updated_at = Utc::now().to_rfc3339();
    write_journal(&journal_path, &journal)?;
    for file in &journal.files {
        let _ = fs::remove_file(&file.displaced);
        let _ = fs::remove_file(&file.staged);
    }
    let _ = fs::remove_dir_all(&staging_dir);
    Ok(ProviderSyncReport {
        operation_id,
        provider,
        rollout_count,
        database_row_count,
        journal_path,
    })
}

fn count_database_rows(databases: &[PathBuf], provider: &str) -> Result<usize> {
    let mut count = 0_usize;
    for database in databases {
        let connection = Connection::open(database)?;
        for table in provider_tables(&connection)? {
            let table = quote_identifier(&table);
            let rows: i64 = connection.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE model_provider IS NULL OR model_provider != ?1"
                ),
                [provider],
                |row| row.get(0),
            )?;
            count = count.saturating_add(usize::try_from(rows.max(0)).unwrap_or(usize::MAX));
        }
    }
    Ok(count)
}

fn read_rollout_provider(path: &Path) -> Result<Option<String>> {
    use std::io::BufRead;

    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    let value: serde_json::Value = serde_json::from_slice(&line).with_context(|| {
        format!(
            "failed to parse rollout session metadata {}",
            path.display()
        )
    })?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        bail!(
            "first rollout record is not session_meta: {}",
            path.display()
        );
    }
    Ok(value
        .get("payload")
        .and_then(|payload| payload.get("model_provider"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}

fn update_database_providers(databases: &[PathBuf], provider: &str) -> Result<usize> {
    let mut updated = 0_usize;
    for database in databases {
        let mut connection = Connection::open(database)?;
        connection.busy_timeout(std::time::Duration::from_millis(250))?;
        let tables = provider_tables(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for table in tables {
            updated = updated.saturating_add(transaction.execute(
                &format!(
                    "UPDATE {} SET model_provider = ?1 WHERE model_provider IS NULL OR model_provider != ?1",
                    quote_identifier(&table)
                ),
                [provider],
            )?);
        }
        transaction.commit()?;
    }
    Ok(updated)
}

fn provider_tables(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut provider_tables = Vec::new();
    for table in tables {
        let mut columns =
            connection.prepare(&format!("PRAGMA table_info({})", quote_identifier(&table)))?;
        if columns
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "model_provider")
        {
            provider_tables.push(table);
        }
    }
    Ok(provider_tables)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn stage_provider_rollout(
    source: &Path,
    destination: &Path,
    thread_id: &str,
    provider: &str,
    control: &OperationControl,
) -> Result<ObjectDescriptor> {
    transform_rollout_file(source, destination, thread_id, provider, control)
}

fn replace_with_staged(file: &ProviderSyncFilePlan) -> Result<()> {
    if file.displaced.exists() {
        bail!(
            "provider sync displaced path already exists: {}",
            file.displaced.display()
        );
    }
    fs::rename(&file.target, &file.displaced)?;
    if let Err(error) = fs::rename(&file.staged, &file.target) {
        let _ = fs::rename(&file.displaced, &file.target);
        return Err(error.into());
    }
    fs::remove_file(&file.displaced)?;
    Ok(())
}

fn validate_provider_sync(
    codex_home: &Path,
    provider: &str,
    control: &OperationControl,
) -> Result<()> {
    let report = scan_codex_home_with_control(codex_home, control)?;
    let mismatched = report
        .threads
        .iter()
        .filter(|thread| thread.model_provider.as_deref() != Some(provider))
        .count();
    if mismatched > 0 {
        bail!("provider synchronization validation found {mismatched} mismatched threads");
    }
    Ok(())
}

fn hash_file(path: &Path, control: &OperationControl) -> Result<ObjectDescriptor> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        control.check_cancelled()?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        length = length.saturating_add(count as u64);
    }
    Ok(ObjectDescriptor {
        sha256: format!("sha256:{}", hex::encode(hasher.finalize())),
        byte_length: length,
    })
}

fn write_journal(path: &Path, journal: &ProviderSyncJournal) -> Result<()> {
    atomic_write_json(path, journal)
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn provider_sync_updates_rollout_and_database_without_creating_a_backup() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let repository = temp.path().join("repository");
        let thread_id = Uuid::now_v7().to_string();
        let rollout = home
            .join("sessions")
            .join("2026")
            .join("08")
            .join("03")
            .join(format!("rollout-{thread_id}.jsonl"));
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join("sqlite")).unwrap();
        fs::write(home.join("config.toml"), "model_provider = \"custom\"\n").unwrap();
        let bytes = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":thread_id,"cwd":"C:/work","model_provider":"openai"}}),
            json!({"type":"message","payload":{"text":"unchanged"}})
        );
        fs::write(&rollout, bytes).unwrap();
        let database = home.join("sqlite").join("codex.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    archived INTEGER NOT NULL,
                    rollout_path TEXT,
                    cwd TEXT,
                    model_provider TEXT NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, title, archived, rollout_path, cwd, model_provider)
                 VALUES (?1, 'Thread', 0, ?2, 'C:/work', 'openai')",
                params![thread_id, rollout.to_string_lossy()],
            )
            .unwrap();
        drop(connection);

        let preview = preview_provider_sync(&home, &OperationControl::default()).unwrap();
        assert_eq!(preview.provider, "custom");
        assert_eq!(preview.rollout_count, 1);
        assert_eq!(preview.database_row_count, 1);
        assert!(!preview.no_changes);

        let result =
            synchronize_local_provider(&home, &repository, true, &OperationControl::default())
                .unwrap();
        assert_eq!(result.rollout_count, 1);
        assert!(!repository.join("backups/provider-sync").exists());
        assert_eq!(
            read_rollout_provider(&rollout).unwrap().as_deref(),
            Some("custom")
        );
        let connection = Connection::open(database).unwrap();
        let provider: String = connection
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        assert!(
            preview_provider_sync(&home, &OperationControl::default())
                .unwrap()
                .no_changes
        );
        let unchanged =
            synchronize_local_provider(&home, &repository, true, &OperationControl::default())
                .unwrap();
        assert_eq!(unchanged.rollout_count, 0);
        assert_eq!(unchanged.database_row_count, 0);
        assert!(!repository.join("backups/provider-sync").exists());
    }

    #[test]
    fn database_failure_keeps_completed_rollouts_for_the_next_run() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let repository = temp.path().join("repository");
        let thread_id = Uuid::now_v7().to_string();
        let rollout = home
            .join("sessions")
            .join(format!("rollout-{thread_id}.jsonl"));
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join("sqlite")).unwrap();
        fs::write(home.join("config.toml"), "model_provider = \"custom\"\n").unwrap();
        let original = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":thread_id,"cwd":"C:/work","model_provider":"openai"}}),
            json!({"type":"message","payload":{"text":"must survive rollback"}})
        );
        fs::write(&rollout, &original).unwrap();
        let database = home.join("sqlite/codex.db");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, archived INTEGER NOT NULL, rollout_path TEXT, cwd TEXT, model_provider TEXT NOT NULL CHECK(model_provider = 'openai'));").unwrap();
        connection.execute("INSERT INTO threads (id, title, archived, rollout_path, cwd, model_provider) VALUES (?1, 'Thread', 0, ?2, 'C:/work', 'openai')", params![thread_id, rollout.to_string_lossy()]).unwrap();
        drop(connection);

        let error =
            synchronize_local_provider(&home, &repository, true, &OperationControl::default())
                .unwrap_err();
        assert!(error.to_string().contains("CHECK constraint failed"));
        assert_ne!(fs::read_to_string(&rollout).unwrap(), original);
        assert_eq!(
            read_rollout_provider(&rollout).unwrap().as_deref(),
            Some("custom")
        );
        let provider: String = Connection::open(database)
            .unwrap()
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        let journal_path = fs::read_dir(repository.join("journal"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let journal: ProviderSyncJournal =
            serde_json::from_reader(File::open(journal_path).unwrap()).unwrap();
        assert_eq!(journal.status, ProviderSyncStatus::Failed);
        assert!(!repository.join("backups/provider-sync").exists());
    }
}
