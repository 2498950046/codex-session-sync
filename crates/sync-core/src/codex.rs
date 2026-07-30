use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::models::{
    ContentObject, QuarantinedRollout, RelatedRecords, ScanDashboardReport, ScanReport,
    ScanWarning, ScanWarningKind, THREAD_BUNDLE_SCHEMA_VERSION, ThreadBundle, ThreadPreview,
    WorkspacePathUsage, WorkspaceRef,
};
use crate::operation::{OperationControl, OperationProgress};

const SESSION_DIRS: [(&str, bool); 2] = [("sessions", false), ("archived_sessions", true)];
const MAX_SESSION_META_BYTES: u64 = 1024 * 1024;

pub fn quarantine_empty_rollout(
    codex_home: impl AsRef<Path>,
    repository_root: impl AsRef<Path>,
    rollout_path: impl AsRef<Path>,
    confirmed_codex_closed: bool,
) -> Result<QuarantinedRollout> {
    if !confirmed_codex_closed {
        bail!("empty rollout cleanup requires confirmation that Codex is fully closed");
    }

    let codex_home = fs::canonicalize(codex_home.as_ref()).with_context(|| {
        format!(
            "failed to normalize Codex Home {}",
            codex_home.as_ref().display()
        )
    })?;
    let requested_path = rollout_path.as_ref();
    let link_metadata = fs::symlink_metadata(requested_path)
        .with_context(|| format!("failed to inspect rollout {}", requested_path.display()))?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        bail!("empty rollout cleanup accepts only regular non-symlink files");
    }
    let original_path = fs::canonicalize(requested_path)
        .with_context(|| format!("failed to normalize rollout {}", requested_path.display()))?;
    let allowed = SESSION_DIRS.iter().any(|(directory, _)| {
        fs::canonicalize(codex_home.join(directory))
            .map(|root| original_path.starts_with(root))
            .unwrap_or(false)
    });
    if !allowed {
        bail!(
            "rollout {} is outside the selected Codex Home session directories",
            original_path.display()
        );
    }

    let file_name = original_path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| value.starts_with("rollout-") && value.ends_with(".jsonl"))
        .context("empty rollout cleanup requires a rollout-*.jsonl file")?;
    let metadata = fs::metadata(&original_path)
        .with_context(|| format!("failed to inspect rollout {}", original_path.display()))?;
    if metadata.len() != 0 {
        bail!(
            "rollout {} is no longer empty; cleanup was refused",
            original_path.display()
        );
    }

    let quarantine_dir = repository_root
        .as_ref()
        .join("quarantine")
        .join("empty-rollouts");
    fs::create_dir_all(&quarantine_dir).with_context(|| {
        format!(
            "failed to create empty rollout quarantine {}",
            quarantine_dir.display()
        )
    })?;
    let quarantine_path = quarantine_dir.join(format!("{}-{file_name}", Uuid::now_v7()));

    match fs::rename(&original_path, &quarantine_path) {
        Ok(()) => {
            if fs::metadata(&quarantine_path)?.len() != 0 {
                fs::rename(&quarantine_path, &original_path).context(
                    "rollout changed while it was being quarantined; the original was restored",
                )?;
                bail!("rollout changed while it was being quarantined; cleanup was refused");
            }
        }
        Err(error) if error.kind() == ErrorKind::CrossesDevices => {
            let destination = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&quarantine_path)
                .with_context(|| {
                    format!(
                        "failed to create quarantined rollout {}",
                        quarantine_path.display()
                    )
                })?;
            destination.sync_all()?;
            drop(destination);
            if fs::metadata(&original_path)?.len() != 0 {
                let _ = fs::remove_file(&quarantine_path);
                bail!("rollout changed while it was being quarantined; cleanup was refused");
            }
            if let Err(error) = fs::remove_file(&original_path) {
                let _ = fs::remove_file(&quarantine_path);
                return Err(error).with_context(|| {
                    format!("failed to remove empty rollout {}", original_path.display())
                });
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to move empty rollout {} to {}",
                    original_path.display(),
                    quarantine_path.display()
                )
            });
        }
    }

    Ok(QuarantinedRollout {
        original_path,
        quarantine_path,
    })
}

