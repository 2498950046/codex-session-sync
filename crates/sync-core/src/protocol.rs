use std::collections::BTreeSet;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::models::{ContentObject, THREAD_BUNDLE_SCHEMA_VERSION, ThreadBundle};

pub const REMOTE_PROTOCOL_VERSION: u32 = 1;
pub const REVISION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    pub id: Uuid,
    pub display_name: String,
    pub head: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceListResponse {
    pub namespaces: Vec<Namespace>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateNamespaceRequest {
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RenameNamespaceRequest {
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceHeadResponse {
    pub namespace_id: Uuid,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectDescriptor {
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MissingObjectsRequest {
    pub objects: Vec<ObjectDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MissingObjectsResponse {
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RevisionPayload {
    pub schema_version: u32,
    pub namespace_id: Uuid,
    pub parent_revision: Option<String>,
    pub created_at: String,
    pub threads: Vec<ThreadBundle>,
    pub warning_count: usize,
}

impl RevisionPayload {
    pub fn normalized(&self) -> Result<Self, RevisionValidationError> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized
            .threads
            .sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        Ok(normalized)
    }

    pub fn validate(&self) -> Result<(), RevisionValidationError> {
        if self.schema_version != REVISION_SCHEMA_VERSION {
            return Err(RevisionValidationError::UnsupportedRevisionSchema {
                actual: self.schema_version,
            });
        }
        if self.namespace_id.get_version_num() != 7 {
            return Err(RevisionValidationError::InvalidNamespaceId(
                self.namespace_id.to_string(),
            ));
        }
        if let Some(parent) = &self.parent_revision {
            validate_sha256(parent)
                .map_err(|_| RevisionValidationError::InvalidParentRevision(parent.clone()))?;
        }
        DateTime::parse_from_rfc3339(&self.created_at)
            .map_err(|_| RevisionValidationError::InvalidCreatedAt(self.created_at.clone()))?;

        let mut thread_ids = BTreeSet::new();
        for thread in &self.threads {
            validate_thread(thread)?;
            if !thread_ids.insert(thread.thread_id.as_str()) {
                return Err(RevisionValidationError::DuplicateThreadId(
                    thread.thread_id.clone(),
                ));
            }
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, RevisionValidationError> {
        canonical_revision_json(self)
    }

    pub fn revision_id(&self) -> Result<String, RevisionValidationError> {
        revision_id(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RevisionManifest {
    pub revision_id: String,
    pub payload: RevisionPayload,
}

impl RevisionManifest {
    pub fn from_payload(payload: RevisionPayload) -> Result<Self, RevisionValidationError> {
        let revision_id = payload.revision_id()?;
        Ok(Self {
            revision_id,
            payload,
        })
    }

    pub fn validate(&self) -> Result<(), RevisionValidationError> {
        validate_sha256(&self.revision_id)
            .map_err(|_| RevisionValidationError::InvalidRevisionId(self.revision_id.clone()))?;
        let actual = self.payload.revision_id()?;
        if actual != self.revision_id {
            return Err(RevisionValidationError::RevisionIdMismatch {
                expected: self.revision_id.clone(),
                actual,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommitRevisionRequest {
    pub expected_head: Option<String>,
    pub revision: RevisionManifest,
}

impl CommitRevisionRequest {
    pub fn validate(&self) -> Result<(), RevisionValidationError> {
        if let Some(expected_head) = &self.expected_head {
            validate_sha256(expected_head)
                .map_err(|_| RevisionValidationError::InvalidExpectedHead(expected_head.clone()))?;
        }
        self.revision.validate()?;
        if self.expected_head != self.revision.payload.parent_revision {
            return Err(RevisionValidationError::ExpectedHeadParentMismatch {
                expected_head: self.expected_head.clone(),
                parent_revision: self.revision.payload.parent_revision.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitRevisionResponse {
    pub namespace_id: Uuid,
    pub head: String,
    pub created: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    Unauthorized,
    InvalidRequest,
    InvalidDigest,
    HashMismatch,
    LengthMismatch,
    ObjectTooLarge,
    ObjectNotFound,
    MissingObjects,
    NamespaceNotFound,
    RevisionNotFound,
    HeadMismatch,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_head: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_objects: Vec<String>,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum RevisionValidationError {
    #[error("unsupported revision schema version {actual}")]
    UnsupportedRevisionSchema { actual: u32 },
    #[error("namespace ID must be a UUIDv7: {0}")]
    InvalidNamespaceId(String),
    #[error("parent revision must be a canonical SHA-256 identifier: {0}")]
    InvalidParentRevision(String),
    #[error("expected head must be a canonical SHA-256 identifier: {0}")]
    InvalidExpectedHead(String),
    #[error("revision ID must be a canonical SHA-256 identifier: {0}")]
    InvalidRevisionId(String),
    #[error("revision timestamp must be RFC 3339: {0}")]
    InvalidCreatedAt(String),
    #[error("thread ID must not be empty")]
    EmptyThreadId,
    #[error("duplicate thread ID: {0}")]
    DuplicateThreadId(String),
    #[error("thread {thread_id} uses unsupported schema version {actual}")]
    UnsupportedThreadSchema { thread_id: String, actual: u32 },
    #[error("thread {thread_id} has an invalid {object_kind} hash: {sha256}")]
    InvalidObjectHash {
        thread_id: String,
        object_kind: String,
        sha256: String,
    },
    #[error("thread {thread_id} has an empty media type for {object_kind}")]
    EmptyObjectMediaType {
        thread_id: String,
        object_kind: String,
    },
    #[error("revision ID mismatch: expected {expected}, calculated {actual}")]
    RevisionIdMismatch { expected: String, actual: String },
    #[error("expected head {expected_head:?} must equal revision parent {parent_revision:?}")]
    ExpectedHeadParentMismatch {
        expected_head: Option<String>,
        parent_revision: Option<String>,
    },
    #[error("failed to serialize canonical revision JSON: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("SHA-256 identifier must use sha256: followed by 64 lowercase hexadecimal digits")]
pub struct InvalidSha256;

pub fn validate_sha256(value: &str) -> Result<(), InvalidSha256> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(InvalidSha256);
    };
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(InvalidSha256)
    }
}

pub fn canonical_revision_json(
    payload: &RevisionPayload,
) -> Result<Vec<u8>, RevisionValidationError> {
    let normalized = payload.normalized()?;
    let value = serde_json::to_value(normalized)
        .map_err(|error| RevisionValidationError::Serialization(error.to_string()))?;
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;
    Ok(canonical.into_bytes())
}

pub fn revision_id(payload: &RevisionPayload) -> Result<String, RevisionValidationError> {
    let canonical = canonical_revision_json(payload)?;
    let digest = Sha256::digest(canonical);
    Ok(format!("sha256:{}", hex::encode(digest)))
}

fn validate_thread(thread: &ThreadBundle) -> Result<(), RevisionValidationError> {
    if thread.thread_id.trim().is_empty() {
        return Err(RevisionValidationError::EmptyThreadId);
    }
    if thread.schema_version != THREAD_BUNDLE_SCHEMA_VERSION {
        return Err(RevisionValidationError::UnsupportedThreadSchema {
            thread_id: thread.thread_id.clone(),
            actual: thread.schema_version,
        });
    }
    validate_content_object(thread, "rollout", &thread.rollout)?;
    for (index, attachment) in thread.attachments.iter().enumerate() {
        validate_content_object(thread, &format!("attachment[{index}]"), attachment)?;
    }
    Ok(())
}

fn validate_content_object(
    thread: &ThreadBundle,
    object_kind: &str,
    object: &ContentObject,
) -> Result<(), RevisionValidationError> {
    validate_sha256(&object.sha256).map_err(|_| RevisionValidationError::InvalidObjectHash {
        thread_id: thread.thread_id.clone(),
        object_kind: object_kind.to_string(),
        sha256: object.sha256.clone(),
    })?;
    if object.media_type.trim().is_empty() {
        return Err(RevisionValidationError::EmptyObjectMediaType {
            thread_id: thread.thread_id.clone(),
            object_kind: object_kind.to_string(),
        });
    }
    Ok(())
}

fn write_canonical_json(value: &Value, output: &mut String) -> Result<(), RevisionValidationError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(
            &serde_json::to_string(value)
                .map_err(|error| RevisionValidationError::Serialization(error.to_string()))?,
        ),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).map_err(|error| {
                        RevisionValidationError::Serialization(error.to_string())
                    })?,
                );
                output.push(':');
                write_canonical_json(&values[key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::models::{RelatedRecords, WorkspaceRef};

    fn namespace_id() -> Uuid {
        Uuid::parse_str("01890f3a-6b4c-7cc2-98c8-0123456789ab").unwrap()
    }

    fn thread(thread_id: &str, object_byte: char) -> ThreadBundle {
        ThreadBundle {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: thread_id.to_string(),
            title: format!("Thread {thread_id}"),
            archived: false,
            created_at_ms: Some(1_700_000_000_000),
            updated_at_ms: Some(1_700_000_100_000),
            model_provider: Some("openai".to_string()),
            workspace: WorkspaceRef {
                logical_id: Some("workspace-1".to_string()),
                source_path: Some("C:/work/demo".to_string()),
            },
            rollout: ContentObject {
                sha256: format!("sha256:{}", object_byte.to_string().repeat(64)),
                byte_length: 42,
                media_type: "application/x-ndjson".to_string(),
                logical_path: Some(format!("sessions/rollout-{thread_id}.jsonl")),
                source_path: None,
            },
            related_records: RelatedRecords {
                source_database: None,
                tables: BTreeMap::from([(
                    "threads".to_string(),
                    vec![json!({"z": 2, "a": {"second": 2, "first": 1}})],
                )]),
            },
            attachments: Vec::new(),
        }
    }

    fn payload(threads: Vec<ThreadBundle>) -> RevisionPayload {
        RevisionPayload {
            schema_version: REVISION_SCHEMA_VERSION,
            namespace_id: namespace_id(),
            parent_revision: Some(format!("sha256:{}", "c".repeat(64))),
            created_at: "2026-07-26T10:30:00Z".to_string(),
            threads,
            warning_count: 1,
        }
    }

    #[test]
    fn revision_hash_is_stable() {
        let payload = payload(vec![thread("thread-b", 'b'), thread("thread-a", 'a')]);

        assert_eq!(
            payload.revision_id().unwrap(),
            payload.revision_id().unwrap()
        );
        assert_eq!(
            payload.revision_id().unwrap(),
            "sha256:70e9a48dfa2db54a6770321190818b19f657df3a00ded0d676d860150c4bece0"
        );
    }

    #[test]
    fn api_models_use_confirmed_json_field_names() {
        let namespace = serde_json::to_value(Namespace {
            id: namespace_id(),
            display_name: "Laptop A".to_string(),
            head: None,
            created_at: "2026-07-26T10:30:00Z".to_string(),
            updated_at: "2026-07-26T10:30:00Z".to_string(),
        })
        .unwrap();
        assert_eq!(namespace["displayName"], "Laptop A");

        let create = serde_json::to_value(CreateNamespaceRequest {
            display_name: "Laptop A".to_string(),
        })
        .unwrap();
        assert_eq!(create, json!({"displayName": "Laptop A"}));

        let rename = serde_json::to_value(RenameNamespaceRequest {
            display_name: "Laptop B".to_string(),
        })
        .unwrap();
        assert_eq!(rename, json!({"displayName": "Laptop B"}));

        let missing = serde_json::to_value(MissingObjectsRequest {
            objects: vec![ObjectDescriptor {
                sha256: format!("sha256:{}", "a".repeat(64)),
                byte_length: 42,
            }],
        })
        .unwrap();
        assert_eq!(
            missing,
            json!({
                "objects": [{
                    "sha256": format!("sha256:{}", "a".repeat(64)),
                    "byteLength": 42
                }]
            })
        );
    }

    #[test]
    fn changing_a_hashed_field_changes_revision_id() {
        let original = payload(vec![thread("thread-a", 'a')]);
        let mut changed = original.clone();
        changed.threads[0].title = "Changed title".to_string();

        assert_ne!(
            original.revision_id().unwrap(),
            changed.revision_id().unwrap()
        );

        let mut changed_warning_count = original.clone();
        changed_warning_count.warning_count += 1;
        assert_ne!(
            original.revision_id().unwrap(),
            changed_warning_count.revision_id().unwrap()
        );
    }

    #[test]
    fn thread_input_order_does_not_change_revision_id() {
        let first = payload(vec![thread("thread-b", 'b'), thread("thread-a", 'a')]);
        let second = payload(vec![thread("thread-a", 'a'), thread("thread-b", 'b')]);

        assert_eq!(first.revision_id().unwrap(), second.revision_id().unwrap());
        let canonical: Value = serde_json::from_slice(&first.canonical_json().unwrap()).unwrap();
        assert_eq!(canonical["threads"][0]["threadId"], "thread-a");
        assert_eq!(canonical["threads"][1]["threadId"], "thread-b");
    }

    #[test]
    fn rejects_invalid_namespace_and_parent() {
        let mut invalid_namespace = payload(Vec::new());
        invalid_namespace.namespace_id = Uuid::nil();
        assert!(matches!(
            invalid_namespace.validate(),
            Err(RevisionValidationError::InvalidNamespaceId(_))
        ));

        let mut invalid_parent = payload(Vec::new());
        invalid_parent.parent_revision = Some("sha256:not-a-digest".to_string());
        assert!(matches!(
            invalid_parent.validate(),
            Err(RevisionValidationError::InvalidParentRevision(_))
        ));
    }

    #[test]
    fn rejects_duplicate_threads_and_expected_head_parent_mismatch() {
        let duplicate = payload(vec![thread("same", 'a'), thread("same", 'b')]);
        assert_eq!(
            duplicate.validate(),
            Err(RevisionValidationError::DuplicateThreadId(
                "same".to_string()
            ))
        );

        let revision = RevisionManifest::from_payload(payload(Vec::new())).unwrap();
        let request = CommitRevisionRequest {
            expected_head: None,
            revision,
        };
        assert!(matches!(
            request.validate(),
            Err(RevisionValidationError::ExpectedHeadParentMismatch { .. })
        ));
    }

    #[test]
    fn manifest_validation_detects_a_changed_payload() {
        let mut manifest =
            RevisionManifest::from_payload(payload(vec![thread("one", 'a')])).unwrap();
        manifest.payload.threads[0].archived = true;

        assert!(matches!(
            manifest.validate(),
            Err(RevisionValidationError::RevisionIdMismatch { .. })
        ));
    }
}
