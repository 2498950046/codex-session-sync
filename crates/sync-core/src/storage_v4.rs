//! Codex Session Sync v4 semantic object graph.
//!
//! This module intentionally has no dependency on Tauri, Axum, or the v3
//! bundle model.  It provides the immutable v4 objects and the streaming
//! rollout normalizer/materializer used by both local and remote adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::models::{
    ContentObject, LOCAL_SNAPSHOT_SCHEMA_VERSION, LocalSnapshot, RelatedRecords, ThreadBundle,
    WorkspaceRef,
};
use crate::operation::OperationControl;

pub const STORAGE_PROTOCOL_VERSION_V4: u32 = 4;
pub const NORMALIZATION_SCHEMA_VERSION_V4: u32 = 1;
pub const V4_FORMAT_MARKER: &str = "codex-session-sync-v4";
pub const PROVIDER_TOKEN_V4: &str = "__codex_session_sync_local_provider_v4__";
pub const WORKSPACE_TOKEN_V4: &str = "__codex_session_sync_local_workspace_v4__";
pub const CHUNK_SIZE_V4: u32 = 4 * 1024 * 1024;
pub const MAX_METADATA_LINE_BYTES_V4: usize = 16 * 1024 * 1024;
pub const MAX_LOGICAL_CONTENT_BYTES_V4: u64 = 64 * 1024 * 1024 * 1024;
pub const MAX_CHUNKS_PER_CONTENT_V4: usize = 16_384;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum StorageRefV4 {
    Whole { object_sha256: String },
    Chunked { manifest_sha256: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentRefV4 {
    pub logical_sha256: String,
    pub byte_length: u64,
    pub storage: StorageRefV4,
    pub media_type: String,
    pub logical_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SemanticWorkspaceV4 {
    pub logical_id: Option<String>,
    pub canonical_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SemanticThreadDescriptorV4 {
    pub schema_version: u32,
    pub normalization_schema_version: u32,
    pub thread_id: String,
    pub title: String,
    pub archived: bool,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub semantic_workspace: SemanticWorkspaceV4,
    pub rollout: ContentRefV4,
    #[serde(default)]
    pub related_records: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    pub attachments: Vec<ContentRefV4>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct LocalThreadOverlayV4 {
    pub observed_provider: Option<String>,
    pub local_workspace_path: Option<String>,
    pub local_rollout_path: Option<String>,
    #[serde(default)]
    pub source_database_hints: Vec<String>,
    #[serde(default)]
    pub local_fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotOverlayV4 {
    pub schema_version: u32,
    pub normalization_schema_version: u32,
    #[serde(default)]
    pub threads: BTreeMap<String, LocalThreadOverlayV4>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRefV4 {
    pub thread_id: String,
    pub descriptor_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRootV4 {
    pub schema_version: u32,
    pub normalization_schema_version: u32,
    pub snapshot_id: String,
    pub created_at: String,
    pub threads: Vec<ThreadRefV4>,
    pub overlay_sha256: String,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RevisionRootV4 {
    pub schema_version: u32,
    pub normalization_schema_version: u32,
    pub namespace_id: Uuid,
    pub parent_revision: Option<String>,
    pub created_at: String,
    pub threads: Vec<ThreadRefV4>,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChunkDescriptorV4 {
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChunkManifestV4 {
    pub schema_version: u32,
    pub logical_sha256: String,
    pub byte_length: u64,
    pub chunk_size: u32,
    pub chunks: Vec<ChunkDescriptorV4>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FormatMarkerV4 {
    pub format: String,
    pub storage_protocol_version: u32,
    pub normalization_schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NormalizationWarningV4 {
    pub line: u64,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedRolloutV4 {
    pub logical_sha256: String,
    pub byte_length: u64,
    pub chunks: Vec<ChunkDescriptorV4>,
    pub warnings: Vec<NormalizationWarningV4>,
    /// Populated by the byte convenience helper; streaming callers leave it
    /// empty and consume the chunk sink instead.
    #[serde(skip)]
    pub normalized_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MachineContextV4 {
    pub configured_provider: String,
    pub workspace_path: Option<String>,
    pub target_codex_home: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub enum StorageObjectKindV4 {
    Whole,
    Chunk,
    ChunkManifest,
    Thread,
    RevisionRoot,
    SnapshotOverlay,
}

impl StorageObjectKindV4 {
    pub const REMOTE: [Self; 5] = [
        Self::Whole,
        Self::Chunk,
        Self::ChunkManifest,
        Self::Thread,
        Self::RevisionRoot,
    ];

    pub const ALL: [Self; 6] = [
        Self::Whole,
        Self::Chunk,
        Self::ChunkManifest,
        Self::Thread,
        Self::RevisionRoot,
        Self::SnapshotOverlay,
    ];

    pub fn directory(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Chunk => "chunks",
            Self::ChunkManifest => "chunk-manifests",
            Self::Thread => "threads",
            Self::RevisionRoot => "revision-roots",
            Self::SnapshotOverlay => "snapshot-overlays",
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Chunk => "chunk",
            Self::ChunkManifest => "chunkManifest",
            Self::Thread => "thread",
            Self::RevisionRoot => "revisionRoot",
            Self::SnapshotOverlay => "snapshotOverlay",
        }
    }

    pub fn max_bytes(self) -> u64 {
        match self {
            Self::Whole => MAX_LOGICAL_CONTENT_BYTES_V4,
            Self::Chunk => u64::from(CHUNK_SIZE_V4),
            Self::ChunkManifest | Self::Thread | Self::RevisionRoot | Self::SnapshotOverlay => {
                16 * 1024 * 1024
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub struct StorageObjectRefV4 {
    pub kind: StorageObjectKindV4,
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalTrashPurgePlanV4 {
    pub schema_version: u32,
    pub created_at: String,
    pub operation_ids: Vec<String>,
    pub trash_entry_count: usize,
    pub candidate_objects: Vec<StorageObjectRefV4>,
    pub object_reclaimable_bytes: u64,
    pub trash_metadata_bytes: u64,
    pub retained_shared_bytes: u64,
    pub plan_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalTrashPurgeResultV4 {
    pub operation_id: String,
    pub deleted_trash_entries: usize,
    pub deleted_object_count: usize,
    pub freed_bytes: u64,
    pub retained_shared_bytes: u64,
    pub journal_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LocalTrashPurgeStatusV4 {
    Preparing,
    EntriesStaged,
    Committed,
    ObjectsStaged,
    Completed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalTrashPurgeJournalV4 {
    schema_version: u32,
    operation_id: String,
    status: LocalTrashPurgeStatusV4,
    repository_root: PathBuf,
    staging_root: PathBuf,
    plan: LocalTrashPurgePlanV4,
    created_at: String,
    updated_at: String,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FilesystemContentStoreV4 {
    root: PathBuf,
    chunk_size: u32,
}

impl FilesystemContentStoreV4 {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        initialize_v4_repository(&root)?;
        Ok(Self {
            root,
            chunk_size: CHUNK_SIZE_V4,
        })
    }

    pub fn with_chunk_size(root: impl Into<PathBuf>, chunk_size: u32) -> Result<Self> {
        if chunk_size == 0 || u64::from(chunk_size) > u64::from(CHUNK_SIZE_V4) {
            bail!("v4 chunk size must be between 1 and {CHUNK_SIZE_V4}");
        }
        let mut store = Self::open(root)?;
        store.chunk_size = chunk_size;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, object: &StorageObjectRefV4) -> Result<PathBuf> {
        typed_object_path_v4(&self.root, object.kind, &object.sha256)
    }

    pub fn object_path_by_id(&self, kind: StorageObjectKindV4, sha256: &str) -> Result<PathBuf> {
        typed_object_path_v4(&self.root, kind, sha256)
    }

    pub fn store_json<T: Serialize>(
        &self,
        kind: StorageObjectKindV4,
        value: &T,
    ) -> Result<StorageObjectRefV4> {
        let bytes = canonical_json_v4(value)?;
        if bytes.len() as u64 > kind.max_bytes() {
            bail!("{} object exceeds size limit", kind.wire_name());
        }
        self.install_bytes(
            StorageObjectRefV4 {
                kind,
                sha256: digest_bytes_v4(&bytes),
                byte_length: bytes.len() as u64,
            },
            &bytes,
        )
    }

    pub fn read_json<T: for<'de> Deserialize<'de>>(
        &self,
        kind: StorageObjectKindV4,
        sha256: &str,
    ) -> Result<T> {
        let path = self.object_path_by_id(kind, sha256)?;
        let bytes = fs::read(&path)?;
        if bytes.len() as u64 > kind.max_bytes() {
            bail!("{} object exceeds size limit", kind.wire_name());
        }
        if digest_bytes_v4(&bytes) != sha256 {
            bail!("{} object hash mismatch", kind.wire_name());
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn install_bytes(
        &self,
        object: StorageObjectRefV4,
        bytes: &[u8],
    ) -> Result<StorageObjectRefV4> {
        validate_sha256_v4(&object.sha256)?;
        if bytes.len() as u64 != object.byte_length {
            bail!("object length does not match declaration");
        }
        if bytes.len() as u64 > object.kind.max_bytes() {
            bail!("object exceeds kind size limit");
        }
        if digest_bytes_v4(bytes) != object.sha256 {
            bail!("object hash does not match declaration");
        }
        let path = self.object_path(&object)?;
        if path.is_file() {
            let existing = fs::read(&path)?;
            if existing != bytes {
                bail!("immutable object hash collision or corruption");
            }
            return Ok(object);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self
            .root
            .join("objects/tmp")
            .join(format!("{}.tmp", Uuid::now_v7()));
        fs::write(&temporary, bytes)?;
        let file = OpenOptions::new().write(true).open(&temporary)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, &path) {
            Ok(()) => {}
            Err(error) if path.exists() => {
                let _ = fs::remove_file(&temporary);
                let existing = fs::read(&path)?;
                if existing != bytes {
                    return Err(error.into());
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
        Ok(object)
    }

    pub fn ingest_normalized_file(
        &self,
        source: &Path,
        media_type: impl Into<String>,
        logical_path: Option<String>,
        control: &OperationControl,
    ) -> Result<ContentRefV4> {
        let mut input = BufReader::new(File::open(source)?);
        self.ingest_chunk_reader(&mut input, media_type.into(), logical_path, control)
    }

    /// Normalize and install a rollout directly into immutable chunks. No
    /// provider-specific or complete normalized temporary file is created.
    pub fn normalize_and_ingest_rollout(
        &self,
        source: &Path,
        thread_id: &str,
        media_type: impl Into<String>,
        logical_path: Option<String>,
        control: &OperationControl,
    ) -> Result<(ContentRefV4, Vec<NormalizationWarningV4>)> {
        let input = File::open(source)?;
        let result = normalize_rollout_reader(
            BufReader::new(input),
            thread_id,
            self.chunk_size as usize,
            |chunk| {
                let sha256 = digest_bytes_v4(chunk);
                self.install_bytes(
                    StorageObjectRefV4 {
                        kind: StorageObjectKindV4::Chunk,
                        sha256,
                        byte_length: chunk.len() as u64,
                    },
                    chunk,
                )?;
                Ok(())
            },
            control,
        )?;
        let manifest = ChunkManifestV4 {
            schema_version: 4,
            logical_sha256: result.logical_sha256.clone(),
            byte_length: result.byte_length,
            chunk_size: self.chunk_size,
            chunks: result.chunks.clone(),
        };
        let manifest_ref = self.store_json(StorageObjectKindV4::ChunkManifest, &manifest)?;
        Ok((
            ContentRefV4 {
                logical_sha256: result.logical_sha256,
                byte_length: result.byte_length,
                storage: StorageRefV4::Chunked {
                    manifest_sha256: manifest_ref.sha256,
                },
                media_type: media_type.into(),
                logical_path,
            },
            result.warnings,
        ))
    }

    pub fn ingest_chunk_reader<R: Read>(
        &self,
        input: &mut R,
        media_type: String,
        logical_path: Option<String>,
        control: &OperationControl,
    ) -> Result<ContentRefV4> {
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut chunks = Vec::new();
        let mut buffer = vec![0_u8; self.chunk_size as usize];
        loop {
            control.check_cancelled()?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            if chunks.len() >= MAX_CHUNKS_PER_CONTENT_V4 {
                bail!("content exceeds chunk limit");
            }
            let bytes = &buffer[..count];
            hasher.update(bytes);
            total = total
                .checked_add(count as u64)
                .context("content length overflow")?;
            if total > MAX_LOGICAL_CONTENT_BYTES_V4 {
                bail!("content exceeds size limit");
            }
            let sha256 = digest_bytes_v4(bytes);
            self.install_bytes(
                StorageObjectRefV4 {
                    kind: StorageObjectKindV4::Chunk,
                    sha256: sha256.clone(),
                    byte_length: count as u64,
                },
                bytes,
            )?;
            chunks.push(ChunkDescriptorV4 {
                sha256,
                byte_length: count as u64,
            });
        }
        let logical_sha256 = format!("sha256:{}", hex::encode(hasher.finalize()));
        let manifest = ChunkManifestV4 {
            schema_version: 4,
            logical_sha256: logical_sha256.clone(),
            byte_length: total,
            chunk_size: self.chunk_size,
            chunks,
        };
        manifest.validate()?;
        let manifest_ref = self.store_json(StorageObjectKindV4::ChunkManifest, &manifest)?;
        Ok(ContentRefV4 {
            logical_sha256,
            byte_length: total,
            storage: StorageRefV4::Chunked {
                manifest_sha256: manifest_ref.sha256,
            },
            media_type,
            logical_path,
        })
    }

    pub fn materialize(&self, content: &ContentRefV4, destination: &Path) -> Result<()> {
        let mut output = BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)?,
        );
        match &content.storage {
            StorageRefV4::Whole { object_sha256 } => {
                let bytes =
                    fs::read(self.object_path_by_id(StorageObjectKindV4::Whole, object_sha256)?)?;
                output.write_all(&bytes)?;
            }
            StorageRefV4::Chunked { manifest_sha256 } => {
                let manifest: ChunkManifestV4 =
                    self.read_json(StorageObjectKindV4::ChunkManifest, manifest_sha256)?;
                manifest.validate()?;
                for chunk in &manifest.chunks {
                    let bytes = fs::read(
                        self.object_path_by_id(StorageObjectKindV4::Chunk, &chunk.sha256)?,
                    )?;
                    if bytes.len() as u64 != chunk.byte_length
                        || digest_bytes_v4(&bytes) != chunk.sha256
                    {
                        bail!("storage object chunk validation failed");
                    }
                    output.write_all(&bytes)?;
                }
            }
        }
        output.flush()?;
        output.get_ref().sync_all()?;
        Ok(())
    }

    pub fn validate_content(&self, content: &ContentRefV4) -> Result<()> {
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        match &content.storage {
            StorageRefV4::Whole { object_sha256 } => {
                let bytes =
                    fs::read(self.object_path_by_id(StorageObjectKindV4::Whole, object_sha256)?)?;
                if digest_bytes_v4(&bytes) != *object_sha256 {
                    bail!("storage object whole hash mismatch");
                }
                hasher.update(&bytes);
                total = bytes.len() as u64;
            }
            StorageRefV4::Chunked { manifest_sha256 } => {
                let manifest: ChunkManifestV4 =
                    self.read_json(StorageObjectKindV4::ChunkManifest, manifest_sha256)?;
                manifest.validate()?;
                for chunk in &manifest.chunks {
                    let bytes = fs::read(
                        self.object_path_by_id(StorageObjectKindV4::Chunk, &chunk.sha256)?,
                    )?;
                    if bytes.len() as u64 != chunk.byte_length
                        || digest_bytes_v4(&bytes) != chunk.sha256
                    {
                        bail!("storage object chunk validation failed");
                    }
                    hasher.update(&bytes);
                    total = total
                        .checked_add(bytes.len() as u64)
                        .context("content length overflow")?;
                }
            }
        }
        let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
        if total != content.byte_length || actual != content.logical_sha256 {
            bail!("v4 logical content hash or length mismatch");
        }
        Ok(())
    }

    pub fn content_objects_present(&self, content: &ContentRefV4) -> Result<bool> {
        match &content.storage {
            StorageRefV4::Whole { object_sha256 } => Ok(self
                .object_path_by_id(StorageObjectKindV4::Whole, object_sha256)?
                .is_file()),
            StorageRefV4::Chunked { manifest_sha256 } => {
                let manifest_path =
                    self.object_path_by_id(StorageObjectKindV4::ChunkManifest, manifest_sha256)?;
                if !manifest_path.is_file() {
                    return Ok(false);
                }
                let manifest: ChunkManifestV4 =
                    match self.read_json(StorageObjectKindV4::ChunkManifest, manifest_sha256) {
                        Ok(manifest) => manifest,
                        Err(_) => return Ok(false),
                    };
                Ok(manifest.chunks.iter().all(|chunk| {
                    self.object_path_by_id(StorageObjectKindV4::Chunk, &chunk.sha256)
                        .map(|path| path.is_file())
                        .unwrap_or(false)
                }))
            }
        }
    }

    pub fn content_objects(&self, content: &ContentRefV4) -> Result<Vec<StorageObjectRefV4>> {
        validate_content_ref_v4(content)?;
        match &content.storage {
            StorageRefV4::Whole { object_sha256 } => Ok(vec![StorageObjectRefV4 {
                kind: StorageObjectKindV4::Whole,
                sha256: object_sha256.clone(),
                byte_length: content.byte_length,
            }]),
            StorageRefV4::Chunked { manifest_sha256 } => {
                let manifest: ChunkManifestV4 =
                    self.read_json(StorageObjectKindV4::ChunkManifest, manifest_sha256)?;
                manifest.validate()?;
                if manifest.logical_sha256 != content.logical_sha256
                    || manifest.byte_length != content.byte_length
                {
                    bail!("v4 chunk manifest identity mismatch");
                }
                let manifest_bytes = canonical_json_v4(&manifest)?;
                let mut objects = vec![StorageObjectRefV4 {
                    kind: StorageObjectKindV4::ChunkManifest,
                    sha256: manifest_sha256.clone(),
                    byte_length: manifest_bytes.len() as u64,
                }];
                objects.extend(manifest.chunks.into_iter().map(|chunk| StorageObjectRefV4 {
                    kind: StorageObjectKindV4::Chunk,
                    sha256: chunk.sha256,
                    byte_length: chunk.byte_length,
                }));
                Ok(objects)
            }
        }
    }
}

impl ChunkManifestV4 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 4 || self.chunk_size == 0 || self.chunk_size > CHUNK_SIZE_V4 {
            bail!("unsupported v4 chunk manifest");
        }
        validate_sha256_v4(&self.logical_sha256)?;
        let mut total = 0_u64;
        for (index, chunk) in self.chunks.iter().enumerate() {
            validate_sha256_v4(&chunk.sha256)?;
            if index + 1 < self.chunks.len() && chunk.byte_length != u64::from(self.chunk_size) {
                bail!("non-final chunk has an invalid length");
            }
            if chunk.byte_length == 0 || chunk.byte_length > u64::from(self.chunk_size) {
                bail!("chunk has an invalid length");
            }
            total = total
                .checked_add(chunk.byte_length)
                .context("chunk length overflow")?;
        }
        if total != self.byte_length {
            bail!("chunk manifest length mismatch");
        }
        if self.byte_length == 0 && !self.chunks.is_empty() {
            bail!("empty content cannot have chunks");
        }
        Ok(())
    }
}

pub fn typed_object_path_v4(
    root: &Path,
    kind: StorageObjectKindV4,
    sha256: &str,
) -> Result<PathBuf> {
    validate_sha256_v4(sha256)?;
    let digest = &sha256[7..];
    Ok(root
        .join("objects")
        .join(kind.directory())
        .join("sha256")
        .join(&digest[..2])
        .join(&digest[2..]))
}

fn collect_snapshot_graph_objects_v4(
    store: &FilesystemContentStoreV4,
    root: &SnapshotRootV4,
) -> Result<BTreeSet<StorageObjectRefV4>> {
    root.validate()?;
    let mut objects = BTreeSet::new();
    let overlay_path =
        store.object_path_by_id(StorageObjectKindV4::SnapshotOverlay, &root.overlay_sha256)?;
    objects.insert(StorageObjectRefV4 {
        kind: StorageObjectKindV4::SnapshotOverlay,
        sha256: root.overlay_sha256.clone(),
        byte_length: fs::metadata(&overlay_path)
            .with_context(|| format!("missing snapshot overlay {}", overlay_path.display()))?
            .len(),
    });
    for reference in &root.threads {
        let descriptor: SemanticThreadDescriptorV4 =
            store.read_json(StorageObjectKindV4::Thread, &reference.descriptor_sha256)?;
        descriptor.validate()?;
        let descriptor_path =
            store.object_path_by_id(StorageObjectKindV4::Thread, &reference.descriptor_sha256)?;
        objects.insert(StorageObjectRefV4 {
            kind: StorageObjectKindV4::Thread,
            sha256: reference.descriptor_sha256.clone(),
            byte_length: fs::metadata(descriptor_path)?.len(),
        });
        for content in std::iter::once(&descriptor.rollout).chain(descriptor.attachments.iter()) {
            objects.extend(store.content_objects(content)?);
        }
    }
    Ok(objects)
}

fn collect_revision_graph_objects_v4(
    store: &FilesystemContentStoreV4,
    revision_id: &str,
) -> Result<BTreeSet<StorageObjectRefV4>> {
    let root: RevisionRootV4 = store.read_json(StorageObjectKindV4::RevisionRoot, revision_id)?;
    root.validate()?;
    let root_path = store.object_path_by_id(StorageObjectKindV4::RevisionRoot, revision_id)?;
    let mut objects = BTreeSet::from([StorageObjectRefV4 {
        kind: StorageObjectKindV4::RevisionRoot,
        sha256: revision_id.to_string(),
        byte_length: fs::metadata(root_path)?.len(),
    }]);
    for reference in &root.threads {
        let descriptor: SemanticThreadDescriptorV4 =
            store.read_json(StorageObjectKindV4::Thread, &reference.descriptor_sha256)?;
        descriptor.validate()?;
        let descriptor_path =
            store.object_path_by_id(StorageObjectKindV4::Thread, &reference.descriptor_sha256)?;
        objects.insert(StorageObjectRefV4 {
            kind: StorageObjectKindV4::Thread,
            sha256: reference.descriptor_sha256.clone(),
            byte_length: fs::metadata(descriptor_path)?.len(),
        });
        for content in std::iter::once(&descriptor.rollout).chain(descriptor.attachments.iter()) {
            objects.extend(store.content_objects(content)?);
        }
    }
    Ok(objects)
}

fn enumerate_repository_objects_v4(
    repository_root: &Path,
    kinds: impl IntoIterator<Item = StorageObjectKindV4>,
) -> Result<BTreeSet<StorageObjectRefV4>> {
    let mut objects = BTreeSet::new();
    for kind in kinds {
        let root = repository_root
            .join("objects")
            .join(kind.directory())
            .join("sha256");
        if !root.exists() {
            continue;
        }
        for prefix in fs::read_dir(root)? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                continue;
            }
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            if prefix_name.len() != 2 {
                bail!("invalid v4 object prefix directory {prefix_name}");
            }
            for entry in fs::read_dir(prefix.path())? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_symlink() || !file_type.is_file() {
                    bail!("v4 object store contains a non-regular object");
                }
                let sha256 = format!(
                    "sha256:{prefix_name}{}",
                    entry.file_name().to_string_lossy()
                );
                validate_sha256_v4(&sha256)?;
                objects.insert(StorageObjectRefV4 {
                    kind,
                    sha256,
                    byte_length: entry.metadata()?.len(),
                });
            }
        }
    }
    Ok(objects)
}

fn load_local_trash_roots_v4(
    repository_root: &Path,
) -> Result<BTreeMap<String, (PathBuf, SnapshotRootV4)>> {
    let directory = repository_root.join("trash/snapshots");
    let mut roots = BTreeMap::new();
    if !directory.exists() {
        return Ok(roots);
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let operation_id = entry.file_name().to_string_lossy().into_owned();
        Uuid::parse_str(&operation_id).context("invalid local trash operation ID")?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            bail!("local trash entry must be a regular directory");
        }
        let path = entry.path();
        let manifest_path = path.join("snapshot.json");
        let entry_path = path.join("entry.json");
        let trash_entry: crate::storage_v3::SnapshotTrashEntry =
            read_bounded_v4(&entry_path, 16 * 1024 * 1024)?;
        if trash_entry.operation_id != operation_id {
            bail!("local trash entry operation ID mismatch");
        }
        let root: SnapshotRootV4 = read_bounded_v4(&manifest_path, 16 * 1024 * 1024)?;
        root.validate()?;
        if root.snapshot_id != trash_entry.snapshot_id {
            bail!("local trash snapshot ID mismatch");
        }
        roots.insert(operation_id, (path, root));
    }
    Ok(roots)
}

fn non_terminal_repository_journals_v4(repository_root: &Path) -> Result<Vec<PathBuf>> {
    let directory = repository_root.join("journal");
    let mut journals = Vec::new();
    if !directory.exists() {
        return Ok(journals);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            continue;
        }
        let value: Value = read_bounded_v4(&path, 16 * 1024 * 1024)?;
        let Some(status) = value.get("status").and_then(Value::as_str) else {
            continue;
        };
        if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with("local-trash-purge-"))
        {
            continue;
        }
        let status = status.to_ascii_lowercase();
        if !matches!(status.as_str(), "completed" | "rolled_back" | "rolledback") {
            journals.push(path);
        }
    }
    Ok(journals)
}

fn collect_reachable_objects_v4(
    repository_root: &Path,
    excluded_trash_operations: &BTreeSet<String>,
) -> Result<BTreeSet<StorageObjectRefV4>> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let mut reachable = BTreeSet::new();
    let snapshot_directory = repository_root.join("snapshots");
    if snapshot_directory.exists() {
        for entry in fs::read_dir(snapshot_directory)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                let root: SnapshotRootV4 = read_bounded_v4(&path, 16 * 1024 * 1024)?;
                reachable.extend(collect_snapshot_graph_objects_v4(&store, &root)?);
            }
        }
    }
    for (operation_id, (_, root)) in load_local_trash_roots_v4(repository_root)? {
        if !excluded_trash_operations.contains(&operation_id) {
            reachable.extend(collect_snapshot_graph_objects_v4(&store, &root)?);
        }
    }
    let revision_dir = repository_root.join("objects/revision-roots/sha256");
    if revision_dir.exists() {
        for prefix in fs::read_dir(revision_dir)? {
            let prefix = prefix?.path();
            if !prefix.is_dir() {
                continue;
            }
            for entry in fs::read_dir(prefix)? {
                let path = entry?.path();
                if !path.is_file() {
                    continue;
                }
                let digest = format!(
                    "sha256:{}{}",
                    path.parent()
                        .and_then(|parent| parent.file_name())
                        .unwrap()
                        .to_string_lossy(),
                    path.file_name().unwrap().to_string_lossy()
                );
                reachable.extend(collect_revision_graph_objects_v4(&store, &digest)?);
            }
        }
    }
    Ok(reachable)
}

/// Build a conservative local GC plan for a v4 repository. Snapshot overlays
/// stay local-only, so the legacy GC response omits them; permanent trash
/// purge uses the native v4 planner below and includes every v4 object kind.
pub fn plan_local_gc_v4(repository_root: &Path) -> Result<crate::storage_v3::GcPlan> {
    let reachable_v4 = collect_reachable_objects_v4(repository_root, &BTreeSet::new())?;
    let all = enumerate_repository_objects_v4(repository_root, StorageObjectKindV4::REMOTE)?;
    let mut unreachable = all
        .difference(&reachable_v4)
        .filter_map(|object| {
            storage_kind_v3(object.kind).map(|kind| crate::storage_v3::StorageObjectRef {
                kind,
                sha256: object.sha256.clone(),
                byte_length: object.byte_length,
            })
        })
        .collect::<Vec<_>>();
    unreachable.sort();
    let reclaimable_bytes = unreachable.iter().try_fold(0_u64, |total, object| {
        total
            .checked_add(object.byte_length)
            .context("GC byte count overflow")
    })?;
    Ok(crate::storage_v3::GcPlan {
        schema_version: 4,
        created_at: chrono::Utc::now().to_rfc3339(),
        reachable_objects: reachable_v4.len(),
        unreachable_objects: unreachable,
        reclaimable_bytes,
    })
}

pub fn plan_local_trash_purge_v4(
    repository_root: &Path,
    requested_operation_ids: &[String],
    purge_all: bool,
) -> Result<LocalTrashPurgePlanV4> {
    if !repository_root.join("format.json").is_file() {
        bail!("permanent local trash deletion requires a v4 repository");
    }
    recover_local_trash_purges_v4(repository_root)?;
    let active_journals = non_terminal_repository_journals_v4(repository_root)?;
    if !active_journals.is_empty() {
        bail!(
            "local trash cannot be permanently deleted while {} recovery operation(s) require attention",
            active_journals.len()
        );
    }
    if purge_all && !requested_operation_ids.is_empty() {
        bail!("purge-all cannot be combined with explicit trash operation IDs");
    }
    if !purge_all && requested_operation_ids.is_empty() {
        bail!("at least one local trash operation ID is required");
    }
    let trash_roots = load_local_trash_roots_v4(repository_root)?;
    let mut operation_ids = if purge_all {
        trash_roots.keys().cloned().collect::<Vec<_>>()
    } else {
        requested_operation_ids.to_vec()
    };
    operation_ids.sort();
    operation_ids.dedup();
    if operation_ids.is_empty() {
        bail!("local snapshot trash is empty");
    }
    let selected = operation_ids.iter().cloned().collect::<BTreeSet<_>>();
    if selected.len() != requested_operation_ids.len() && !purge_all {
        bail!("local trash purge request contains duplicate operation IDs");
    }
    for operation_id in &selected {
        Uuid::parse_str(operation_id).context("invalid local trash operation ID")?;
        if !trash_roots.contains_key(operation_id) {
            bail!("local trash operation not found: {operation_id}");
        }
    }

    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let mut selected_objects = BTreeSet::new();
    let mut selected_roots = BTreeMap::new();
    let mut trash_metadata_bytes = 0_u64;
    for operation_id in &operation_ids {
        let (directory, root) = trash_roots
            .get(operation_id)
            .context("selected local trash operation disappeared")?;
        selected_objects.extend(collect_snapshot_graph_objects_v4(&store, root)?);
        selected_roots.insert(operation_id.clone(), root.clone());
        trash_metadata_bytes = trash_metadata_bytes
            .checked_add(directory_bytes_v4(directory)?)
            .context("local trash metadata byte count overflow")?;
    }
    let reachable = collect_reachable_objects_v4(repository_root, &selected)?;
    let all_objects = enumerate_repository_objects_v4(repository_root, StorageObjectKindV4::ALL)?;
    let candidate_objects = all_objects
        .difference(&reachable)
        .cloned()
        .collect::<Vec<_>>();
    let object_reclaimable_bytes = candidate_objects.iter().try_fold(0_u64, |total, object| {
        total
            .checked_add(object.byte_length)
            .context("local purge byte count overflow")
    })?;
    let retained_shared_bytes =
        selected_objects
            .intersection(&reachable)
            .try_fold(0_u64, |total, object| {
                total
                    .checked_add(object.byte_length)
                    .context("shared local purge byte count overflow")
            })?;
    let plan_fingerprint = digest_bytes_v4(&canonical_json_v4(&(
        &operation_ids,
        &selected_roots,
        &candidate_objects,
        trash_metadata_bytes,
        retained_shared_bytes,
    ))?);
    Ok(LocalTrashPurgePlanV4 {
        schema_version: 1,
        created_at: chrono::Utc::now().to_rfc3339(),
        operation_ids,
        trash_entry_count: selected.len(),
        candidate_objects,
        object_reclaimable_bytes,
        trash_metadata_bytes,
        retained_shared_bytes,
        plan_fingerprint,
    })
}

pub fn purge_local_trash_v4(
    repository_root: &Path,
    expected: &LocalTrashPurgePlanV4,
) -> Result<LocalTrashPurgeResultV4> {
    recover_local_trash_purges_v4(repository_root)?;
    let current = plan_local_trash_purge_v4(repository_root, &expected.operation_ids, false)?;
    if current.plan_fingerprint != expected.plan_fingerprint {
        bail!("local trash purge plan became stale");
    }
    let operation_id = Uuid::now_v7().to_string();
    let staging_root = repository_root
        .join("quarantine/trash-purge")
        .join(&operation_id);
    let journal_path = repository_root
        .join("journal")
        .join(format!("local-trash-purge-{operation_id}.json"));
    fs::create_dir_all(staging_root.join("entries"))?;
    fs::create_dir_all(staging_root.join("objects"))?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut journal = LocalTrashPurgeJournalV4 {
        schema_version: 1,
        operation_id: operation_id.clone(),
        status: LocalTrashPurgeStatusV4::Preparing,
        repository_root: repository_root.to_path_buf(),
        staging_root: staging_root.clone(),
        plan: current,
        created_at: now.clone(),
        updated_at: now,
        error: None,
    };
    write_v4_root(&journal_path, &journal)?;

    let stage_result = (|| -> Result<()> {
        for trash_operation_id in &journal.plan.operation_ids {
            let source = repository_root
                .join("trash/snapshots")
                .join(trash_operation_id);
            let destination = staging_root.join("entries").join(trash_operation_id);
            fs::rename(&source, &destination).with_context(|| {
                format!("failed to stage local trash entry {trash_operation_id}")
            })?;
        }
        journal.status = LocalTrashPurgeStatusV4::EntriesStaged;
        journal.updated_at = chrono::Utc::now().to_rfc3339();
        write_v4_root(&journal_path, &journal)?;
        Ok(())
    })();
    if let Err(error) = stage_result {
        rollback_local_trash_purge_v4(repository_root, &journal_path, &mut journal)?;
        return Err(error);
    }

    journal.status = LocalTrashPurgeStatusV4::Committed;
    journal.updated_at = chrono::Utc::now().to_rfc3339();
    write_v4_root(&journal_path, &journal)?;
    match finish_local_trash_purge_v4(&journal_path, &mut journal) {
        Ok((deleted_object_count, freed_object_bytes)) => Ok(LocalTrashPurgeResultV4 {
            operation_id,
            deleted_trash_entries: journal.plan.trash_entry_count,
            deleted_object_count,
            freed_bytes: freed_object_bytes.saturating_add(journal.plan.trash_metadata_bytes),
            retained_shared_bytes: journal.plan.retained_shared_bytes,
            journal_path,
        }),
        Err(error) => {
            journal.error = Some(format!("{error:#}"));
            journal.updated_at = chrono::Utc::now().to_rfc3339();
            let _ = write_v4_root(&journal_path, &journal);
            Err(error)
        }
    }
}

pub fn recover_local_trash_purges_v4(repository_root: &Path) -> Result<()> {
    let directory = repository_root.join("journal");
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name.starts_with("local-trash-purge-") || !name.ends_with(".json") {
            continue;
        }
        let mut journal: LocalTrashPurgeJournalV4 = read_bounded_v4(&path, 16 * 1024 * 1024)?;
        if journal.schema_version != 1 || journal.repository_root != repository_root {
            bail!("invalid local trash purge recovery journal");
        }
        match journal.status {
            LocalTrashPurgeStatusV4::Preparing | LocalTrashPurgeStatusV4::EntriesStaged => {
                rollback_local_trash_purge_v4(repository_root, &path, &mut journal)?;
            }
            LocalTrashPurgeStatusV4::Committed | LocalTrashPurgeStatusV4::ObjectsStaged => {
                finish_local_trash_purge_v4(&path, &mut journal)?;
            }
            LocalTrashPurgeStatusV4::Completed | LocalTrashPurgeStatusV4::RolledBack => {}
        }
    }
    Ok(())
}

fn rollback_local_trash_purge_v4(
    repository_root: &Path,
    journal_path: &Path,
    journal: &mut LocalTrashPurgeJournalV4,
) -> Result<()> {
    for operation_id in journal.plan.operation_ids.iter().rev() {
        let staged = journal.staging_root.join("entries").join(operation_id);
        let original = repository_root.join("trash/snapshots").join(operation_id);
        if staged.exists() && !original.exists() {
            fs::rename(staged, original)?;
        } else if staged.exists() && original.exists() {
            bail!("cannot roll back local trash purge because both entry paths exist");
        }
    }
    journal.status = LocalTrashPurgeStatusV4::RolledBack;
    journal.updated_at = chrono::Utc::now().to_rfc3339();
    write_v4_root(journal_path, journal)?;
    if journal.staging_root.exists() {
        fs::remove_dir_all(&journal.staging_root)?;
    }
    Ok(())
}

fn finish_local_trash_purge_v4(
    journal_path: &Path,
    journal: &mut LocalTrashPurgeJournalV4,
) -> Result<(usize, u64)> {
    let repository_root = &journal.repository_root;
    let reachable = collect_reachable_objects_v4(repository_root, &BTreeSet::new())?;
    let mut deleted_object_count = 0_usize;
    let mut freed_object_bytes = 0_u64;
    for object in &journal.plan.candidate_objects {
        let source = typed_object_path_v4(repository_root, object.kind, &object.sha256)?;
        let digest = object
            .sha256
            .strip_prefix("sha256:")
            .expect("validated hash");
        let destination = journal
            .staging_root
            .join("objects")
            .join(object.kind.directory())
            .join(digest);
        if source.exists() {
            let observed = fs::metadata(&source)?;
            if !observed.is_file() || observed.len() != object.byte_length {
                bail!(
                    "local purge object changed after planning: {}",
                    object.sha256
                );
            }
            if reachable.contains(object) {
                continue;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&source, &destination)?;
        }
        if destination.is_file() {
            let observed = fs::metadata(&destination)?.len();
            fs::remove_file(&destination)?;
            deleted_object_count += 1;
            freed_object_bytes = freed_object_bytes
                .checked_add(observed)
                .context("local purge result byte count overflow")?;
        }
    }
    journal.status = LocalTrashPurgeStatusV4::ObjectsStaged;
    journal.updated_at = chrono::Utc::now().to_rfc3339();
    write_v4_root(journal_path, journal)?;
    let entries = journal.staging_root.join("entries");
    if entries.exists() {
        fs::remove_dir_all(entries)?;
    }
    let objects = journal.staging_root.join("objects");
    if objects.exists() {
        fs::remove_dir_all(objects)?;
    }
    if journal.staging_root.exists() {
        fs::remove_dir(&journal.staging_root)?;
    }
    crate::local::invalidate_source_object_index(repository_root)?;
    journal.status = LocalTrashPurgeStatusV4::Completed;
    journal.updated_at = chrono::Utc::now().to_rfc3339();
    journal.error = None;
    write_v4_root(journal_path, journal)?;
    Ok((deleted_object_count, freed_object_bytes))
}

pub fn repository_storage_summary_v4(
    repository_root: &Path,
) -> Result<crate::storage_v3::RepositoryStorageSummary> {
    let plan = plan_local_gc_v4(repository_root)?;
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let mut logical_bytes = 0_u64;
    let snapshots = repository_root.join("snapshots");
    if snapshots.exists() {
        for entry in fs::read_dir(snapshots)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let root: SnapshotRootV4 = serde_json::from_slice(&fs::read(path)?)?;
            for reference in root.threads {
                let descriptor = store.load_descriptor(&reference)?;
                logical_bytes = logical_bytes
                    .checked_add(descriptor.rollout.byte_length)
                    .and_then(|total| {
                        descriptor
                            .attachments
                            .iter()
                            .try_fold(total, |total, content| {
                                total.checked_add(content.byte_length)
                            })
                    })
                    .context("logical storage byte count overflow")?;
            }
        }
    }
    let repository_physical_bytes = directory_bytes_v4(&repository_root.join("objects"))?;
    let trash_bytes = directory_bytes_v4(&repository_root.join("trash"))?;
    let quarantine_bytes = directory_bytes_v4(&repository_root.join("quarantine"))?;
    let backup_bytes = directory_bytes_v4(&repository_root.join("backups"))?;
    let reclaimable_bytes = plan.reclaimable_bytes;
    let active_physical_bytes = repository_physical_bytes.saturating_sub(reclaimable_bytes);
    Ok(crate::storage_v3::RepositoryStorageSummary {
        logical_bytes,
        repository_physical_bytes,
        active_physical_bytes,
        shared_physical_bytes: 0,
        exclusive_physical_bytes: active_physical_bytes,
        trash_bytes,
        gc_quarantine_bytes: quarantine_bytes,
        reclaimable_bytes,
        protected_by_journal_bytes: backup_bytes,
    })
}

pub fn list_local_snapshots_v4(
    repository_root: &Path,
) -> Result<Vec<crate::storage_v3::LocalSnapshotListItem>> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let directory = repository_root.join("snapshots");
    let mut items = Vec::new();
    if !directory.exists() {
        return Ok(items);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let root: SnapshotRootV4 = serde_json::from_slice(&fs::read(&path)?)?;
        root.validate()?;
        let mut objects = BTreeSet::<StorageObjectRefV4>::new();
        let mut logical_bytes = 0_u64;
        for reference in &root.threads {
            let descriptor = store.load_descriptor(reference)?;
            let descriptor_path = store
                .object_path_by_id(StorageObjectKindV4::Thread, &reference.descriptor_sha256)?;
            objects.insert(StorageObjectRefV4 {
                kind: StorageObjectKindV4::Thread,
                sha256: reference.descriptor_sha256.clone(),
                byte_length: fs::metadata(descriptor_path)?.len(),
            });
            logical_bytes = logical_bytes
                .checked_add(descriptor.rollout.byte_length)
                .context("snapshot logical byte count overflow")?;
            for content in std::iter::once(&descriptor.rollout).chain(descriptor.attachments.iter())
            {
                for object in store.content_objects(content)? {
                    objects.insert(object);
                }
            }
        }
        let overlay_path =
            store.object_path_by_id(StorageObjectKindV4::SnapshotOverlay, &root.overlay_sha256)?;
        let overlay_bytes = fs::metadata(overlay_path)?.len();
        let physical_referenced_bytes =
            objects.iter().try_fold(overlay_bytes, |total, object| {
                total
                    .checked_add(object.byte_length)
                    .context("snapshot physical byte count overflow")
            })?;
        let metadata_path = repository_root
            .join("metadata/snapshots")
            .join(format!("{}.json", root.snapshot_id));
        let metadata = if metadata_path.is_file() {
            serde_json::from_slice(&fs::read(metadata_path)?)?
        } else {
            crate::storage_v3::SnapshotMetadata::default()
        };
        items.push(crate::storage_v3::LocalSnapshotListItem {
            snapshot_id: root.snapshot_id,
            created_at: root.created_at,
            manifest_path: path,
            thread_count: root.threads.len(),
            object_count: objects.len() + 1,
            logical_bytes,
            physical_referenced_bytes,
            warning_count: root.warning_count,
            metadata,
        });
    }
    items.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.snapshot_id.cmp(&left.snapshot_id))
    });
    Ok(items)
}

pub fn plan_snapshot_deletion_v4(
    repository_root: &Path,
    snapshot_id: &str,
) -> Result<crate::storage_v3::SnapshotDeletionPlan> {
    let manifest_path = repository_root
        .join("snapshots")
        .join(format!("{snapshot_id}.json"));
    if !manifest_path.is_file() {
        bail!("snapshot not found: {snapshot_id}");
    }
    let snapshots = list_local_snapshots_v4(repository_root)?;
    let target = snapshots
        .iter()
        .find(|item| item.snapshot_id == snapshot_id)
        .context("snapshot not found")?;
    let protected_by_operations = Vec::new();
    let fingerprint = digest_bytes_v4(&canonical_json_v4(&(
        snapshot_id,
        target.physical_referenced_bytes,
        &protected_by_operations,
    ))?);
    Ok(crate::storage_v3::SnapshotDeletionPlan {
        snapshot_id: snapshot_id.to_string(),
        manifest_path,
        pinned: target.metadata.pinned,
        protected_by_operations,
        shared_object_count: 0,
        exclusive_object_count: target.object_count,
        estimated_reclaimable_bytes: target.physical_referenced_bytes,
        plan_fingerprint: fingerprint,
    })
}

fn directory_bytes_v4(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total = total
                    .checked_add(metadata.len())
                    .context("storage byte count overflow")?;
            }
        }
    }
    Ok(total)
}

fn storage_kind_v3(kind: StorageObjectKindV4) -> Option<crate::storage_v3::StorageObjectKind> {
    Some(match kind {
        StorageObjectKindV4::Whole => crate::storage_v3::StorageObjectKind::Whole,
        StorageObjectKindV4::Chunk => crate::storage_v3::StorageObjectKind::Chunk,
        StorageObjectKindV4::ChunkManifest => crate::storage_v3::StorageObjectKind::ChunkManifest,
        StorageObjectKindV4::Thread => crate::storage_v3::StorageObjectKind::Thread,
        StorageObjectKindV4::RevisionRoot => crate::storage_v3::StorageObjectKind::RevisionRoot,
        StorageObjectKindV4::SnapshotOverlay => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRecordState {
    Missing,
    Found,
}

/// Validate the v4 provider identifier, including the reserved normalizer
/// token which must never be user-configurable.
pub fn validate_provider_id_v4(provider: &str) -> Result<&str> {
    let provider = provider.trim();
    if provider.is_empty() || provider.len() > 128 {
        bail!("provider ID must contain between 1 and 128 characters");
    }
    if provider == PROVIDER_TOKEN_V4 || provider == WORKSPACE_TOKEN_V4 {
        bail!("provider ID uses a reserved v4 normalization token");
    }
    if !provider
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("provider ID contains unsupported characters");
    }
    Ok(provider)
}

/// Normalize a rollout while preserving unknown/non-session records byte for
/// byte. The callback receives output chunks at most `chunk_size` bytes.
pub fn normalize_rollout_reader<R: BufRead, F: FnMut(&[u8]) -> Result<()>>(
    mut reader: R,
    thread_id: &str,
    chunk_size: usize,
    mut sink: F,
    control: &OperationControl,
) -> Result<NormalizedRolloutV4> {
    if thread_id.trim().is_empty() {
        bail!("thread ID must not be empty");
    }
    if chunk_size == 0 {
        bail!("chunk size must not be zero");
    }
    let mut logical_hasher = Sha256::new();
    let mut total = 0_u64;
    let mut chunks = Vec::new();
    let mut pending = Vec::with_capacity(chunk_size);
    let mut warnings = Vec::new();
    let mut line = Vec::new();
    let mut line_number = 0_u64;
    let mut first = FirstRecordState::Missing;
    loop {
        control.check_cancelled()?;
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        line_number += 1;
        if line.len() > MAX_METADATA_LINE_BYTES_V4 {
            if first == FirstRecordState::Missing {
                bail!(
                    "rollout metadata line exceeds {} bytes",
                    MAX_METADATA_LINE_BYTES_V4
                );
            }
            warnings.push(NormalizationWarningV4 {
                line: line_number,
                kind: "oversized_line".to_string(),
                message: "line was preserved without parsing".to_string(),
            });
            emit_bytes(
                &line,
                chunk_size,
                &mut pending,
                &mut logical_hasher,
                &mut total,
                &mut chunks,
                &mut sink,
            )?;
            continue;
        }
        let (body, ending) = split_line_ending(&line);
        let parsed = serde_json::from_slice::<Value>(body);
        let transformed = match parsed {
            Ok(mut value) if value.get("type").and_then(Value::as_str) == Some("session_meta") => {
                if let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) {
                    if first == FirstRecordState::Missing {
                        if payload.get("id").and_then(Value::as_str) != Some(thread_id) {
                            bail!("rollout session_meta thread ID does not match {thread_id}");
                        }
                        first = FirstRecordState::Found;
                        payload.insert(
                            "cwd".to_string(),
                            Value::String(WORKSPACE_TOKEN_V4.to_string()),
                        );
                    }
                    payload.insert(
                        "model_provider".to_string(),
                        Value::String(PROVIDER_TOKEN_V4.to_string()),
                    );
                    let mut bytes = canonical_json_v4(&value)?;
                    bytes.extend_from_slice(ending);
                    Some(bytes)
                } else {
                    if first == FirstRecordState::Missing {
                        bail!("rollout session_meta payload is not an object");
                    }
                    warnings.push(NormalizationWarningV4 {
                        line: line_number,
                        kind: "invalid_session_meta".to_string(),
                        message: "payload is not an object".to_string(),
                    });
                    None
                }
            }
            Ok(_) => {
                if first == FirstRecordState::Missing {
                    bail!("first valid rollout record must be session_meta");
                }
                None
            }
            Err(error) => {
                if first == FirstRecordState::Missing {
                    bail!("first rollout record is not valid JSON: {error}");
                }
                warnings.push(NormalizationWarningV4 {
                    line: line_number,
                    kind: "invalid_json".to_string(),
                    message: "malformed JSON preserved byte-for-byte".to_string(),
                });
                None
            }
        };
        let bytes = transformed.as_deref().unwrap_or(&line);
        emit_bytes(
            bytes,
            chunk_size,
            &mut pending,
            &mut logical_hasher,
            &mut total,
            &mut chunks,
            &mut sink,
        )?;
    }
    if first == FirstRecordState::Missing {
        bail!("rollout has no valid session_meta record");
    }
    if !pending.is_empty() {
        emit_chunk(
            &pending,
            &mut logical_hasher,
            &mut total,
            &mut chunks,
            &mut sink,
        )?;
    }
    if total > MAX_LOGICAL_CONTENT_BYTES_V4 {
        bail!("normalized rollout exceeds {MAX_LOGICAL_CONTENT_BYTES_V4} bytes");
    }
    Ok(NormalizedRolloutV4 {
        logical_sha256: format!("sha256:{}", hex::encode(logical_hasher.finalize())),
        byte_length: total,
        chunks,
        warnings,
        normalized_bytes: Vec::new(),
    })
}

pub fn normalize_rollout_bytes(bytes: &[u8], thread_id: &str) -> Result<NormalizedRolloutV4> {
    let mut output = Vec::new();
    let mut result = normalize_rollout_reader(
        BufReader::new(bytes),
        thread_id,
        CHUNK_SIZE_V4 as usize,
        |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        },
        &OperationControl::default(),
    )?;
    result.normalized_bytes = output;
    Ok(result)
}

pub fn normalize_rollout_file(
    source: &Path,
    destination: &Path,
    thread_id: &str,
    control: &OperationControl,
) -> Result<NormalizedRolloutV4> {
    let input = File::open(source)
        .with_context(|| format!("failed to open rollout {}", source.display()))?;
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut writer = BufWriter::new(output);
    let result = normalize_rollout_reader(
        BufReader::new(input),
        thread_id,
        CHUNK_SIZE_V4 as usize,
        |chunk| writer.write_all(chunk).map_err(Into::into),
        control,
    );
    if result.is_ok() {
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    result
}

pub fn materialize_rollout_bytes(
    normalized: &[u8],
    thread_id: &str,
    context: &MachineContextV4,
) -> Result<Vec<u8>> {
    validate_provider_id_v4(&context.configured_provider)?;
    let mut output = Vec::new();
    materialize_rollout_reader(
        BufReader::new(normalized),
        thread_id,
        context,
        |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        },
        &OperationControl::default(),
    )?;
    Ok(output)
}

pub fn materialize_rollout_file(
    source: &Path,
    destination: &Path,
    thread_id: &str,
    context: &MachineContextV4,
    control: &OperationControl,
) -> Result<()> {
    validate_provider_id_v4(&context.configured_provider)?;
    let input = File::open(source)?;
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut writer = BufWriter::new(output);
    let result = materialize_rollout_reader(
        BufReader::new(input),
        thread_id,
        context,
        |chunk| writer.write_all(chunk).map_err(Into::into),
        control,
    );
    if result.is_ok() {
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    result
}

fn materialize_rollout_reader<R: BufRead, F: FnMut(&[u8]) -> Result<()>>(
    mut reader: R,
    thread_id: &str,
    context: &MachineContextV4,
    mut sink: F,
    control: &OperationControl,
) -> Result<()> {
    let mut line = Vec::new();
    let mut found = false;
    loop {
        control.check_cancelled()?;
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let (body, ending) = split_line_ending(&line);
        let mut transformed = None;
        if let Ok(mut value) = serde_json::from_slice::<Value>(body)
            && value.get("type").and_then(Value::as_str) == Some("session_meta")
            && let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut)
        {
            if !found {
                if payload.get("id").and_then(Value::as_str) != Some(thread_id) {
                    bail!("materialized rollout thread ID does not match {thread_id}");
                }
                found = true;
                if let Some(path) = &context.workspace_path {
                    payload.insert("cwd".to_string(), Value::String(path.clone()));
                }
            }
            payload.insert(
                "model_provider".to_string(),
                Value::String(context.configured_provider.clone()),
            );
            let mut bytes = canonical_json_v4(&value)?;
            bytes.extend_from_slice(ending);
            transformed = Some(bytes);
        }
        sink(transformed.as_deref().unwrap_or(&line))?;
    }
    if !found {
        bail!("normalized rollout has no session_meta record");
    }
    Ok(())
}

fn split_line_ending(line: &[u8]) -> (&[u8], &[u8]) {
    if let Some(body) = line.strip_suffix(b"\r\n") {
        (body, b"\r\n")
    } else if let Some(body) = line.strip_suffix(b"\n") {
        (body, b"\n")
    } else {
        (line, b"")
    }
}

fn emit_bytes<F: FnMut(&[u8]) -> Result<()>>(
    bytes: &[u8],
    chunk_size: usize,
    pending: &mut Vec<u8>,
    hasher: &mut Sha256,
    total: &mut u64,
    chunks: &mut Vec<ChunkDescriptorV4>,
    sink: &mut F,
) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let take = (chunk_size - pending.len()).min(bytes.len() - offset);
        pending.extend_from_slice(&bytes[offset..offset + take]);
        offset += take;
        if pending.len() == chunk_size {
            emit_chunk(pending, hasher, total, chunks, sink)?;
            pending.clear();
        }
    }
    Ok(())
}

fn emit_chunk<F: FnMut(&[u8]) -> Result<()>>(
    bytes: &[u8],
    hasher: &mut Sha256,
    total: &mut u64,
    chunks: &mut Vec<ChunkDescriptorV4>,
    sink: &mut F,
) -> Result<()> {
    if chunks.len() >= MAX_CHUNKS_PER_CONTENT_V4 {
        bail!("normalized rollout exceeds chunk limit");
    }
    hasher.update(bytes);
    *total = total
        .checked_add(bytes.len() as u64)
        .context("normalized rollout length overflow")?;
    chunks.push(ChunkDescriptorV4 {
        sha256: digest_bytes_v4(bytes),
        byte_length: bytes.len() as u64,
    });
    sink(bytes)
}

pub fn canonical_json_v4<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value)?;
    let mut output = String::new();
    write_canonical_json_v4(&value, &mut output)?;
    Ok(output.into_bytes())
}

pub fn digest_bytes_v4(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn write_canonical_json_v4(value: &Value, output: &mut String) -> Result<()> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(v) => output.push_str(if *v { "true" } else { "false" }),
        Value::Number(v) => output.push_str(&v.to_string()),
        Value::String(v) => output.push_str(&serde_json::to_string(v)?),
        Value::Array(values) => {
            output.push('[');
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    output.push(',');
                }
                write_canonical_json_v4(v, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(k)?);
                output.push(':');
                write_canonical_json_v4(&values[k], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

pub fn initialize_v4_repository(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let marker_path = root.join("format.json");
    if marker_path.exists() {
        let bytes = fs::read(&marker_path)?;
        let marker: FormatMarkerV4 =
            serde_json::from_slice(&bytes).context("invalid repository format marker")?;
        if marker.format != V4_FORMAT_MARKER
            || marker.storage_protocol_version != STORAGE_PROTOCOL_VERSION_V4
            || marker.normalization_schema_version != NORMALIZATION_SCHEMA_VERSION_V4
        {
            bail!(
                "repository uses an incompatible storage format; v3 and earlier formats are not supported"
            );
        }
    } else {
        let marker = FormatMarkerV4 {
            format: V4_FORMAT_MARKER.to_string(),
            storage_protocol_version: STORAGE_PROTOCOL_VERSION_V4,
            normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
        };
        let bytes = canonical_json_v4(&marker)?;
        let temp = root.join("format.json.tmp");
        fs::write(&temp, &bytes)?;
        let file = OpenOptions::new().write(true).open(&temp)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temp, marker_path)?;
    }
    for directory in [
        "objects/whole/sha256",
        "objects/chunks/sha256",
        "objects/chunk-manifests/sha256",
        "objects/threads/sha256",
        "objects/revision-roots/sha256",
        "objects/snapshot-overlays/sha256",
        "objects/tmp",
        "snapshots",
        "metadata/snapshots",
        "index",
        "tracking",
        "backups",
        "journal",
        "trash/snapshots",
        "trash/gc",
        "quarantine",
    ] {
        fs::create_dir_all(root.join(directory))?;
    }
    Ok(())
}

pub fn verify_v4_repository(root: &Path) -> Result<FormatMarkerV4> {
    let path = root.join("format.json");
    let marker: FormatMarkerV4 =
        serde_json::from_slice(&fs::read(&path)?).context("missing or invalid v4 format marker")?;
    if marker.format != V4_FORMAT_MARKER
        || marker.storage_protocol_version != STORAGE_PROTOCOL_VERSION_V4
    {
        bail!("repository is not a compatible v4 repository");
    }
    Ok(marker)
}

impl SnapshotOverlayV4 {
    pub fn new(threads: BTreeMap<String, LocalThreadOverlayV4>) -> Self {
        Self {
            schema_version: 4,
            normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
            threads,
        }
    }
    pub fn object_id(&self) -> Result<String> {
        Ok(digest_bytes_v4(&canonical_json_v4(self)?))
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 4
            || self.normalization_schema_version != NORMALIZATION_SCHEMA_VERSION_V4
        {
            bail!("unsupported v4 snapshot overlay schema");
        }
        Ok(())
    }
}

impl SemanticThreadDescriptorV4 {
    pub fn from_bundle(bundle: &ThreadBundle, rollout: ContentRefV4) -> Result<Self> {
        if rollout.logical_sha256 != bundle.rollout.sha256
            || rollout.byte_length != bundle.rollout.byte_length
        {
            bail!("v4 rollout reference does not match thread bundle");
        }
        let attachments = bundle
            .attachments
            .iter()
            .map(content_ref_from_object_v4)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            schema_version: 4,
            normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
            thread_id: bundle.thread_id.clone(),
            title: bundle.title.clone(),
            archived: bundle.archived,
            created_at_ms: bundle.created_at_ms,
            updated_at_ms: bundle.updated_at_ms,
            semantic_workspace: SemanticWorkspaceV4 {
                logical_id: bundle.workspace.logical_id.clone(),
                // source_path is intentionally excluded; canonical_hint is a
                // logical workspace identity, never a machine absolute path.
                canonical_hint: bundle.workspace.logical_id.clone(),
            },
            rollout,
            related_records: project_related_records_v4(&bundle.related_records),
            attachments,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 4
            || self.normalization_schema_version != NORMALIZATION_SCHEMA_VERSION_V4
        {
            bail!("unsupported v4 semantic descriptor schema");
        }
        if self.thread_id.trim().is_empty() {
            bail!("v4 semantic descriptor has an empty thread ID");
        }
        if self.rollout.media_type.trim().is_empty() {
            bail!("v4 semantic descriptor has an empty rollout media type");
        }
        validate_content_ref_v4(&self.rollout)?;
        for attachment in &self.attachments {
            validate_content_ref_v4(attachment)?;
        }
        if self.related_records.contains_key("thread_timeline_ledger") {
            bail!("thread_timeline_ledger is not part of the v4 semantic model");
        }
        Ok(())
    }

    pub fn into_bundle(self, overlay: Option<&LocalThreadOverlayV4>) -> ThreadBundle {
        let provider = overlay.and_then(|item| item.observed_provider.clone());
        let workspace = WorkspaceRef {
            logical_id: self.semantic_workspace.logical_id,
            source_path: overlay
                .and_then(|item| item.local_workspace_path.clone())
                .or(self.semantic_workspace.canonical_hint),
        };
        ThreadBundle {
            schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
            thread_id: self.thread_id,
            title: self.title,
            archived: self.archived,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            model_provider: provider,
            workspace,
            rollout: content_object_from_ref_v4(&self.rollout),
            related_records: RelatedRecords {
                source_database: overlay
                    .and_then(|item| item.source_database_hints.first())
                    .map(PathBuf::from),
                tables: self.related_records,
            },
            attachments: self
                .attachments
                .iter()
                .map(content_object_from_ref_v4)
                .collect(),
        }
    }
}

impl FilesystemContentStoreV4 {
    pub fn store_descriptor(
        &self,
        bundle: &ThreadBundle,
        rollout: ContentRefV4,
    ) -> Result<ThreadRefV4> {
        let descriptor = SemanticThreadDescriptorV4::from_bundle(bundle, rollout)?;
        descriptor.validate()?;
        let object = self.store_json(StorageObjectKindV4::Thread, &descriptor)?;
        Ok(ThreadRefV4 {
            thread_id: bundle.thread_id.clone(),
            descriptor_sha256: object.sha256,
        })
    }

    pub fn load_descriptor(&self, reference: &ThreadRefV4) -> Result<SemanticThreadDescriptorV4> {
        validate_sha256_v4(&reference.descriptor_sha256)?;
        let descriptor: SemanticThreadDescriptorV4 =
            self.read_json(StorageObjectKindV4::Thread, &reference.descriptor_sha256)?;
        descriptor.validate()?;
        if descriptor.thread_id != reference.thread_id {
            bail!("v4 thread reference ID does not match descriptor");
        }
        Ok(descriptor)
    }

    pub fn store_overlay(&self, overlay: &SnapshotOverlayV4) -> Result<StorageObjectRefV4> {
        overlay.validate()?;
        self.store_json(StorageObjectKindV4::SnapshotOverlay, overlay)
    }

    pub fn load_overlay(&self, sha256: &str) -> Result<SnapshotOverlayV4> {
        let overlay: SnapshotOverlayV4 =
            self.read_json(StorageObjectKindV4::SnapshotOverlay, sha256)?;
        overlay.validate()?;
        Ok(overlay)
    }

    pub fn store_revision_root(&self, root: &RevisionRootV4) -> Result<StorageObjectRefV4> {
        root.validate()?;
        self.store_json(StorageObjectKindV4::RevisionRoot, root)
    }
}

pub fn write_v4_snapshot(
    snapshot: &LocalSnapshot,
    contents: &BTreeMap<String, ContentRefV4>,
    repository_root: &Path,
) -> Result<PathBuf> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let root = build_v4_snapshot_root(snapshot, contents, &store)?;
    let path = repository_root
        .join("snapshots")
        .join(format!("{}.json", snapshot.snapshot_id));
    write_v4_root(&path, &root)?;
    Ok(path)
}

/// Materializes the immutable object graph for a local snapshot without
/// registering a manifest in `snapshots/`.  Portable treasure export uses
/// this to keep its short-lived composition root out of local history.
pub fn build_v4_snapshot_root(
    snapshot: &LocalSnapshot,
    contents: &BTreeMap<String, ContentRefV4>,
    store: &FilesystemContentStoreV4,
) -> Result<SnapshotRootV4> {
    let mut references = Vec::with_capacity(snapshot.threads.len());
    let mut overlays = BTreeMap::new();
    for thread in &snapshot.threads {
        let content = contents
            .get(&thread.thread_id)
            .with_context(|| format!("missing v4 content for thread {}", thread.thread_id))?;
        references.push(store.store_descriptor(thread, content.clone())?);
        overlays.insert(
            thread.thread_id.clone(),
            LocalThreadOverlayV4 {
                observed_provider: thread.model_provider.clone(),
                local_workspace_path: thread.workspace.source_path.clone(),
                local_rollout_path: thread.rollout.logical_path.clone(),
                source_database_hints: thread
                    .related_records
                    .source_database
                    .as_ref()
                    .map(|path| vec![path.to_string_lossy().to_string()])
                    .unwrap_or_default(),
                local_fields: BTreeMap::new(),
            },
        );
    }
    references.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    let overlay = SnapshotOverlayV4::new(overlays);
    let overlay_object = store.store_overlay(&overlay)?;
    let root = SnapshotRootV4 {
        schema_version: 4,
        normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
        snapshot_id: snapshot.snapshot_id.clone(),
        created_at: snapshot.created_at.clone(),
        threads: references,
        overlay_sha256: overlay_object.sha256,
        warning_count: snapshot.warning_count,
    };
    root.validate()?;
    Ok(root)
}

pub fn load_v4_snapshot(
    root_path: &Path,
    repository_root: &Path,
) -> Result<(LocalSnapshot, BTreeMap<String, ContentRefV4>)> {
    let root: SnapshotRootV4 = read_bounded_v4(root_path, 16 * 1024 * 1024)?;
    root.validate()?;
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let overlay = store.load_overlay(&root.overlay_sha256)?;
    let mut threads = Vec::with_capacity(root.threads.len());
    let mut contents = BTreeMap::new();
    for reference in &root.threads {
        let descriptor = store.load_descriptor(reference)?;
        let content = descriptor.rollout.clone();
        if let Some(previous) = contents.insert(reference.thread_id.clone(), content)
            && previous.logical_sha256 != descriptor.rollout.logical_sha256
        {
            bail!("v4 snapshot contains conflicting content references");
        }
        threads.push(descriptor.into_bundle(overlay.threads.get(&reference.thread_id)));
    }
    Ok((
        LocalSnapshot {
            schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: root.snapshot_id,
            created_at: root.created_at,
            threads,
            warning_count: root.warning_count,
        },
        contents,
    ))
}

pub fn snapshot_to_revision_root_v4(
    snapshot: &LocalSnapshot,
    namespace_id: Uuid,
    parent_revision: Option<String>,
    repository_root: &Path,
    contents: &BTreeMap<String, ContentRefV4>,
) -> Result<(RevisionRootV4, StorageObjectRefV4)> {
    let store = FilesystemContentStoreV4::open(repository_root.to_path_buf())?;
    let mut threads = Vec::with_capacity(snapshot.threads.len());
    for thread in &snapshot.threads {
        let content = contents
            .get(&thread.thread_id)
            .with_context(|| format!("missing v4 content for thread {}", thread.thread_id))?;
        let descriptor = SemanticThreadDescriptorV4::from_bundle(thread, content.clone())?;
        descriptor.validate()?;
        let object = store.store_json(StorageObjectKindV4::Thread, &descriptor)?;
        threads.push(ThreadRefV4 {
            thread_id: thread.thread_id.clone(),
            descriptor_sha256: object.sha256,
        });
    }
    threads.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    let root = RevisionRootV4 {
        schema_version: 4,
        normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
        namespace_id,
        parent_revision,
        created_at: snapshot.created_at.clone(),
        threads,
        warning_count: snapshot.warning_count,
    };
    let object = store.store_revision_root(&root)?;
    Ok((root, object))
}

pub fn collect_revision_graph_v4(
    root: &RevisionRootV4,
    store: &FilesystemContentStoreV4,
) -> Result<BTreeSet<StorageObjectRefV4>> {
    root.validate()?;
    let root_id = root.revision_id()?;
    let root_path = store.object_path_by_id(StorageObjectKindV4::RevisionRoot, &root_id)?;
    let mut graph = BTreeSet::from([StorageObjectRefV4 {
        kind: StorageObjectKindV4::RevisionRoot,
        sha256: root_id,
        byte_length: fs::metadata(&root_path)?.len(),
    }]);
    graph.extend(collect_thread_graph_v4(&root.threads, store)?);
    Ok(graph)
}

/// Collect just the descriptor/content objects for a subset of a revision.
/// This supports staged Push: unchanged remote `ThreadRef`s can stay in the
/// new root without being downloaded, revalidated, or uploaded locally.
pub fn collect_thread_graph_v4(
    thread_refs: &[ThreadRefV4],
    store: &FilesystemContentStoreV4,
) -> Result<BTreeSet<StorageObjectRefV4>> {
    let mut graph = BTreeSet::new();
    let mut references = 0_usize;
    for thread_ref in thread_refs {
        references += 1;
        if references > 500_000 {
            bail!("v4 revision graph exceeds object-reference limit");
        }
        let descriptor_path =
            store.object_path_by_id(StorageObjectKindV4::Thread, &thread_ref.descriptor_sha256)?;
        graph.insert(StorageObjectRefV4 {
            kind: StorageObjectKindV4::Thread,
            sha256: thread_ref.descriptor_sha256.clone(),
            byte_length: fs::metadata(&descriptor_path)?.len(),
        });
        let descriptor = store.load_descriptor(thread_ref)?;
        for content in std::iter::once(&descriptor.rollout).chain(descriptor.attachments.iter()) {
            for object in store.content_objects(content)? {
                references += 1;
                if references > 500_000 {
                    bail!("v4 revision graph exceeds object-reference limit");
                }
                graph.insert(object);
            }
        }
    }
    Ok(graph)
}

fn content_ref_from_object_v4(object: &ContentObject) -> Result<ContentRefV4> {
    let storage = match object
        .storage
        .as_ref()
        .context("v4 content object has no physical storage reference")?
    {
        crate::storage_v3::StorageRef::Whole { object_sha256 } => StorageRefV4::Whole {
            object_sha256: object_sha256.clone(),
        },
        crate::storage_v3::StorageRef::Chunked { manifest_sha256 } => StorageRefV4::Chunked {
            manifest_sha256: manifest_sha256.clone(),
        },
    };
    Ok(ContentRefV4 {
        logical_sha256: object.sha256.clone(),
        byte_length: object.byte_length,
        storage,
        media_type: object.media_type.clone(),
        logical_path: object.logical_path.clone(),
    })
}

fn content_object_from_ref_v4(content: &ContentRefV4) -> ContentObject {
    let storage = match &content.storage {
        StorageRefV4::Whole { object_sha256 } => crate::storage_v3::StorageRef::Whole {
            object_sha256: object_sha256.clone(),
        },
        StorageRefV4::Chunked { manifest_sha256 } => crate::storage_v3::StorageRef::Chunked {
            manifest_sha256: manifest_sha256.clone(),
        },
    };
    ContentObject {
        sha256: content.logical_sha256.clone(),
        byte_length: content.byte_length,
        media_type: content.media_type.clone(),
        logical_path: content.logical_path.clone(),
        source_path: None,
        storage: Some(storage),
    }
}

fn validate_content_ref_v4(content: &ContentRefV4) -> Result<()> {
    validate_sha256_v4(&content.logical_sha256)?;
    if content.media_type.trim().is_empty() {
        bail!("content media type must not be empty");
    }
    match &content.storage {
        StorageRefV4::Whole { object_sha256 } => validate_sha256_v4(object_sha256)?,
        StorageRefV4::Chunked { manifest_sha256 } => validate_sha256_v4(manifest_sha256)?,
    }
    Ok(())
}

fn project_related_records_v4(records: &RelatedRecords) -> BTreeMap<String, Vec<Value>> {
    records
        .tables
        .iter()
        .filter(|(table, _)| table.as_str() != "thread_timeline_ledger")
        .map(|(table, rows)| {
            (
                table.clone(),
                rows.iter().map(project_value_v4).collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn project_value_v4(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut projected = serde_json::Map::new();
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "model_provider" | "rollout_path" | "codex_home"
                ) {
                    continue;
                }
                projected.insert(key.clone(), project_value_v4(value));
            }
            Value::Object(projected)
        }
        Value::Array(values) => Value::Array(values.iter().map(project_value_v4).collect()),
        _ => value.clone(),
    }
}

impl SnapshotRootV4 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 4
            || self.normalization_schema_version != NORMALIZATION_SCHEMA_VERSION_V4
        {
            bail!("unsupported v4 snapshot root schema");
        }
        validate_thread_refs_v4(&self.threads)
    }
    pub fn root_id(&self) -> Result<String> {
        self.validate()?;
        Ok(digest_bytes_v4(&canonical_json_v4(self)?))
    }
}