#[derive(Debug, Clone)]
struct RolloutRecord {
    thread_id: String,
    archived: bool,
    cwd: Option<String>,
    model_provider: Option<String>,
    rollout: ScannedRollout,
}

#[derive(Debug, Clone)]
pub(crate) struct ScannedRollout {
    pub(crate) byte_length: u64,
    pub(crate) media_type: String,
    pub(crate) logical_path: Option<String>,
    pub(crate) source_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ScannedThread {
    pub(crate) schema_version: u32,
    pub(crate) thread_id: String,
    pub(crate) title: String,
    pub(crate) archived: bool,
    pub(crate) created_at_ms: Option<i64>,
    pub(crate) updated_at_ms: Option<i64>,
    pub(crate) model_provider: Option<String>,
    pub(crate) workspace: WorkspaceRef,
    pub(crate) rollout: ScannedRollout,
    pub(crate) related_records: RelatedRecords,
}

impl ScannedThread {
    pub(crate) fn into_bundle(self, sha256: String) -> ThreadBundle {
        ThreadBundle {
            schema_version: self.schema_version,
            thread_id: self.thread_id,
            title: self.title,
            archived: self.archived,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            model_provider: self.model_provider,
            workspace: self.workspace,
            rollout: ContentObject {
                sha256,
                byte_length: self.rollout.byte_length,
                media_type: self.rollout.media_type,
                logical_path: self.rollout.logical_path,
                source_path: Some(self.rollout.source_path),
                storage: None,
            },
            related_records: self.related_records,
            attachments: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct MetadataScanReport {
    pub(crate) codex_home: PathBuf,
    pub(crate) database_paths: Vec<PathBuf>,
    pub(crate) workspace_path_usage: Vec<WorkspacePathUsage>,
    pub(crate) active_count: usize,
    pub(crate) archived_count: usize,
    pub(crate) total_rollout_bytes: u64,
    pub(crate) threads: Vec<ScannedThread>,
    pub(crate) warnings: Vec<ScanWarning>,
}

#[derive(Debug, Clone)]
struct DbThreadRecord {
    database: PathBuf,
    tables: BTreeMap<String, Vec<Value>>,
    title: Option<String>,
    archived: Option<bool>,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
    model_provider: Option<String>,
    cwd: Option<String>,
}

pub fn default_codex_home() -> PathBuf {
    if let Some(value) = std::env::var_os("CODEX_HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() && !path.to_string_lossy().trim().is_empty() {
            return path;
        }
    }

    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

pub fn scan_codex_home(codex_home: impl AsRef<Path>) -> Result<ScanReport> {
    scan_codex_home_with_control(codex_home, &OperationControl::default())
}

pub fn scan_codex_home_dashboard(codex_home: impl AsRef<Path>) -> Result<ScanDashboardReport> {
    scan_codex_home_dashboard_with_control(codex_home, &OperationControl::default())
}

pub fn scan_codex_home_dashboard_with_control(
    codex_home: impl AsRef<Path>,
    control: &OperationControl,
) -> Result<ScanDashboardReport> {
    let report = scan_codex_home_metadata_with_control(codex_home, control)?;
    Ok(ScanDashboardReport {
        codex_home: report.codex_home,
        database_paths: report.database_paths,
        active_count: report.active_count,
        archived_count: report.archived_count,
        total_rollout_bytes: report.total_rollout_bytes,
        total_count: report.threads.len(),
        threads: report
            .threads
            .iter()
            .take(8)
            .map(|thread| ThreadPreview {
                thread_id: thread.thread_id.clone(),
                title: thread.title.clone(),
                archived: thread.archived,
                model_provider: thread.model_provider.clone(),
                workspace: thread.workspace.clone(),
            })
            .collect(),
        warnings: report.warnings,
    })
}

pub fn scan_codex_home_workspace_usage(
    codex_home: impl AsRef<Path>,
) -> Result<Vec<WorkspacePathUsage>> {
    let report = scan_codex_home_metadata_with_control(codex_home, &OperationControl::default())?;
    Ok(report.workspace_path_usage)
}

pub fn scan_codex_home_workspace_paths(codex_home: impl AsRef<Path>) -> Result<Vec<String>> {
    Ok(scan_codex_home_workspace_usage(codex_home)?
        .into_iter()
        .map(|usage| usage.path)
        .collect())
}

pub fn scan_codex_home_with_control(
    codex_home: impl AsRef<Path>,
    control: &OperationControl,
) -> Result<ScanReport> {
    let report = scan_codex_home_metadata_with_control(codex_home, control)?;
    let thread_count = report.threads.len() as u64;
    let mut threads = Vec::with_capacity(report.threads.len());
    for (index, thread) in report.threads.into_iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "hash_rollouts".to_string(),
            message: thread.title.clone(),
            completed: index as u64,
            total: Some(thread_count),
            unit: "files".to_string(),
            cancellable: true,
        });
        let mut file = File::open(&thread.rollout.source_path).with_context(|| {
            format!(
                "failed to open rollout {}",
                thread.rollout.source_path.display()
            )
        })?;
        let hash = sha256_reader(&mut file, control).with_context(|| {
            format!(
                "failed to hash rollout {}",
                thread.rollout.source_path.display()
            )
        })?;
        threads.push(thread.into_bundle(hash));
    }

