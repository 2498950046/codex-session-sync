use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sync_core::repository_object_path;
use sync_core::{
    ChangeKind, CheckoutReport, CheckoutTrackingUpdate, FilesystemContentStoreV4, LocalSnapshot,
    OperationControl, OperationProgress, RevisionCommitRequestV4, SnapshotMetadata, SnapshotScope,
    StagedChangeSet, StorageObjectKindV4, ThreadBundle, ThreadConflict, ThreadConflictResolution,
    ThreadMergeOutcome, TrackingRecord, TrackingStore, WorkspacePathMapper, build_change_plan,
    canonicalize_snapshot_provider_metadata,
    checkout_local_snapshot_with_tracking_and_projects_control, compose_staged_thread_refs,
    create_local_snapshot_with_control, detect_configured_provider, load_local_snapshot,
    materialize_snapshot_provider_metadata, merge_thread_sets,
    prepare_workspace_snapshot_with_control, remote_thread_view, resolve_thread_sets,
    semantic_thread_hash, store_local_snapshot, write_v4_snapshot,
};
use uuid::Uuid;

#[cfg(not(test))]
use sync_core::detect_codex_processes;

use crate::remote::RemoteClient;

const MAX_ANCESTRY_DEPTH: usize = 10_000;
const DEFAULT_UPLOAD_CONCURRENCY: usize = 4;
const UPLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const UPLOAD_SPEED_WINDOW: Duration = Duration::from_secs(5);

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

