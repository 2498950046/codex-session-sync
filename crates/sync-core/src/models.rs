use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const THREAD_BUNDLE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentObject {
    pub sha256: String,
    pub byte_length: u64,
    pub media_type: String,
    pub logical_path: Option<String>,
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRef {
    pub logical_id: Option<String>,
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathUsage {
    pub path: String,
    pub active_count: usize,
    pub archived_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RelatedRecords {
    #[serde(skip)]
    pub source_database: Option<PathBuf>,
    pub tables: BTreeMap<String, Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBundle {
    pub schema_version: u32,
    pub thread_id: String,
    pub title: String,
    pub archived: bool,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub model_provider: Option<String>,
    pub workspace: WorkspaceRef,
    pub rollout: ContentObject,
    pub related_records: RelatedRecords,
    pub attachments: Vec<ContentObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanWarningKind {
    EmptyRollout,
    InvalidUtf8,
    InvalidJson,
    MissingSessionMeta,
    MissingThreadId,
    DuplicateThread,
    DatabaseUnavailable,
    DatabaseSchemaUnsupported,
    RolloutMissing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScanWarning {
    pub kind: ScanWarningKind,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuarantinedRollout {
    pub original_path: PathBuf,
    pub quarantine_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub codex_home: PathBuf,
    pub database_paths: Vec<PathBuf>,
    pub active_count: usize,
    pub archived_count: usize,
    pub total_rollout_bytes: u64,
    pub threads: Vec<ThreadBundle>,
    pub warnings: Vec<ScanWarning>,
}

impl ScanReport {
    pub fn total_count(&self) -> usize {
        self.threads.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPreview {
    pub thread_id: String,
    pub title: String,
    pub archived: bool,
    pub model_provider: Option<String>,
    pub workspace: WorkspaceRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScanDashboardReport {
    pub codex_home: PathBuf,
    pub database_paths: Vec<PathBuf>,
    pub active_count: usize,
    pub archived_count: usize,
    pub total_rollout_bytes: u64,
    pub total_count: usize,
    pub threads: Vec<ThreadPreview>,
    pub warnings: Vec<ScanWarning>,
}

impl From<&ScanReport> for ScanDashboardReport {
    fn from(report: &ScanReport) -> Self {
        Self {
            codex_home: report.codex_home.clone(),
            database_paths: report.database_paths.clone(),
            active_count: report.active_count,
            archived_count: report.archived_count,
            total_rollout_bytes: report.total_rollout_bytes,
            total_count: report.total_count(),
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
            warnings: report.warnings.clone(),
        }
    }
}

pub const LOCAL_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const OPERATION_JOURNAL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalSnapshot {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub created_at: String,
    pub threads: Vec<ThreadBundle>,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub manifest_path: PathBuf,
    pub thread_count: usize,
    pub object_count: usize,
    pub total_bytes: u64,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotValidationReport {
    pub snapshot_id: String,
    pub manifest_path: PathBuf,
    pub thread_count: usize,
    pub object_count: usize,
    pub total_bytes: u64,
    pub valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Preparing,
    BackedUp,
    Applying,
    Validating,
    Completed,
    RolledBack,
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperationJournal {
    pub schema_version: u32,
    pub operation_id: String,
    pub snapshot_id: String,
    pub target_codex_home: PathBuf,
    pub backup_dir: PathBuf,
    pub status: OperationStatus,
    pub started_at: String,
    pub updated_at: String,
    pub planned_rollouts: Vec<JournalRollout>,
    pub imported_thread_ids: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JournalRollout {
    pub target_path: PathBuf,
    pub temporary_path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub operation_id: String,
    pub snapshot_id: String,
    pub imported_count: usize,
    pub skipped_count: usize,
    pub backup_dir: PathBuf,
    pub journal_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dashboard_projection_omits_large_sqlite_records() {
        let payload = "x".repeat(256 * 1024);
        let threads = (0..12)
            .map(|index| ThreadBundle {
                schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
                thread_id: format!("thread-{index}"),
                title: format!("Thread {index}"),
                archived: false,
                created_at_ms: None,
                updated_at_ms: None,
                model_provider: Some("openai".to_string()),
                workspace: WorkspaceRef::default(),
                rollout: ContentObject {
                    sha256:
                        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string(),
                    byte_length: 0,
                    media_type: "application/x-ndjson".to_string(),
                    logical_path: Some(format!("sessions/rollout-{index}.jsonl")),
                    source_path: None,
                },
                related_records: RelatedRecords {
                    source_database: None,
                    tables: BTreeMap::from([(
                        "threads".to_string(),
                        vec![json!({"large_sqlite_payload": payload})],
                    )]),
                },
                attachments: Vec::new(),
            })
            .collect();
        let report = ScanReport {
            codex_home: PathBuf::from("/tmp/.codex"),
            database_paths: Vec::new(),
            active_count: 12,
            archived_count: 0,
            total_rollout_bytes: 0,
            threads,
            warnings: Vec::new(),
        };

        let dashboard = ScanDashboardReport::from(&report);
        assert_eq!(dashboard.threads.len(), 8);
        assert!(serde_json::to_vec(&dashboard).unwrap().len() < 16 * 1024);
        assert!(serde_json::to_vec(&report).unwrap().len() > 3 * 1024 * 1024);
    }
}