    Ok(ScanReport {
        codex_home: report.codex_home,
        database_paths: report.database_paths,
        active_count: report.active_count,
        archived_count: report.archived_count,
        total_rollout_bytes: report.total_rollout_bytes,
        threads,
        warnings: report.warnings,
    })
}

pub(crate) fn scan_codex_home_metadata_with_control(
    codex_home: impl AsRef<Path>,
    control: &OperationControl,
) -> Result<MetadataScanReport> {
    let codex_home = codex_home.as_ref().to_path_buf();
    control.report(OperationProgress::indeterminate(
        "scan",
        "正在检查 Codex 会话目录",
    ));
    if !codex_home.exists() {
        anyhow::bail!("Codex home does not exist: {}", codex_home.display());
    }
    if !codex_home.is_dir() {
        anyhow::bail!("Codex home is not a directory: {}", codex_home.display());
    }

    control.check_cancelled()?;
    control.report(OperationProgress::indeterminate(
        "scan_databases",
        "正在读取 SQLite 会话索引",
    ));
    let database_paths = discover_database_paths(&codex_home);
    let mut warnings = Vec::new();
    let db_records = load_database_records(&database_paths, &mut warnings);
    let mut rollouts = BTreeMap::<String, RolloutRecord>::new();
    let mut rollout_count = 0_u64;
    for (directory, archived) in SESSION_DIRS {
        let root = codex_home.join(directory);
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(&root).follow_links(false) {
            control.check_cancelled()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(ScanWarning {
                        kind: ScanWarningKind::RolloutMissing,
                        path: root.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if !entry.file_type().is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
            {
                continue;
            }

            rollout_count += 1;
            control.report(OperationProgress {
                phase: "scan_rollouts".to_string(),
                message: path.to_string_lossy().into_owned(),
                completed: rollout_count,
                total: None,
                unit: "files".to_string(),
                cancellable: true,
            });

            let logical_path = path
                .strip_prefix(&codex_home)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let Some(record) =
                read_rollout_record(path, &logical_path, archived, &mut warnings, control)?
            else {
                continue;
            };

            if rollouts.contains_key(&record.thread_id) {
                warnings.push(ScanWarning {
                    kind: ScanWarningKind::DuplicateThread,
                    path: path.to_path_buf(),
                    message: format!(
                        "Thread {} was already discovered; keeping the first rollout",
                        record.thread_id
                    ),
                });
                continue;
            }
            rollouts.insert(record.thread_id.clone(), record);
        }
    }

    let mut threads = Vec::with_capacity(rollouts.len());
    let mut total_rollout_bytes = 0_u64;
    let mut active_count = 0_usize;
    let mut archived_count = 0_usize;
    let mut scanned_thread_ids = BTreeSet::new();
    let mut workspace_path_usage = BTreeMap::<String, WorkspacePathUsage>::new();

    for (thread_id, rollout) in rollouts {
        scanned_thread_ids.insert(thread_id.clone());
        let db = db_records.get(&thread_id);
        let title = db
            .and_then(|record| record.title.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| thread_id.clone());
        let archived = db
            .and_then(|record| record.archived)
            .unwrap_or(rollout.archived);
        let cwd = db
            .and_then(|record| record.cwd.clone())
            .or_else(|| rollout.cwd.clone());
        let model_provider = db
            .and_then(|record| record.model_provider.clone())
            .or_else(|| rollout.model_provider.clone());

        if archived {
            archived_count += 1;
        } else {
            active_count += 1;
        }
        record_workspace_path_usage(&mut workspace_path_usage, cwd.as_deref(), archived);
        total_rollout_bytes += rollout.rollout.byte_length;

        let related_records = db.map_or_else(RelatedRecords::default, |record| RelatedRecords {
            source_database: Some(record.database.clone()),
            tables: record.tables.clone(),
        });

        threads.push(ScannedThread {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id,
            title,
            archived,
            created_at_ms: db.and_then(|record| record.created_at_ms),
            updated_at_ms: db.and_then(|record| record.updated_at_ms),
            model_provider,
            workspace: WorkspaceRef {
                logical_id: None,
                source_path: cwd,
            },
            rollout: rollout.rollout,
            related_records,
        });
    }

    for (thread_id, record) in &db_records {
        if !scanned_thread_ids.contains(thread_id) {
            record_workspace_path_usage(
                &mut workspace_path_usage,
                record.cwd.as_deref(),
                record.archived.unwrap_or(false),
            );
        }
    }

    Ok(MetadataScanReport {
        codex_home,
        database_paths,
        workspace_path_usage: workspace_path_usage.into_values().collect(),
        active_count,
        archived_count,
        total_rollout_bytes,
        threads,
        warnings,
    })
}

fn record_workspace_path_usage(
    usage: &mut BTreeMap<String, WorkspacePathUsage>,
    path: Option<&str>,
    archived: bool,
) {
    let Some(path) = path.map(str::trim).filter(|path| !path.is_empty()) else {
        return;
    };
    let entry = usage
        .entry(crate::workspace::normalize_workspace_path_for_match(path))
        .or_insert_with(|| WorkspacePathUsage {
            path: path.to_string(),
            active_count: 0,
            archived_count: 0,
        });
    if archived {
        entry.archived_count += 1;
    } else {
        entry.active_count += 1;
    }
}

fn read_rollout_record(
    path: &Path,
    logical_path: &str,
    archived: bool,
    warnings: &mut Vec<ScanWarning>,
    control: &OperationControl,
) -> Result<Option<RolloutRecord>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat rollout {}", path.display()))?;
    if metadata.len() == 0 {
        warnings.push(ScanWarning {
            kind: ScanWarningKind::EmptyRollout,
            path: path.to_path_buf(),
            message: "Empty rollout file was skipped".to_string(),
        });
        return Ok(None);
    }

    control.check_cancelled()?;
    let file =
        File::open(path).with_context(|| format!("failed to open rollout {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut first_line = Vec::new();
    reader
        .take(MAX_SESSION_META_BYTES + 1)
        .read_until(b'\n', &mut first_line)
        .with_context(|| format!("failed to read rollout {}", path.display()))?;
    if first_line.len() as u64 > MAX_SESSION_META_BYTES {
        warnings.push(ScanWarning {
            kind: ScanWarningKind::InvalidJson,
            path: path.to_path_buf(),
            message: format!(
                "Rollout session metadata exceeds {} bytes",
                MAX_SESSION_META_BYTES
            ),
        });
        return Ok(None);
    }

    let first_line = match std::str::from_utf8(&first_line) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(ScanWarning {
                kind: ScanWarningKind::InvalidUtf8,
                path: path.to_path_buf(),
                message: format!("Invalid UTF-8 in rollout metadata: {error}"),
            });
            return Ok(None);
        }
    };

    if first_line.trim().is_empty() {
        warnings.push(ScanWarning {
            kind: ScanWarningKind::EmptyRollout,
            path: path.to_path_buf(),
            message: "Rollout has no non-empty metadata line".to_string(),
        });
        return Ok(None);
    }

    let first_line = first_line.trim_start_matches('\u{feff}');
    let value: Value = match serde_json::from_str(first_line) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(ScanWarning {
                kind: ScanWarningKind::InvalidJson,
                path: path.to_path_buf(),
                message: format!("Invalid rollout metadata JSON: {error}"),
            });
            return Ok(None);
        }
    };

    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        warnings.push(ScanWarning {
            kind: ScanWarningKind::MissingSessionMeta,
            path: path.to_path_buf(),
            message: "First rollout record is not session_meta".to_string(),
        });
        return Ok(None);
    }

    let payload = value
        .get("payload")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(thread_id) = payload.get("id").and_then(Value::as_str) else {
        warnings.push(ScanWarning {
            kind: ScanWarningKind::MissingThreadId,
            path: path.to_path_buf(),
            message: "session_meta has no string payload.id".to_string(),
        });
        return Ok(None);
    };

    Ok(Some(RolloutRecord {
        thread_id: thread_id.to_string(),
        archived,
        cwd: string_field(&Value::Object(payload.clone()), "cwd"),
        model_provider: string_field(&Value::Object(payload), "model_provider"),
        rollout: ScannedRollout {
            byte_length: metadata.len(),
            media_type: "application/x-ndjson".to_string(),
            logical_path: Some(logical_path.to_string()),
            source_path: path.to_path_buf(),
        },
    }))
}

