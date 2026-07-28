use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sync_core::{
    CheckoutReport, CheckoutTrackingUpdate, CommitRevisionRequest, LocalSnapshot, OperationControl,
    OperationProgress, ThreadBundle, ThreadConflict, ThreadConflictResolution, ThreadMergeOutcome,
    TrackingRecord, TrackingStore, WorkspacePathMapper,
    checkout_local_snapshot_with_tracking_control, collect_object_descriptors,
    create_local_snapshot_with_control, install_repository_object, load_local_snapshot,
    merge_thread_sets, remote_thread_view, repository_object_path, resolve_thread_sets,
    revision_to_snapshot, semantic_thread_hash, snapshot_to_revision, store_local_snapshot,
    validate_repository_object,
};
use uuid::Uuid;

#[cfg(not(test))]
use sync_core::detect_codex_processes;

use crate::remote::RemoteClient;

const MAX_ANCESTRY_DEPTH: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcomeKind {
    Pushed,
    Pulled,
    Merged,
    Switched,
    Remapped,
    NoChanges,
    Conflict,
}

#[derive(Clone, Copy)]
pub struct LocalSyncContext<'a> {
    codex_home: &'a Path,
    repository_root: &'a Path,
    workspace_mapper: &'a WorkspacePathMapper,
    control: &'a OperationControl,
}

impl<'a> LocalSyncContext<'a> {
    pub fn new(
        codex_home: &'a Path,
        repository_root: &'a Path,
        workspace_mapper: &'a WorkspacePathMapper,
        control: &'a OperationControl,
    ) -> Self {
        Self {
            codex_home,
            repository_root,
            workspace_mapper,
            control,
        }
    }
}

pub fn reapply_workspace_mappings(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    client.info()?;
    let tracking = TrackingStore::open(repository_root)?;
    let active = tracking
        .active(codex_home)?
        .context("no namespace is active for this Codex home")?;
    if active.remote_id != remote_id || active.namespace_id != namespace_id {
        bail!("the selected namespace is not active; switch namespaces before applying mappings");
    }
    let record = tracking
        .load(codex_home, remote_id, namespace_id)?
        .context("active namespace has no tracking record")?;
    let reference = record
        .integrated_head
        .as_deref()
        .map(|head| client.revision(head))
        .transpose()?;
    if let Some(revision) = &reference {
        validate_revision_namespace(revision, namespace_id)?;
    }
    let local_summary =
        create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
    if local_summary.warning_count > 0 {
        bail!("workspace remap is blocked because the local scan contains warnings");
    }
    let local = load_local_snapshot(local_summary.manifest_path)?;
    let snapshot = workspace_mapper.materialize_snapshot_objects_with_reference(
        &local,
        reference
            .as_ref()
            .map(|revision| revision.payload.threads.as_slice())
            .unwrap_or_default(),
        repository_root,
        control,
    )?;
    let thread_count = snapshot.threads.len();
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    ensure_codex_closed()?;
    let checkout = checkout_local_snapshot_with_tracking_control(
        manifest,
        codex_home,
        repository_root,
        true,
        CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: Some(record.generation),
            integrated_head: record.integrated_head.clone(),
            activate_namespace: true,
        },
        control,
    )?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Remapped,
        namespace_id,
        previous_head: record.integrated_head.clone(),
        head: record.integrated_head.clone(),
        revision_id: record.integrated_head,
        uploaded_objects: 0,
        downloaded_objects: 0,
        thread_count,
        conflicts: Vec::new(),
        checkout: Some(checkout),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub kind: SyncOutcomeKind,
    pub namespace_id: Uuid,
    pub previous_head: Option<String>,
    pub head: Option<String>,
    pub revision_id: Option<String>,
    pub uploaded_objects: usize,
    pub downloaded_objects: usize,
    pub thread_count: usize,
    pub conflicts: Vec<ThreadConflict>,
    pub checkout: Option<CheckoutReport>,
}