/// Compact, UI-safe projection of an exact staging plan.  The full local and
/// baseline bundles never cross IPC; the plan is regenerated and revalidated
/// immediately before a selected Push.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StagingPlanReport {
    pub base_revision_id: Option<String>,
    pub candidates: Vec<StagingCandidatePreview>,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StagingCandidatePreview {
    pub thread_id: String,
    pub kind: ChangeKind,
    pub archived: bool,
    pub title: String,
    pub workspace: sync_core::WorkspaceRef,
    pub model_provider: Option<String>,
    pub byte_length: u64,
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

pub fn download_remote_revision_as_snapshot(
    namespace_id: Uuid,
    revision_id: &str,
    client: &RemoteClient,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<sync_core::SnapshotSummary> {
    let (root, threads, _) =
        download_revision_graph(client, revision_id, repository_root, control)?;
    validate_revision_root_namespace(&root, namespace_id)?;
    let snapshot = LocalSnapshot {
        schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: Uuid::now_v7().to_string(),
        created_at: Utc::now().to_rfc3339(),
        threads,
        warning_count: root.warning_count,
    };
    let manifest_path = store_local_snapshot(&snapshot, repository_root)?;
    let item = sync_core::list_local_snapshots(repository_root)?
        .into_iter()
        .find(|item| item.snapshot_id == snapshot.snapshot_id)
        .context("downloaded snapshot was not indexed")?;
    sync_core::update_snapshot_metadata(
        repository_root,
        &snapshot.snapshot_id,
        sync_core::SnapshotMetadata {
            description: format!("Remote {}", &revision_id["sha256:".len()..][..12]),
            tags: vec!["remote".to_string()],
            pinned: false,
            automatic: false,
            ..SnapshotMetadata::default()
        },
    )?;
    Ok(sync_core::SnapshotSummary {
        snapshot_id: snapshot.snapshot_id,
        manifest_path,
        thread_count: item.thread_count,
        object_count: item.object_count,
        total_bytes: item.logical_bytes,
        warning_count: item.warning_count,
    })
}

pub fn restore_remote_revision_locally(
    remote_id: Uuid,
    namespace_id: Uuid,
    revision_id: &str,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<CheckoutReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    let current = client
        .namespace_head(namespace_id)?
        .context("remote namespace has no Head")?;
    let tracking = TrackingStore::open(repository_root)?;
    let record = tracking
        .load(codex_home, remote_id, namespace_id)?
        .context("namespace has no local tracking state")?;
    if record.integrated_head.as_deref() != Some(current.as_str()) {
        bail!("pull the current remote Head before restoring an older revision");
    }
    let (root, threads, _) =
        download_revision_graph(client, revision_id, repository_root, control)?;
    validate_revision_root_namespace(&root, namespace_id)?;
    if root.warning_count > 0 {
        bail!("incomplete remote revisions cannot be restored");
    }
    let snapshot = materialize_snapshot_provider_metadata(
        &LocalSnapshot {
            schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: Uuid::now_v7().to_string(),
            created_at: Utc::now().to_rfc3339(),
            threads,
            warning_count: 0,
        },
        &detect_configured_provider(codex_home)?,
    )?;
    let snapshot = workspace_mapper.materialize_snapshot(&snapshot);
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    sync_core::checkout_local_snapshot_with_control(
        manifest,
        codex_home,
        repository_root,
        true,
        control,
    )
}

pub fn restore_remote_revision_and_publish(
    remote_id: Uuid,
    namespace_id: Uuid,
    revision_id: &str,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    restore_remote_revision_locally(remote_id, namespace_id, revision_id, client, context)?;
    let publish = context.control.non_cancellable();
    push_namespace(
        remote_id,
        namespace_id,
        client,
        LocalSyncContext::new(
            context.codex_home,
            context.repository_root,
            context.workspace_mapper,
            &publish,
        ),
    )
    .context("the revision was restored locally, but publishing failed; use Push to retry")
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
    let reference_threads = match record.integrated_head.as_deref() {
        Some(head) => {
            let (root, threads, _) =
                download_revision_graph(client, head, repository_root, control)?;
            validate_revision_root_namespace(&root, namespace_id)?;
            threads
        }
        None => Vec::new(),
    };
    let local_summary =
        create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
    if local_summary.warning_count > 0 {
        bail!("workspace remap is blocked because the local scan contains warnings");
    }
    let local = load_local_snapshot(local_summary.manifest_path)?;
    let snapshot =
        workspace_mapper.materialize_snapshot_with_reference(&local, &reference_threads)?;
    let thread_count = snapshot.threads.len();
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    let project_roots = workspace_mapper.local_prefixes();
    ensure_codex_closed()?;
    let checkout = checkout_local_snapshot_with_tracking_and_projects_control(
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
        &project_roots,
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
        push_metrics: None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_metrics: Option<PushMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PushMetrics {
    pub missing_query_ms: u64,
    pub upload_ms: u64,
    pub commit_ms: u64,
    pub transferred_objects: usize,
    pub transferred_bytes: u64,
    pub created_objects: usize,
    pub max_concurrency: usize,
}

#[derive(Debug)]
struct UploadBatchReport {
    elapsed_ms: u64,
    transferred_objects: usize,
    transferred_bytes: u64,
    created_objects: usize,
    max_concurrency: usize,
}

struct UploadProgressAggregator {
    control: OperationControl,
    total_bytes: u64,
    total_objects: u64,
    transferred_bytes: AtomicU64,
    completed_objects: AtomicU64,
    state: Mutex<UploadProgressState>,
}

struct UploadProgressState {
    last_report: Instant,
    samples: VecDeque<(Instant, u64)>,
}

impl UploadProgressAggregator {
    fn new(control: OperationControl, total_bytes: u64, total_objects: usize) -> Self {
        let now = Instant::now();
        Self {
            control,
            total_bytes,
            total_objects: total_objects as u64,
            transferred_bytes: AtomicU64::new(0),
            completed_objects: AtomicU64::new(0),
            state: Mutex::new(UploadProgressState {
                last_report: now.checked_sub(UPLOAD_PROGRESS_INTERVAL).unwrap_or(now),
                samples: VecDeque::from([(now, 0)]),
            }),
        }
    }

    fn add_bytes(&self, count: u64) {
        self.transferred_bytes.fetch_add(count, Ordering::Relaxed);
        self.report(false);
    }

    fn complete_object(&self) {
        self.completed_objects.fetch_add(1, Ordering::Relaxed);
        self.report(false);
    }

    fn report(&self, force: bool) {
        let now = Instant::now();
        let transferred_bytes = self.transferred_bytes.load(Ordering::Relaxed);
        let completed_objects = self.completed_objects.load(Ordering::Relaxed);
        let mut state = self.state.lock().expect("upload progress lock poisoned");
        if !force && now.duration_since(state.last_report) < UPLOAD_PROGRESS_INTERVAL {
            return;
        }
        state.last_report = now;
        state.samples.push_back((now, transferred_bytes));
        while state.samples.len() > 2
            && now.duration_since(state.samples[0].0) > UPLOAD_SPEED_WINDOW
        {
            state.samples.pop_front();
        }
        let bytes_per_second = state.samples.front().and_then(|(started, initial_bytes)| {
            let elapsed = now.duration_since(*started).as_secs_f64();
            (elapsed >= 0.1).then(|| {
                ((transferred_bytes.saturating_sub(*initial_bytes)) as f64 / elapsed) as u64
            })
        });
        drop(state);

        let speed = bytes_per_second
            .filter(|speed| *speed > 0)
            .map(format_bytes_per_second)
            .unwrap_or_else(|| "正在计算速度".to_string());
        let eta = bytes_per_second
            .filter(|speed| *speed > 0)
            .map(|speed| {
                format_duration(
                    self.total_bytes
                        .saturating_sub(transferred_bytes)
                        .div_ceil(speed),
                )
            })
            .unwrap_or_else(|| "--".to_string());
        self.control.report(OperationProgress {
            phase: "push_typed_objects".to_string(),
            message: format!(
                "已传输 {}/{}，对象 {}/{}，{}，剩余约 {}",
                format_bytes(transferred_bytes),
                format_bytes(self.total_bytes),
                completed_objects,
                self.total_objects,
                speed,
                eta
            ),
            completed: transferred_bytes,
            total: Some(self.total_bytes),
            unit: "bytes".to_string(),
            cancellable: true,
        });
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_bytes_per_second(bytes: u64) -> String {
    format!("{}/s", format_bytes(bytes))
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} 秒")
    } else {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    }
}

fn upload_missing_v4_objects(
    client: &RemoteClient,
    store: &FilesystemContentStoreV4,
    objects: Vec<sync_core::StorageObjectRefV4>,
    control: &OperationControl,
) -> Result<UploadBatchReport> {
    let started = Instant::now();
    let transferred_objects = objects.len();
    let transferred_bytes = objects.iter().try_fold(0_u64, |total, object| {
        total
            .checked_add(object.byte_length)
            .context("missing object byte total overflow")
    })?;
    if objects.is_empty() {
        return Ok(UploadBatchReport {
            elapsed_ms: duration_ms(started.elapsed()),
            transferred_objects: 0,
            transferred_bytes: 0,
            created_objects: 0,
            max_concurrency: 0,
        });
    }
    let mut queued = VecDeque::with_capacity(transferred_objects);
    for object in objects {
        queued.push_back((store.object_path(&object)?, object));
    }
    let worker_count = DEFAULT_UPLOAD_CONCURRENCY.min(transferred_objects);
    let queue = Arc::new(Mutex::new(queued));
    let stop_scheduling = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(None::<anyhow::Error>));
    let created_objects = Arc::new(AtomicUsize::new(0));
    let progress = Arc::new(UploadProgressAggregator::new(
        control.clone(),
        transferred_bytes,
        transferred_objects,
    ));
    progress.report(true);

    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let client = client.clone();
            let control = control.clone();
            let queue = queue.clone();
            let stop_scheduling = stop_scheduling.clone();
            let first_error = first_error.clone();
            let created_objects = created_objects.clone();
            let progress = progress.clone();
            workers.push(scope.spawn(move || {
                loop {
                    if control.is_cancelled() || stop_scheduling.load(Ordering::Relaxed) {
                        break;
                    }
                    let next = queue
                        .lock()
                        .expect("upload queue lock poisoned")
                        .pop_front();
                    let Some((path, object)) = next else {
                        break;
                    };
                    if control.is_cancelled() || stop_scheduling.load(Ordering::Relaxed) {
                        break;
                    }
                    let byte_progress = progress.clone();
                    let on_bytes: Arc<dyn Fn(u64) + Send + Sync> =
                        Arc::new(move |count| byte_progress.add_bytes(count));
                    match client.upload_v4_object_with_progress(
                        &object,
                        &path,
                        &control,
                        stop_scheduling.clone(),
                        on_bytes,
                    ) {
                        Ok(created) => {
                            if created {
                                created_objects.fetch_add(1, Ordering::Relaxed);
                            }
                            progress.complete_object();
                        }
                        Err(error) => {
                            if !control.is_cancelled() {
                                let mut first =
                                    first_error.lock().expect("upload error lock poisoned");
                                if first.is_none() {
                                    *first = Some(error.context(format!(
                                        "failed to upload {} {}",
                                        object.kind.wire_name(),
                                        object.sha256
                                    )));
                                }
                            }
                            stop_scheduling.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }));
        }
        for worker in workers {
            if worker.join().is_err() {
                let mut first = first_error.lock().expect("upload error lock poisoned");
                if first.is_none() {
                    *first = Some(anyhow::anyhow!("parallel upload worker panicked"));
                }
                stop_scheduling.store(true, Ordering::Relaxed);
            }
        }
    });

    control.check_cancelled()?;
    if let Some(error) = first_error
        .lock()
        .expect("upload error lock poisoned")
        .take()
    {
        return Err(error);
    }
    progress.report(true);
    Ok(UploadBatchReport {
        elapsed_ms: duration_ms(started.elapsed()),
        transferred_objects,
        transferred_bytes,
        created_objects: created_objects.load(Ordering::Relaxed),
        max_concurrency: worker_count,
    })
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub fn prepare_staging_plan(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<StagingPlanReport> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    client.info()?;
    let tracking = TrackingStore::open(repository_root)?;
    let record = tracking.load(codex_home, remote_id, namespace_id)?;
    let base_revision_id = record
        .as_ref()
        .and_then(|record| record.integrated_head.clone());
    let base = match base_revision_id.as_deref() {
        Some(revision_id) => {
            let (root, descriptors, _) =
                download_revision_descriptors(client, revision_id, repository_root, control)?;
            validate_revision_root_namespace(&root, namespace_id)?;
            descriptors
        }
        None => Vec::new(),
    };
    let prepared = prepare_workspace_snapshot_with_control(codex_home, repository_root, control)?;
    let mut workspace = LocalSnapshot {
        schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: "staging-preview".to_string(),
        created_at: Utc::now().to_rfc3339(),
        threads: prepared.threads,
        warning_count: prepared.warning_count,
    };
    workspace = workspace_mapper.canonicalize_snapshot(&workspace);
    project_workspace_identities(&mut workspace);
    let workspace = canonicalize_snapshot_provider_metadata(&workspace);
    let plan = build_change_plan(base_revision_id.clone(), &base, &workspace.threads)?;
    let candidates =
        plan.candidates
            .into_iter()
            .map(|candidate| {
                let thread = candidate.local.as_ref().or(candidate.baseline.as_ref()).expect(
                "a change candidate always retains either local or baseline display metadata",
            );
                StagingCandidatePreview {
                    thread_id: candidate.thread_id,
                    kind: candidate.kind,
                    archived: candidate.archived.unwrap_or(thread.archived),
                    title: thread.title.clone(),
                    workspace: thread.workspace.clone(),
                    model_provider: thread.model_provider.clone(),
                    byte_length: thread.rollout.byte_length,
                }
            })
            .collect();
    Ok(StagingPlanReport {
        base_revision_id,
        candidates,
        warning_count: prepared.warning_count,
    })
}

/// Create a full remote Revision by applying only explicitly staged local
/// changes to the current remote Head.  Unselected refs remain remote-only,
/// so this path never reads or uploads their rollout content.
pub fn push_staged_namespace(
    remote_id: Uuid,
    namespace_id: Uuid,
    selected_thread_ids: BTreeSet<String>,
    expected_base_revision_id: Option<String>,
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
    if selected_thread_ids.is_empty() {
        bail!("select at least one local change before pushing");
    }
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
    if integrated_head != expected_base_revision_id {
        bail!("the staging plan is stale; regenerate it before pushing");
    }
    let remote_state = client.namespace_head_state(namespace_id)?;
    if let Some(record) = &record
        && record.remote_epoch != remote_state.namespace_epoch
    {
        bail!("remote history epoch changed; regenerate the staging plan");
    }
    if remote_state.head != integrated_head {
        bail!("remote namespace changed after staging; pull or resolve conflicts before pushing");
    }

    let base = match integrated_head.as_deref() {
        Some(revision_id) => {
            let (root, descriptors, _) =
                download_revision_descriptors(client, revision_id, repository_root, control)?;
            validate_revision_root_namespace(&root, namespace_id)?;
            descriptors
        }
        None => Vec::new(),
    };
    let prepared = prepare_workspace_snapshot_with_control(codex_home, repository_root, control)?;
    if prepared.warning_count > 0 {
        bail!(
            "selected push is blocked because the local workspace contains {} warning(s)",
            prepared.warning_count
        );
    }
    let mut workspace = LocalSnapshot {
        schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: "staging-push".to_string(),
        created_at: Utc::now().to_rfc3339(),
        threads: prepared.threads,
        warning_count: prepared.warning_count,
    };
    workspace = workspace_mapper.canonicalize_snapshot(&workspace);
    project_workspace_identities(&mut workspace);
    let workspace = canonicalize_snapshot_provider_metadata(&workspace);
    let plan = build_change_plan(integrated_head.clone(), &base, &workspace.threads)?;
    let staged = StagedChangeSet::from_selected(&plan, selected_thread_ids)?;

    // Preserve the exact staged intent before publishing. Unlike the working
    // object cache, this writes a Snapshot Root that is visible in local
    // history and remains recoverable if the network commit later fails.
    store_staged_selection_snapshot(&plan, &staged, &prepared.contents, repository_root)?;

    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let mut staged_refs = Vec::new();
    for candidate in plan
        .changed()
        .filter(|candidate| staged.selected_thread_ids.contains(&candidate.thread_id))
    {
        if candidate.kind == ChangeKind::Deleted {
            continue;
        }
        let local = candidate
            .local
            .as_ref()
            .context("staged local thread is missing")?;
        let content = prepared
            .contents
            .get(&candidate.thread_id)
            .context("staged local content reference is missing")?;
        staged_refs.push(store.store_descriptor(local, content.clone())?);
    }
    let remote_refs = match remote_state.head.as_deref() {
        Some(revision_id) => load_revision_root(client, revision_id, repository_root)?.threads,
        None => Vec::new(),
    };
    let threads = compose_staged_thread_refs(&remote_refs, &plan, &staged, &staged_refs)?;
    let thread_count = threads.len();
    let revision_root = sync_core::RevisionRootV4 {
        schema_version: sync_core::STORAGE_PROTOCOL_VERSION_V4,
        normalization_schema_version: sync_core::NORMALIZATION_SCHEMA_VERSION_V4,
        namespace_id,
        parent_revision: remote_state.head.clone(),
        created_at: Utc::now().to_rfc3339(),
        threads,
        warning_count: 0,
    };
    let root_object = store.store_revision_root(&revision_root)?;
    let revision_id = revision_root.revision_id()?;
    let mut upload_objects = sync_core::collect_thread_graph_v4(&staged_refs, &store)?;
    upload_objects.insert(root_object);
    let missing_query_started = Instant::now();
    let missing = client.missing_v4_objects(upload_objects.into_iter().collect())?;
    let missing_query_ms = duration_ms(missing_query_started.elapsed());
    let upload = upload_missing_v4_objects(client, &store, missing, control)?;

    control.check_cancelled()?;
    control.report(OperationProgress {
        phase: "push_commit".to_string(),
        message: "Committing staged remote revision".to_string(),
        completed: 0,
        total: None,
        unit: "steps".to_string(),
        cancellable: false,
    });
    let commit_started = Instant::now();
    let commit = client.commit_revision_v4(
        namespace_id,
        &RevisionCommitRequestV4 {
            expected_head: remote_state.head.clone(),
            expected_namespace_epoch: remote_state.namespace_epoch,
            revision: revision_root,
        },
    )?;
    let commit_ms = duration_ms(commit_started.elapsed());
    if commit.namespace_id != namespace_id || commit.head != revision_id {
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
    let updated = tracking.update_remote_epoch(
        codex_home,
        remote_id,
        namespace_id,
        updated.generation,
        commit.namespace_epoch,
    )?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Pushed,
        namespace_id,
        previous_head: remote_state.head,
        head: updated.integrated_head,
        revision_id: Some(revision_id),
        uploaded_objects: upload.created_objects,
        downloaded_objects: 0,
        thread_count,
        conflicts: Vec::new(),
        checkout: None,
        push_metrics: Some(PushMetrics {
            missing_query_ms,
            upload_ms: upload.elapsed_ms,
            commit_ms,
            transferred_objects: upload.transferred_objects,
            transferred_bytes: upload.transferred_bytes,
            created_objects: upload.created_objects,
            max_concurrency: upload.max_concurrency,
        }),
    })
}

fn store_staged_selection_snapshot(
    plan: &sync_core::ChangePlan,
    staged: &StagedChangeSet,
    contents: &BTreeMap<String, sync_core::ContentRefV4>,
    repository_root: &Path,
) -> Result<sync_core::SnapshotSummary> {
    let deleted_thread_ids = plan
        .changed()
        .filter(|candidate| {
            candidate.kind == ChangeKind::Deleted
                && staged.selected_thread_ids.contains(&candidate.thread_id)
        })
        .map(|candidate| candidate.thread_id.clone())
        .collect::<Vec<_>>();
    let threads = plan
        .changed()
        .filter(|candidate| {
            candidate.kind != ChangeKind::Deleted
                && staged.selected_thread_ids.contains(&candidate.thread_id)
        })
        .map(|candidate| {
            candidate
                .local
                .clone()
                .context("staged local thread is missing")
        })
        .collect::<Result<Vec<_>>>()?;
    let selected_contents = threads
        .iter()
        .map(|thread| {
            contents
                .get(&thread.thread_id)
                .cloned()
                .map(|content| (thread.thread_id.clone(), content))
                .context("staged local content reference is missing")
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let snapshot = LocalSnapshot {
        schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: Uuid::now_v7().to_string(),
        created_at: Utc::now().to_rfc3339(),
        threads,
        warning_count: 0,
    };
    let manifest_path = write_v4_snapshot(&snapshot, &selected_contents, repository_root)?;
    let total_bytes = snapshot
        .threads
        .iter()
        .map(|thread| thread.rollout.byte_length)
        .sum();
    sync_core::update_snapshot_metadata(
        repository_root,
        &snapshot.snapshot_id,
        SnapshotMetadata {
            description: "选择推送快照".to_string(),
            tags: vec!["selection".to_string(), "push".to_string()],
            pinned: false,
            automatic: true,
            scope: SnapshotScope::Selection,
            base_revision_id: plan.base_revision_id.clone(),
            selected_thread_ids: staged.selected_thread_ids.iter().cloned().collect(),
            deleted_thread_ids,
        },
    )?;
    Ok(sync_core::SnapshotSummary {
        snapshot_id: snapshot.snapshot_id,
        manifest_path,
        thread_count: snapshot.threads.len(),
        object_count: selected_contents.len(),
        total_bytes,
        warning_count: 0,
    })
}

/// Persist the selected local additions/modifications as a compact local
/// Snapshot. Deletions are retained as explicit metadata, never fabricated as
/// empty rollout objects.
pub fn create_staged_snapshot(
    remote_id: Uuid,
    namespace_id: Uuid,
    selected_thread_ids: BTreeSet<String>,
    expected_base_revision_id: Option<String>,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<sync_core::SnapshotSummary> {
    let LocalSyncContext {
        codex_home,
        repository_root,
        workspace_mapper,
        control,
    } = context;
    ensure_codex_closed()?;
    if selected_thread_ids.is_empty() {
        bail!("select at least one local change before snapshotting");
    }
    let tracking = TrackingStore::open(repository_root)?;
    let integrated_head = tracking
        .load(codex_home, remote_id, namespace_id)?
        .and_then(|record| record.integrated_head);
    if integrated_head != expected_base_revision_id {
        bail!("the staging plan is stale; regenerate it before creating a snapshot");
    }
    let base = match integrated_head.as_deref() {
        Some(revision_id) => {
            let (root, descriptors, _) =
                download_revision_descriptors(client, revision_id, repository_root, control)?;
            validate_revision_root_namespace(&root, namespace_id)?;
            descriptors
        }
        None => Vec::new(),
    };
    let prepared = prepare_workspace_snapshot_with_control(codex_home, repository_root, control)?;
    if prepared.warning_count > 0 {
        bail!(
            "selected snapshot is blocked because the local workspace contains {} warning(s)",
            prepared.warning_count
        );
    }
    let mut workspace = LocalSnapshot {
        schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: "staging-snapshot".to_string(),
        created_at: Utc::now().to_rfc3339(),
        threads: prepared.threads,
        warning_count: prepared.warning_count,
    };
    workspace = workspace_mapper.canonicalize_snapshot(&workspace);
    project_workspace_identities(&mut workspace);
    let workspace = canonicalize_snapshot_provider_metadata(&workspace);
    let plan = build_change_plan(integrated_head.clone(), &base, &workspace.threads)?;
    let staged = StagedChangeSet::from_selected(&plan, selected_thread_ids)?;
    let deleted_thread_ids = plan
        .changed()
        .filter(|candidate| {
            candidate.kind == ChangeKind::Deleted
                && staged.selected_thread_ids.contains(&candidate.thread_id)
        })
        .map(|candidate| candidate.thread_id.clone())
        .collect::<Vec<_>>();
    let threads = plan
        .changed()
        .filter(|candidate| {
            candidate.kind != ChangeKind::Deleted
                && staged.selected_thread_ids.contains(&candidate.thread_id)
        })
        .map(|candidate| {
            candidate
                .local
                .clone()
                .context("staged local thread is missing")
        })
        .collect::<Result<Vec<_>>>()?;
    let contents = threads
        .iter()
        .map(|thread| {
            prepared
                .contents
                .get(&thread.thread_id)
                .cloned()
                .map(|content| (thread.thread_id.clone(), content))
                .context("staged local content reference is missing")
        })
        .collect::<Result<_>>()?;
    let snapshot = LocalSnapshot {
        schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: Uuid::now_v7().to_string(),
        created_at: Utc::now().to_rfc3339(),
        threads,
        warning_count: 0,
    };
    let manifest_path = write_v4_snapshot(&snapshot, &contents, repository_root)?;
    let total_bytes = snapshot
        .threads
        .iter()
        .map(|thread| thread.rollout.byte_length)
        .sum();
    sync_core::update_snapshot_metadata(
        repository_root,
        &snapshot.snapshot_id,
        SnapshotMetadata {
            description: "选择快照".to_string(),
            tags: vec!["selection".to_string()],
            pinned: false,
            automatic: false,
            scope: SnapshotScope::Selection,
            base_revision_id: integrated_head,
            selected_thread_ids: staged.selected_thread_ids.into_iter().collect(),
            deleted_thread_ids,
        },
    )?;
    Ok(sync_core::SnapshotSummary {
        snapshot_id: snapshot.snapshot_id,
        manifest_path,
        thread_count: snapshot.threads.len(),
        object_count: contents.len(),
        total_bytes,
        warning_count: 0,
    })
}

pub fn push_namespace(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
) -> Result<SyncReport> {
    push_namespace_with_snapshot(remote_id, namespace_id, client, context, None)
}

pub fn push_latest_snapshot_namespace(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
    manifest_path: &Path,
) -> Result<SyncReport> {
    push_namespace_with_snapshot(
        remote_id,
        namespace_id,
        client,
        context,
        Some(manifest_path),
    )
}

fn push_namespace_with_snapshot(
    remote_id: Uuid,
    namespace_id: Uuid,
    client: &RemoteClient,
    context: LocalSyncContext<'_>,
    selected_manifest: Option<&Path>,
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
    let remote_state = client.namespace_head_state(namespace_id)?;
    if let Some(record) = &record
        && record.remote_epoch != remote_state.namespace_epoch
    {
        bail!(
            "remote history epoch changed from {} to {}; use an exact namespace switch to coordinate the rewritten history",
            record.remote_epoch,
            remote_state.namespace_epoch
        );
    }
    let remote_head = remote_state.head.clone();
    let manifest_path = if let Some(path) = selected_manifest {
        path.to_path_buf()
    } else {
        let summary =
            create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
        if summary.warning_count > 0 {
            bail!(
                "push is blocked because the local snapshot contains {} warning(s)",
                summary.warning_count
            );
        }
        summary.manifest_path
    };
    let (snapshot, contents) = sync_core::load_v4_snapshot(&manifest_path, repository_root)?;
    if snapshot.warning_count > 0 {
        bail!(
            "push is blocked because the selected snapshot contains {} warning(s)",
            snapshot.warning_count
        );
    }
    let mut snapshot = workspace_mapper.canonicalize_snapshot(&snapshot);
    project_workspace_identities(&mut snapshot);
    let (revision_root, _) = sync_core::snapshot_to_revision_root_v4(
        &snapshot,
        namespace_id,
        remote_head.clone(),
        repository_root,
        &contents,
    )?;
    let revision_id = revision_root.revision_id()?;
    if let Some(head) = remote_head.as_deref() {
        let (current_root, current_threads, _) =
            download_revision_graph(client, head, repository_root, control)?;
        validate_revision_root_namespace(&current_root, namespace_id)?;
        if thread_states_equal(&current_threads, &snapshot.threads)?
            && current_root.warning_count == revision_root.warning_count
        {
            let reconciled = if integrated_head.as_deref() == Some(head) && active.is_some() {
                record
                    .clone()
                    .context("active namespace has no tracking record")?
            } else {
                tracking.reconcile_checkout(
                    codex_home,
                    remote_id,
                    namespace_id,
                    record.as_ref().map(|record| record.generation),
                    Some(head),
                    true,
                )?
            };
            let reconciled = tracking.update_remote_epoch(
                codex_home,
                remote_id,
                namespace_id,
                reconciled.generation,
                remote_state.namespace_epoch,
            )?;
            return Ok(SyncReport {
                kind: SyncOutcomeKind::NoChanges,
                namespace_id,
                previous_head: remote_head.clone(),
                head: reconciled.integrated_head,
                revision_id: Some(head.to_string()),
                uploaded_objects: 0,
                downloaded_objects: 0,
                thread_count: snapshot.threads.len(),
                conflicts: Vec::new(),
                checkout: None,
                push_metrics: None,
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

    let typed_store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let typed_objects = sync_core::collect_revision_graph_v4(&revision_root, &typed_store)?;
    let missing_query_started = Instant::now();
    let missing_typed = client.missing_v4_objects(typed_objects.into_iter().collect())?;
    let missing_query_ms = duration_ms(missing_query_started.elapsed());
    let upload = upload_missing_v4_objects(client, &typed_store, missing_typed, control)?;

    control.check_cancelled()?;
    let request = RevisionCommitRequestV4 {
        expected_head: remote_head.clone(),
        expected_namespace_epoch: remote_state.namespace_epoch,
        revision: revision_root.clone(),
    };
    control.report(OperationProgress {
        phase: "push_commit".to_string(),
        message: "Committing remote revision".to_string(),
        completed: 0,
        total: None,
        unit: "steps".to_string(),
        cancellable: false,
    });
    let commit_started = Instant::now();
    let commit = match client.commit_revision_v4(namespace_id, &request) {
        Ok(commit) => commit,
        Err(error) => {
            if client.namespace_head(namespace_id)?.as_deref() == Some(revision_id.as_str()) {
                sync_core::RevisionCommitResponseV4 {
                    namespace_id,
                    head: revision_id.clone(),
                    created: true,
                    namespace_epoch: remote_state.namespace_epoch,
                }
            } else {
                return Err(error);
            }
        }
    };
    let commit_ms = duration_ms(commit_started.elapsed());
    if commit.namespace_id != namespace_id || commit.head != revision_id {
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
    let updated = tracking.update_remote_epoch(
        codex_home,
        remote_id,
        namespace_id,
        updated.generation,
        commit.namespace_epoch,
    )?;
    Ok(SyncReport {
        kind: SyncOutcomeKind::Pushed,
        namespace_id,
        previous_head: remote_head,
        head: updated.integrated_head,
        revision_id: Some(revision_id),
        uploaded_objects: upload.created_objects,
        downloaded_objects: 0,
        thread_count: snapshot.threads.len(),
        conflicts: Vec::new(),
        checkout: None,
        push_metrics: Some(PushMetrics {
            missing_query_ms,
            upload_ms: upload.elapsed_ms,
            commit_ms,
            transferred_objects: upload.transferred_objects,
            transferred_bytes: upload.transferred_bytes,
            created_objects: upload.created_objects,
            max_concurrency: upload.max_concurrency,
        }),
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
                push_metrics: None,
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
            push_metrics: None,
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
    let tracking = TrackingStore::open(context.repository_root)?;
    let applied = tracking
        .load(context.codex_home, remote_id, namespace_id)?
        .context("checkout completed without tracking state")?;
    tracking.update_remote_epoch(
        context.codex_home,
        remote_id,
        namespace_id,
        applied.generation,
        prepared.remote_epoch,
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
        push_metrics: None,
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
        push_metrics: pushed.push_metrics,
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
    remote_epoch: u64,
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
    let remote_state = client.namespace_head_state(namespace_id)?;
    if record.remote_epoch != remote_state.namespace_epoch {
        bail!(
            "remote history epoch changed from {} to {}; use an exact namespace switch to coordinate the rewritten history",
            record.remote_epoch,
            remote_state.namespace_epoch
        );
    }
    let remote_head = remote_state.head;
    if remote_head == previous_head {
        return Ok(PreparedPull::NoChanges {
            head: previous_head,
        });
    }
    let remote_head = remote_head.context("remote namespace has no revision to pull")?;
    if let Some(previous) = previous_head.as_deref() {
        ensure_ancestor(client, namespace_id, &remote_head, previous)?;
    }
    let (remote_root, remote_threads, downloaded) =
        download_revision_graph(client, &remote_head, repository_root, control)?;
    validate_revision_root_namespace(&remote_root, namespace_id)?;
    if remote_root.warning_count > 0 {
        bail!("remote revision is incomplete and cannot be checked out safely");
    }
    let local_summary =
        create_local_snapshot_with_control(codex_home, repository_root, true, control)?;
    if local_summary.warning_count > 0 {
        bail!("pull is blocked because the local scan contains warnings");
    }
    let mut local =
        workspace_mapper.canonicalize_snapshot(&load_local_snapshot(local_summary.manifest_path)?);
    project_workspace_identities(&mut local);
    let local = canonicalize_snapshot_provider_metadata(&local);
    let base = match previous_head.as_deref() {
        Some(head) => {
            let (root, threads, _) =
                download_revision_graph(client, head, repository_root, control)?;
            validate_revision_root_namespace(&root, namespace_id)?;
            threads
        }
        None => Vec::new(),
    };
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
        remote_epoch: remote_state.namespace_epoch,
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
    let snapshot = materialize_snapshot_provider_metadata(
        &LocalSnapshot {
            schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: Uuid::now_v7().to_string(),
            created_at: Utc::now().to_rfc3339(),
            threads,
            warning_count: 0,
        },
        &detect_configured_provider(codex_home)?,
    )?;
    let snapshot = workspace_mapper.materialize_snapshot(&snapshot);
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    let project_roots = workspace_mapper.local_prefixes();
    ensure_codex_closed()?;
    checkout_local_snapshot_with_tracking_and_projects_control(
        manifest,
        codex_home,
        repository_root,
        true,
        tracking_update,
        &project_roots,
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
            let record = tracking
                .load(codex_home, remote_id, namespace_id)?
                .context("active namespace has no tracking record")?;
            let remote = target_client.namespace_head_state(namespace_id)?;
            if record.remote_epoch == remote.namespace_epoch {
                return pull_namespace(remote_id, namespace_id, target_client, context);
            }
        } else {
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
    }

    let previous = tracking.load(codex_home, remote_id, namespace_id)?;
    let target_state = target_client.namespace_head_state(namespace_id)?;
    let target_head = target_state.head.clone();
    let (snapshot, downloaded) = match target_head.as_deref() {
        Some(head) => {
            let (root, threads, downloaded) =
                download_revision_graph(target_client, head, repository_root, control)?;
            validate_revision_root_namespace(&root, namespace_id)?;
            if root.warning_count > 0 {
                bail!("target namespace revision is incomplete and cannot be checked out");
            }
            (
                LocalSnapshot {
                    schema_version: sync_core::LOCAL_SNAPSHOT_SCHEMA_VERSION,
                    snapshot_id: Uuid::now_v7().to_string(),
                    created_at: root.created_at,
                    threads,
                    warning_count: root.warning_count,
                },
                downloaded,
            )
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
    let snapshot = materialize_snapshot_provider_metadata(
        &snapshot,
        &detect_configured_provider(codex_home)?,
    )?;
    let snapshot = target_workspace_mapper.materialize_snapshot(&snapshot);
    let manifest = store_local_snapshot(&snapshot, repository_root)?;
    let project_roots = target_workspace_mapper.local_prefixes();
    ensure_codex_closed()?;
    let checkout = checkout_local_snapshot_with_tracking_and_projects_control(
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
        &project_roots,
        control,
    )?;
    let applied = tracking
        .load(codex_home, remote_id, namespace_id)?
        .context("namespace switch completed without tracking state")?;
    tracking.update_remote_epoch(
        codex_home,
        remote_id,
        namespace_id,
        applied.generation,
        target_state.namespace_epoch,
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
        push_metrics: None,
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
    let mut local =
        workspace_mapper.canonicalize_snapshot(&load_local_snapshot(local_summary.manifest_path)?);
    project_workspace_identities(&mut local);
    let local = canonicalize_snapshot_provider_metadata(&local);
    let base = match record.integrated_head.as_deref() {
        Some(head) => {
            let (root, threads, _) =
                download_revision_graph(client, head, repository_root, control)?;
            validate_revision_root_namespace(&root, active.namespace_id)?;
            threads
        }
        None => Vec::new(),
    };
    if !thread_states_equal(&local.threads, &base)? {
        bail!("the active namespace has unpushed local changes; push before switching");
    }
    Ok(())
}

fn load_revision_root(
    client: &RemoteClient,
    revision_id: &str,
    repository_root: &Path,
) -> Result<sync_core::RevisionRootV4> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let root_path = store.object_path_by_id(StorageObjectKindV4::RevisionRoot, revision_id)?;
    if root_path.exists() {
        let root: sync_core::RevisionRootV4 =
            store.read_json(StorageObjectKindV4::RevisionRoot, revision_id)?;
        root.validate()?;
        Ok(root)
    } else {
        let root = client.revision_root_v4(revision_id)?;
        store.store_revision_root(&root)?;
        Ok(root)
    }
}

/// Fetch only immutable revision root and thread descriptors.  In particular,
/// it does not materialize rollout objects, making it suitable for baseline
/// comparison and staged Push planning.
fn download_revision_descriptors(
    client: &RemoteClient,
    revision_id: &str,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<(sync_core::RevisionRootV4, Vec<ThreadBundle>, usize)> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let root_path = store.object_path_by_id(StorageObjectKindV4::RevisionRoot, revision_id)?;
    let root_was_cached = root_path.exists();
    let root = load_revision_root(client, revision_id, repository_root)?;
    let mut downloaded = usize::from(!root_was_cached);
    let mut threads = Vec::with_capacity(root.threads.len());
    for (index, thread_ref) in root.threads.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "staging_baseline".to_string(),
            message: thread_ref.thread_id.clone(),
            completed: index as u64,
            total: Some(root.threads.len() as u64),
            unit: "threads".to_string(),
            cancellable: true,
        });
        let descriptor_path =
            store.object_path_by_id(StorageObjectKindV4::Thread, &thread_ref.descriptor_sha256)?;
        if !descriptor_path.exists() {
            downloaded += download_unknown_v4_object(
                client,
                &store,
                StorageObjectKindV4::Thread,
                &thread_ref.descriptor_sha256,
                control,
            )? as usize;
        }
        threads.push(store.load_descriptor(thread_ref)?.into_bundle(None));
    }
    Ok((root, threads, downloaded))
}

pub(crate) fn download_revision_graph(
    client: &RemoteClient,
    revision_id: &str,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<(sync_core::RevisionRootV4, Vec<ThreadBundle>, usize)> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let root_path = store.object_path_by_id(StorageObjectKindV4::RevisionRoot, revision_id)?;
    let root_was_cached = root_path.exists();
    let root = load_revision_root(client, revision_id, repository_root)?;
    let mut downloaded = usize::from(!root_was_cached);
    let mut threads = Vec::with_capacity(root.threads.len());
    for (index, thread_ref) in root.threads.iter().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: "pull_revision_graph".to_string(),
            message: thread_ref.thread_id.clone(),
            completed: index as u64,
            total: Some(root.threads.len() as u64),
            unit: "threads".to_string(),
            cancellable: true,
        });
        let descriptor_path =
            store.object_path_by_id(StorageObjectKindV4::Thread, &thread_ref.descriptor_sha256)?;
        if !descriptor_path.exists() {
            downloaded += download_unknown_v4_object(
                client,
                &store,
                StorageObjectKindV4::Thread,
                &thread_ref.descriptor_sha256,
                control,
            )? as usize;
        }
        let descriptor = store.load_descriptor(thread_ref)?;
        descriptor.validate()?;
        if descriptor.thread_id != thread_ref.thread_id {
            bail!("thread reference ID does not match its descriptor");
        }
        for content in std::iter::once(&descriptor.rollout).chain(descriptor.attachments.iter()) {
            match &content.storage {
                sync_core::StorageRefV4::Whole { object_sha256 } => {
                    let object = sync_core::StorageObjectRefV4 {
                        kind: StorageObjectKindV4::Whole,
                        sha256: object_sha256.clone(),
                        byte_length: content.byte_length,
                    };
                    if !store.object_path(&object)?.is_file()
                        && download_known_v4_object(client, &store, &object, control)?
                    {
                        downloaded += 1;
                    }
                }
                sync_core::StorageRefV4::Chunked { manifest_sha256 } => {
                    let manifest_path = store
                        .object_path_by_id(StorageObjectKindV4::ChunkManifest, manifest_sha256)?;
                    if !manifest_path.exists() {
                        downloaded += download_unknown_v4_object(
                            client,
                            &store,
                            StorageObjectKindV4::ChunkManifest,
                            manifest_sha256,
                            control,
                        )? as usize;
                    }
                    let manifest: sync_core::ChunkManifestV4 =
                        store.read_json(StorageObjectKindV4::ChunkManifest, manifest_sha256)?;
                    manifest.validate()?;
                    if manifest.logical_sha256 != content.logical_sha256
                        || manifest.byte_length != content.byte_length
                    {
                        bail!("downloaded chunk manifest does not match rollout");
                    }
                    for chunk in manifest.chunks {
                        let object = sync_core::StorageObjectRefV4 {
                            kind: StorageObjectKindV4::Chunk,
                            sha256: chunk.sha256,
                            byte_length: chunk.byte_length,
                        };
                        if !store.object_path(&object)?.is_file()
                            && download_known_v4_object(client, &store, &object, control)?
                        {
                            downloaded += 1;
                        }
                    }
                }
            }
        }
        threads.push(descriptor.into_bundle(None));
    }
    Ok((root, threads, downloaded))
}

fn download_unknown_v4_object(
    client: &RemoteClient,
    store: &FilesystemContentStoreV4,
    kind: StorageObjectKindV4,
    sha256: &str,
    control: &OperationControl,
) -> Result<bool> {
    let probe = sync_core::StorageObjectRefV4 {
        kind,
        sha256: sha256.to_string(),
        byte_length: 0,
    };
    let response = client.download_v4_object(&probe)?;
    let byte_length = response
        .content_length()
        .context("typed object response has no Content-Length")?;
    let object = sync_core::StorageObjectRefV4 {
        byte_length,
        ..probe
    };
    download_known_v4_response(store, &object, response, control)
}

fn download_known_v4_object(
    client: &RemoteClient,
    store: &FilesystemContentStoreV4,
    object: &sync_core::StorageObjectRefV4,
    control: &OperationControl,
) -> Result<bool> {
    let response = client.download_v4_object(object)?;
    download_known_v4_response(store, object, response, control)
}

fn download_known_v4_response(
    store: &FilesystemContentStoreV4,
    object: &sync_core::StorageObjectRefV4,
    response: reqwest::blocking::Response,
    control: &OperationControl,
) -> Result<bool> {
    control.check_cancelled()?;
    let bytes = response.bytes()?;
    let existed = store.object_path(object)?.is_file();
    store.install_bytes(object.clone(), &bytes)?;
    Ok(!existed)
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
        let revision = client.revision_root_v4(&current)?;
        validate_revision_root_namespace(&revision, namespace_id)?;
        let Some(parent) = revision.parent_revision else {
            bail!("remote revision history does not contain the locally tracked head");
        };
        current = parent;
    }
    bail!("remote revision ancestry exceeds the safety limit")
}

fn validate_revision_root_namespace(
    revision: &sync_core::RevisionRootV4,
    namespace_id: Uuid,
) -> Result<()> {
    revision.validate()?;
    if revision.namespace_id != namespace_id {
        bail!("server returned a revision for the wrong namespace");
    }
    Ok(())
}

fn thread_states_equal(left: &[ThreadBundle], right: &[ThreadBundle]) -> Result<bool> {
    Ok(thread_state(left)? == thread_state(right)?)
}

fn project_workspace_identities(snapshot: &mut LocalSnapshot) {
    for thread in &mut snapshot.threads {
        if thread.workspace.logical_id.is_none() {
            thread.workspace.logical_id = thread.workspace.source_path.clone();
        }
    }
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
    use std::sync::atomic::AtomicUsize;

    use axum::body::Bytes;
    use axum::extract::{Path as AxumPath, State};
    use axum::routing::put;
    use axum::{Json, Router};
    use rusqlite::{Connection, params};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::remote::SecretToken;

    #[test]
    fn upload_progress_reports_bytes_and_object_counts() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let captured = reports.clone();
        let control = OperationControl::new(Arc::new(AtomicBool::new(false)), move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let progress = UploadProgressAggregator::new(control, 1024, 2);

        progress.add_bytes(512);
        progress.complete_object();
        progress.report(true);

        let reports = reports.lock().unwrap();
        let latest = reports.last().unwrap();
        assert_eq!(latest.phase, "push_typed_objects");
        assert_eq!(latest.completed, 512);
        assert_eq!(latest.total, Some(1024));
        assert_eq!(latest.unit, "bytes");
        assert!(latest.message.contains("对象 1/2"));
    }

    #[test]
    fn missing_object_uploads_use_at_most_four_concurrent_requests() {
        #[derive(Clone)]
        struct ConcurrencyState {
            active: Arc<AtomicUsize>,
            maximum: Arc<AtomicUsize>,
        }

        async fn upload(
            State(state): State<ConcurrencyState>,
            AxumPath((_kind, digest)): AxumPath<(String, String)>,
            body: Bytes,
        ) -> Json<sync_core::PutObjectResponse> {
            let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
            state.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            state.active.fetch_sub(1, Ordering::SeqCst);
            Json(sync_core::PutObjectResponse {
                sha256: format!("sha256:{digest}"),
                byte_length: body.len() as u64,
                created: true,
            })
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = ConcurrencyState {
            active: Arc::new(AtomicUsize::new(0)),
            maximum: Arc::new(AtomicUsize::new(0)),
        };
        let maximum = state.maximum.clone();
        let (address, task) = runtime.block_on(async {
            let app = Router::new()
                .route("/api/v4/objects/{kind}/{digest}", put(upload))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await });
            (address, task)
        });
        let client = RemoteClient::new(
            &format!("http://{address}"),
            SecretToken::new("test-token-that-is-long-enough-for-auth".to_string()).unwrap(),
        )
        .unwrap();
        let repository = tempdir().unwrap();
        let store = FilesystemContentStoreV4::open(repository.path()).unwrap();
        let mut objects = Vec::new();
        for index in 0..8 {
            let bytes = format!("parallel-object-{index}").into_bytes();
            let object = sync_core::StorageObjectRefV4 {
                kind: StorageObjectKindV4::Chunk,
                sha256: sync_core::digest_bytes(&bytes),
                byte_length: bytes.len() as u64,
            };
            store.install_bytes(object.clone(), &bytes).unwrap();
            objects.push(object);
        }

        let report =
            upload_missing_v4_objects(&client, &store, objects, &OperationControl::default())
                .unwrap();

        assert_eq!(report.transferred_objects, 8);
        assert_eq!(report.created_objects, 8);
        assert_eq!(report.max_concurrency, DEFAULT_UPLOAD_CONCURRENCY);
        assert!((2..=DEFAULT_UPLOAD_CONCURRENCY).contains(&maximum.load(Ordering::SeqCst)));
        task.abort();
        runtime.block_on(async {
            let _ = task.await;
        });
    }

    #[test]
    fn upload_failure_does_not_mark_operation_as_user_cancelled() {
        async fn fail_upload() -> axum::http::StatusCode {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (address, task) = runtime.block_on(async {
            let app = Router::new().route("/api/v4/objects/{kind}/{digest}", put(fail_upload));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await });
            (address, task)
        });
        let client = RemoteClient::new(
            &format!("http://{address}"),
            SecretToken::new("test-token-that-is-long-enough-for-auth".to_string()).unwrap(),
        )
        .unwrap();
        let repository = tempdir().unwrap();
        let store = FilesystemContentStoreV4::open(repository.path()).unwrap();
        let bytes = b"failed-object";
        let object = sync_core::StorageObjectRefV4 {
            kind: StorageObjectKindV4::Chunk,
            sha256: sync_core::digest_bytes(bytes),
            byte_length: bytes.len() as u64,
        };
        store.install_bytes(object.clone(), bytes).unwrap();
        let control = OperationControl::default();

        let error = upload_missing_v4_objects(&client, &store, vec![object], &control)
            .expect_err("server failure must fail the upload batch");

        assert!(!control.is_cancelled());
        assert!(error.to_string().contains("failed to upload chunk"));
        task.abort();
        runtime.block_on(async {
            let _ = task.await;
        });
    }

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
        let metrics = pushed_a.push_metrics.as_ref().unwrap();
        assert!(metrics.transferred_objects > 0);
        assert!(metrics.transferred_bytes > 0);
        assert_eq!(metrics.max_concurrency, DEFAULT_UPLOAD_CONCURRENCY);
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

        let mut advanced_root = client.revision_root_v4(&first_head).unwrap();
        advanced_root.parent_revision = Some(first_head.clone());
        advanced_root.created_at = "2100-01-01T00:00:00Z".to_string();
        let advanced_head = advanced_root.revision_id().unwrap();
        assert_ne!(advanced_head, first_head);
        let store = FilesystemContentStoreV4::open(&repository_b).unwrap();
        let root_object = store.store_revision_root(&advanced_root).unwrap();
        client
            .upload_v4_object(
                &root_object,
                &store.object_path(&root_object).unwrap(),
                &OperationControl::default(),
            )
            .unwrap();
        client
            .commit_revision_v4(
                namespace.id,
                &sync_core::RevisionCommitRequestV4 {
                    expected_head: Some(first_head),
                    expected_namespace_epoch: 0,
                    revision: advanced_root,
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
        fs::write(home_b.join("config.toml"), "model_provider = \"custom\"\n").unwrap();
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
        let (_, pushed_a_threads, _) = download_revision_graph(
            &client,
            pushed_a_head,
            &repository_a,
            &OperationControl::default(),
        )
        .unwrap();
        let thread_a_remote_hash = pushed_a_threads
            .iter()
            .find(|thread| thread.thread_id == "thread-a")
            .unwrap()
            .rollout
            .sha256
            .clone();
        assert_eq!(
            object_cwd(&repository_a, &thread_a_remote_hash),
            sync_core::WORKSPACE_TOKEN_V4
        );

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
        assert_eq!(
            sync_core::scan_codex_home(&home_b).unwrap().threads[0]
                .model_provider
                .as_deref(),
            Some("custom")
        );
        let provider_only_push = push_namespace(
            remote_id,
            namespace.id,
            &client,
            LocalSyncContext::new(
                &home_b,
                &repository_b,
                &workspace_mapper_a,
                &OperationControl::default(),
            ),
        )
        .unwrap();
        assert_eq!(provider_only_push.kind, SyncOutcomeKind::NoChanges);
        assert_eq!(provider_only_push.head.as_deref(), Some(pushed_a_head));
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
        let project_state: serde_json::Value =
            serde_json::from_reader(File::open(home_b.join(".codex-global-state.json")).unwrap())
                .unwrap();
        let assignment_a = &project_state["thread-project-assignments"]["thread-a"];
        let assignment_b = &project_state["thread-project-assignments"]["thread-b"];
        assert_eq!(assignment_a["projectId"], assignment_b["projectId"]);
        assert_eq!(
            assignment_a["cwd"].as_str().unwrap().replace('\\', "/"),
            "F:/workspace"
        );
        assert_eq!(
            assignment_b["cwd"].as_str().unwrap().replace('\\', "/"),
            "F:/workspace/new"
        );
        assert!(
            remapped_b
                .checkout
                .as_ref()
                .unwrap()
                .local_backup_dir
                .join("project-state/global-state.json")
                .is_file()
        );

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
        let (_, pushed_b_revision_threads, _) = download_revision_graph(
            &client,
            pushed_b.head.as_deref().unwrap(),
            &repository_b,
            &OperationControl::default(),
        )
        .unwrap();
        let pushed_b_threads = pushed_b_revision_threads
            .iter()
            .map(|thread| (thread.thread_id.as_str(), &thread.rollout.sha256))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            pushed_b_threads.get("thread-a").map(|hash| hash.as_str()),
            Some(thread_a_remote_hash.as_str())
        );
        assert_eq!(
            object_cwd(&repository_b, pushed_b_threads["thread-a"]),
            sync_core::WORKSPACE_TOKEN_V4
        );
        assert_eq!(
            object_cwd(&repository_b, pushed_b_threads["thread-b"]),
            sync_core::WORKSPACE_TOKEN_V4
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
        fs::write(
            home.join(".codex-global-state.json"),
            serde_json::to_vec(&json!({
                "electron-saved-workspace-roots": [],
                "project-order": [],
                "local-projects": {},
                "thread-project-assignments": {},
                "projectless-thread-ids": ["thread-a", "thread-b"]
            }))
            .unwrap(),
        )
        .unwrap();
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
                storage: None,
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