impl RevisionRootV4 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 4
            || self.normalization_schema_version != NORMALIZATION_SCHEMA_VERSION_V4
        {
            bail!("unsupported v4 revision root schema");
        }
        if self.namespace_id.get_version_num() != 7 {
            bail!("namespace ID must be UUIDv7");
        }
        if let Some(parent) = &self.parent_revision {
            validate_sha256_v4(parent)?;
        }
        validate_thread_refs_v4(&self.threads)
    }
    pub fn revision_id(&self) -> Result<String> {
        self.validate()?;
        Ok(digest_bytes_v4(&canonical_json_v4(self)?))
    }
}

fn validate_thread_refs_v4(threads: &[ThreadRefV4]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for thread in threads {
        if thread.thread_id.trim().is_empty() {
            bail!("thread ID must not be empty");
        }
        validate_sha256_v4(&thread.descriptor_sha256)?;
        if !ids.insert(&thread.thread_id) {
            bail!("duplicate thread ID {}", thread.thread_id);
        }
    }
    Ok(())
}

pub fn validate_sha256_v4(value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        bail!("invalid SHA-256 identifier")
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("invalid SHA-256 identifier")
    }
    Ok(())
}

/// Write a v4 root atomically. This helper is deliberately small and is used
/// by Snapshot/Revision adapters after they have installed immutable objects.
pub fn write_v4_root<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = canonical_json_v4(value)?;
    let temp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&temp, &bytes)?;
    let file = OpenOptions::new().write(true).open(&temp)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)?;
    Ok(())
}