pub fn push_namespace(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    client.info()?;
    let tracking = TrackingStore::open(repository_root)?;
    let active = tracking.active(codex_home)?;
    if let Some(active) = &active
        && (active.remote_id != remote_id || active.namespace_id != namespace_id)
    {
        bail!("the selected namespace is not active; switch namespaces before pushing");
    }
    let record = tracking.load(codex_home, remote_id, namespace_id)?;
    let integrated_head = record
        .as_ref()
        .and_then(|record| record.integrated_head.clone());
    let remote_head = client.namespace_head(namespace_id)?;
    let summary = create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
    if summary.warning_count > 0 {
        bail!(
            "push is blocked because the local snapshot contains {} warning(s)",
            summary.warning_count
        );
    }
    let snapshot = workspace_mapper.canonicalize_snapshot_objects(
        &load_local_snapshot(&summary.manifest_path)?,
        repository_root,
        control,
    )?;
    let revision = snapshot_to_revision(&snapshot, namespace_id, remote_head.clone())?;
    if let Some(head) = remote_head.as_deref() {
        let current = client.revision(head)?;
        validate_revision_namespace(&current, namespace_id)?;
        if thread_states_equal(&current.payload.threads, &revision.payload.threads)?
            && current.payload.warning_count == revision.payload.warning_count
        {
            let reconciled_head = if integrated_head.as_deref() == Some(head) && active.is_some() {
                remote_head.clone()
            } else {
                tracking
                    .reconcile_checkout(
                        codex_home,
                        remote_id,
                        namespace_id,
                        record.as_ref().map(|record| record.generation),
                        Some(head),
                        true,
                    )?
                    .integrated_head
            };
            return Ok(SyncReport {
                kind: SyncOutcomeKind::NoChanges,
                namespace_id,
                previous_head: remote_head.clone(),
                head: reconciled_head,
                revision_id: Some(current.revision_id),
                uploaded_objects: 0,
                downloaded_objects: 0,
                thread_count: snapshot.threads.len(),
                conflicts: Vec::new(),
                checkout: None,
            });
        }
    }
    if remote_head != integrated_head {
        bail!(
            "remote namespace has advanced; pull before pushing (tracked {:?}, remote {:?})",
            integrated_head,
            remote_head
        );
    }

    let descriptors = collect_object_descriptors(&revision.payload.threads)?;
    let missing = client
        .missing_objects(descriptors.clone())?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let descriptor_map = descriptors
        .iter()
        .map(|descriptor| (descriptor.sha256.as_str(), descriptor))
        .collect::<BTreeMap<_, _>>();
    if missing
        .iter()
        .any(|sha256| !descriptor_map.contains_key(sha256.as_str()))
    {
        bail!("server requested an object that is not part of the revision");
    }
    let mut uploaded = 0;
    for (index, sha256) in missing.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "push_objects".to_string(),
            message: sha256.clone(),
            completed: index as u64,
            total: Some(missing.len() as u64),
            unit: "objects".to_string(),
            cancellable: true,
        });
        let descriptor = descriptor_map[sha256.as_str()];
        let path = repository_object_path(repository_root, sha256)?;
        if client.upload_object(descriptor, &path, control)? {
            uploaded += 1;
        }
    }
    control.check_cancelled()?;
    let request = CommitRevisionRequest {
        expected_head: remote_head.clone(),
        revision: revision.clone(),
    };
    control.report(OperationProgress {
        phase: "push_commit".to_string(),
        message: "Committing remote revision".to_string(),
        completed: 0,
        total: None,
        unit: "steps".to_string(),
        cancellable: false,
    });
    let commit = match client.commit(namespace_id, &request) {
        Ok(commit) => commit,
        Err(error) => {
            if client.namespace_head(namespace_id)?.as_deref()
                == Some(revision.revision_id.as_str())
            {
                sync_core::CommitRevisionResponse {
                    namespace_id,
                    head: revision.revision_id.clone(),
                    created: true,
                }
            } else {
                return Err(error);
            }
        }
    };
    if commit.namespace_id != namespace_id || commit.head != revision.revision_id {
        bail!("server committed an unexpected revision");
    }
    let updated = tracking.reconcile_checkout(
        codex_home,
        remote_id,
        namespace_id,
        record.as_ref().map(|record| record.generation),
        Some(&commit.head),
        true,
    )?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Pushed,
        namespace_id,
        previous_head: remote_head,
        head: updated.integrated_head,
        revision_id: Some(revision.revision_id),
        uploaded_objects: uploaded,
        downloaded_objects: 0,
        thread_count: snapshot.threads.len(),
        conflicts: Vec::new(),
        checkout: None,
    })
}

pub fn pull_namespace(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    let prepared = match prepare_pull(remote_id, namespace_id, client, context)? {
        PreparedPull::NoChanges { head } => {
            return Ok(SyncReport {
                kind: SyncOutcomeKind::NoChanges,
                namespace_id,
                previous_head: head.clone(),
                head: head.clone(),
                revision_id: head,
                uploaded_objects: 0,
                downloaded_objects: 0,
                thread_count: 0,
                conflicts: Vec::new(),
                checkout: None,
            });
        }
        PreparedPull::Merge(prepared) => *prepared,
    };
    if !prepared.merged.conflicts.is_empty() {
        return Ok(SyncReport {
            kind: SyncOutcomeKind::Conflict,
            namespace_id,
            previous_head: prepared.previous_head,
            head: Some(prepared.remote_head.clone()),
            revision_id: Some(prepared.remote_head),
            uploaded_objects: 0,
            downloaded_objects: prepared.downloaded,
            thread_count: prepared.merged.threads.len(),
            conflicts: prepared.merged.conflicts,
            checkout: None,
        });
    }
    let thread_count = prepared.merged.threads.len();
    let checkout = apply_prepared_pull(
        CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: Some(prepared.record.generation),
            integrated_head: Some(prepared.remote_head.clone()),
            activate_namespace: true,
        },
        prepared.merged.threads,
        context,
    )?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Pulled,
        namespace_id,
        previous_head: prepared.previous_head,
        head: Some(prepared.remote_head.clone()),
        revision_id: Some(prepared.remote_head),
        uploaded_objects: 0,
        downloaded_objects: prepared.downloaded,
        thread_count,
        conflicts: Vec::new(),
        checkout: Some(checkout),
    })
}

