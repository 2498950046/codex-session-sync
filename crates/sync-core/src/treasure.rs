//! Portable, self-contained local treasure packages.
//!
//! A treasure is deliberately not a reference into the normal object store.
//! The on-disk format is newline-delimited JSON: a short magic line, one
//! bounded header and one independently hash-checked record per v4 object.
//! Content is normally chunked at 4 MiB by the v4 store, so encoding remains
//! bounded without a second archive dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    ContentRefV4, FilesystemContentStoreV4, LocalSnapshot, LocalSnapshotListItem, OperationControl,
    SnapshotMetadata, SnapshotRootV4, SnapshotScope, SnapshotSummary, StorageObjectKindV4,
    StorageObjectRefV4, StorageRefV4, ThreadBundle, ThreadMessage, ThreadMessagesPage,
    ThreadPreview, build_v4_snapshot_root, canonical_json_v4, digest_bytes_v4, load_v4_snapshot,
    semantic_thread_hash, write_v4_root,
};

pub const TREASURE_SCHEMA_VERSION: u32 = 1;
const TREASURE_MAGIC: &str = "codex-session-sync-treasure-v1";
const MAX_TREASURE_HEADER_BYTES: usize = 16 * 1024 * 1024;
const MAX_TREASURE_OBJECTS: usize = 500_000;
const MAX_TREASURE_TOTAL_BYTES: u64 = 128 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureCandidate {
    pub source_snapshot_id: String,
    pub semantic_hash: String,
    pub title: String,
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureConflict {
    pub thread_id: String,
    pub candidates: Vec<TreasureCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasurePreview {
    pub source_snapshot_ids: Vec<String>,
    pub thread_count: usize,
    pub conflict_count: usize,
    pub conflicts: Vec<TreasureConflict>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureResolution {
    pub thread_id: String,
    pub semantic_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureObjectRecord {
    pub object: StorageObjectRefV4,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureHeaderV1 {
    pub schema_version: u32,
    pub treasure_id: String,
    pub display_name: String,
    pub created_at: String,
    pub snapshot_root: SnapshotRootV4,
    pub manifest_sha256: String,
    pub object_count: usize,
    pub logical_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureListItem {
    pub treasure_id: String,
    pub display_name: String,
    pub created_at: String,
    pub path: PathBuf,
    pub thread_count: usize,
    pub logical_bytes: u64,
    pub object_count: usize,
    pub file_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureExportResult {
    pub treasure: TreasureListItem,
    pub preview: TreasurePreview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureValidationReport {
    pub treasure: TreasureListItem,
    pub valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TreasureImportResult {
    pub snapshot: SnapshotSummary,
    pub treasure: TreasureListItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPreview {
    pub snapshot_id: String,
    pub threads: Vec<ThreadPreview>,
}

#[derive(Clone)]
struct CandidateContent {
    source_snapshot_id: String,
    semantic_hash: String,
    thread: ThreadBundle,
    content: ContentRefV4,
}

pub fn plan_treasure(
    repository_root: &Path,
    source_snapshot_ids: &[String],
) -> Result<TreasurePreview> {
    let candidates = load_candidates(repository_root, source_snapshot_ids)?;
    preview_from_candidates(source_snapshot_ids, &candidates)
}

pub fn export_treasure_to_vault(
    repository_root: &Path,
    display_name: &str,
    source_snapshot_ids: &[String],
    expected_fingerprint: &str,
    resolutions: &[TreasureResolution],
    control: &OperationControl,
) -> Result<TreasureExportResult> {
    let name = display_name.trim().chars().take(200).collect::<String>();
    if name.is_empty() {
        bail!("treasure name cannot be empty");
    }
    let candidates = load_candidates(repository_root, source_snapshot_ids)?;
    let preview = preview_from_candidates(source_snapshot_ids, &candidates)?;
    if preview.fingerprint != expected_fingerprint {
        bail!("treasure preview is stale; refresh it before exporting");
    }
    let chosen = choose_candidates(&candidates, &preview, resolutions)?;
    let snapshot_id = Uuid::now_v7().to_string();
    let snapshot = LocalSnapshot {
        schema_version: crate::LOCAL_SNAPSHOT_SCHEMA_VERSION,
        snapshot_id,
        created_at: Utc::now().to_rfc3339(),
        threads: chosen
            .iter()
            .map(|candidate| candidate.thread.clone())
            .collect(),
        warning_count: 0,
    };
    let contents = chosen
        .into_iter()
        .map(|candidate| (candidate.thread.thread_id.clone(), candidate.content))
        .collect();
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let root = build_v4_snapshot_root(&snapshot, &contents, &store)?;
    let objects = collect_root_objects(&root, &store)?;
    let logical_bytes = snapshot
        .threads
        .iter()
        .map(|thread| thread.rollout.byte_length)
        .sum();
    let treasure_id = Uuid::now_v7().to_string();
    let vault = repository_root.join("vault");
    fs::create_dir_all(&vault)?;
    let path = vault.join(format!("{treasure_id}.codex-treasure"));
    let temporary = vault.join(format!(".{treasure_id}.tmp"));
    let header = TreasureHeaderV1 {
        schema_version: TREASURE_SCHEMA_VERSION,
        treasure_id: treasure_id.clone(),
        display_name: name,
        created_at: Utc::now().to_rfc3339(),
        manifest_sha256: digest_bytes_v4(&canonical_json_v4(&root)?),
        snapshot_root: root,
        object_count: objects.len(),
        logical_bytes,
    };
    let result = (|| -> Result<()> {
        let mut writer = BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?,
        );
        writer.write_all(TREASURE_MAGIC.as_bytes())?;
        writer.write_all(b"\n")?;
        serde_json::to_writer(&mut writer, &header)?;
        writer.write_all(b"\n")?;
        for (index, object) in objects.iter().enumerate() {
            control.check_cancelled()?;
            control.report(crate::OperationProgress {
                phase: "treasure_export".into(),
                message: object.kind.wire_name().into(),
                completed: index as u64,
                total: Some(objects.len() as u64),
                unit: "objects".into(),
                cancellable: true,
            });
            let bytes = fs::read(store.object_path(object)?)?;
            if bytes.len() as u64 != object.byte_length || digest_bytes_v4(&bytes) != object.sha256
            {
                bail!("source object changed or is corrupt");
            }
            let record = TreasureObjectRecord {
                object: object.clone(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            };
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        validate_treasure_file(&temporary, control)?;
        fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    let treasure = read_treasure_list_item(&path)?;
    Ok(TreasureExportResult { treasure, preview })
}

pub fn list_treasures(repository_root: &Path) -> Result<Vec<TreasureListItem>> {
    let directory = repository_root.join("vault");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut items = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("codex-treasure"))
        .filter_map(|path| read_treasure_list_item(&path).ok())
        .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.treasure_id.cmp(&a.treasure_id))
    });
    Ok(items)
}

/// Moves a treasure into the vault recycle area.  This is intentionally
/// limited to the managed vault directory so an IPC caller cannot delete an
/// arbitrary path supplied by a malicious front end.
pub fn trash_treasure(repository_root: &Path, treasure_id: &str) -> Result<PathBuf> {
    Uuid::parse_str(treasure_id)?;
    let source = repository_root
        .join("vault")
        .join(format!("{treasure_id}.codex-treasure"));
    if !source.is_file() {
        bail!("treasure no longer exists");
    }
    let trash = repository_root.join("vault").join("trash");
    fs::create_dir_all(&trash)?;
    let destination = trash.join(format!(
        "{}-{}.codex-treasure",
        Utc::now().format("%Y%m%d%H%M%S"),
        treasure_id
    ));
    fs::rename(&source, &destination)?;
    Ok(destination)
}

pub fn validate_treasure(
    path: &Path,
    control: &OperationControl,
) -> Result<TreasureValidationReport> {
    validate_treasure_file(path, control)?;
    Ok(TreasureValidationReport {
        treasure: read_treasure_list_item(path)?,
        valid: true,
    })
}

pub fn import_treasure_as_snapshot(
    repository_root: &Path,
    path: &Path,
    control: &OperationControl,
) -> Result<TreasureImportResult> {
    let (header, records) = read_treasure_records(path, control)?;
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    for (index, record) in records.iter().enumerate() {
        control.check_cancelled()?;
        control.report(crate::OperationProgress {
            phase: "treasure_import".into(),
            message: record.object.kind.wire_name().into(),
            completed: index as u64,
            total: Some(records.len() as u64),
            unit: "objects".into(),
            cancellable: true,
        });
        let bytes = base64::engine::general_purpose::STANDARD.decode(&record.bytes_base64)?;
        store.install_bytes(record.object.clone(), &bytes)?;
    }
    let mut root = header.snapshot_root;
    root.snapshot_id = Uuid::now_v7().to_string();
    let manifest_path = repository_root
        .join("snapshots")
        .join(format!("{}.json", root.snapshot_id));
    write_v4_root(&manifest_path, &root)?;
    let (snapshot, contents) = load_v4_snapshot(&manifest_path, repository_root)?;
    for content in contents.values() {
        store.validate_content(content)?;
    }
    crate::update_snapshot_metadata(
        repository_root,
        &snapshot.snapshot_id,
        SnapshotMetadata {
            description: header.display_name,
            scope: SnapshotScope::Full,
            ..Default::default()
        },
    )?;
    let item = crate::list_local_snapshots(repository_root)?
        .into_iter()
        .find(|item| item.snapshot_id == snapshot.snapshot_id)
        .context("imported treasure snapshot was not indexed")?;
    Ok(TreasureImportResult {
        snapshot: summary_from_item(item),
        treasure: read_treasure_list_item(path)?,
    })
}

pub fn load_snapshot_preview(repository_root: &Path, snapshot_id: &str) -> Result<SnapshotPreview> {
    let item = crate::list_local_snapshots(repository_root)?
        .into_iter()
        .find(|item| item.snapshot_id == snapshot_id)
        .context("snapshot not found")?;
    let (snapshot, _) = load_v4_snapshot(&item.manifest_path, repository_root)?;
    Ok(SnapshotPreview {
        snapshot_id: snapshot.snapshot_id,
        threads: snapshot
            .threads
            .into_iter()
            .map(|thread| ThreadPreview {
                thread_id: thread.thread_id,
                title: thread.title,
                archived: thread.archived,
                model_provider: thread.model_provider,
                workspace: thread.workspace,
            })
            .collect(),
    })
}

pub fn load_snapshot_thread_messages(
    repository_root: &Path,
    snapshot_id: &str,
    thread_id: &str,
    page: usize,
    page_size: usize,
) -> Result<ThreadMessagesPage> {
    let item = crate::list_local_snapshots(repository_root)?
        .into_iter()
        .find(|item| item.snapshot_id == snapshot_id)
        .context("snapshot not found")?;
    let (snapshot, contents) = load_v4_snapshot(&item.manifest_path, repository_root)?;
    if !snapshot
        .threads
        .iter()
        .any(|thread| thread.thread_id == thread_id)
    {
        bail!("thread was not found in snapshot");
    }
    let content = contents
        .get(thread_id)
        .context("snapshot thread content missing")?;
    load_content_messages(repository_root, content, thread_id, page, page_size)
}

/// Reads messages from a semantic thread whose content was already installed
/// in the local immutable cache.  Remote Revision preview uses this after its
/// authenticated object download, without creating a local Snapshot root.
pub fn load_thread_bundle_messages(
    repository_root: &Path,
    thread: &ThreadBundle,
    page: usize,
    page_size: usize,
) -> Result<ThreadMessagesPage> {
    let storage = match thread
        .rollout
        .storage
        .as_ref()
        .context("thread has no v4 storage reference")?
    {
        crate::storage_v3::StorageRef::Whole { object_sha256 } => StorageRefV4::Whole {
            object_sha256: object_sha256.clone(),
        },
        crate::storage_v3::StorageRef::Chunked { manifest_sha256 } => StorageRefV4::Chunked {
            manifest_sha256: manifest_sha256.clone(),
        },
    };
    let content = ContentRefV4 {
        logical_sha256: thread.rollout.sha256.clone(),
        byte_length: thread.rollout.byte_length,
        storage,
        media_type: thread.rollout.media_type.clone(),
        logical_path: thread.rollout.logical_path.clone(),
    };
    load_content_messages(
        repository_root,
        &content,
        &thread.thread_id,
        page,
        page_size,
    )
}

fn load_content_messages(
    repository_root: &Path,
    content: &ContentRefV4,
    thread_id: &str,
    page: usize,
    page_size: usize,
) -> Result<ThreadMessagesPage> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    store.validate_content(content)?;
    let temporary = repository_root
        .join("objects")
        .join("tmp")
        .join(format!("preview-{}.jsonl", Uuid::now_v7()));
    store.materialize(content, &temporary)?;
    let result = parse_rollout_messages(&temporary, thread_id, page, page_size);
    let _ = fs::remove_file(temporary);
    result
}

fn summary_from_item(item: LocalSnapshotListItem) -> SnapshotSummary {
    SnapshotSummary {
        snapshot_id: item.snapshot_id,
        manifest_path: item.manifest_path,
        thread_count: item.thread_count,
        object_count: item.object_count,
        total_bytes: item.logical_bytes,
        warning_count: item.warning_count,
    }
}

fn load_candidates(
    repository_root: &Path,
    source_snapshot_ids: &[String],
) -> Result<BTreeMap<String, Vec<CandidateContent>>> {
    let ids = source_snapshot_ids.iter().cloned().collect::<BTreeSet<_>>();
    if ids.is_empty() {
        bail!("add at least one snapshot to the treasure preview");
    }
    if ids.len() != source_snapshot_ids.len() {
        bail!("treasure preview contains duplicate snapshot IDs");
    }
    let snapshots = crate::list_local_snapshots(repository_root)?
        .into_iter()
        .map(|item| (item.snapshot_id.clone(), item))
        .collect::<BTreeMap<_, _>>();
    let mut groups = BTreeMap::<String, Vec<CandidateContent>>::new();
    for id in ids {
        let item = snapshots
            .get(&id)
            .with_context(|| format!("source snapshot no longer exists: {id}"))?;
        let (snapshot, contents) = load_v4_snapshot(&item.manifest_path, repository_root)?;
        let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
        for thread in snapshot.threads {
            let content = contents
                .get(&thread.thread_id)
                .context("snapshot thread content missing")?
                .clone();
            store.validate_content(&content)?;
            groups
                .entry(thread.thread_id.clone())
                .or_default()
                .push(CandidateContent {
                    source_snapshot_id: id.clone(),
                    semantic_hash: semantic_thread_hash(&thread)?,
                    thread,
                    content,
                });
        }
    }
    Ok(groups)
}

fn preview_from_candidates(
    source_snapshot_ids: &[String],
    groups: &BTreeMap<String, Vec<CandidateContent>>,
) -> Result<TreasurePreview> {
    let mut conflicts = Vec::new();
    for (thread_id, candidates) in groups {
        let unique = candidates
            .iter()
            .map(|candidate| candidate.semantic_hash.clone())
            .collect::<BTreeSet<_>>();
        if unique.len() > 1 {
            let mut versions = candidates
                .iter()
                .map(|candidate| TreasureCandidate {
                    source_snapshot_id: candidate.source_snapshot_id.clone(),
                    semantic_hash: candidate.semantic_hash.clone(),
                    title: candidate.thread.title.clone(),
                    updated_at_ms: candidate.thread.updated_at_ms,
                })
                .collect::<Vec<_>>();
            versions.sort_by(|a, b| {
                a.semantic_hash
                    .cmp(&b.semantic_hash)
                    .then_with(|| a.source_snapshot_id.cmp(&b.source_snapshot_id))
            });
            versions.dedup_by(|a, b| a.semantic_hash == b.semantic_hash);
            conflicts.push(TreasureConflict {
                thread_id: thread_id.clone(),
                candidates: versions,
            });
        }
    }
    let stable = groups
        .iter()
        .map(|(id, versions)| {
            (
                id,
                versions
                    .iter()
                    .map(|v| (&v.source_snapshot_id, &v.semantic_hash))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    Ok(TreasurePreview {
        source_snapshot_ids: source_snapshot_ids.to_vec(),
        thread_count: groups.len(),
        conflict_count: conflicts.len(),
        conflicts,
        fingerprint: digest_bytes_v4(&canonical_json_v4(&(source_snapshot_ids, stable))?),
    })
}

fn choose_candidates(
    groups: &BTreeMap<String, Vec<CandidateContent>>,
    preview: &TreasurePreview,
    resolutions: &[TreasureResolution],
) -> Result<Vec<CandidateContent>> {
    let selected = resolutions
        .iter()
        .map(|r| (r.thread_id.clone(), r.semantic_hash.clone()))
        .collect::<BTreeMap<_, _>>();
    if selected.len() != resolutions.len() {
        bail!("treasure conflict choices contain duplicate thread IDs");
    }
    let conflicts = preview
        .conflicts
        .iter()
        .map(|conflict| conflict.thread_id.as_str())
        .collect::<BTreeSet<_>>();
    if selected.keys().any(|id| !conflicts.contains(id.as_str())) {
        bail!("treasure conflict choice does not match current preview");
    }
    if selected.len() != conflicts.len() {
        bail!("choose a version for every conflicting conversation before exporting");
    }
    let mut result = Vec::new();
    for (thread_id, candidates) in groups {
        let hashes = candidates
            .iter()
            .map(|candidate| candidate.semantic_hash.clone())
            .collect::<BTreeSet<_>>();
        let wanted = selected
            .get(thread_id)
            .cloned()
            .or_else(|| hashes.first().cloned())
            .expect("candidate group is nonempty");
        result.push(
            candidates
                .iter()
                .find(|candidate| candidate.semantic_hash == wanted)
                .cloned()
                .context("selected treasure version no longer exists")?,
        );
    }
    Ok(result)
}

fn collect_root_objects(
    root: &SnapshotRootV4,
    store: &FilesystemContentStoreV4,
) -> Result<Vec<StorageObjectRefV4>> {
    root.validate()?;
    let mut objects = crate::collect_thread_graph_v4(&root.threads, store)?;
    let overlay_path =
        store.object_path_by_id(StorageObjectKindV4::SnapshotOverlay, &root.overlay_sha256)?;
    let overlay_bytes = fs::read(&overlay_path)?;
    if digest_bytes_v4(&overlay_bytes) != root.overlay_sha256 {
        bail!("snapshot overlay hash mismatch");
    }
    objects.insert(StorageObjectRefV4 {
        kind: StorageObjectKindV4::SnapshotOverlay,
        sha256: root.overlay_sha256.clone(),
        byte_length: overlay_bytes.len() as u64,
    });
    Ok(objects.into_iter().collect())
}

fn read_treasure_list_item(path: &Path) -> Result<TreasureListItem> {
    let header = read_treasure_header(path)?;
    Ok(TreasureListItem {
        treasure_id: header.treasure_id,
        display_name: header.display_name,
        created_at: header.created_at,
        path: path.to_path_buf(),
        thread_count: header.snapshot_root.threads.len(),
        logical_bytes: header.logical_bytes,
        object_count: header.object_count,
        file_bytes: fs::metadata(path)?.len(),
    })
}

fn read_treasure_header(path: &Path) -> Result<TreasureHeaderV1> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut magic = String::new();
    reader.read_line(&mut magic)?;
    if magic.trim_end_matches(['\r', '\n']) != TREASURE_MAGIC {
        bail!("unsupported treasure package");
    }
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.len() > MAX_TREASURE_HEADER_BYTES {
        bail!("treasure header exceeds size limit");
    }
    let header: TreasureHeaderV1 = serde_json::from_str(&line)?;
    validate_header(&header)?;
    Ok(header)
}

fn read_treasure_records(
    path: &Path,
    control: &OperationControl,
) -> Result<(TreasureHeaderV1, Vec<TreasureObjectRecord>)> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut magic = String::new();
    reader.read_line(&mut magic)?;
    if magic.trim_end_matches(['\r', '\n']) != TREASURE_MAGIC {
        bail!("unsupported treasure package");
    }
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.len() > MAX_TREASURE_HEADER_BYTES {
        bail!("treasure header exceeds size limit");
    }
    let header: TreasureHeaderV1 = serde_json::from_str(&line)?;
    validate_header(&header)?;
    let mut records = Vec::with_capacity(header.object_count);
    let mut total = 0_u64;
    loop {
        control.check_cancelled()?;
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.len() > (StorageObjectKindV4::Chunk.max_bytes() as usize * 2 + 1024) {
            bail!("treasure object record exceeds size limit");
        }
        let record: TreasureObjectRecord = serde_json::from_str(&line)?;
        if records.len() >= MAX_TREASURE_OBJECTS
            || record.object.byte_length > record.object.kind.max_bytes()
        {
            bail!("treasure contains too many or oversized objects");
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(&record.bytes_base64)?;
        if bytes.len() as u64 != record.object.byte_length
            || digest_bytes_v4(&bytes) != record.object.sha256
        {
            bail!("treasure object hash or length mismatch");
        }
        total = total
            .checked_add(record.object.byte_length)
            .context("treasure size overflow")?;
        if total > MAX_TREASURE_TOTAL_BYTES {
            bail!("treasure exceeds total size limit");
        }
        if records.iter().any(|prior: &TreasureObjectRecord| {
            prior.object.kind == record.object.kind && prior.object.sha256 == record.object.sha256
        }) {
            bail!("treasure contains duplicate objects");
        }
        records.push(record);
    }
    if records.len() != header.object_count {
        bail!("treasure object count does not match header");
    }
    Ok((header, records))
}

fn validate_header(header: &TreasureHeaderV1) -> Result<()> {
    if header.schema_version != TREASURE_SCHEMA_VERSION || header.display_name.trim().is_empty() {
        bail!("unsupported or invalid treasure header");
    }
    Uuid::parse_str(&header.treasure_id)?;
    header.snapshot_root.validate()?;
    if digest_bytes_v4(&canonical_json_v4(&header.snapshot_root)?) != header.manifest_sha256 {
        bail!("treasure snapshot manifest hash mismatch");
    }
    if header.object_count > MAX_TREASURE_OBJECTS || header.logical_bytes > MAX_TREASURE_TOTAL_BYTES
    {
        bail!("treasure header exceeds limits");
    }
    Ok(())
}

fn validate_treasure_file(path: &Path, control: &OperationControl) -> Result<()> {
    let (header, records) = read_treasure_records(path, control)?;
    let keys = records
        .iter()
        .map(|record| (record.object.kind, record.object.sha256.clone()))
        .collect::<BTreeSet<_>>();
    for reference in &header.snapshot_root.threads {
        if !keys.contains(&(
            StorageObjectKindV4::Thread,
            reference.descriptor_sha256.clone(),
        )) {
            bail!("treasure misses a thread descriptor");
        }
    }
    if !keys.contains(&(
        StorageObjectKindV4::SnapshotOverlay,
        header.snapshot_root.overlay_sha256.clone(),
    )) {
        bail!("treasure misses its overlay");
    }
    Ok(())
}

fn parse_rollout_messages(
    path: &Path,
    thread_id: &str,
    page: usize,
    page_size: usize,
) -> Result<ThreadMessagesPage> {
    let page = page.max(1);
    let page_size = page_size.clamp(1, 100);
    let start = (page - 1) * page_size;
    let end = start.saturating_add(page_size);
    let mut total_count = 0;
    let mut messages = Vec::new();
    let mut warnings = Vec::new();
    for (line_number, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = match line {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("第 {} 行读取失败：{}", line_number + 1, error));
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!(
                    "第 {} 行 JSON 无法解析：{}",
                    line_number + 1,
                    error
                ));
                continue;
            }
        };
        let Some((role, text)) = extract_message(&value) else {
            continue;
        };
        let index = total_count;
        total_count += 1;
        if index >= start && index < end {
            messages.push(ThreadMessage {
                index,
                role,
                text,
                timestamp: value
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    Ok(ThreadMessagesPage {
        thread_id: thread_id.to_string(),
        page,
        page_size,
        total_count,
        messages,
        warnings,
    })
}

fn extract_message(value: &Value) -> Option<(String, String)> {
    let payload = value.get("payload").unwrap_or(value);
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let payload_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let role = match (record_type, payload_type) {
        ("event_msg", "user_message") | (_, "user_message") => "user",
        ("event_msg", "agent_message")
        | ("event_msg", "assistant_message")
        | (_, "agent_message")
        | (_, "assistant_message") => "assistant",
        _ => return None,
    };
    let text = payload
        .get("message")
        .or_else(|| payload.get("text"))
        .and_then(Value::as_str)?;
    Some((role.to_string(), text.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tempfile::tempdir;

    use super::*;
    use crate::{ContentObject, RelatedRecords, WorkspaceRef, write_v4_snapshot};

    fn write_snapshot(repository: &Path, id: &str, title: &str, payload: &[u8]) -> String {
        let store = FilesystemContentStoreV4::open(repository.to_path_buf()).unwrap();
        let content = store
            .ingest_chunk_reader(
                &mut Cursor::new(payload.to_vec()),
                "application/x-ndjson".to_string(),
                Some("sessions/demo.jsonl".to_string()),
                &OperationControl::default(),
            )
            .unwrap();
        let thread = ThreadBundle {
            schema_version: 1,
            thread_id: "thread-1".to_string(),
            title: title.to_string(),
            archived: false,
            created_at_ms: Some(1),
            updated_at_ms: Some(2),
            model_provider: Some("openai".to_string()),
            workspace: WorkspaceRef::default(),
            rollout: ContentObject {
                sha256: content.logical_sha256.clone(),
                byte_length: content.byte_length,
                media_type: content.media_type.clone(),
                logical_path: content.logical_path.clone(),
                source_path: None,
                storage: None,
            },
            related_records: RelatedRecords::default(),
            attachments: Vec::new(),
        };
        let snapshot = LocalSnapshot {
            schema_version: crate::LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: id.to_string(),
            created_at: Utc::now().to_rfc3339(),
            threads: vec![thread],
            warning_count: 0,
        };
        write_v4_snapshot(
            &snapshot,
            &BTreeMap::from([("thread-1".to_string(), content)]),
            repository,
        )
        .unwrap();
        id.to_string()
    }

    #[test]
    fn treasure_requires_resolution_then_survives_source_loss_and_imports() {
        let source = tempdir().unwrap();
        let first = write_snapshot(source.path(), "source-a", "one", b"{\"id\":\"thread-1\"}\n");
        let second = write_snapshot(
            source.path(),
            "source-b",
            "two",
            b"{\"id\":\"thread-1\",\"changed\":true}\n",
        );
        let ids = vec![first, second];
        let preview = plan_treasure(source.path(), &ids).unwrap();
        assert_eq!(preview.conflict_count, 1);
        let rejected = export_treasure_to_vault(
            source.path(),
            "Important",
            &ids,
            &preview.fingerprint,
            &[],
            &OperationControl::default(),
        );
        assert!(rejected.is_err());
        let selected_hash = preview.conflicts[0].candidates[0].semantic_hash.clone();
        let exported = export_treasure_to_vault(
            source.path(),
            "Important",
            &ids,
            &preview.fingerprint,
            &[TreasureResolution {
                thread_id: "thread-1".to_string(),
                semantic_hash: selected_hash,
            }],
            &OperationControl::default(),
        )
        .unwrap();
        assert!(exported.treasure.path.is_file());
        let destination = tempdir().unwrap();
        let imported = import_treasure_as_snapshot(
            destination.path(),
            &exported.treasure.path,
            &OperationControl::default(),
        )
        .unwrap();
        assert_eq!(imported.snapshot.thread_count, 1);
        assert_eq!(
            crate::list_local_snapshots(destination.path())
                .unwrap()
                .len(),
            1
        );
        validate_treasure(&exported.treasure.path, &OperationControl::default()).unwrap();
    }
}
