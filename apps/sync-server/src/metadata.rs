use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sync_core::{
    Namespace, ObjectDescriptor, RevisionManifest, RevisionValidationError, StorageObjectRef,
    validate_sha256,
};
use thiserror::Error;
use uuid::Uuid;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const METADATA_SCHEMA_VERSION: i64 = 2;
const MAX_NAMESPACE_NAME_CHARS: usize = 128;
pub const MAX_REVISION_OBJECTS: usize = 10_000;
pub const MAX_REVISION_OBJECT_REFERENCES: usize = 20_000;

#[derive(Debug, Clone)]
pub struct MetadataStore {
    db_path: Arc<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRevisionMetadata {
    id: String,
    namespace_id: Uuid,
    parent_revision: Option<String>,
    created_at: String,
    manifest_byte_length: u64,
    thread_count: u64,
    objects: Vec<ObjectDescriptor>,
}

impl NewRevisionMetadata {
    pub fn from_manifest(manifest: &RevisionManifest) -> Result<Self, MetadataError> {
        let mut object_lengths = BTreeMap::new();
        for (reference_index, object) in manifest
            .payload
            .threads
            .iter()
            .flat_map(|thread| std::iter::once(&thread.rollout).chain(&thread.attachments))
            .enumerate()
        {
            if reference_index >= MAX_REVISION_OBJECT_REFERENCES {
                return Err(MetadataError::TooManyObjectReferences {
                    max: MAX_REVISION_OBJECT_REFERENCES,
                });
            }
            match object_lengths.get(&object.sha256) {
                Some(existing) if *existing != object.byte_length => {
                    return Err(conflict(&manifest.revision_id));
                }
                Some(_) => continue,
                None if object_lengths.len() >= MAX_REVISION_OBJECTS => {
                    return Err(MetadataError::TooManyObjects {
                        max: MAX_REVISION_OBJECTS,
                    });
                }
                None => {
                    object_lengths.insert(object.sha256.clone(), object.byte_length);
                }
            }
        }
        manifest.validate()?;
        let canonical = manifest.payload.canonical_json()?;
        let thread_count = u64::try_from(manifest.payload.threads.len())
            .map_err(|_| conflict(&manifest.revision_id))?;
        let objects = object_lengths
            .into_iter()
            .map(|(sha256, byte_length)| ObjectDescriptor {
                sha256,
                byte_length,
            })
            .collect();

        Ok(Self {
            id: manifest.revision_id.clone(),
            namespace_id: manifest.payload.namespace_id,
            parent_revision: manifest.payload.parent_revision.clone(),
            created_at: manifest.payload.created_at.clone(),
            manifest_byte_length: canonical.len() as u64,
            thread_count,
            objects,
        })
    }