pub fn resolve_pull_conflicts(
    remote_id: Uuid,
    namespace_id: Uuid,
    resolutions: &[ThreadConflictResolution],
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    let prepared = match prepare_pull(remote_id, namespace_id, client, context)? {
        PreparedPull::NoChanges { .. } => {
            bail!("there are no remote changes to resolve; pull again")
        }
        PreparedPull::Merge(prepared) => *prepared,
    };
    if prepared.merged.conflicts.is_empty() {
        bail!("the conflict set changed and now merges automatically; pull again");
    }
    control.report(OperationProgress {
        phase: "resolve_conflicts".to_string(),
        message: "Validating explicit conflict choices".to_string(),
        completed: 0,
        total: Some(prepared.merged.conflicts.len() as u64),
        unit: "conflicts".to_string(),
        cancellable: true,
    });
    let resolved_threads = resolve_thread_sets(
        &prepared.base,
        &prepared.local.threads,
        &prepared.remote_threads,
        resolutions,
    )?;
    let thread_count = resolved_threads.len();
    let previous_head = prepared.previous_head.clone();
    let downloaded = prepared.downloaded;
    let checkout = apply_prepared_pull(
        CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: Some(prepared.record.generation),
            integrated_head: Some(prepared.remote_head.clone()),
            activate_namespace: true,
        },
        resolved_threads,
        context,
    )?;

    // Applying first preserves the user's explicit merge locally if the remote Head changes
    // before the following CAS push. Tracking still points at the integrated remote parent, so
    // the ordinary pull path can safely re-plan instead of overwriting either side.
    let publish_control = control.non_cancellable();
    publish_control.report(OperationProgress {
        phase: "resolve_publish".to_string(),
        message: "Publishing the resolved revision".to_string(),
        completed: 0,
        total: None,
        unit: "steps".to_string(),
        cancellable: false,
    });
    let pushed = push_namespace(
        remote_id,
        namespace_id,
        client,
        LocalSyncContext::new(
            codex_home,
            repository_root,
            workspace_mapper,
            &publish_control,
        ),
    )
    .context("conflicts were safely applied locally, but publishing failed; use Push to retry")?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Merged,
        namespace_id,
        previous_head,
        head: pushed.head,
        revision_id: pushed.revision_id,
        uploaded_objects: pushed.uploaded_objects,
        downloaded_objects: downloaded + pushed.downloaded_objects,
        thread_count,
        conflicts: Vec::new(),
        checkout: Some(checkout),
    })
}

enum PreparedPull {
    NoChanges { head: Option<String> },
    Merge(Box<PullMergePreparation>),
}

struct PullMergePreparation {
    record: TrackingRecord,
    previous_head: Option<String>,
    remote_head: String,
    base: Vec<ThreadBundle>,
    local: LocalSnapshot,
    remote_threads: Vec<ThreadBundle>,
    merged: ThreadMergeOutcome,
    downloaded: usize,
}

