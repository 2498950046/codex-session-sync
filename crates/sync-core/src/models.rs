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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRef {
    pub logical_id: Option<String>,
    pub source_path: Option<String>,
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