pub(crate) fn discover_database_paths(codex_home: &Path) -> Vec<PathBuf> {
    let sqlite_home = std::env::var_os("CODEX_SQLITE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| codex_home.to_path_buf());

    let mut paths = Vec::new();
    let modern_dir = sqlite_home.join("sqlite");
    if let Ok(entries) = fs::read_dir(modern_dir) {
        let mut candidates = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && matches!(
                        path.extension().and_then(|value| value.to_str()),
                        Some("db" | "sqlite" | "sqlite3")
                    )
            })
            .collect::<Vec<_>>();
        candidates.sort();
        for path in candidates {
            if sqlite_has_threads_table(&path) {
                paths.push(path);
            }
        }
    }

    let legacy = sqlite_home.join("state_5.sqlite");
    if legacy.is_file() && sqlite_has_threads_table(&legacy) && !paths.contains(&legacy) {
        paths.push(legacy);
    }
    paths
}

fn sqlite_has_threads_table(path: &Path) -> bool {
    let Ok(connection) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'threads' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .is_ok()
}

fn load_database_records(
    database_paths: &[PathBuf],
    warnings: &mut Vec<ScanWarning>,
) -> HashMap<String, DbThreadRecord> {
    let mut records = HashMap::new();
    for database in database_paths {
        let connection =
            match Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY) {
                Ok(connection) => connection,
                Err(error) => {
                    warnings.push(ScanWarning {
                        kind: ScanWarningKind::DatabaseUnavailable,
                        path: database.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };

        let related = load_direct_thread_records(&connection, database, warnings);
        let mut statement = match connection.prepare("SELECT * FROM threads") {
            Ok(statement) => statement,
            Err(error) => {
                warnings.push(ScanWarning {
                    kind: ScanWarningKind::DatabaseSchemaUnsupported,
                    path: database.clone(),
                    message: format!("Cannot read threads table: {error}"),
                });
                continue;
            }
        };
        let column_names = statement
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !column_names.iter().any(|column| column == "id") {
            warnings.push(ScanWarning {
                kind: ScanWarningKind::DatabaseSchemaUnsupported,
                path: database.clone(),
                message: "threads table has no id column".to_string(),
            });
            continue;
        }

        let rows = match statement.query_map([], |row| row_to_json(row, &column_names)) {
            Ok(rows) => rows,
            Err(error) => {
                warnings.push(ScanWarning {
                    kind: ScanWarningKind::DatabaseUnavailable,
                    path: database.clone(),
                    message: format!("Cannot enumerate threads: {error}"),
                });
                continue;
            }
        };

        for row in rows {
            let row = match row {
                Ok(row) => row,
                Err(error) => {
                    warnings.push(ScanWarning {
                        kind: ScanWarningKind::DatabaseUnavailable,
                        path: database.clone(),
                        message: format!("Cannot read thread row: {error}"),
                    });
                    continue;
                }
            };
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            if records.contains_key(id) {
                continue;
            }
            records.insert(
                id.to_string(),
                DbThreadRecord {
                    database: database.clone(),
                    title: string_field(&row, "title"),
                    archived: bool_field(&row, "archived"),
                    created_at_ms: integer_field(&row, "created_at_ms"),
                    updated_at_ms: integer_field(&row, "updated_at_ms"),
                    model_provider: string_field(&row, "model_provider"),
                    cwd: string_field(&row, "cwd"),
                    tables: {
                        let mut tables = related.get(id).cloned().unwrap_or_default();
                        tables.insert("threads".to_string(), vec![row]);
                        tables
                    },
                },
            );
        }
    }
    records
}

fn load_direct_thread_records(
    connection: &Connection,
    database: &Path,
    warnings: &mut Vec<ScanWarning>,
) -> HashMap<String, BTreeMap<String, Vec<Value>>> {
    let mut records = HashMap::<String, BTreeMap<String, Vec<Value>>>::new();
    let mut tables = match connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != 'threads'
         ORDER BY name",
    ) {
        Ok(statement) => statement,
        Err(error) => {
            warnings.push(ScanWarning {
                kind: ScanWarningKind::DatabaseSchemaUnsupported,
                path: database.to_path_buf(),
                message: format!("Cannot enumerate related tables: {error}"),
            });
            return records;
        }
    };
    let table_names = match tables.query_map([], |row| row.get::<_, String>(0)) {
        Ok(rows) => rows.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(error) => {
            warnings.push(ScanWarning {
                kind: ScanWarningKind::DatabaseSchemaUnsupported,
                path: database.to_path_buf(),
                message: format!("Cannot enumerate related tables: {error}"),
            });
            return records;
        }
    };

    for table in table_names {
        let pragma = format!("PRAGMA foreign_key_list({})", quote_identifier(&table));
        let mut foreign_keys = match connection.prepare(&pragma) {
            Ok(statement) => statement,
            Err(_) => continue,
        };
        let references = match foreign_keys.query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        }) {
            Ok(rows) => rows.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(_) => continue,
        };
        let Some((_, thread_column, _)) = references.into_iter().find(|(target, _, column)| {
            target == "threads" && column.as_deref().is_none_or(|column| column == "id")
        }) else {
            continue;
        };
        let sql = format!("SELECT * FROM {}", quote_identifier(&table));
        let mut statement = match connection.prepare(&sql) {
            Ok(statement) => statement,
            Err(error) => {
                warnings.push(ScanWarning {
                    kind: ScanWarningKind::DatabaseUnavailable,
                    path: database.to_path_buf(),
                    message: format!("Cannot read related table {table}: {error}"),
                });
                continue;
            }
        };
        let columns = statement
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let Some(thread_index) = columns.iter().position(|column| column == &thread_column) else {
            continue;
        };
        let rows = match statement.query_map([], |row| {
            let thread_id = match row.get_ref(thread_index)? {
                ValueRef::Text(value) => Some(String::from_utf8_lossy(value).into_owned()),
                _ => None,
            };
            Ok((thread_id, row_to_json(row, &columns)?))
        }) {
            Ok(rows) => rows,
            Err(error) => {
                warnings.push(ScanWarning {
                    kind: ScanWarningKind::DatabaseUnavailable,
                    path: database.to_path_buf(),
                    message: format!("Cannot enumerate related table {table}: {error}"),
                });
                continue;
            }
        };
        for row in rows.flatten() {
            if let Some(thread_id) = row.0 {
                records
                    .entry(thread_id)
                    .or_default()
                    .entry(table.clone())
                    .or_default()
                    .push(row.1);
            }
        }
    }
    records
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn row_to_json(row: &Row<'_>, columns: &[String]) -> rusqlite::Result<Value> {
    let mut object = Map::new();
    for (index, column) in columns.iter().enumerate() {
        object.insert(column.clone(), sqlite_value_to_json(row.get_ref(index)?));
    }
    Ok(Value::Object(object))
}