fn prepare_pull(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<PreparedPull> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    client.info()?;
    let tracking = TrackingStore::open(repository_root)?;
    let active = tracking
        .active(codex_home)?
        .context("no namespace is active for this Codex home")?;
    if active.remote_id != remote_id || active.namespace_id != namespace_id {
        bail!("the selected namespace is not active; use namespace switch instead");
    }
    let record = tracking
        .load(codex_home, remote_id, namespace_id)?
        .context("active namespace has no tracking record")?;
    let previous_head = record.integrated_head.clone();
    let remote_head = client.namespace_head(namespace_id)?;
    if remote_head == previous_head {
        return Ok(PreparedPull::NoChanges {
            head: previous_head,
        });
    }
    let remote_head = remote_head.context("remote namespace has no revision to pull")?;
    if let Some(previous) = previous_head.as_deref() {
        ensure_ancestor(client, namespace_id, &remote_head, previous)?;
    }
    let remote_revision = client.revision(&remote_head)?;
    validate_revision_namespace(&remote_revision, namespace_id)?;
    if remote_revision.payload.warning_count > 0 {
        bail!("remote revision is incomplete and cannot be checked out safely");
    }
    let downloaded = download_revision_objects(
        client,
        &remote_revision.payload.threads,
        repository_root,
        control,
    )?;
    let local_summary =
        create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
    if local_summary.warning_count > 0 {
        bail!("pull is blocked because the local scan contains warnings");
    }
    let local = workspace_mapper.canonicalize_snapshot_objects(
        &load_local_snapshot(local_summary.manifest_path)?,
        repository_root,
        control,
    )?;
    let base = match previous_head.as_deref() {
        Some(head) => {
            let revision = client.revision(head)?;
            validate_revision_namespace(&revision, namespace_id)?;
            revision.payload.threads
        }
        None => Vec::new(),
    };
    let remote_threads = remote_revision.payload.threads;
    let merged = merge_thread_sets(&base, &local.threads, &remote_threads)?;
    Ok(PreparedPull::Merge(Box::new(PullMergePreparation {
        record,
        previous_head,
        remote_head,
        base,
        local,
        remote_threads,
        merged,
        downloaded,
    })))
}

fn apply_prepared_pull(
    tracking_update: CheckoutTrackingUpdate,
    threads: Vec<ThreadBundle>,
    context: LocalSyncContext<'_>,
) -> Result<CheckoutReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    let snapshot = workspace_mapper.materialize_snapshot_objects(
        &LocalSnapshot {
            schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: Uuid::now_v7().to_string(),
            created_at: Utc::now().to_rfc3339(),
            threads,
            warning_count: 0,
        },
        repository_root,
        control,
    )?;
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    ensure_codex_closed()?;
    checkout_local_snapshot_with_tracking_control(
        manifest,
        codex_home,
        repository_root,
        true,
        tracking_update,
        control,
    )
}

pub fn switch_namespace(
    remote_id: Uuid,
    namespace_id: Uuid,
    target_client: &RemoteClient,
    current_client: Option<&RemoteClient>,
    current_workspace_mapper: Option<&WorkspacePathMapper>,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper: target_workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    target_client.info()?;
    let tracking = TrackingStore::open(repository_root)?;
    if let Some(active) = tracking.active(codex_home)? {
        if active.remote_id == remote_id && active.namespace_id == namespace_id {
            return pull_namespace(remote_id, namespace_id, target_client, context);
        }
        let current_client = current_client.context(
            "the active namespace uses a different remote, but its client is unavailable",
        )?;
        ensure_active_namespace_clean(
            &tracking,
            &active,
            current_client,
            codex_home,
            repository_root,
            current_workspace_mapper.unwrap_or(target_workspace_mapper),
            control,
        )?;
    }

    let previous = tracking.load(codex_home, remote_id, namespace_id)?;
    let target_head = target_client.namespace_head(namespace_id)?;
    let (snapshot, downloaded) = match target_head.as_deref() {
        Some(head) => {
            let revision = target_client.revision(head)?;
            validate_revision_namespace(&revision, namespace_id)?;
            if revision.payload.warning_count > 0 {
                bail!("target namespace revision is incomplete and cannot be checked out");
            }
            let downloaded = download_revision_objects(
                target_client,
                &revision.payload.threads,
                repository_root,
                control,
            )?;
            (revision_to_snapshot(&revision)?, downloaded)
        }
        None => (
            LocalSnapshot {
                schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
                snapshot_id: Uuid::now_v7().to_string(),
                created_at: Utc::now().to_rfc3339(),
                threads: Vec::new(),
                warning_count: 0,
            },
            0,
        ),
    };
    let snapshot = target_workspace_mapper.materialize_snapshot_objects(
        &snapshot,
        repository_root,
        control,
    )?;
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    ensure_codex_closed()?;
    let checkout = checkout_local_snapshot_with_tracking_control(
        manifest,
        codex_home,
        repository_root,
        true,
        CheckoutTrackingUpdate {
            remote_id,
            namespace_id,
            expected_generation: previous.as_ref().map(|record| record.generation),
            integrated_head: target_head.clone(),
            activate_namespace: true,
        },
        control,
    )?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Switched,
        namespace_id,
        previous_head: previous.and_then(|record| record.integrated_head),
        head: target_head.clone(),
        revision_id: target_head,
        uploaded_objects: 0,
        downloaded_objects: downloaded,
        thread_count: snapshot.threads.len(),
        conflicts: Vec::new(),
        checkout: Some(checkout),
    })
}