    pub fn objects(&self) -> &[ObjectDescriptor] {
        &self.objects
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionMetadata {
    pub id: String,
    pub namespace_id: Uuid,
    pub parent_revision: Option<String>,
    pub created_at: String,
    pub manifest_byte_length: u64,
    pub thread_count: u64,
    pub object_count: u64,
    pub total_bytes: u64,
    pub objects: Vec<ObjectDescriptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitRevisionOutcome {
    Created,
    AlreadyCommitted,
}

impl CommitRevisionOutcome {
    pub fn created(self) -> bool {
        matches!(self, Self::Created)
    }
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: String },
    #[error("namespace head mismatch; current head is {current:?}")]
    HeadMismatch { current: Option<String> },
    #[error("revision conflicts with immutable metadata: {revision_id}")]
    RevisionConflict { revision_id: String },
    #[error("revision references more than {max} unique objects")]
    TooManyObjects { max: usize },
    #[error("revision contains more than {max} object references")]
    TooManyObjectReferences { max: usize },
    #[error("namespace display name must contain 1 to 128 non-control characters")]
    InvalidName,
    #[error("unsupported metadata schema version {actual}")]
    UnsupportedSchema { actual: i64 },
    #[error(transparent)]
    RevisionValidation(#[from] RevisionValidationError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("metadata worker task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl MetadataStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: Arc::new(db_path.into()),
        }
    }

    pub fn db_path(&self) -> &Path {
        self.db_path.as_ref().as_path()
    }

    pub async fn initialize(&self) -> Result<(), MetadataError> {
        self.with_connection(|connection| {
            let schema_version: i64 =
                connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if schema_version > METADATA_SCHEMA_VERSION {
                return Err(MetadataError::UnsupportedSchema {
                    actual: schema_version,
                });
            }
            let journal_mode: String =
                connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
            debug_assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
            connection.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS namespaces (
                     id TEXT PRIMARY KEY NOT NULL,
                     display_name TEXT NOT NULL,
                     head_revision TEXT,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     FOREIGN KEY (head_revision) REFERENCES revisions(id)
                 );
                 CREATE TABLE IF NOT EXISTS revisions (
                     id TEXT PRIMARY KEY NOT NULL,
                     namespace_id TEXT NOT NULL,
                     parent_revision TEXT,
                     created_at TEXT NOT NULL,
                     manifest_byte_length INTEGER NOT NULL CHECK (manifest_byte_length >= 0),
                     thread_count INTEGER NOT NULL CHECK (thread_count >= 0),
                     object_count INTEGER NOT NULL CHECK (object_count >= 0),
                     total_bytes INTEGER NOT NULL CHECK (total_bytes >= 0),
                     FOREIGN KEY (namespace_id) REFERENCES namespaces(id),
                     FOREIGN KEY (parent_revision) REFERENCES revisions(id)
                 );
                 CREATE TABLE IF NOT EXISTS revision_objects (
                     revision_id TEXT NOT NULL,
                     object_sha256 TEXT NOT NULL,
                     byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
                     PRIMARY KEY (revision_id, object_sha256),
                     FOREIGN KEY (revision_id) REFERENCES revisions(id) ON DELETE CASCADE
                 );
                 CREATE TABLE IF NOT EXISTS storage_objects (
                     kind TEXT NOT NULL,
                     sha256 TEXT NOT NULL,
                     byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
                     created_at TEXT NOT NULL,
                     PRIMARY KEY (kind, sha256)
                 );
                 CREATE TABLE IF NOT EXISTS object_edges (
                     owner_kind TEXT NOT NULL,
                     owner_sha256 TEXT NOT NULL,
                     target_kind TEXT NOT NULL,
                     target_sha256 TEXT NOT NULL,
                     PRIMARY KEY (owner_kind, owner_sha256, target_kind, target_sha256)
                 );
                 CREATE TABLE IF NOT EXISTS revision_roots (
                     revision_id TEXT PRIMARY KEY,
                     root_sha256 TEXT NOT NULL,
                     root_schema_version INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS revisions_namespace_id_idx
                     ON revisions(namespace_id, created_at, id);
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
            Ok(())
        })
        .await
    }

    pub async fn create_namespace(
        &self,
        display_name: impl Into<String>,
    ) -> Result<Namespace, MetadataError> {
        let display_name = normalize_name(display_name.into())?;
        self.with_connection(move |connection| {
            let id = Uuid::now_v7();
            let now = now_rfc3339();
            connection.execute(
                "INSERT INTO namespaces (id, display_name, head_revision, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?3)",
                params![id.to_string(), display_name, now],
            )?;
            read_namespace(connection, id)
        })
        .await
    }

    pub async fn list_namespaces(&self) -> Result<Vec<Namespace>, MetadataError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, display_name, head_revision, created_at, updated_at
                 FROM namespaces ORDER BY created_at, id",
            )?;
            let rows = statement.query_map([], namespace_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await
    }

    pub async fn rename_namespace(
        &self,
        namespace_id: Uuid,
        display_name: impl Into<String>,
    ) -> Result<Namespace, MetadataError> {
        let display_name = normalize_name(display_name.into())?;
        self.with_connection(move |connection| {
            let changed = connection.execute(
                "UPDATE namespaces SET display_name = ?1, updated_at = ?2 WHERE id = ?3",
                params![display_name, now_rfc3339(), namespace_id.to_string()],
            )?;
            if changed == 0 {
                return Err(not_found("namespace", namespace_id));
            }
            read_namespace(connection, namespace_id)
        })
        .await
    }

    pub async fn get_head(&self, namespace_id: Uuid) -> Result<Option<String>, MetadataError> {
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT head_revision FROM namespaces WHERE id = ?1",
                    [namespace_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| not_found("namespace", namespace_id))
        })
        .await
    }