fn sqlite_value_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => json!({
            "$type": "blob",
            "base64": base64::engine::general_purpose::STANDARD.encode(value),
        }),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn integer_field(value: &Value, key: &str) -> Option<i64> {
    value
        .get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(|value| {
        value
            .as_bool()
            .or_else(|| value.as_i64().map(|value| value != 0))
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn sha256_reader(reader: &mut impl Read, control: &OperationControl) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        control.check_cancelled()?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

    fn write_rollout(root: &Path, name: &str, payload: Value, body: &str) -> PathBuf {
        let path = root.join(name);
        let metadata = json!({
            "type": "session_meta",
            "timestamp": "2026-07-23T00:00:00Z",
            "payload": payload,
        });
        fs::write(&path, format!("{}\n{}", metadata, body)).unwrap();
        path
    }

    #[test]
    fn scans_valid_rollout_and_sqlite_metadata() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let id = "thread-1";
        write_rollout(
            &sessions,
            "rollout-thread-1.jsonl",
            json!({"id": id, "cwd": "/tmp/demo", "model_provider": "openai"}),
            "{\"type\":\"event\"}",
        );
        let db = temp.path().join("state_5.sqlite");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, archived INTEGER, cwd TEXT, model_provider TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5)",
                (id, "Demo title", 0_i64, "/tmp/db-demo", "provider-a"),
            )
            .unwrap();
        drop(connection);

        let report = scan_codex_home(temp.path()).unwrap();
        assert_eq!(report.total_count(), 1);
        assert_eq!(report.threads[0].title, "Demo title");
        assert_eq!(
            report.threads[0].model_provider.as_deref(),
            Some("provider-a")
        );
        assert_eq!(
            report.threads[0].workspace.source_path.as_deref(),
            Some("/tmp/db-demo")
        );
        assert!(report.warnings.is_empty());
        let usage = scan_codex_home_workspace_usage(temp.path()).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].path, "/tmp/db-demo");
        assert_eq!(usage[0].active_count, 1);
        assert_eq!(usage[0].archived_count, 0);
    }

    #[test]
    fn workspace_path_scan_counts_active_and_archived_database_rows_without_rollouts() {
        let temp = tempdir().unwrap();
        let database = temp.path().join("state_5.sqlite");
        let connection = Connection::open(database).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, archived INTEGER)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, cwd, archived) VALUES
                    ('active-1', 'F:/history/referenced', 0),
                    ('active-2', 'F:/history/referenced', 0),
                    ('archived-1', 'F:/history/referenced', 1)",
                [],
            )
            .unwrap();
        drop(connection);

        let usage = scan_codex_home_workspace_usage(temp.path()).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].path, "F:/history/referenced");
        assert_eq!(usage[0].active_count, 2);
        assert_eq!(usage[0].archived_count, 1);
        assert_eq!(
            scan_codex_home_workspace_paths(temp.path()).unwrap(),
            vec!["F:/history/referenced".to_string()]
        );
    }

    #[test]
    fn dashboard_scan_reads_metadata_without_hashing_rollouts() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_rollout(
            &sessions,
            "rollout-thread-1.jsonl",
            json!({"id": "thread-1"}),
            &"x".repeat(1024 * 1024),
        );
        let phases = Arc::new(Mutex::new(Vec::new()));
        let reporter_phases = phases.clone();
        let control = OperationControl::new(Arc::new(AtomicBool::new(false)), move |progress| {
            reporter_phases.lock().unwrap().push(progress.phase);
        });

        let dashboard = scan_codex_home_dashboard_with_control(temp.path(), &control).unwrap();

        assert_eq!(dashboard.total_count, 1);
        assert!(
            !phases
                .lock()
                .unwrap()
                .iter()
                .any(|phase| phase == "hash_rollouts")
        );
    }

    #[test]
    fn empty_rollout_is_a_warning_not_a_scan_failure() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        File::create(sessions.join("rollout-empty.jsonl")).unwrap();

        let report = scan_codex_home(temp.path()).unwrap();
        assert_eq!(report.total_count(), 0);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].kind, ScanWarningKind::EmptyRollout);
    }

    #[test]
    fn quarantines_only_an_empty_rollout_inside_the_selected_codex_home() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        let sessions = codex_home.join("sessions/2026/07/27");
        let repository = temp.path().join("repository");
        fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join("rollout-empty.jsonl");
        File::create(&rollout).unwrap();
        let expected_original = fs::canonicalize(&rollout).unwrap();

        let report = quarantine_empty_rollout(&codex_home, &repository, &rollout, true).unwrap();

        assert_eq!(report.original_path, expected_original);
        assert!(!rollout.exists());
        assert!(report.quarantine_path.is_file());
        assert_eq!(fs::metadata(&report.quarantine_path).unwrap().len(), 0);
        assert!(scan_codex_home(&codex_home).unwrap().warnings.is_empty());
    }

    #[test]
    fn empty_rollout_quarantine_rejects_nonempty_and_outside_files() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        let sessions = codex_home.join("sessions");
        let repository = temp.path().join("repository");
        fs::create_dir_all(&sessions).unwrap();
        let nonempty = sessions.join("rollout-nonempty.jsonl");
        fs::write(&nonempty, "not empty").unwrap();
        let outside = temp.path().join("rollout-outside.jsonl");
        File::create(&outside).unwrap();

        let nonempty_error =
            quarantine_empty_rollout(&codex_home, &repository, &nonempty, true).unwrap_err();
        let outside_error =
            quarantine_empty_rollout(&codex_home, &repository, &outside, true).unwrap_err();

        assert!(nonempty_error.to_string().contains("no longer empty"));
        assert!(outside_error.to_string().contains("outside"));
        assert!(nonempty.is_file());
        assert!(outside.is_file());
    }

    #[test]
    fn empty_rollout_quarantine_requires_closed_codex_confirmation() {
        let temp = tempdir().unwrap();
        let error =
            quarantine_empty_rollout(temp.path(), temp.path(), temp.path(), false).unwrap_err();
        assert!(error.to_string().contains("confirmation"));
    }

    #[test]
    fn malformed_rollout_is_skipped_with_warning() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("rollout-invalid.jsonl"), "not-json\n").unwrap();

        let report = scan_codex_home(temp.path()).unwrap();
        assert_eq!(report.total_count(), 0);
        assert_eq!(report.warnings[0].kind, ScanWarningKind::InvalidJson);
    }

    #[test]
    fn oversized_metadata_line_is_bounded_and_skipped() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-oversized.jsonl"),
            vec![b'x'; MAX_SESSION_META_BYTES as usize + 2],
        )
        .unwrap();

        let dashboard = scan_codex_home_dashboard(temp.path()).unwrap();

        assert_eq!(dashboard.total_count, 0);
        assert_eq!(dashboard.warnings[0].kind, ScanWarningKind::InvalidJson);
        assert!(dashboard.warnings[0].message.contains("exceeds"));
    }

    #[test]
    fn invalid_utf8_rollout_is_skipped_with_warning() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-invalid-utf8.jsonl"),
            [0xff_u8, 0xfe, b'\n'],
        )
        .unwrap();

        let report = scan_codex_home(temp.path()).unwrap();
        assert_eq!(report.total_count(), 0);
        assert_eq!(report.warnings[0].kind, ScanWarningKind::InvalidUtf8);
    }

    #[test]
    fn cancelled_scan_stops_before_reading_session_files() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_rollout(
            &sessions,
            "rollout-thread-1.jsonl",
            json!({"id": "thread-1"}),
            "{\"type\":\"event\"}",
        );
        let control = OperationControl::default();
        control.cancel_handle().store(true, Ordering::Relaxed);
        let error = scan_codex_home_with_control(temp.path(), &control).unwrap_err();
        assert!(error.to_string().contains("operation cancelled"));
    }
}