fn ensure_active_namespace_clean(
    tracking: &TrackingStore,
    active: &sync_core::ActiveNamespaceBinding,
    client: &RemoteClient,
    codex_home: &Path,
    repository_root: &Path,
    workspace_mapper: &WorkspacePathMapper,
    control: &OperationControl,
) -> Result<()> {
    client.info()?;
    let record = tracking
        .load(codex_home, active.remote_id, active.namespace_id)?
        .context("active namespace has no tracking record")?;
    let local_summary =
        create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
    if local_summary.warning_count > 0 {
        bail!("namespace switch is blocked because the local scan contains warnings");
    }
    let local = workspace_mapper.canonicalize_snapshot_objects(
        &load_local_snapshot(local_summary.manifest_path)?,
        repository_root,
        control,
    )?;
    let base = match record.integrated_head.as_deref() {
        Some(head) => {
            let revision = client.revision(head)?;
            validate_revision_namespace(&revision, active.namespace_id)?;
            revision.payload.threads
        }
        None => Vec::new(),
    };
    if !thread_states_equal(&local.threads, &base)? {
        bail!("the active namespace has unpushed local changes; push before switching");
    }
    Ok(())
}

fn download_revision_objects(
    client: &RemoteClient,
    threads: &[ThreadBundle],
    repository_root: &Path,
    control: &OperationControl,
) -> Result<usize> {
    let descriptors = collect_object_descriptors(threads)?;
    let mut downloaded = 0;
    for (index, descriptor) in descriptors.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "pull_objects".to_string(),
            message: descriptor.sha256.clone(),
            completed: index as u64,
            total: Some(descriptors.len() as u64),
            unit: "objects".to_string(),
            cancellable: true,
        });
        let path = repository_object_path(repository_root, &descriptor.sha256)?;
        if path.exists() {
            validate_repository_object(repository_root, descriptor)?;
            continue;
        }
        let response = client.download_object(descriptor)?;
        if install_repository_object(repository_root, descriptor, response, control)? {
            downloaded += 1;
        }
    }
    Ok(downloaded)
}

fn ensure_ancestor(
    client: &RemoteClient,
    namespace_id: Uuid,
    descendant: &str,
    ancestor: &str,
) -> Result<()> {
    let mut current = descendant.to_string();
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current == ancestor {
            return Ok(());
        }
        let revision = client.revision(&current)?;
        validate_revision_namespace(&revision, namespace_id)?;
        let Some(parent) = revision.payload.parent_revision else {
            bail!("remote revision history does not contain the locally tracked head");
        };
        current = parent;
    }
    bail!("remote revision ancestry exceeds the safety limit")
}

fn validate_revision_namespace(
    revision: &sync_core::RevisionManifest,
    namespace_id: Uuid,
) -> Result<()> {
    revision.validate()?;
    if revision.payload.namespace_id != namespace_id {
        bail!("server returned a revision for the wrong namespace");
    }
    Ok(())
}

fn thread_states_equal(left: &[ThreadBundle], right: &[ThreadBundle]) -> Result<bool> {
    Ok(thread_state(left)? == thread_state(right)?)
}

fn thread_state(threads: &[ThreadBundle]) -> Result<BTreeMap<String, String>> {
    let mut state = BTreeMap::new();
    for thread in threads {
        if state
            .insert(
                thread.thread_id.clone(),
                semantic_thread_hash(&remote_thread_view(thread))?,
            )
            .is_some()
        {
            bail!("duplicate thread ID {}", thread.thread_id);
        }
    }
    Ok(state)
}

#[cfg(not(test))]
fn ensure_codex_closed() -> Result<()> {
    let processes = detect_codex_processes();
    if processes.is_empty() {
        return Ok(());
    }
    let details = processes
        .iter()
        .map(|process| format!("{} (PID {})", process.name, process.pid))
        .collect::<Vec<_>>()
        .join(", ");
    bail!("Codex is still running: {details}")
}