    pub async fn get_revision_metadata(
        &self,
        revision_id: impl Into<String>,
    ) -> Result<RevisionMetadata, MetadataError> {
        let revision_id = revision_id.into();
        self.with_connection(move |connection| {
            read_revision(connection, &revision_id)?.ok_or_else(|| MetadataError::NotFound {
                kind: "revision",
                id: revision_id,
            })
        })
        .await
    }

    pub async fn record_storage_object(
        &self,
        object: StorageObjectRef,
    ) -> Result<(), MetadataError> {
        self.with_connection(move |connection| {
            let byte_length =
                i64::try_from(object.byte_length).map_err(|_| conflict(&object.sha256))?;
            connection.execute(
                "INSERT INTO storage_objects (kind, sha256, byte_length, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(kind, sha256) DO UPDATE SET byte_length = excluded.byte_length
                 WHERE storage_objects.byte_length = excluded.byte_length",
                params![
                    object.kind.wire_name(),
                    object.sha256,
                    byte_length,
                    now_rfc3339()
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn replace_object_edges(
        &self,
        owner: StorageObjectRef,
        targets: Vec<StorageObjectRef>,
    ) -> Result<(), MetadataError> {
        self.with_connection(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "DELETE FROM object_edges WHERE owner_kind = ?1 AND owner_sha256 = ?2",
                params![owner.kind.wire_name(), owner.sha256],
            )?;
            for target in targets {
                transaction.execute(
                    "INSERT INTO object_edges (owner_kind, owner_sha256, target_kind, target_sha256)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![owner.kind.wire_name(), owner.sha256, target.kind.wire_name(), target.sha256],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn commit_revision(
        &self,
        expected_head: Option<String>,
        revision: NewRevisionMetadata,
    ) -> Result<CommitRevisionOutcome, MetadataError> {
        self.with_connection(move |connection| {
            let revision = normalize_revision(revision)?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let namespace_id = revision.namespace_id.to_string();
            let current_head: Option<String> = transaction
                .query_row(
                    "SELECT head_revision FROM namespaces WHERE id = ?1",
                    [&namespace_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| MetadataError::NotFound {
                    kind: "namespace",
                    id: namespace_id.clone(),
                })?;

            if let Some(existing) = read_revision(&transaction, &revision.id)? {
                if existing != revision {
                    return Err(conflict(&revision.id));
                }
                if current_head == Some(revision.id.clone()) {
                    transaction.commit()?;
                    return Ok(CommitRevisionOutcome::AlreadyCommitted);
                }
                if current_head != expected_head {
                    return Err(MetadataError::HeadMismatch {
                        current: current_head,
                    });
                }
                return Err(conflict(&revision.id));
            }

            if current_head != expected_head {
                return Err(MetadataError::HeadMismatch {
                    current: current_head,
                });
            }
            if revision.parent_revision != expected_head {
                return Err(conflict(&revision.id));
            }
            if let Some(parent_revision) = &revision.parent_revision {
                let parent_namespace: Option<String> = transaction
                    .query_row(
                        "SELECT namespace_id FROM revisions WHERE id = ?1",
                        [parent_revision],
                        |row| row.get(0),
                    )
                    .optional()?;
                if parent_namespace.as_deref() != Some(namespace_id.as_str()) {
                    return Err(conflict(&revision.id));
                }
            }

            transaction.execute(
                "INSERT INTO revisions (
                     id, namespace_id, parent_revision, created_at,
                     manifest_byte_length, thread_count, object_count, total_bytes
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    revision.id,
                    namespace_id,
                    revision.parent_revision,
                    revision.created_at,
                    sql_integer(revision.manifest_byte_length, &revision.id)?,
                    sql_integer(revision.thread_count, &revision.id)?,
                    sql_integer(revision.object_count, &revision.id)?,
                    sql_integer(revision.total_bytes, &revision.id)?,
                ],
            )?;
            for object in &revision.objects {
                transaction.execute(
                    "INSERT INTO revision_objects (revision_id, object_sha256, byte_length)
                     VALUES (?1, ?2, ?3)",
                    params![
                        revision.id,
                        object.sha256,
                        sql_integer(object.byte_length, &revision.id)?,
                    ],
                )?;
            }

            let changed = match &expected_head {
                Some(expected) => transaction.execute(
                    "UPDATE namespaces SET head_revision = ?1, updated_at = ?2
                     WHERE id = ?3 AND head_revision = ?4",
                    params![revision.id, now_rfc3339(), namespace_id, expected],
                )?,
                None => transaction.execute(
                    "UPDATE namespaces SET head_revision = ?1, updated_at = ?2
                     WHERE id = ?3 AND head_revision IS NULL",
                    params![revision.id, now_rfc3339(), namespace_id],
                )?,
            };
            if changed != 1 {
                let current = transaction.query_row(
                    "SELECT head_revision FROM namespaces WHERE id = ?1",
                    [&namespace_id],
                    |row| row.get(0),
                )?;
                return Err(MetadataError::HeadMismatch { current });
            }
            transaction.commit()?;
            Ok(CommitRevisionOutcome::Created)
        })
        .await
    }

    async fn with_connection<T, F>(&self, operation: F) -> Result<T, MetadataError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, MetadataError> + Send + 'static,
    {
        let db_path = self.db_path.as_ref().clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_connection(&db_path)?;
            operation(&mut connection)
        })
        .await?
    }
}

fn open_connection(db_path: &Path) -> Result<Connection, MetadataError> {
    let connection = Connection::open(db_path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(connection)
}

fn normalize_name(display_name: String) -> Result<String, MetadataError> {
    let display_name = display_name.trim();
    if display_name.is_empty()
        || display_name.chars().count() > MAX_NAMESPACE_NAME_CHARS
        || display_name.chars().any(char::is_control)
    {
        return Err(MetadataError::InvalidName);
    }
    Ok(display_name.to_string())
}

fn normalize_revision(revision: NewRevisionMetadata) -> Result<RevisionMetadata, MetadataError> {
    validate_sha256(&revision.id).map_err(|_| conflict(&revision.id))?;
    if revision.namespace_id.get_version_num() != 7 {
        return Err(conflict(&revision.id));
    }
    if let Some(parent_revision) = &revision.parent_revision {
        validate_sha256(parent_revision).map_err(|_| conflict(&revision.id))?;
    }
    DateTime::parse_from_rfc3339(&revision.created_at).map_err(|_| conflict(&revision.id))?;

    let mut objects = BTreeMap::new();
    for object in revision.objects {
        validate_sha256(&object.sha256).map_err(|_| conflict(&revision.id))?;
        match objects.insert(object.sha256.clone(), object.byte_length) {
            Some(existing) if existing != object.byte_length => return Err(conflict(&revision.id)),
            _ => {}
        }
    }
    let total_bytes = objects.values().try_fold(0_u64, |total, length| {
        total
            .checked_add(*length)
            .ok_or_else(|| conflict(&revision.id))
    })?;
    let objects = objects
        .into_iter()
        .map(|(sha256, byte_length)| ObjectDescriptor {
            sha256,
            byte_length,
        })
        .collect::<Vec<_>>();
    Ok(RevisionMetadata {
        id: revision.id,
        namespace_id: revision.namespace_id,
        parent_revision: revision.parent_revision,
        created_at: revision.created_at,
        manifest_byte_length: revision.manifest_byte_length,
        thread_count: revision.thread_count,
        object_count: objects.len() as u64,
        total_bytes,
        objects,
    })
}

fn read_namespace(connection: &Connection, namespace_id: Uuid) -> Result<Namespace, MetadataError> {
    connection
        .query_row(
            "SELECT id, display_name, head_revision, created_at, updated_at
             FROM namespaces WHERE id = ?1",
            [namespace_id.to_string()],
            namespace_from_row,
        )
        .optional()?
        .ok_or_else(|| not_found("namespace", namespace_id))
}

fn namespace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Namespace> {
    let id: String = row.get(0)?;
    Ok(Namespace {
        id: parse_uuid(id, 0)?,
        display_name: row.get(1)?,
        head: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn read_revision(
    connection: &Connection,
    revision_id: &str,
) -> Result<Option<RevisionMetadata>, MetadataError> {
    let row = connection
        .query_row(
            "SELECT id, namespace_id, parent_revision, created_at,
                    manifest_byte_length, thread_count, object_count, total_bytes
             FROM revisions WHERE id = ?1",
            [revision_id],
            |row| {
                let namespace_id: String = row.get(1)?;
                Ok((
                    row.get::<_, String>(0)?,
                    parse_uuid(namespace_id, 1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? as u64,
                    row.get::<_, i64>(5)? as u64,
                    row.get::<_, i64>(6)? as u64,
                    row.get::<_, i64>(7)? as u64,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        namespace_id,
        parent_revision,
        created_at,
        manifest_byte_length,
        thread_count,
        object_count,
        total_bytes,
    )) = row
    else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT object_sha256, byte_length FROM revision_objects
         WHERE revision_id = ?1 ORDER BY object_sha256",
    )?;
    let objects = statement
        .query_map([revision_id], |row| {
            Ok(ObjectDescriptor {
                sha256: row.get(0)?,
                byte_length: row.get::<_, i64>(1)? as u64,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(RevisionMetadata {
        id,
        namespace_id,
        parent_revision,
        created_at,
        manifest_byte_length,
        thread_count,
        object_count,
        total_bytes,
        objects,
    }))
}

fn parse_uuid(value: String, column: usize) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn sql_integer(value: u64, revision_id: &str) -> Result<i64, MetadataError> {
    i64::try_from(value).map_err(|_| conflict(revision_id))
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn not_found(kind: &'static str, id: impl ToString) -> MetadataError {
    MetadataError::NotFound {
        kind,
        id: id.to_string(),
    }
}

fn conflict(revision_id: &str) -> MetadataError {
    MetadataError::RevisionConflict {
        revision_id: revision_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sync_core::{
        ContentObject, REVISION_SCHEMA_VERSION, RelatedRecords, RevisionManifest, RevisionPayload,
        THREAD_BUNDLE_SCHEMA_VERSION, ThreadBundle, WorkspaceRef,
    };
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    use super::*;

    async fn store() -> (TempDir, MetadataStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = MetadataStore::new(directory.path().join("metadata.sqlite"));
        store.initialize().await.unwrap();
        (directory, store)
    }

    fn digest(value: char) -> String {
        format!("sha256:{}", value.to_string().repeat(64))
    }

    fn revision(
        namespace_id: Uuid,
        id: char,
        parent_revision: Option<String>,
    ) -> NewRevisionMetadata {
        NewRevisionMetadata::from_manifest(&revision_manifest(namespace_id, id, parent_revision))
            .unwrap()
    }

    fn revision_manifest(
        namespace_id: Uuid,
        id: char,
        parent_revision: Option<String>,
    ) -> RevisionManifest {
        RevisionManifest::from_payload(RevisionPayload {
            schema_version: REVISION_SCHEMA_VERSION,
            namespace_id,
            parent_revision,
            created_at: "2026-07-26T10:30:00Z".to_string(),
            threads: vec![ThreadBundle {
                schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
                thread_id: format!("thread-{id}"),
                title: format!("Thread {id}"),
                archived: false,
                created_at_ms: None,
                updated_at_ms: None,
                model_provider: Some("openai".to_string()),
                workspace: WorkspaceRef::default(),
                rollout: ContentObject {
                    sha256: digest(id),
                    byte_length: 10,
                    media_type: "application/x-ndjson".to_string(),
                    logical_path: None,
                    source_path: None,
                    storage: None,
                },
                related_records: RelatedRecords::default(),
                attachments: Vec::new(),
            }],
            warning_count: 0,
        })
        .unwrap()
    }

    #[test]
    fn revision_metadata_deduplicates_objects_and_rejects_conflicting_lengths() {
        let namespace_id = Uuid::now_v7();
        let mut manifest = revision_manifest(namespace_id, 'a', None);
        let duplicate = manifest.payload.threads[0].rollout.clone();
        manifest.payload.threads[0]
            .attachments
            .push(duplicate.clone());
        let manifest = RevisionManifest::from_payload(manifest.payload).unwrap();
        let metadata = NewRevisionMetadata::from_manifest(&manifest).unwrap();
        assert_eq!(metadata.objects.len(), 1);

        let mut conflicting_payload = manifest.payload;
        conflicting_payload.threads[0].attachments[0].byte_length += 1;
        let conflicting = RevisionManifest::from_payload(conflicting_payload).unwrap();
        assert!(matches!(
            NewRevisionMetadata::from_manifest(&conflicting),
            Err(MetadataError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn revision_metadata_limits_unique_objects() {
        let mut manifest = revision_manifest(Uuid::now_v7(), 'a', None);
        manifest.payload.threads[0].attachments = (0..MAX_REVISION_OBJECTS)
            .map(|index| ContentObject {
                sha256: format!("sha256:{index:064x}"),
                byte_length: index as u64,
                media_type: "application/octet-stream".to_string(),
                logical_path: None,
                source_path: None,
                storage: None,
            })
            .collect();
        let manifest = RevisionManifest::from_payload(manifest.payload).unwrap();
        assert!(matches!(
            NewRevisionMetadata::from_manifest(&manifest),
            Err(MetadataError::TooManyObjects {
                max: MAX_REVISION_OBJECTS
            })
        ));
    }

    #[test]
    fn revision_metadata_limits_total_object_references_before_hashing() {
        let mut manifest = revision_manifest(Uuid::now_v7(), 'a', None);
        let duplicate = manifest.payload.threads[0].rollout.clone();
        manifest.payload.threads[0].attachments = vec![duplicate; MAX_REVISION_OBJECT_REFERENCES];
        manifest.revision_id = "not-yet-validated".to_string();

        assert!(matches!(
            NewRevisionMetadata::from_manifest(&manifest),
            Err(MetadataError::TooManyObjectReferences {
                max: MAX_REVISION_OBJECT_REFERENCES
            })
        ));
    }

    #[tokio::test]
    async fn creates_lists_and_renames_namespaces() {
        let (_directory, store) = store().await;
        assert!(matches!(
            store.create_namespace("  ").await,
            Err(MetadataError::InvalidName)
        ));
        assert!(matches!(
            store.create_namespace("x".repeat(129)).await,
            Err(MetadataError::InvalidName)
        ));
        let namespace = store.create_namespace("  Personal  ").await.unwrap();
        assert_eq!(namespace.display_name, "Personal");
        assert_eq!(
            store.list_namespaces().await.unwrap(),
            vec![namespace.clone()]
        );

        let renamed = store
            .rename_namespace(namespace.id, "Desktop")
            .await
            .unwrap();
        assert_eq!(renamed.id, namespace.id);
        assert_eq!(renamed.display_name, "Desktop");
        assert_eq!(renamed.head, None);
    }

    #[tokio::test]
    async fn commits_first_revision_and_fast_forwards() {
        let (_directory, store) = store().await;
        let namespace = store.create_namespace("Personal").await.unwrap();
        let first = revision(namespace.id, 'a', None);
        assert_eq!(
            store.commit_revision(None, first.clone()).await.unwrap(),
            CommitRevisionOutcome::Created
        );
        let second = revision(namespace.id, 'b', Some(first.id.clone()));
        assert_eq!(
            store
                .commit_revision(Some(first.id.clone()), second.clone())
                .await
                .unwrap(),
            CommitRevisionOutcome::Created
        );
        assert_eq!(
            store.get_head(namespace.id).await.unwrap(),
            Some(second.id.clone())
        );
        let metadata = store.get_revision_metadata(second.id).await.unwrap();
        assert_eq!(metadata.object_count, 1);
        assert_eq!(metadata.total_bytes, 10);
    }

    #[tokio::test]
    async fn rejects_a_stale_head() {
        let (_directory, store) = store().await;
        let namespace = store.create_namespace("Personal").await.unwrap();
        let first = revision(namespace.id, 'a', None);
        store.commit_revision(None, first.clone()).await.unwrap();
        let stale = revision(namespace.id, 'b', None);
        assert!(matches!(
            store.commit_revision(None, stale).await,
            Err(MetadataError::HeadMismatch { current }) if current == Some(first.id)
        ));
    }

    #[tokio::test]
    async fn identical_retry_is_idempotent() {
        let (_directory, store) = store().await;
        let namespace = store.create_namespace("Personal").await.unwrap();
        let revision = revision(namespace.id, 'a', None);
        store.commit_revision(None, revision.clone()).await.unwrap();
        assert_eq!(
            store.commit_revision(None, revision).await.unwrap(),
            CommitRevisionOutcome::AlreadyCommitted
        );
    }

    #[tokio::test]
    async fn metadata_survives_store_restart() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("metadata.sqlite");
        let store = MetadataStore::new(&db_path);
        store.initialize().await.unwrap();
        let namespace = store.create_namespace("Personal").await.unwrap();
        let revision = revision(namespace.id, 'a', None);
        store.commit_revision(None, revision.clone()).await.unwrap();
        drop(store);

        let reopened = MetadataStore::new(db_path);
        reopened.initialize().await.unwrap();
        assert_eq!(
            reopened.get_head(namespace.id).await.unwrap(),
            Some(revision.id.clone())
        );
        assert_eq!(
            reopened
                .get_revision_metadata(revision.id)
                .await
                .unwrap()
                .total_bytes,
            10
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_pushes_only_allow_one_new_head() {
        let (_directory, store) = store().await;
        let namespace = store.create_namespace("Personal").await.unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut tasks = Vec::new();
        for id in ['a', 'b'] {
            let store = store.clone();
            let barrier = barrier.clone();
            let revision = revision(namespace.id, id, None);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                store.commit_revision(None, revision).await
            }));
        }
        barrier.wait().await;
        let first = tasks.remove(0).await.unwrap();
        let second = tasks.remove(0).await.unwrap();
        let created = [&first, &second]
            .into_iter()
            .filter(|result| matches!(result, Ok(CommitRevisionOutcome::Created)))
            .count();
        let mismatched = [&first, &second]
            .into_iter()
            .filter(|result| matches!(result, Err(MetadataError::HeadMismatch { .. })))
            .count();
        assert_eq!((created, mismatched), (1, 1));
    }

    #[tokio::test]
    async fn rejects_a_newer_metadata_schema() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("metadata.sqlite");
        let connection = Connection::open(&db_path).unwrap();
        connection.pragma_update(None, "user_version", 3).unwrap();
        drop(connection);

        let store = MetadataStore::new(db_path);
        assert!(matches!(
            store.initialize().await,
            Err(MetadataError::UnsupportedSchema { actual: 3 })
        ));
    }
}