pub fn read_bounded_v4<T: for<'de> Deserialize<'de>>(path: &Path, max_bytes: u64) -> Result<T> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("object exceeds size limit");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(provider: &str, workspace: &str) -> MachineContextV4 {
        MachineContextV4 {
            configured_provider: provider.to_string(),
            workspace_path: Some(workspace.to_string()),
            target_codex_home: PathBuf::from("/tmp/.codex"),
        }
    }

    #[test]
    fn provider_and_workspace_only_changes_have_same_hash() {
        let first = br#"{"type":"session_meta","payload":{"id":"thread","model_provider":"openai","cwd":"C:\\work"}}
{"type":"message","payload":{"text":"same"}}
"#;
        let second = br#"{"payload":{"cwd":"/Users/me/work","model_provider":"custom","id":"thread"},"type":"session_meta"}
{"type":"message","payload":{"text":"same"}}
"#;
        let a = normalize_rollout_bytes(first, "thread").unwrap();
        let b = normalize_rollout_bytes(second, "thread").unwrap();
        assert_eq!(a.logical_sha256, b.logical_sha256);
        assert_eq!(a.normalized_bytes, b.normalized_bytes);
    }

    #[test]
    fn normalization_is_idempotent_and_materialization_round_trips() {
        let raw=br#"{"type":"session_meta","payload":{"id":"thread","model_provider":"custom","cwd":"C:\\work"}}
{"type":"session_meta","payload":{"id":"other","model_provider":"openai","cwd":"C:\\other"}}
{"type":"message","payload":{"model_provider":"do-not-touch"}}
bad-json
"#;
        let normalized = normalize_rollout_bytes(raw, "thread").unwrap();
        assert!(!normalized.warnings.is_empty());
        let renormalized = normalize_rollout_bytes(&normalized.normalized_bytes, "thread").unwrap();
        assert_eq!(normalized.logical_sha256, renormalized.logical_sha256);
        let materialized = materialize_rollout_bytes(
            &normalized.normalized_bytes,
            "thread",
            &context("freemodel", "/Users/me/work"),
        )
        .unwrap();
        let again = normalize_rollout_bytes(&materialized, "thread").unwrap();
        assert_eq!(normalized.logical_sha256, again.logical_sha256);
    }

    #[test]
    fn rejects_old_or_incompatible_repository_marker() {
        let dir = tempfile::tempdir().unwrap();
        initialize_v4_repository(dir.path()).unwrap();
        assert_eq!(
            verify_v4_repository(dir.path())
                .unwrap()
                .storage_protocol_version,
            4
        );
        fs::write(dir.path().join("format.json"), br#"{"format":"codex-session-sync-v3","storageProtocolVersion":3,"normalizationSchemaVersion":1}"#).unwrap();
        assert!(initialize_v4_repository(dir.path()).is_err());
    }

    #[test]
    fn permanent_trash_purge_deletes_overlay_but_preserves_shared_content() {
        let dir = tempfile::tempdir().unwrap();
        initialize_v4_repository(dir.path()).unwrap();
        let store = FilesystemContentStoreV4::open(dir.path().to_path_buf()).unwrap();
        let content = store
            .ingest_chunk_reader(
                &mut std::io::Cursor::new(b"shared rollout"),
                "application/x-ndjson".to_string(),
                None,
                &OperationControl::default(),
            )
            .unwrap();
        let descriptor = SemanticThreadDescriptorV4 {
            schema_version: 4,
            normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
            thread_id: "thread".to_string(),
            title: "Shared".to_string(),
            archived: false,
            created_at_ms: None,
            updated_at_ms: None,
            semantic_workspace: SemanticWorkspaceV4::default(),
            rollout: content,
            related_records: BTreeMap::new(),
            attachments: Vec::new(),
        };
        let descriptor_object = store
            .store_json(StorageObjectKindV4::Thread, &descriptor)
            .unwrap();
        let first_overlay = store
            .store_overlay(&SnapshotOverlayV4::new(BTreeMap::from([(
                "thread".to_string(),
                LocalThreadOverlayV4 {
                    observed_provider: Some("first".to_string()),
                    ..LocalThreadOverlayV4::default()
                },
            )])))
            .unwrap();
        let second_overlay = store
            .store_overlay(&SnapshotOverlayV4::new(BTreeMap::from([(
                "thread".to_string(),
                LocalThreadOverlayV4 {
                    observed_provider: Some("second".to_string()),
                    ..LocalThreadOverlayV4::default()
                },
            )])))
            .unwrap();
        let first_id = Uuid::now_v7().to_string();
        let second_id = Uuid::now_v7().to_string();
        let thread = ThreadRefV4 {
            thread_id: "thread".to_string(),
            descriptor_sha256: descriptor_object.sha256.clone(),
        };
        for (snapshot_id, overlay_sha256) in [
            (first_id.clone(), first_overlay.sha256.clone()),
            (second_id.clone(), second_overlay.sha256.clone()),
        ] {
            write_v4_root(
                &dir.path()
                    .join("snapshots")
                    .join(format!("{snapshot_id}.json")),
                &SnapshotRootV4 {
                    schema_version: 4,
                    normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
                    snapshot_id,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    threads: vec![thread.clone()],
                    overlay_sha256,
                    warning_count: 0,
                },
            )
            .unwrap();
        }
        let deletion = plan_snapshot_deletion_v4(dir.path(), &first_id).unwrap();
        let trashed = crate::storage_v3::trash_local_snapshot(dir.path(), &deletion).unwrap();
        let plan = plan_local_trash_purge_v4(
            dir.path(),
            std::slice::from_ref(&trashed.operation_id),
            false,
        )
        .unwrap();
        assert!(
            plan.candidate_objects
                .iter()
                .any(|object| object.sha256 == first_overlay.sha256)
        );
        assert!(
            !plan
                .candidate_objects
                .iter()
                .any(|object| object.sha256 == descriptor_object.sha256)
        );
        let result = purge_local_trash_v4(dir.path(), &plan).unwrap();
        assert_eq!(result.deleted_trash_entries, 1);
        assert!(!store.object_path(&first_overlay).unwrap().exists());
        assert!(store.object_path(&second_overlay).unwrap().is_file());
        assert!(store.object_path(&descriptor_object).unwrap().is_file());
        assert!(load_local_trash_roots_v4(dir.path()).unwrap().is_empty());
        assert!(
            dir.path()
                .join("snapshots")
                .join(format!("{second_id}.json"))
                .is_file()
        );
    }

    #[test]
    fn permanent_trash_purge_recovery_rolls_back_before_commit_and_finishes_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        initialize_v4_repository(dir.path()).unwrap();
        let store = FilesystemContentStoreV4::open(dir.path().to_path_buf()).unwrap();
        let content = store
            .ingest_chunk_reader(
                &mut std::io::Cursor::new(b"recoverable rollout"),
                "application/x-ndjson".to_string(),
                None,
                &OperationControl::default(),
            )
            .unwrap();
        let descriptor = store
            .store_json(
                StorageObjectKindV4::Thread,
                &SemanticThreadDescriptorV4 {
                    schema_version: 4,
                    normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
                    thread_id: "thread".to_string(),
                    title: "Recovery".to_string(),
                    archived: false,
                    created_at_ms: None,
                    updated_at_ms: None,
                    semantic_workspace: SemanticWorkspaceV4::default(),
                    rollout: content,
                    related_records: BTreeMap::new(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        let overlay = store
            .store_overlay(&SnapshotOverlayV4::new(BTreeMap::new()))
            .unwrap();
        let snapshot_id = Uuid::now_v7().to_string();
        write_v4_root(
            &dir.path()
                .join("snapshots")
                .join(format!("{snapshot_id}.json")),
            &SnapshotRootV4 {
                schema_version: 4,
                normalization_schema_version: NORMALIZATION_SCHEMA_VERSION_V4,
                snapshot_id: snapshot_id.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                threads: vec![ThreadRefV4 {
                    thread_id: "thread".to_string(),
                    descriptor_sha256: descriptor.sha256.clone(),
                }],
                overlay_sha256: overlay.sha256.clone(),
                warning_count: 0,
            },
        )
        .unwrap();
        let deletion = plan_snapshot_deletion_v4(dir.path(), &snapshot_id).unwrap();
        let trashed = crate::storage_v3::trash_local_snapshot(dir.path(), &deletion).unwrap();
        let plan = plan_local_trash_purge_v4(
            dir.path(),
            std::slice::from_ref(&trashed.operation_id),
            false,
        )
        .unwrap();

        let first_id = Uuid::now_v7().to_string();
        let first_staging = dir.path().join("quarantine/trash-purge").join(&first_id);
        fs::create_dir_all(first_staging.join("entries")).unwrap();
        fs::create_dir_all(first_staging.join("objects")).unwrap();
        let first_journal_path = dir
            .path()
            .join("journal")
            .join(format!("local-trash-purge-{first_id}.json"));
        let now = chrono::Utc::now().to_rfc3339();
        let first_journal = LocalTrashPurgeJournalV4 {
            schema_version: 1,
            operation_id: first_id,
            status: LocalTrashPurgeStatusV4::Preparing,
            repository_root: dir.path().to_path_buf(),
            staging_root: first_staging.clone(),
            plan: plan.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            error: None,
        };
        write_v4_root(&first_journal_path, &first_journal).unwrap();
        fs::rename(
            dir.path()
                .join("trash/snapshots")
                .join(&trashed.operation_id),
            first_staging.join("entries").join(&trashed.operation_id),
        )
        .unwrap();
        recover_local_trash_purges_v4(dir.path()).unwrap();
        assert!(
            dir.path()
                .join("trash/snapshots")
                .join(&trashed.operation_id)
                .is_dir()
        );
        let recovered: LocalTrashPurgeJournalV4 =
            read_bounded_v4(&first_journal_path, 16 * 1024 * 1024).unwrap();
        assert_eq!(recovered.status, LocalTrashPurgeStatusV4::RolledBack);

        let second_id = Uuid::now_v7().to_string();
        let second_staging = dir.path().join("quarantine/trash-purge").join(&second_id);
        fs::create_dir_all(second_staging.join("entries")).unwrap();
        fs::create_dir_all(second_staging.join("objects")).unwrap();
        let second_journal_path = dir
            .path()
            .join("journal")
            .join(format!("local-trash-purge-{second_id}.json"));
        fs::rename(
            dir.path()
                .join("trash/snapshots")
                .join(&trashed.operation_id),
            second_staging.join("entries").join(&trashed.operation_id),
        )
        .unwrap();
        write_v4_root(
            &second_journal_path,
            &LocalTrashPurgeJournalV4 {
                schema_version: 1,
                operation_id: second_id,
                status: LocalTrashPurgeStatusV4::Committed,
                repository_root: dir.path().to_path_buf(),
                staging_root: second_staging,
                plan,
                created_at: now.clone(),
                updated_at: now,
                error: None,
            },
        )
        .unwrap();
        recover_local_trash_purges_v4(dir.path()).unwrap();
        assert!(load_local_trash_roots_v4(dir.path()).unwrap().is_empty());
        assert!(!store.object_path(&overlay).unwrap().exists());
        assert!(!store.object_path(&descriptor).unwrap().exists());
        let recovered: LocalTrashPurgeJournalV4 =
            read_bounded_v4(&second_journal_path, 16 * 1024 * 1024).unwrap();
        assert_eq!(recovered.status, LocalTrashPurgeStatusV4::Completed);
    }
}
