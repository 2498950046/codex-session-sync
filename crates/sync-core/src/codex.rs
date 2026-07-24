use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::models::{
    ContentObject, RelatedRecords, ScanReport, ScanWarning, ScanWarningKind,
    THREAD_BUNDLE_SCHEMA_VERSION, ThreadBundle, WorkspaceRef,
};

const SESSION_DIRS: [(&str, bool); 2] = [("sessions", false), ("archived_sessions", true)];

#[derive(Debug, Clone)]
struct RolloutRecord {
    thread_id: String,
    archived: bool,
    cwd: Option<String>,
    model_provider: Option<String>,
    rollout: ContentObject,
}

#[derive(Debug, Clone)]
struct DbThreadRecord {
    database: PathBuf,
    row: Value,
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
    let codex_home = codex_home.as_ref().to_path_buf();
    if !codex_home.exists() {
        anyhow::bail!("Codex home does not exist: {}", codex_home.display());
    }
    if !codex_home.is_dir() {
        anyhow::bail!("Codex home is not a directory: {}", codex_home.display());
    }

    let database_paths = discover_database_paths(&codex_home);
    let mut warnings = Vec::new();
    let db_records = load_database_records(&database_paths, &mut warnings);

    let mut rollouts = BTreeMap::<String, RolloutRecord>::new();
    for (directory, archived) in SESSION_DIRS {
        let root = codex_home.join(directory);
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(&root).follow_links(false) {
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

            let logical_path = path
                .strip_prefix(&codex_home)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let Some(record) = read_rollout_record(path, &logical_path, archived, &mut warnings)?
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

    for (thread_id, rollout) in rollouts {
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
        total_rollout_bytes += rollout.rollout.byte_length;

        let related_records = db.map_or_else(RelatedRecords::default, |record| RelatedRecords {
            source_database: Some(record.database.clone()),
            tables: BTreeMap::from([("threads".to_string(), vec![record.row.clone()])]),
        });

        threads.push(ThreadBundle {
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
            attachments: Vec::new(),
        });
    }

    Ok(ScanReport {
        codex_home,
        database_paths,
        active_count,
        archived_count,
        total_rollout_bytes,
        threads,
        warnings,
    })
}

fn read_rollout_record(
    path: &Path,
    logical_path: &str,
    archived: bool,
    warnings: &mut Vec<ScanWarning>,
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

    let mut file =
        File::open(path).with_context(|| format!("failed to open rollout {}", path.display()))?;
    let hash = sha256_reader(&mut file)
        .with_context(|| format!("failed to hash rollout {}", path.display()))?;

    let file =
        File::open(path).with_context(|| format!("failed to reopen rollout {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut first_line = Vec::new();
    reader
        .read_until(b'\n', &mut first_line)
        .with_context(|| format!("failed to read rollout {}", path.display()))?;

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
        rollout: ContentObject {
            sha256: hash,
            byte_length: metadata.len(),
            media_type: "application/x-ndjson".to_string(),
            logical_path: Some(logical_path.to_string()),
            source_path: Some(path.to_path_buf()),
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
                    row,
                },
            );
        }
    }
    records
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

fn sha256_reader(reader: &mut impl Read) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
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
}
