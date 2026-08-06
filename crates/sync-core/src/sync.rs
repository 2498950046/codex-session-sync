use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::models::{LOCAL_SNAPSHOT_SCHEMA_VERSION, LocalSnapshot, ThreadBundle};
use crate::protocol::{
    REVISION_SCHEMA_VERSION, RevisionManifest, RevisionPayload, validate_sha256,
};

const TRACKING_SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrackingRecord {
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub codex_home_key: String,
    pub integrated_head: Option<String>,
    pub remote_epoch: u64,
    pub generation: u64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveNamespaceBinding {
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub codex_home_key: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadConflictKind {
    BothModified,
    LocalDeletedRemoteModified,
    RemoteDeletedLocalModified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadConflictVersion {
    pub title: String,
    pub archived: bool,
    pub updated_at_ms: Option<i64>,
    pub model_provider: Option<String>,
    pub workspace_source_path: Option<String>,
    pub semantic_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadConflict {
    pub conflict_id: String,
    pub thread_id: String,
    pub title: String,
    pub kind: ThreadConflictKind,
    pub base: Option<ThreadConflictVersion>,
    pub local: Option<ThreadConflictVersion>,
    pub remote: Option<ThreadConflictVersion>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadResolutionChoice {
    Local,
    Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadConflictResolution {
    pub conflict_id: String,
    pub thread_id: String,
    pub choice: ThreadResolutionChoice,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadMergeOutcome {
    pub threads: Vec<ThreadBundle>,
    pub conflicts: Vec<ThreadConflict>,
}

#[derive(Debug, Clone)]
pub struct TrackingStore {
    path: PathBuf,
}

impl TrackingStore {
    pub fn open(repository_root: impl AsRef<Path>) -> Result<Self> {
        let config_dir = repository_root.as_ref().join("config");
        fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "failed to create tracking directory {}",
                config_dir.display()
            )
        })?;
        let store = Self {
            path: config_dir.join("tracking.sqlite"),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(
        &self,
        codex_home: impl AsRef<Path>,
        remote_id: Uuid,
        namespace_id: Uuid,
    ) -> Result<Option<TrackingRecord>> {
        let key = codex_home_key(codex_home.as_ref())?;
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT integrated_head, remote_epoch, generation, updated_at
                 FROM namespace_tracking
                 WHERE codex_home_key = ?1 AND remote_id = ?2 AND namespace_id = ?3",
                params![key, remote_id.to_string(), namespace_id.to_string()],
                |row| {
                    let generation = row.get::<_, i64>(2)?;
                    Ok(TrackingRecord {
                        remote_id,
                        namespace_id,
                        codex_home_key: key.clone(),
                        integrated_head: row.get(0)?,
                        remote_epoch: row.get::<_, i64>(1)? as u64,
                        generation: generation as u64,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("failed to read namespace tracking state")
    }

    pub fn compare_and_set(
        &self,
        codex_home: impl AsRef<Path>,
        remote_id: Uuid,
        namespace_id: Uuid,
        expected_generation: Option<u64>,
        integrated_head: Option<&str>,
    ) -> Result<TrackingRecord> {
        if let Some(head) = integrated_head {
            validate_sha256(head).map_err(|_| anyhow::anyhow!("invalid integrated head {head}"))?;
        }
        let key = codex_home_key(codex_home.as_ref())?;
        let now = Utc::now().to_rfc3339();
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to lock tracking database")?;
        let current = transaction
            .query_row(
                "SELECT generation, remote_epoch FROM namespace_tracking
                 WHERE codex_home_key = ?1 AND remote_id = ?2 AND namespace_id = ?3",
                params![key, remote_id.to_string(), namespace_id.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let expected = expected_generation.map(|value| value as i64);
        if current.map(|value| value.0) != expected {
            bail!(
                "tracking state changed concurrently: expected generation {:?}, current {:?}",
                expected_generation,
                current.map(|value| value.0)
            );
        }
        let generation = current.map(|value| value.0).unwrap_or(0) + 1;
        let remote_epoch = current.map(|value| value.1).unwrap_or(0);
        transaction.execute(
            "INSERT INTO namespace_tracking (
                 codex_home_key, remote_id, namespace_id, integrated_head, remote_epoch, generation, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(codex_home_key, remote_id, namespace_id) DO UPDATE SET
                 integrated_head = excluded.integrated_head,
                 remote_epoch = excluded.remote_epoch,
                 generation = excluded.generation,
                 updated_at = excluded.updated_at",
            params![
                key,
                remote_id.to_string(),
                namespace_id.to_string(),
                integrated_head,
                remote_epoch,
                generation,
                now
            ],
        )?;
        transaction.commit()?;
        Ok(TrackingRecord {
            remote_id,
            namespace_id,
            codex_home_key: key,
            integrated_head: integrated_head.map(str::to_string),
            remote_epoch: remote_epoch as u64,
            generation: generation as u64,
            updated_at: now,
        })
    }

    pub fn reconcile_checkout(
        &self,
        codex_home: impl AsRef<Path>,
        remote_id: Uuid,
        namespace_id: Uuid,
        expected_generation: Option<u64>,
        integrated_head: Option<&str>,
        activate_namespace: bool,
    ) -> Result<TrackingRecord> {
        if let Some(head) = integrated_head {
            validate_sha256(head).map_err(|_| anyhow::anyhow!("invalid integrated head {head}"))?;
        }
        let key = codex_home_key(codex_home.as_ref())?;
        let remote_id_text = remote_id.to_string();
        let namespace_id_text = namespace_id.to_string();
        let expected = expected_generation
            .map(i64::try_from)
            .transpose()
            .context("tracking generation exceeds SQLite integer range")?;
        let next_generation = expected
            .unwrap_or(0)
            .checked_add(1)
            .context("tracking generation overflow")?;
        let now = Utc::now().to_rfc3339();
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to lock tracking database")?;
        let current = transaction
            .query_row(
                "SELECT integrated_head, remote_epoch, generation, updated_at
                 FROM namespace_tracking
                 WHERE codex_home_key = ?1 AND remote_id = ?2 AND namespace_id = ?3",
                params![key, remote_id_text, namespace_id_text],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let already_applied = current.as_ref().is_some_and(|(head, _, generation, _)| {
            *generation == next_generation && head.as_deref() == integrated_head
        });

        if already_applied {
            if activate_namespace {
                let active = transaction
                    .query_row(
                        "SELECT remote_id, namespace_id FROM active_namespace
                         WHERE codex_home_key = ?1",
                        [&key],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?;
                if active
                    .as_ref()
                    .map(|(remote, namespace)| (remote.as_str(), namespace.as_str()))
                    != Some((remote_id_text.as_str(), namespace_id_text.as_str()))
                {
                    bail!("tracking checkout was committed without the expected active namespace");
                }
            }
            let (head, remote_epoch, generation, updated_at) = current.expect("checked above");
            transaction.commit()?;
            return Ok(TrackingRecord {
                remote_id,
                namespace_id,
                codex_home_key: key,
                integrated_head: head,
                remote_epoch: remote_epoch as u64,
                generation: generation as u64,
                updated_at,
            });
        }

        let current_generation = current.as_ref().map(|(_, _, generation, _)| *generation);
        if current_generation != expected {
            bail!(
                "tracking state changed concurrently: expected generation {:?}, current {:?}",
                expected_generation,
                current_generation
            );
        }
        transaction.execute(
            "INSERT INTO namespace_tracking (
                 codex_home_key, remote_id, namespace_id, integrated_head, remote_epoch, generation, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(codex_home_key, remote_id, namespace_id) DO UPDATE SET
                 integrated_head = excluded.integrated_head,
                 remote_epoch = excluded.remote_epoch,
                 generation = excluded.generation,
                 updated_at = excluded.updated_at",
            params![
                key,
                remote_id_text,
                namespace_id_text,
                integrated_head,
                current.as_ref().map(|value| value.1).unwrap_or(0),
                next_generation,
                now
            ],
        )?;
        if activate_namespace {
            transaction.execute(
                "INSERT INTO active_namespace (codex_home_key, remote_id, namespace_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(codex_home_key) DO UPDATE SET
                     remote_id = excluded.remote_id,
                     namespace_id = excluded.namespace_id,
                     updated_at = excluded.updated_at",
                params![key, remote_id_text, namespace_id_text, now],
            )?;
        }
        transaction.commit()?;
        Ok(TrackingRecord {
            remote_id,
            namespace_id,
            codex_home_key: key,
            integrated_head: integrated_head.map(str::to_string),
            remote_epoch: current.as_ref().map(|value| value.1 as u64).unwrap_or(0),
            generation: next_generation as u64,
            updated_at: now,
        })
    }

    pub fn update_remote_epoch(
        &self,
        codex_home: impl AsRef<Path>,
        remote_id: Uuid,
        namespace_id: Uuid,
        expected_generation: u64,
        remote_epoch: u64,
    ) -> Result<TrackingRecord> {
        let key = codex_home_key(codex_home.as_ref())?;
        let connection = self.connection()?;
        let changed = connection.execute(
            "UPDATE namespace_tracking SET remote_epoch = ?1, updated_at = ?2
             WHERE codex_home_key = ?3 AND remote_id = ?4 AND namespace_id = ?5 AND generation = ?6",
            params![remote_epoch as i64, Utc::now().to_rfc3339(), key, remote_id.to_string(), namespace_id.to_string(), expected_generation as i64],
        )?;
        if changed != 1 {
            bail!("tracking state changed before remote epoch reconciliation");
        }
        self.load(codex_home, remote_id, namespace_id)?
            .context("tracking record disappeared")
    }

    pub fn active(&self, codex_home: impl AsRef<Path>) -> Result<Option<ActiveNamespaceBinding>> {
        let key = codex_home_key(codex_home.as_ref())?;
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT remote_id, namespace_id, updated_at
                 FROM active_namespace WHERE codex_home_key = ?1",
                [&key],
                |row| {
                    let remote_id: String = row.get(0)?;
                    let namespace_id: String = row.get(1)?;
                    Ok((remote_id, namespace_id, row.get::<_, String>(2)?))
                },
            )
            .optional()?
            .map(|(remote_id, namespace_id, updated_at)| {
                Ok(ActiveNamespaceBinding {
                    remote_id: Uuid::parse_str(&remote_id)
                        .context("tracking database contains an invalid remote ID")?,
                    namespace_id: Uuid::parse_str(&namespace_id)
                        .context("tracking database contains an invalid namespace ID")?,
                    codex_home_key: key,
                    updated_at,
                })
            })
            .transpose()
    }

    pub fn set_active(
        &self,
        codex_home: impl AsRef<Path>,
        remote_id: Uuid,
        namespace_id: Uuid,
    ) -> Result<ActiveNamespaceBinding> {
        let key = codex_home_key(codex_home.as_ref())?;
        let now = Utc::now().to_rfc3339();
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO active_namespace (codex_home_key, remote_id, namespace_id, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(codex_home_key) DO UPDATE SET
                 remote_id = excluded.remote_id,
                 namespace_id = excluded.namespace_id,
                 updated_at = excluded.updated_at",
            params![key, remote_id.to_string(), namespace_id.to_string(), now],
        )?;
        Ok(ActiveNamespaceBinding {
            remote_id,
            namespace_id,
            codex_home_key: key,
            updated_at: now,
        })
    }

    fn initialize(&self) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to initialize tracking database")?;
        let has_schema_info = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_info'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_schema_info {
            let actual = transaction.query_row(
                "SELECT version FROM schema_info WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            if actual > TRACKING_SCHEMA_VERSION {
                bail!("unsupported tracking schema version {actual}");
            }
            if actual == 1 {
                transaction.execute("ALTER TABLE namespace_tracking ADD COLUMN remote_epoch INTEGER NOT NULL DEFAULT 0", [])?;
                transaction.execute("UPDATE schema_info SET version = 2 WHERE id = 1", [])?;
            }
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_info (
                 id INTEGER PRIMARY KEY CHECK(id = 1),
                 version INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS namespace_tracking (
                 codex_home_key TEXT NOT NULL,
                 remote_id TEXT NOT NULL,
                 namespace_id TEXT NOT NULL,
                 integrated_head TEXT,
                 remote_epoch INTEGER NOT NULL DEFAULT 0,
                 generation INTEGER NOT NULL,
                 updated_at TEXT NOT NULL,
                 PRIMARY KEY(codex_home_key, remote_id, namespace_id)
             );
             CREATE TABLE IF NOT EXISTS active_namespace (
                 codex_home_key TEXT PRIMARY KEY,
                 remote_id TEXT NOT NULL,
                 namespace_id TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );",
        )?;
        if !has_schema_info {
            transaction.execute(
                "INSERT INTO schema_info (id, version) VALUES (1, ?1)",
                [TRACKING_SCHEMA_VERSION],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open tracking database {}", self.path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(connection)
    }
}

pub fn codex_home_key(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve Codex home {}", path.display()))?;
    let normalized = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();
    Ok(normalized)
}

pub fn snapshot_to_revision(
    snapshot: &LocalSnapshot,
    namespace_id: Uuid,
    parent_revision: Option<String>,
) -> Result<RevisionManifest> {
    if snapshot.schema_version != LOCAL_SNAPSHOT_SCHEMA_VERSION {
        bail!(
            "unsupported local snapshot schema version {}",
            snapshot.schema_version
        );
    }
    RevisionManifest::from_payload(RevisionPayload {
        schema_version: REVISION_SCHEMA_VERSION,
        namespace_id,
        parent_revision,
        created_at: snapshot.created_at.clone(),
        threads: snapshot
            .threads
            .iter()
            .map(|thread| {
                let mut remote = remote_thread_view(thread);
                remote.rollout.storage = thread.rollout.storage.clone();
                for (remote_attachment, attachment) in
                    remote.attachments.iter_mut().zip(&thread.attachments)
                {
                    remote_attachment.storage = attachment.storage.clone();
                }
                remote
            })
            .collect(),
        warning_count: snapshot.warning_count,
    })
    .map_err(Into::into)
}

pub fn revision_to_snapshot(revision: &RevisionManifest) -> Result<LocalSnapshot> {
    revision.validate()?;
    Ok(LocalSnapshot {
        schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: Uuid::now_v7().to_string(),
        created_at: revision.payload.created_at.clone(),
        threads: revision.payload.threads.clone(),
        warning_count: revision.payload.warning_count,
    })
}

pub fn merge_thread_sets(
    base: &[ThreadBundle],
    local: &[ThreadBundle],
    remote: &[ThreadBundle],
) -> Result<ThreadMergeOutcome> {
    let base = thread_map(base)?;
    let local = thread_map(local)?;
    let remote = thread_map(remote)?;
    let ids = base
        .keys()
        .chain(local.keys())
        .chain(remote.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut threads = Vec::new();
    let mut conflicts = Vec::new();

    for id in ids {
        let base_thread = base.get(&id);
        let local_thread = local.get(&id);
        let remote_thread = remote.get(&id);
        let local_changed = !semantic_option_eq(local_thread, base_thread)?;
        let remote_changed = !semantic_option_eq(remote_thread, base_thread)?;

        let selected = match (local_changed, remote_changed) {
            (false, false) => local_thread.or(remote_thread),
            (true, false) => local_thread,
            (false, true) => remote_thread,
            (true, true) if semantic_option_eq(local_thread, remote_thread)? => local_thread,
            (true, true) => {
                let kind = match (local_thread, remote_thread) {
                    (None, Some(_)) => ThreadConflictKind::LocalDeletedRemoteModified,
                    (Some(_), None) => ThreadConflictKind::RemoteDeletedLocalModified,
                    _ => ThreadConflictKind::BothModified,
                };
                let title = local_thread
                    .or(remote_thread)
                    .or(base_thread)
                    .map(|thread| thread.title.clone())
                    .unwrap_or_else(|| id.clone());
                let base = conflict_version(base_thread)?;
                let local = conflict_version(local_thread)?;
                let remote = conflict_version(remote_thread)?;
                let conflict_id = conflict_fingerprint(&id, &kind, &base, &local, &remote)?;
                conflicts.push(ThreadConflict {
                    conflict_id,
                    thread_id: id,
                    title,
                    kind,
                    base,
                    local,
                    remote,
                });
                None
            }
        };
        if let Some(thread) = selected {
            threads.push((*thread).clone());
        }
    }
    threads.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    Ok(ThreadMergeOutcome { threads, conflicts })
}

pub fn resolve_thread_sets(
    base: &[ThreadBundle],
    local: &[ThreadBundle],
    remote: &[ThreadBundle],
    resolutions: &[ThreadConflictResolution],
) -> Result<Vec<ThreadBundle>> {
    let mut outcome = merge_thread_sets(base, local, remote)?;
    let local = thread_map(local)?;
    let remote = thread_map(remote)?;
    let mut selected = BTreeMap::new();
    for resolution in resolutions {
        if selected
            .insert(resolution.thread_id.as_str(), resolution)
            .is_some()
        {
            bail!(
                "duplicate conflict resolution for thread {}",
                resolution.thread_id
            );
        }
    }
    if selected.len() != outcome.conflicts.len() {
        bail!(
            "conflict set changed: expected {} resolution(s), received {}",
            outcome.conflicts.len(),
            selected.len()
        );
    }

    for conflict in &outcome.conflicts {
        let resolution = selected.get(conflict.thread_id.as_str()).with_context(|| {
            format!(
                "conflict set changed: missing resolution for thread {}",
                conflict.thread_id
            )
        })?;
        if resolution.conflict_id != conflict.conflict_id {
            bail!(
                "conflict {} changed after it was displayed; pull again before resolving",
                conflict.thread_id
            );
        }
        let thread = match resolution.choice {
            ThreadResolutionChoice::Local => local.get(&conflict.thread_id),
            ThreadResolutionChoice::Remote => remote.get(&conflict.thread_id),
        };
        if let Some(thread) = thread {
            outcome.threads.push((*thread).clone());
        }
    }
    outcome
        .threads
        .sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    Ok(outcome.threads)
}

fn conflict_version(thread: Option<&&ThreadBundle>) -> Result<Option<ThreadConflictVersion>> {
    thread
        .map(|thread| {
            Ok(ThreadConflictVersion {
                title: thread.title.clone(),
                archived: thread.archived,
                updated_at_ms: thread.updated_at_ms,
                model_provider: thread.model_provider.clone(),
                workspace_source_path: thread.workspace.source_path.clone(),
                semantic_hash: semantic_thread_hash(thread)?,
            })
        })
        .transpose()
}

fn conflict_fingerprint(
    thread_id: &str,
    kind: &ThreadConflictKind,
    base: &Option<ThreadConflictVersion>,
    local: &Option<ThreadConflictVersion>,
    remote: &Option<ThreadConflictVersion>,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(
        thread_id,
        kind,
        base.as_ref().map(|version| version.semantic_hash.as_str()),
        local.as_ref().map(|version| version.semantic_hash.as_str()),
        remote
            .as_ref()
            .map(|version| version.semantic_hash.as_str()),
    ))
    .context("failed to serialize conflict fingerprint")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

pub fn semantic_thread_hash(thread: &ThreadBundle) -> Result<String> {
    let bytes = serde_json::to_vec(&remote_thread_view(thread))
        .context("failed to serialize thread bundle")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

pub fn remote_thread_view(thread: &ThreadBundle) -> ThreadBundle {
    let mut thread = thread.clone();
    thread.model_provider = None;
    thread.workspace.source_path = None;
    thread.rollout.source_path = None;
    thread.rollout.storage = None;
    for attachment in &mut thread.attachments {
        attachment.source_path = None;
        attachment.storage = None;
    }
    thread.related_records.source_database = None;
    for (table, rows) in &mut thread.related_records.tables {
        for row in rows {
            if let Some(row) = row.as_object_mut() {
                row.remove("rollout_path");
                row.remove("codex_home");
                row.remove("model_provider");
                row.remove("cwd");
                // SQLite scans materialize every column in the local schema. Treat
                // nullable additions and known sentinel defaults as equivalent to
                // an older source schema that did not contain those columns.
                row.retain(|_, value| !value.is_null());
                if table == "threads" {
                    let updated_at = row.get("updated_at").and_then(Value::as_i64);
                    let updated_at_ms = row
                        .get("updated_at_ms")
                        .and_then(Value::as_i64)
                        .or_else(|| updated_at.and_then(|value| value.checked_mul(1_000)));
                    let recency_at = row.get("recency_at").and_then(Value::as_i64);
                    let recency_at_ms = row.get("recency_at_ms").and_then(Value::as_i64);
                    if recency_at == Some(0) || recency_at == updated_at {
                        row.remove("recency_at");
                    }
                    if recency_at_ms == Some(0) || recency_at_ms == updated_at_ms {
                        row.remove("recency_at_ms");
                    }
                    if row.get("history_mode").and_then(Value::as_str) == Some("legacy") {
                        row.remove("history_mode");
                    }
                }
            }
        }
    }
    thread
}

fn semantic_option_eq(left: Option<&&ThreadBundle>, right: Option<&&ThreadBundle>) -> Result<bool> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => {
            Ok(semantic_thread_hash(left)? == semantic_thread_hash(right)?)
        }
        _ => Ok(false),
    }
}

fn thread_map(threads: &[ThreadBundle]) -> Result<BTreeMap<String, &ThreadBundle>> {
    let mut map = BTreeMap::new();
    for thread in threads {
        if map.insert(thread.thread_id.clone(), thread).is_some() {
            bail!("duplicate thread ID {}", thread.thread_id);
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::models::{
        ContentObject, RelatedRecords, THREAD_BUNDLE_SCHEMA_VERSION, WorkspaceRef,
    };

    fn thread(id: &str, content: char) -> ThreadBundle {
        ThreadBundle {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: id.to_string(),
            title: format!("Thread {id}"),
            archived: false,
            created_at_ms: None,
            updated_at_ms: None,
            model_provider: Some("openai".to_string()),
            workspace: WorkspaceRef::default(),
            rollout: ContentObject {
                sha256: format!("sha256:{}", content.to_string().repeat(64)),
                byte_length: 1,
                media_type: "application/x-ndjson".to_string(),
                logical_path: Some(format!("sessions/rollout-{id}.jsonl")),
                source_path: None,
                storage: None,
            },
            related_records: RelatedRecords {
                source_database: None,
                tables: BTreeMap::from([("threads".to_string(), vec![json!({"id": id})])]),
            },
            attachments: Vec::new(),
        }
    }

    #[test]
    fn semantic_hash_ignores_columns_added_by_newer_sqlite_schema_defaults() {
        let older = thread("thread", 'a');
        let mut newer = older.clone();
        let row = newer.related_records.tables.get_mut("threads").unwrap()[0]
            .as_object_mut()
            .unwrap();
        row.insert("recency_at".to_string(), json!(0));
        row.insert("recency_at_ms".to_string(), json!(0));
        row.insert("history_mode".to_string(), json!("legacy"));
        row.insert("name".to_string(), Value::Null);

        assert_eq!(
            semantic_thread_hash(&older).unwrap(),
            semantic_thread_hash(&newer).unwrap()
        );
    }

    #[test]
    fn semantic_hash_ignores_recency_synthesized_from_updated_at() {
        let mut older = thread("thread", 'a');
        let row = older.related_records.tables.get_mut("threads").unwrap()[0]
            .as_object_mut()
            .unwrap();
        row.insert("updated_at".to_string(), json!(1_700_000_000));
        row.insert("updated_at_ms".to_string(), json!(1_700_000_000_123_i64));
        let mut newer = older.clone();
        let row = newer.related_records.tables.get_mut("threads").unwrap()[0]
            .as_object_mut()
            .unwrap();
        row.insert("recency_at".to_string(), json!(1_700_000_000));
        row.insert("recency_at_ms".to_string(), json!(1_700_000_000_123_i64));

        let older_row = older.related_records.tables["threads"][0]
            .as_object()
            .unwrap();
        assert!(!older_row.contains_key("recency_at"));
        assert_eq!(
            semantic_thread_hash(&older).unwrap(),
            semantic_thread_hash(&newer).unwrap()
        );
    }

    #[test]
    fn tracking_isolated_by_home_remote_and_namespace_and_uses_cas() {
        let temp = tempdir().unwrap();
        let first_home = temp.path().join("first-home");
        let second_home = temp.path().join("second-home");
        fs::create_dir_all(&first_home).unwrap();
        fs::create_dir_all(&second_home).unwrap();
        let remote = Uuid::now_v7();
        let namespace = Uuid::now_v7();
        let store = TrackingStore::open(temp.path()).unwrap();
        assert!(
            store
                .load(&first_home, remote, namespace)
                .unwrap()
                .is_none()
        );

        let first = store
            .compare_and_set(&first_home, remote, namespace, None, None)
            .unwrap();
        assert_eq!(first.generation, 1);
        assert!(
            store
                .compare_and_set(&first_home, remote, namespace, None, None)
                .is_err()
        );
        let head = format!("sha256:{}", "a".repeat(64));
        let second = store
            .compare_and_set(
                &first_home,
                remote,
                namespace,
                Some(first.generation),
                Some(&head),
            )
            .unwrap();
        assert_eq!(second.integrated_head.as_deref(), Some(head.as_str()));
        assert!(
            store
                .load(&second_home, remote, namespace)
                .unwrap()
                .is_none()
        );

        store.set_active(&first_home, remote, namespace).unwrap();
        assert_eq!(
            store.active(&first_home).unwrap().unwrap().namespace_id,
            namespace
        );
        assert!(store.active(&second_home).unwrap().is_none());
    }

    #[test]
    fn checkout_reconciliation_is_atomic_idempotent_and_conflict_safe() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = TrackingStore::open(temp.path()).unwrap();
        let old_head = format!("sha256:{}", "a".repeat(64));
        let target_head = format!("sha256:{}", "b".repeat(64));
        let conflicting_head = format!("sha256:{}", "c".repeat(64));
        let previous = store
            .compare_and_set(&codex_home, remote_id, namespace_id, None, Some(&old_head))
            .unwrap();

        let applied = store
            .reconcile_checkout(
                &codex_home,
                remote_id,
                namespace_id,
                Some(previous.generation),
                Some(&target_head),
                true,
            )
            .unwrap();
        assert_eq!(applied.generation, previous.generation + 1);
        let active = store.active(&codex_home).unwrap().unwrap();
        assert_eq!(
            (active.remote_id, active.namespace_id),
            (remote_id, namespace_id)
        );

        let retried = store
            .reconcile_checkout(
                &codex_home,
                remote_id,
                namespace_id,
                Some(previous.generation),
                Some(&target_head),
                true,
            )
            .unwrap();
        assert_eq!(retried, applied);

        assert!(
            store
                .reconcile_checkout(
                    &codex_home,
                    remote_id,
                    namespace_id,
                    Some(previous.generation),
                    Some(&conflicting_head),
                    true,
                )
                .is_err()
        );
        assert_eq!(
            store
                .load(&codex_home, remote_id, namespace_id)
                .unwrap()
                .unwrap(),
            applied
        );
        let active = store.active(&codex_home).unwrap().unwrap();
        assert_eq!(
            (active.remote_id, active.namespace_id),
            (remote_id, namespace_id)
        );
    }

    #[test]
    fn merge_combines_independent_changes_and_reports_same_thread_conflicts() {
        let base = vec![thread("base", 'a')];
        let mut local_base = thread("base", 'a');
        local_base.title = "local".to_string();
        let local = vec![local_base, thread("local-only", 'b')];
        let remote = vec![thread("base", 'a'), thread("remote-only", 'c')];
        let merged = merge_thread_sets(&base, &local, &remote).unwrap();
        assert!(merged.conflicts.is_empty());
        assert_eq!(
            merged
                .threads
                .iter()
                .map(|thread| thread.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["base", "local-only", "remote-only"]
        );

        let mut remote_changed = thread("base", 'a');
        remote_changed.archived = true;
        let conflict = merge_thread_sets(&base, &local, &[remote_changed]).unwrap();
        assert_eq!(conflict.conflicts.len(), 1);
        assert_eq!(conflict.conflicts[0].kind, ThreadConflictKind::BothModified);
    }

    #[test]
    fn merge_handles_delete_modify_conflicts() {
        let base = vec![thread("thread", 'a')];
        let mut remote = thread("thread", 'a');
        remote.title = "remote changed".to_string();
        let outcome = merge_thread_sets(&base, &[], &[remote]).unwrap();
        assert_eq!(
            outcome.conflicts[0].kind,
            ThreadConflictKind::LocalDeletedRemoteModified
        );
    }

    #[test]
    fn conflict_fingerprint_binds_all_three_versions() {
        let base = vec![thread("thread", 'a')];
        let mut local = thread("thread", 'a');
        local.title = "local".to_string();
        let mut remote = thread("thread", 'a');
        remote.title = "remote".to_string();
        let first = merge_thread_sets(&base, &[local.clone()], &[remote.clone()])
            .unwrap()
            .conflicts
            .remove(0);
        assert!(first.base.is_some());
        assert_eq!(first.local.as_ref().unwrap().title, "local");
        assert_eq!(first.remote.as_ref().unwrap().title, "remote");

        remote.archived = true;
        let changed = merge_thread_sets(&base, &[local], &[remote])
            .unwrap()
            .conflicts
            .remove(0);
        assert_ne!(first.conflict_id, changed.conflict_id);
    }

    #[test]
    fn explicit_resolution_selects_versions_and_rejects_stale_choices() {
        let base = vec![thread("thread", 'a'), thread("deleted-remotely", 'b')];
        let mut local_thread = thread("thread", 'a');
        local_thread.title = "local".to_string();
        let mut local_deleted_remotely = thread("deleted-remotely", 'b');
        local_deleted_remotely.title = "local survives".to_string();
        let local = vec![local_thread.clone(), local_deleted_remotely.clone()];
        let mut remote_thread = thread("thread", 'a');
        remote_thread.title = "remote".to_string();
        let remote = vec![remote_thread];
        let conflicts = merge_thread_sets(&base, &local, &remote).unwrap().conflicts;
        assert_eq!(conflicts.len(), 2);
        let resolutions = conflicts
            .iter()
            .map(|conflict| ThreadConflictResolution {
                conflict_id: conflict.conflict_id.clone(),
                thread_id: conflict.thread_id.clone(),
                choice: if conflict.thread_id == "thread" {
                    ThreadResolutionChoice::Remote
                } else {
                    ThreadResolutionChoice::Local
                },
            })
            .collect::<Vec<_>>();
        let resolved = resolve_thread_sets(&base, &local, &remote, &resolutions).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved
                .iter()
                .find(|thread| thread.thread_id == "thread")
                .unwrap()
                .title,
            "remote"
        );
        assert_eq!(
            resolved
                .iter()
                .find(|thread| thread.thread_id == "deleted-remotely")
                .unwrap()
                .title,
            "local survives"
        );

        let mut changed_local = local;
        changed_local[0].archived = true;
        let error = resolve_thread_sets(&base, &changed_local, &remote, &resolutions).unwrap_err();
        assert!(error.to_string().contains("changed after it was displayed"));
    }

    #[test]
    fn explicit_resolution_requires_one_choice_per_conflict() {
        let base = vec![thread("thread", 'a')];
        let mut local = thread("thread", 'a');
        local.title = "local".to_string();
        let mut remote = thread("thread", 'a');
        remote.title = "remote".to_string();
        let error = resolve_thread_sets(&base, &[local], &[remote], &[]).unwrap_err();
        assert!(error.to_string().contains("conflict set changed"));
    }

    #[test]
    fn snapshot_revision_round_trip_preserves_remote_view() {
        let snapshot = LocalSnapshot {
            schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: Uuid::now_v7().to_string(),
            created_at: "2026-07-26T10:30:00Z".to_string(),
            threads: vec![thread("one", 'a')],
            warning_count: 0,
        };
        let manifest = snapshot_to_revision(&snapshot, Uuid::now_v7(), None).unwrap();
        let restored = revision_to_snapshot(&manifest).unwrap();
        assert_eq!(
            restored.threads,
            vec![remote_thread_view(&snapshot.threads[0])]
        );
        assert_eq!(restored.warning_count, snapshot.warning_count);
    }
}