#[cfg(test)]
fn ensure_codex_closed() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::{BufRead, BufReader, Write};

    use axum::Router;
    use rusqlite::{Connection, params};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::remote::SecretToken;

    #[test]
    fn thread_state_ignores_machine_local_database_paths() {
        let mut left = test_thread("one");
        let mut right = left.clone();
        left.related_records.tables.get_mut("threads").unwrap()[0]["rollout_path"] =
            serde_json::json!("C:/first/rollout.jsonl");
        right.related_records.tables.get_mut("threads").unwrap()[0]["rollout_path"] =
            serde_json::json!("/home/second/rollout.jsonl");
        assert!(thread_states_equal(&[left], &[right]).unwrap());
    }

    #[test]
    fn retry_push_reconciles_missing_or_stale_tracking_when_remote_content_matches() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = tempdir().unwrap();
        let token = "test-token-that-is-long-enough-for-auth";
        let config = sync_server::ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            data_dir: temp.path().join("server"),
            token: token.to_string(),
            max_object_bytes: 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
        };
        let (address, task) = runtime.block_on(async {
            let state = sync_server::AppState::initialize(&config).await.unwrap();
            let app: Router = sync_server::build_router(state, &config);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await });
            (address, task)
        });
        let client = RemoteClient::new(
            &format!("http://{address}"),
            SecretToken::new(token.to_string()).unwrap(),
        )
        .unwrap();
        let namespace = client.create_namespace("Personal".to_string()).unwrap();
        let remote_id = Uuid::now_v7();
        let home_a = temp.path().join("home-a");
        let home_b = temp.path().join("home-b");
        let repository_a = temp.path().join("repository-a");
        let repository_b = temp.path().join("repository-b");
        let workspace_mapper = WorkspacePathMapper::default();
        initialize_home(&home_a);
        initialize_home(&home_b);
        insert_fixture_thread(&home_a, "shared-thread");
        insert_fixture_thread(&home_b, "shared-thread");

        let pushed_a = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_a,
                &repository_a,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(pushed_a.kind, SyncOutcomeKind::Pushed);
        let first_head = pushed_a.head.unwrap();

        let tracking_b = TrackingStore::open(&repository_b).unwrap();
        assert!(
            tracking_b
                .load(&home_b, remote_id, namespace.id)
                .unwrap()
                .is_none()
        );
        assert!(tracking_b.active(&home_b).unwrap().is_none());

        let retried_b = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(retried_b.kind, SyncOutcomeKind::NoChanges);
        assert_eq!(retried_b.head.as_deref(), Some(first_head.as_str()));
        let first_record = tracking_b
            .load(&home_b, remote_id, namespace.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            first_record.integrated_head.as_deref(),
            Some(first_head.as_str())
        );
        let active = tracking_b.active(&home_b).unwrap().unwrap();
        assert_eq!(
            (active.remote_id, active.namespace_id),
            (remote_id, namespace.id)
        );

        let repeated_b = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(repeated_b.kind, SyncOutcomeKind::NoChanges);
        assert_eq!(
            tracking_b
                .load(&home_b, remote_id, namespace.id)
                .unwrap()
                .unwrap()
                .generation,
            first_record.generation
        );

        let current = client.revision(&first_head).unwrap();
        let mut advanced_payload = current.payload;
        advanced_payload.parent_revision = Some(first_head.clone());
        advanced_payload.created_at = "2100-01-01T00:00:00Z".to_string();
        let advanced_revision =
            sync_core::RevisionManifest::from_payload(advanced_payload).unwrap();
        let advanced_head = advanced_revision.revision_id.clone();
        assert_ne!(advanced_head, first_head);
        client
            .commit(
                namespace.id,
                &CommitRevisionRequest {
                    expected_head: Some(first_head),
                    revision: advanced_revision,
                },
            )
            .unwrap();
        Connection::open(tracking_b.path())
            .unwrap()
            .execute("DELETE FROM active_namespace", [])
            .unwrap();
        assert!(tracking_b.active(&home_b).unwrap().is_none());

        let retried_stale = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(retried_stale.kind, SyncOutcomeKind::NoChanges);
        assert_eq!(retried_stale.head.as_deref(), Some(advanced_head.as_str()));
        let reconciled = tracking_b
            .load(&home_b, remote_id, namespace.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            reconciled.integrated_head.as_deref(),
            Some(advanced_head.as_str())
        );
        assert_eq!(reconciled.generation, first_record.generation + 1);
        let active = tracking_b.active(&home_b).unwrap().unwrap();
        assert_eq!(
            (active.remote_id, active.namespace_id),
            (remote_id, namespace.id)
        );

        task.abort();
        runtime.block_on(async {
            let _ = task.await;
        });
    }

    #[test]
    fn two_homes_push_switch_push_and_pull_to_the_same_thread_union() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = tempdir().unwrap();
        let token = "test-token-that-is-long-enough-for-auth";
        let config = sync_server::ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            data_dir: temp.path().join("server"),
            token: token.to_string(),
            max_object_bytes: 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
        };
        let (address, task) = runtime.block_on(async {
            let state = sync_server::AppState::initialize(&config).await.unwrap();
            let app: Router = sync_server::build_router(state, &config);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await });
            (address, task)
        });
        let client = RemoteClient::new(
            &format!("http://{address}"),
            SecretToken::new(token.to_string()).unwrap(),
        )
        .unwrap();
        let namespace = client.create_namespace("Personal".to_string()).unwrap();
        let remote_id = Uuid::now_v7();
        let home_a = temp.path().join("home-a");
        let home_b = temp.path().join("home-b");
        let repository_a = temp.path().join("repository-a");
        let repository_b = temp.path().join("repository-b");
        let workspace_mapper_a = WorkspacePathMapper::default();
        let workspace_mapper_b = WorkspacePathMapper::new(vec![sync_core::WorkspacePathMapping {
            remote_prefix: "C:/work".to_string(),
            local_prefix: "F:/workspace".to_string(),
        }])
        .unwrap();
        initialize_home(&home_a);
        initialize_home(&home_b);
        insert_fixture_thread(&home_a, "thread-a");

        let pushed_a = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_a,
                &repository_a,
                &workspace_mapper_a,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(pushed_a.kind, SyncOutcomeKind::Pushed);
        let pushed_a_head = pushed_a.head.as_deref().unwrap();
        let pushed_a_revision = client.revision(pushed_a_head).unwrap();
        let thread_a_remote_hash = pushed_a_revision
            .payload
            .threads
            .iter()
            .find(|thread| thread.thread_id == "thread-a")
            .unwrap()
            .rollout
            .sha256
            .clone();
        assert_eq!(object_cwd(&repository_a, &thread_a_remote_hash), "C:/work");

        let switched_b = switch_namespace(
            remote_id,
            namespace.id,
            &client,
            None,
            None,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper_a,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(switched_b.kind, SyncOutcomeKind::Switched);
        assert_eq!(
            sync_core::scan_codex_home(&home_b).unwrap().threads.len(),
            1
        );
        assert_eq!(
            sync_core::scan_codex_home(&home_b).unwrap().threads[0]
                .workspace
                .source_path
                .as_deref(),
            Some("C:/work")
        );
        insert_fixture_thread_at(&home_b, "thread-b", "C:/work/new");

        let remapped_b = reapply_workspace_mappings(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper_b,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(remapped_b.kind, SyncOutcomeKind::Remapped);
        let remapped_paths = sync_core::scan_codex_home(&home_b)
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| (thread.thread_id, thread.workspace.source_path.unwrap()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(remapped_paths["thread-a"], "F:/workspace");
        assert_eq!(remapped_paths["thread-b"], "F:/workspace/new");
        assert_eq!(rollout_cwd(&home_b, "thread-a"), "F:/workspace");
        assert_eq!(rollout_cwd(&home_b, "thread-b"), "F:/workspace/new");

        let pushed_b = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper_b,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(pushed_b.kind, SyncOutcomeKind::Pushed);
        let pushed_b_revision = client.revision(pushed_b.head.as_deref().unwrap()).unwrap();
        let pushed_b_threads = pushed_b_revision
            .payload
            .threads
            .iter()
            .map(|thread| (thread.thread_id.as_str(), &thread.rollout.sha256))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            pushed_b_threads.get("thread-a").map(|hash| hash.as_str()),
            Some(thread_a_remote_hash.as_str())
        );
        assert_eq!(
            object_cwd(&repository_b, pushed_b_threads["thread-a"]),
            "C:/work"
        );
        assert_eq!(
            object_cwd(&repository_b, pushed_b_threads["thread-b"]),
            "C:/work/new"
        );

        let pulled_a = pull_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_a,
                &repository_a,
                &workspace_mapper_a,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(pulled_a.kind, SyncOutcomeKind::Pulled);
        let threads = sync_core::scan_codex_home(&home_a)
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.thread_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            threads,
            BTreeSet::from(["thread-a".to_string(), "thread-b".to_string()])
        );

        task.abort();
        runtime.block_on(async {
            let _ = task.await;
        });
    }

    #[test]
    fn divergent_same_thread_is_resolved_explicitly_then_pushed_and_pulled() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = tempdir().unwrap();
        let token = "test-token-that-is-long-enough-for-auth";
        let config = sync_server::ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            data_dir: temp.path().join("server"),
            token: token.to_string(),
            max_object_bytes: 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
        };
        let (address, task) = runtime.block_on(async {
            let state = sync_server::AppState::initialize(&config).await.unwrap();
            let app: Router = sync_server::build_router(state, &config);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await });
            (address, task)
        });
        let client = RemoteClient::new(
            &format!("http://{address}"),
            SecretToken::new(token.to_string()).unwrap(),
        )
        .unwrap();
        let namespace = client.create_namespace("Personal".to_string()).unwrap();
        let remote_id = Uuid::now_v7();
        let home_a = temp.path().join("home-a");
        let home_b = temp.path().join("home-b");
        let repository_a = temp.path().join("repository-a");
        let repository_b = temp.path().join("repository-b");
        let workspace_mapper = WorkspacePathMapper::default();
        initialize_home(&home_a);
        initialize_home(&home_b);
        insert_fixture_thread(&home_a, "shared-thread");

        push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_a,
                &repository_a,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        switch_namespace(
            remote_id,
            namespace.id,
            &client,
            None,
            None,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();

        modify_fixture_thread(&home_a, "shared-thread", "来自 A", "event-a");
        let pushed_a = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_a,
                &repository_a,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        modify_fixture_thread(&home_b, "shared-thread", "来自 B", "event-b");

        let conflicted = pull_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(conflicted.kind, SyncOutcomeKind::Conflict);
        assert_eq!(conflicted.conflicts.len(), 1);
        let conflict = &conflicted.conflicts[0];
        assert_eq!(conflict.local.as_ref().unwrap().title, "来自 B");
        assert_eq!(conflict.remote.as_ref().unwrap().title, "来自 A");
        let resolutions = vec![ThreadConflictResolution {
            conflict_id: conflict.conflict_id.clone(),
            thread_id: conflict.thread_id.clone(),
            choice: sync_core::ThreadResolutionChoice::Local,
        }];

        let resolved = resolve_pull_conflicts(
            remote_id,
            namespace.id,
            &resolutions,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(resolved.kind, SyncOutcomeKind::Merged);
        assert!(resolved.checkout.is_some());
        assert_ne!(resolved.head, pushed_a.head);
        assert_eq!(
            sync_core::scan_codex_home(&home_b).unwrap().threads[0].title,
            "来自 B"
        );

        let pulled_a = pull_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_a,
                &repository_a,
                &workspace_mapper,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(pulled_a.kind, SyncOutcomeKind::Pulled);
        assert_eq!(
            sync_core::scan_codex_home(&home_a).unwrap().threads[0].title,
            "来自 B"
        );

        task.abort();
        runtime.block_on(async {
            let _ = task.await;
        });
    }

    fn initialize_home(home: &Path) {
        fs::create_dir_all(home.join("sessions/2026/07/26")).unwrap();
        fs::create_dir_all(home.join("archived_sessions")).unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
    }

    fn insert_fixture_thread(home: &Path, thread_id: &str) {
        insert_fixture_thread_at(home, thread_id, "C:/work");
    }

    fn insert_fixture_thread_at(home: &Path, thread_id: &str, cwd: &str) {
        let rollout = home
            .join("sessions/2026/07/26")
            .join(format!("rollout-{thread_id}.jsonl"));
        let mut file = File::create(&rollout).unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "type": "session_meta",
                "payload": {"id": thread_id, "cwd": cwd, "model_provider": "openai"}
            })
        )
        .unwrap();
        writeln!(file, "{}", json!({"type": "event", "thread": thread_id})).unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at, source, model_provider,
                    cwd, title, sandbox_policy, approval_mode, archived
                 ) VALUES (?1, ?2, 1, 1, 'cli', 'openai', ?3, ?1, '{}', 'never', 0)",
                params![thread_id, rollout.to_string_lossy().as_ref(), cwd],
            )
            .unwrap();
    }

    fn modify_fixture_thread(home: &Path, thread_id: &str, title: &str, event: &str) {
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        let rollout_path: String = connection
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "UPDATE threads SET title = ?2, updated_at = updated_at + 1 WHERE id = ?1",
                params![thread_id, title],
            )
            .unwrap();
        let mut rollout = File::options().append(true).open(rollout_path).unwrap();
        writeln!(
            rollout,
            "{}",
            json!({"type": "event", "thread": thread_id, "event": event})
        )
        .unwrap();
    }

    fn rollout_cwd(home: &Path, thread_id: &str) -> String {
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        let rollout_path: String = connection
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .unwrap();
        first_record_cwd(File::open(rollout_path).unwrap())
    }

    fn object_cwd(repository: &Path, sha256: &str) -> String {
        let path = repository_object_path(repository, sha256).unwrap();
        first_record_cwd(File::open(path).unwrap())
    }

    fn first_record_cwd(file: File) -> String {
        let mut first_line = String::new();
        BufReader::new(file).read_line(&mut first_line).unwrap();
        serde_json::from_str::<serde_json::Value>(&first_line).unwrap()["payload"]["cwd"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn test_thread(id: &str) -> ThreadBundle {
        ThreadBundle {
            schema_version: sync_core::THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: id.to_string(),
            title: id.to_string(),
            archived: false,
            created_at_ms: None,
            updated_at_ms: None,
            model_provider: Some("openai".to_string()),
            workspace: sync_core::WorkspaceRef::default(),
            rollout: sync_core::ContentObject {
                sha256: format!("sha256:{}", "a".repeat(64)),
                byte_length: 1,
                media_type: "application/x-ndjson".to_string(),
                logical_path: Some(format!("sessions/rollout-{id}.jsonl")),
                source_path: None,
            },
            related_records: sync_core::RelatedRecords {
                source_database: None,
                tables: BTreeMap::from([(
                    "threads".to_string(),
                    vec![serde_json::json!({"id": id})],
                )]),
            },
            attachments: Vec::new(),
        }
    }
}
