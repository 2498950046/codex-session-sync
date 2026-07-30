use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::models::{
    ContentObject, LOCAL_SNAPSHOT_SCHEMA_VERSION, LocalSnapshot, RelatedRecords,
    THREAD_BUNDLE_SCHEMA_VERSION, ThreadBundle, WorkspaceRef,
};
use crate::operation::OperationControl;
use crate::protocol::validate_sha256;

pub const STORAGE_PROTOCOL_VERSION: u32 = 2;
pub const CHUNK_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const THREAD_DESCRIPTOR_SCHEMA_VERSION: u32 = 1;
pub const SNAPSHOT_ROOT_V2_SCHEMA_VERSION: u32 = 2;
pub const REVISION_ROOT_V2_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_CHUNK_BYTES: u32 = 4 * 1024 * 1024;
pub const DEFAULT_CHUNK_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CHUNKS_PER_CONTENT: usize = 16_384;
pub const MAX_LOGICAL_CONTENT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const MAX_THREADS_PER_REVISION: usize = 100_000;
pub const MAX_OBJECT_REFERENCES: usize = 500_000;
pub const MAX_STRUCTURED_OBJECT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_GRAPH_DEPTH: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentRef {
    pub logical_sha256: String,
    pub byte_length: u64,
    pub storage: StorageRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum StorageRef {
    Whole { object_sha256: String },
    Chunked { manifest_sha256: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChunkManifest {
    pub schema_version: u32,
    pub logical_sha256: String,
    pub byte_length: u64,
    pub chunk_size: u32,
    pub chunks: Vec<ChunkDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct ChunkDescriptor {
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDescriptor {
    pub schema_version: u32,
    pub thread_id: String,
    pub title: String,
    pub archived: bool,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub model_provider: Option<String>,
    pub workspace: WorkspaceRef,
    pub rollout: ContentRef,
    pub related_records: RelatedRecords,
    pub attachments: Vec<ContentRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRef {
    pub thread_id: String,
    pub descriptor_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRootV2 {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub created_at: String,
    pub threads: Vec<ThreadRef>,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RevisionRootV2 {
    pub schema_version: u32,
    pub namespace_id: Uuid,
    pub parent_revision: Option<String>,
    pub created_at: String,
    pub threads: Vec<ThreadRef>,
    pub warning_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub enum StorageObjectKind {
    Whole,
    Chunk,
    ChunkManifest,
    Thread,
    RevisionRoot,
}

impl StorageObjectKind {
    pub const ALL: [Self; 5] = [
        Self::Whole,
        Self::Chunk,
        Self::ChunkManifest,
        Self::Thread,
        Self::RevisionRoot,
    ];

    pub fn directory(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Chunk => "chunks",
            Self::ChunkManifest => "chunk-manifests",
            Self::Thread => "threads",
            Self::RevisionRoot => "revision-roots",
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Chunk => "chunk",
            Self::ChunkManifest => "chunkManifest",
            Self::Thread => "thread",
            Self::RevisionRoot => "revisionRoot",
        }
    }

    pub fn max_bytes(self) -> u64 {
        match self {
            Self::Chunk => MAX_CHUNK_BYTES,
            Self::ChunkManifest | Self::Thread | Self::RevisionRoot => MAX_STRUCTURED_OBJECT_BYTES,
            Self::Whole => MAX_LOGICAL_CONTENT_BYTES,
        }
    }
}

impl fmt::Display for StorageObjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.wire_name())
    }
}

impl FromStr for StorageObjectKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "whole" => Ok(Self::Whole),
            "chunk" => Ok(Self::Chunk),
            "chunkManifest" | "chunk-manifest" => Ok(Self::ChunkManifest),
            "thread" => Ok(Self::Thread),
            "revisionRoot" | "revision-root" => Ok(Self::RevisionRoot),
            _ => bail!("unsupported storage object kind {value}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub struct StorageObjectRef {
    pub kind: StorageObjectKind,
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MaterializeReport {
    pub logical_sha256: String,
    pub byte_length: u64,
    pub chunk_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentValidationReport {
    pub logical_sha256: String,
    pub byte_length: u64,
    pub storage_object_count: usize,
    pub valid: bool,
}

pub trait ContentStore {
    fn ingest(&self, source: &Path, control: &OperationControl) -> Result<ContentRef>;
    fn materialize(
        &self,
        content: &ContentRef,
        destination: &Path,
        control: &OperationControl,
    ) -> Result<MaterializeReport>;
    fn validate(
        &self,
        content: &ContentRef,
        control: &OperationControl,
    ) -> Result<ContentValidationReport>;
    fn contains_storage_object(&self, object: &StorageObjectRef) -> Result<bool>;
}

#[derive(Debug, Clone)]
pub struct FilesystemContentStore {
    root: PathBuf,
    chunk_bytes: u32,
    chunk_threshold_bytes: u64,
}

impl FilesystemContentStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::with_options(root, DEFAULT_CHUNK_BYTES, DEFAULT_CHUNK_THRESHOLD_BYTES)
    }

    pub fn with_options(
        root: impl Into<PathBuf>,
        chunk_bytes: u32,
        chunk_threshold_bytes: u64,
    ) -> Result<Self> {
        if chunk_bytes == 0 || u64::from(chunk_bytes) > MAX_CHUNK_BYTES {
            bail!("chunk size must be between 1 and {MAX_CHUNK_BYTES} bytes");
        }
        let store = Self {
            root: root.into(),
            chunk_bytes,
            chunk_threshold_bytes,
        };
        store.ensure_layout()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, object: &StorageObjectRef) -> Result<PathBuf> {
        typed_object_path(&self.root, object.kind, &object.sha256)
    }

    pub fn object_path_by_id(&self, kind: StorageObjectKind, sha256: &str) -> Result<PathBuf> {
        typed_object_path(&self.root, kind, sha256)
    }

    pub fn install<R: Read>(
        &self,
        object: &StorageObjectRef,
        reader: R,
        control: &OperationControl,
    ) -> Result<bool> {
        validate_storage_object_ref(object)?;
        install_stream(&self.root, object, reader, control)
    }

    pub fn read_json<T: DeserializeOwned>(
        &self,
        kind: StorageObjectKind,
        sha256: &str,
        max_bytes: u64,
    ) -> Result<T> {
        let path = typed_object_path(&self.root, kind, sha256)?;
        validate_file_hash(&path, sha256, None, max_bytes)?;
        let file = File::open(&path)?;
        let mut bytes = Vec::new();
        file.take(max_bytes + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            bail!("{kind} object exceeds {max_bytes} bytes");
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {kind} object {sha256}"))
    }

    pub fn store_json<T: Serialize>(
        &self,
        kind: StorageObjectKind,
        value: &T,
    ) -> Result<StorageObjectRef> {
        let bytes = canonical_json(value)?;
        if bytes.len() as u64 > kind.max_bytes() {
            bail!("{kind} object exceeds {} bytes", kind.max_bytes());
        }
        let sha256 = digest_bytes(&bytes);
        let object = StorageObjectRef {
            kind,
            sha256,
            byte_length: bytes.len() as u64,
        };
        install_stream(
            &self.root,
            &object,
            bytes.as_slice(),
            &OperationControl::default(),
        )?;
        Ok(object)
    }

    pub fn store_thread(&self, bundle: &ThreadBundle, rollout: ContentRef) -> Result<ThreadRef> {
        let descriptor = ThreadDescriptor::from_bundle(bundle, rollout)?;
        let object = self.store_json(StorageObjectKind::Thread, &descriptor)?;
        Ok(ThreadRef {
            thread_id: bundle.thread_id.clone(),
            descriptor_sha256: object.sha256,
        })
    }

    pub fn load_thread(&self, reference: &ThreadRef) -> Result<(ThreadBundle, ContentRef)> {
        validate_sha256(&reference.descriptor_sha256)
            .map_err(|_| anyhow::anyhow!("invalid thread descriptor hash"))?;
        let descriptor: ThreadDescriptor = self.read_json(
            StorageObjectKind::Thread,
            &reference.descriptor_sha256,
            MAX_STRUCTURED_OBJECT_BYTES,
        )?;
        descriptor.validate()?;
        if descriptor.thread_id != reference.thread_id {
            bail!("thread reference ID does not match its descriptor");
        }
        let content = descriptor.rollout.clone();
        Ok((descriptor.into_bundle(), content))
    }

    pub fn content_objects(&self, content: &ContentRef) -> Result<Vec<StorageObjectRef>> {
        validate_content_ref(content)?;
        match &content.storage {
            StorageRef::Whole { object_sha256 } => Ok(vec![StorageObjectRef {
                kind: StorageObjectKind::Whole,
                sha256: object_sha256.clone(),
                byte_length: content.byte_length,
            }]),
            StorageRef::Chunked { manifest_sha256 } => {
                let manifest = self.load_chunk_manifest(manifest_sha256)?;
                if manifest.logical_sha256 != content.logical_sha256
                    || manifest.byte_length != content.byte_length
                {
                    bail!("chunk manifest identity does not match content reference");
                }
                let bytes = canonical_json(&manifest)?;
                let mut objects = vec![StorageObjectRef {
                    kind: StorageObjectKind::ChunkManifest,
                    sha256: manifest_sha256.clone(),
                    byte_length: bytes.len() as u64,
                }];
                objects.extend(manifest.chunks.into_iter().map(|chunk| StorageObjectRef {
                    kind: StorageObjectKind::Chunk,
                    sha256: chunk.sha256,
                    byte_length: chunk.byte_length,
                }));
                Ok(objects)
            }
        }
    }

    pub fn load_chunk_manifest(&self, sha256: &str) -> Result<ChunkManifest> {
        let manifest: ChunkManifest = self.read_json(
            StorageObjectKind::ChunkManifest,
            sha256,
            MAX_STRUCTURED_OBJECT_BYTES,
        )?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn ingest_whole(&self, source: &Path, control: &OperationControl) -> Result<ContentRef> {
        let length = regular_file_length(source)?;
        if length > MAX_LOGICAL_CONTENT_BYTES {
            bail!("content exceeds {MAX_LOGICAL_CONTENT_BYTES} bytes");
        }
        let temporary = self.temporary_path("whole");
        let (sha256, byte_length) = copy_and_hash(source, &temporary, control)?;
        let object = StorageObjectRef {
            kind: StorageObjectKind::Whole,
            sha256: sha256.clone(),
            byte_length,
        };
        install_prepared(&self.root, &temporary, &object)?;
        Ok(ContentRef {
            logical_sha256: sha256.clone(),
            byte_length,
            storage: StorageRef::Whole {
                object_sha256: sha256,
            },
            media_type: None,
            logical_path: None,
        })
    }

    pub fn ingest_chunked(&self, source: &Path, control: &OperationControl) -> Result<ContentRef> {
        let expected_length = regular_file_length(source)?;
        if expected_length > MAX_LOGICAL_CONTENT_BYTES {
            bail!("content exceeds {MAX_LOGICAL_CONTENT_BYTES} bytes");
        }
        let mut input = BufReader::new(File::open(source)?);
        let mut logical_hasher = Sha256::new();
        let mut total = 0_u64;
        let mut chunks = Vec::new();
        let mut buffer = vec![0_u8; self.chunk_bytes as usize];
        loop {
            control.check_cancelled()?;
            let mut count = 0;
            while count < buffer.len() {
                let read = input.read(&mut buffer[count..])?;
                if read == 0 {
                    break;
                }
                count += read;
            }
            if count == 0 {
                break;
            }
            if chunks.len() >= MAX_CHUNKS_PER_CONTENT {
                bail!("content exceeds {MAX_CHUNKS_PER_CONTENT} chunks");
            }
            let bytes = &buffer[..count];
            logical_hasher.update(bytes);
            total = total
                .checked_add(count as u64)
                .context("content length overflow")?;
            let sha256 = digest_bytes(bytes);
            let object = StorageObjectRef {
                kind: StorageObjectKind::Chunk,
                sha256: sha256.clone(),
                byte_length: count as u64,
            };
            install_stream(&self.root, &object, bytes, control)?;
            chunks.push(ChunkDescriptor {
                sha256,
                byte_length: count as u64,
            });
        }
        let observed_length = regular_file_length(source)?;
        if observed_length != expected_length || total != expected_length {
            bail!("source file changed while it was being ingested");
        }
        let logical_sha256 = format!("sha256:{}", hex::encode(logical_hasher.finalize()));
        let manifest = ChunkManifest {
            schema_version: CHUNK_MANIFEST_SCHEMA_VERSION,
            logical_sha256: logical_sha256.clone(),
            byte_length: total,
            chunk_size: self.chunk_bytes,
            chunks,
        };
        manifest.validate()?;
        let manifest_object = self.store_json(StorageObjectKind::ChunkManifest, &manifest)?;
        Ok(ContentRef {
            logical_sha256,
            byte_length: total,
            storage: StorageRef::Chunked {
                manifest_sha256: manifest_object.sha256,
            },
            media_type: None,
            logical_path: None,
        })
    }

    fn ensure_layout(&self) -> Result<()> {
        for kind in StorageObjectKind::ALL {
            fs::create_dir_all(
                self.root
                    .join("objects")
                    .join(kind.directory())
                    .join("sha256"),
            )?;
        }
        for directory in [
            "objects/tmp",
            "snapshots/v1",
            "snapshots/v2",
            "index",
            "journal",
            "backups",
            "trash",
            "quarantine",
        ] {
            fs::create_dir_all(self.root.join(directory))?;
        }
        Ok(())
    }

    fn temporary_path(&self, label: &str) -> PathBuf {
        self.root
            .join("objects/tmp")
            .join(format!("{label}-{}.tmp", Uuid::now_v7()))
    }
}

impl ContentStore for FilesystemContentStore {
    fn ingest(&self, source: &Path, control: &OperationControl) -> Result<ContentRef> {
        if regular_file_length(source)? >= self.chunk_threshold_bytes {
            self.ingest_chunked(source, control)
        } else {
            self.ingest_whole(source, control)
        }
    }

    fn materialize(
        &self,
        content: &ContentRef,
        destination: &Path,
        control: &OperationControl,
    ) -> Result<MaterializeReport> {
        validate_content_ref(content)?;
        if destination.exists() {
            bail!(
                "materialization destination already exists: {}",
                destination.display()
            );
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = destination.with_extension(format!("{}.tmp", Uuid::now_v7()));
        let result = (|| -> Result<MaterializeReport> {
            let output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            let mut writer = BufWriter::new(output);
            let mut logical_hasher = Sha256::new();
            let mut total = 0_u64;
            let mut chunk_count = 0_usize;
            match &content.storage {
                StorageRef::Whole { object_sha256 } => {
                    let path =
                        typed_object_path(&self.root, StorageObjectKind::Whole, object_sha256)?;
                    copy_validated_part(
                        &path,
                        object_sha256,
                        content.byte_length,
                        &mut writer,
                        &mut logical_hasher,
                        control,
                    )?;
                    total = content.byte_length;
                    chunk_count = 1;
                }
                StorageRef::Chunked { manifest_sha256 } => {
                    let manifest = self.load_chunk_manifest(manifest_sha256)?;
                    if manifest.logical_sha256 != content.logical_sha256
                        || manifest.byte_length != content.byte_length
                    {
                        bail!("chunk manifest does not match content reference");
                    }
                    for chunk in &manifest.chunks {
                        control.check_cancelled()?;
                        let path =
                            typed_object_path(&self.root, StorageObjectKind::Chunk, &chunk.sha256)?;
                        copy_validated_part(
                            &path,
                            &chunk.sha256,
                            chunk.byte_length,
                            &mut writer,
                            &mut logical_hasher,
                            control,
                        )?;
                        total = total
                            .checked_add(chunk.byte_length)
                            .context("content length overflow")?;
                        chunk_count += 1;
                    }
                }
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            let actual = format!("sha256:{}", hex::encode(logical_hasher.finalize()));
            if total != content.byte_length || actual != content.logical_sha256 {
                bail!("materialized content failed logical hash or length validation");
            }
            match fs::rename(&temporary, destination) {
                Ok(()) => {}
                Err(error) => return Err(error).context("failed to install materialized content"),
            }
            Ok(MaterializeReport {
                logical_sha256: actual,
                byte_length: total,
                chunk_count,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn validate(
        &self,
        content: &ContentRef,
        control: &OperationControl,
    ) -> Result<ContentValidationReport> {
        let temporary = self.temporary_path("validate");
        let report = self.materialize(content, &temporary, control)?;
        fs::remove_file(&temporary)?;
        Ok(ContentValidationReport {
            logical_sha256: report.logical_sha256,
            byte_length: report.byte_length,
            storage_object_count: report.chunk_count
                + usize::from(matches!(content.storage, StorageRef::Chunked { .. })),
            valid: true,
        })
    }

    fn contains_storage_object(&self, object: &StorageObjectRef) -> Result<bool> {
        validate_storage_object_ref(object)?;
        let path = self.object_path(object)?;
        if !path.exists() {
            return Ok(false);
        }
        validate_file_hash(
            &path,
            &object.sha256,
            Some(object.byte_length),
            object.kind.max_bytes(),
        )?;
        Ok(true)
    }
}

impl ChunkManifest {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CHUNK_MANIFEST_SCHEMA_VERSION {
            bail!(
                "unsupported chunk manifest schema version {}",
                self.schema_version
            );
        }
        validate_sha256(&self.logical_sha256)
            .map_err(|_| anyhow::anyhow!("invalid logical content hash"))?;
        if self.chunk_size == 0 || u64::from(self.chunk_size) > MAX_CHUNK_BYTES {
            bail!("invalid chunk size {}", self.chunk_size);
        }
        if self.chunks.len() > MAX_CHUNKS_PER_CONTENT {
            bail!("chunk manifest exceeds the chunk-count limit");
        }
        if self.byte_length > MAX_LOGICAL_CONTENT_BYTES {
            bail!("chunk manifest exceeds the logical-length limit");
        }
        if self.byte_length == 0 && !self.chunks.is_empty() {
            bail!("empty content must not contain chunks");
        }
        if self.byte_length > 0 && self.chunks.is_empty() {
            bail!("non-empty content must contain at least one chunk");
        }
        let mut total = 0_u64;
        for (index, chunk) in self.chunks.iter().enumerate() {
            validate_sha256(&chunk.sha256).map_err(|_| anyhow::anyhow!("invalid chunk hash"))?;
            if chunk.byte_length == 0 || chunk.byte_length > u64::from(self.chunk_size) {
                bail!("invalid chunk length at index {index}");
            }
            if index + 1 < self.chunks.len() && chunk.byte_length != u64::from(self.chunk_size) {
                bail!("only the final chunk may be shorter than chunkSize");
            }
            total = total
                .checked_add(chunk.byte_length)
                .context("chunk length overflow")?;
        }
        if total != self.byte_length {
            bail!("chunk lengths do not sum to the logical content length");
        }
        Ok(())
    }
}

impl ThreadDescriptor {
    pub fn from_bundle(bundle: &ThreadBundle, rollout: ContentRef) -> Result<Self> {
        if rollout.logical_sha256 != bundle.rollout.sha256
            || rollout.byte_length != bundle.rollout.byte_length
        {
            bail!("rollout content reference does not match thread bundle");
        }
        let attachments = bundle
            .attachments
            .iter()
            .map(|object| ContentRef {
                logical_sha256: object.sha256.clone(),
                byte_length: object.byte_length,
                storage: StorageRef::Whole {
                    object_sha256: object.sha256.clone(),
                },
                media_type: Some(object.media_type.clone()),
                logical_path: object.logical_path.clone(),
            })
            .collect();
        let mut rollout = rollout;
        rollout.media_type = Some(bundle.rollout.media_type.clone());
        rollout.logical_path = bundle.rollout.logical_path.clone();
        let descriptor = Self {
            schema_version: THREAD_DESCRIPTOR_SCHEMA_VERSION,
            thread_id: bundle.thread_id.clone(),
            title: bundle.title.clone(),
            archived: bundle.archived,
            created_at_ms: bundle.created_at_ms,
            updated_at_ms: bundle.updated_at_ms,
            model_provider: bundle.model_provider.clone(),
            workspace: bundle.workspace.clone(),
            rollout,
            related_records: bundle.related_records.clone(),
            attachments,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != THREAD_DESCRIPTOR_SCHEMA_VERSION {
            bail!(
                "unsupported thread descriptor schema version {}",
                self.schema_version
            );
        }
        if self.thread_id.trim().is_empty() {
            bail!("thread descriptor has an empty thread ID");
        }
        validate_content_ref(&self.rollout)?;
        for attachment in &self.attachments {
            validate_content_ref(attachment)?;
        }
        Ok(())
    }

    pub fn into_bundle(self) -> ThreadBundle {
        ThreadBundle {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: self.thread_id,
            title: self.title,
            archived: self.archived,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            model_provider: self.model_provider,
            workspace: self.workspace,
            rollout: content_object(&self.rollout),
            related_records: self.related_records,
            attachments: self.attachments.iter().map(content_object).collect(),
        }
    }
}

impl SnapshotRootV2 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SNAPSHOT_ROOT_V2_SCHEMA_VERSION {
            bail!(
                "unsupported v2 snapshot schema version {}",
                self.schema_version
            );
        }
        validate_thread_refs(&self.threads)
    }
}

impl RevisionRootV2 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != REVISION_ROOT_V2_SCHEMA_VERSION {
            bail!(
                "unsupported v2 revision schema version {}",
                self.schema_version
            );
        }
        if self.namespace_id.get_version_num() != 7 {
            bail!("namespace ID must be UUIDv7");
        }
        if let Some(parent) = &self.parent_revision {
            validate_sha256(parent).map_err(|_| anyhow::anyhow!("invalid parent revision"))?;
        }
        validate_thread_refs(&self.threads)
    }
}

pub fn typed_object_path(
    repository_root: &Path,
    kind: StorageObjectKind,
    sha256: &str,
) -> Result<PathBuf> {
    validate_sha256(sha256).map_err(|_| anyhow::anyhow!("invalid SHA-256 identifier {sha256}"))?;
    let digest = sha256.strip_prefix("sha256:").expect("validated prefix");
    Ok(repository_root
        .join("objects")
        .join(kind.directory())
        .join("sha256")
        .join(&digest[..2])
        .join(&digest[2..]))
}

pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value)?;
    let mut output = String::new();
    write_canonical_json(&value, &mut output)?;
    Ok(output.into_bytes())
}

pub fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

pub fn load_v2_snapshot(
    root_path: &Path,
    repository_root: &Path,
) -> Result<(LocalSnapshot, BTreeMap<String, ContentRef>)> {
    let root: SnapshotRootV2 = read_bounded_json(root_path, MAX_STRUCTURED_OBJECT_BYTES)?;
    root.validate()?;
    let store = FilesystemContentStore::open(repository_root.to_path_buf())?;
    let mut threads = Vec::with_capacity(root.threads.len());
    let mut contents = BTreeMap::new();
    for reference in &root.threads {
        let (thread, content) = store.load_thread(reference)?;
        if let Some(previous) = contents.insert(thread.thread_id.clone(), content)
            && previous.logical_sha256 != thread.rollout.sha256
        {
            bail!("snapshot contains conflicting content references");
        }
        threads.push(thread);
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

pub fn write_v2_snapshot(
    snapshot: &LocalSnapshot,
    contents: &BTreeMap<String, ContentRef>,
    repository_root: &Path,
) -> Result<PathBuf> {
    let store = FilesystemContentStore::open(repository_root.to_path_buf())?;
    let mut references = Vec::with_capacity(snapshot.threads.len());
    for thread in &snapshot.threads {
        let content = contents.get(&thread.thread_id).with_context(|| {
            format!("missing storage reference for thread {}", thread.thread_id)
        })?;
        references.push(store.store_thread(thread, content.clone())?);
    }
    references.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    let root = SnapshotRootV2 {
        schema_version: SNAPSHOT_ROOT_V2_SCHEMA_VERSION,
        snapshot_id: snapshot.snapshot_id.clone(),
        created_at: snapshot.created_at.clone(),
        threads: references,
        warning_count: snapshot.warning_count,
    };
    root.validate()?;
    let path = repository_root
        .join("snapshots/v2")
        .join(format!("{}.json", snapshot.snapshot_id));
    atomic_write_bytes(&path, &canonical_json(&root)?)?;
    Ok(path)
}

pub fn collect_snapshot_graph(
    root: &SnapshotRootV2,
    store: &FilesystemContentStore,
) -> Result<BTreeSet<StorageObjectRef>> {
    root.validate()?;
    let mut objects = BTreeSet::new();
    let mut references = 0_usize;
    for thread_ref in &root.threads {
        references += 1;
        if references > MAX_OBJECT_REFERENCES {
            bail!("snapshot graph exceeds the object-reference limit");
        }
        let descriptor_path =
            store.object_path_by_id(StorageObjectKind::Thread, &thread_ref.descriptor_sha256)?;
        let descriptor_length = fs::metadata(&descriptor_path)?.len();
        objects.insert(StorageObjectRef {
            kind: StorageObjectKind::Thread,
            sha256: thread_ref.descriptor_sha256.clone(),
            byte_length: descriptor_length,
        });
        let (_, rollout) = store.load_thread(thread_ref)?;
        for object in store.content_objects(&rollout)? {
            references += 1;
            if references > MAX_OBJECT_REFERENCES {
                bail!("snapshot graph exceeds the object-reference limit");
            }
            objects.insert(object);
        }
    }
    Ok(objects)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GcPlan {
    pub schema_version: u32,
    pub created_at: String,
    pub reachable_objects: usize,
    pub unreachable_objects: Vec<StorageObjectRef>,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryOptimizationReport {
    pub migrated_snapshots: usize,
    pub migrated_threads: usize,
    pub logical_bytes: u64,
    pub v2_snapshot_paths: Vec<PathBuf>,
}

/// Creates equivalent v2 roots next to retained v1 snapshots. This operation
/// never rewrites or removes a v1 manifest or whole object.
pub fn optimize_v1_repository(
    repository_root: &Path,
    control: &OperationControl,
) -> Result<RepositoryOptimizationReport> {
    let store = FilesystemContentStore::open(repository_root.to_path_buf())?;
    let v1_dir = repository_root.join("snapshots/v1");
    let mut report = RepositoryOptimizationReport {
        migrated_snapshots: 0,
        migrated_threads: 0,
        logical_bytes: 0,
        v2_snapshot_paths: Vec::new(),
    };
    if !v1_dir.exists() {
        return Ok(report);
    }
    for entry in fs::read_dir(v1_dir)? {
        control.check_cancelled()?;
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let mut snapshot: LocalSnapshot = read_bounded_json(&path, 64 * 1024 * 1024)?;
        let mut contents = BTreeMap::new();
        for thread in &mut snapshot.threads {
            control.check_cancelled()?;
            let legacy = legacy_object_path(repository_root, &thread.rollout.sha256)?;
            let mut content = store.ingest(&legacy, control)?;
            if content.logical_sha256 != thread.rollout.sha256
                || content.byte_length != thread.rollout.byte_length
            {
                bail!("v1 object does not match snapshot metadata");
            }
            content.media_type = Some(thread.rollout.media_type.clone());
            content.logical_path = thread.rollout.logical_path.clone();
            thread.rollout.storage = Some(content.storage.clone());
            report.logical_bytes = report
                .logical_bytes
                .checked_add(content.byte_length)
                .context("optimization byte count overflow")?;
            report.migrated_threads += 1;
            contents.insert(thread.thread_id.clone(), content);
        }
        let target = write_v2_snapshot(&snapshot, &contents, repository_root)?;
        load_v2_snapshot(&target, repository_root)?;
        report.migrated_snapshots += 1;
        report.v2_snapshot_paths.push(target);
    }
    Ok(report)
}

pub fn plan_local_gc(repository_root: &Path) -> Result<GcPlan> {
    let store = FilesystemContentStore::open(repository_root.to_path_buf())?;
    let mut reachable = BTreeSet::new();
    let snapshot_dir = repository_root.join("snapshots/v2");
    if snapshot_dir.exists() {
        for entry in fs::read_dir(&snapshot_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let root: SnapshotRootV2 = read_bounded_json(&path, MAX_STRUCTURED_OBJECT_BYTES)?;
            reachable.extend(collect_snapshot_graph(&root, &store)?);
        }
    }
    let mut unreachable = Vec::new();
    let mut reclaimable_bytes = 0_u64;
    for kind in [
        StorageObjectKind::Whole,
        StorageObjectKind::Chunk,
        StorageObjectKind::ChunkManifest,
        StorageObjectKind::Thread,
    ] {
        let kind_root = repository_root
            .join("objects")
            .join(kind.directory())
            .join("sha256");
        if !kind_root.exists() {
            continue;
        }
        for prefix in fs::read_dir(&kind_root)? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                continue;
            }
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(prefix.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let digest = format!("{prefix_name}{}", entry.file_name().to_string_lossy());
                let object = StorageObjectRef {
                    kind,
                    sha256: format!("sha256:{digest}"),
                    byte_length: entry.metadata()?.len(),
                };
                validate_storage_object_ref(&object)?;
                if !reachable.contains(&object) {
                    reclaimable_bytes = reclaimable_bytes
                        .checked_add(object.byte_length)
                        .context("GC byte count overflow")?;
                    unreachable.push(object);
                }
            }
        }
    }
    unreachable.sort();
    Ok(GcPlan {
        schema_version: 1,
        created_at: chrono::Utc::now().to_rfc3339(),
        reachable_objects: reachable.len(),
        unreachable_objects: unreachable,
        reclaimable_bytes,
    })
}

pub fn quarantine_local_gc_plan(repository_root: &Path, plan: &GcPlan) -> Result<PathBuf> {
    let operation_id = Uuid::now_v7().to_string();
    let quarantine = repository_root.join("trash/gc").join(&operation_id);
    let journal = repository_root
        .join("journal")
        .join(format!("gc-{operation_id}.json"));
    fs::create_dir_all(&quarantine)?;
    atomic_write_bytes(&journal, &canonical_json(plan)?)?;
    let store = FilesystemContentStore::open(repository_root.to_path_buf())?;
    // Recompute immediately before moving. This makes a stale externally held
    // plan harmless even before a repository-wide exclusive lease is wired.
    let authoritative = plan_local_gc(repository_root)?;
    let authoritative_set = authoritative
        .unreachable_objects
        .into_iter()
        .collect::<BTreeSet<_>>();
    for object in &plan.unreachable_objects {
        if !authoritative_set.contains(object) {
            bail!("GC plan became stale before quarantine");
        }
        let source = store.object_path(object)?;
        if !source.exists() {
            continue;
        }
        let destination = quarantine.join(object.kind.directory()).join(
            object
                .sha256
                .strip_prefix("sha256:")
                .expect("validated hash"),
        );
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(source, destination)?;
    }
    Ok(quarantine)
}

fn validate_thread_refs(threads: &[ThreadRef]) -> Result<()> {
    if threads.len() > MAX_THREADS_PER_REVISION {
        bail!("root exceeds the thread-count limit");
    }
    let mut ids = BTreeSet::new();
    for reference in threads {
        if reference.thread_id.trim().is_empty() {
            bail!("root contains an empty thread ID");
        }
        if !ids.insert(reference.thread_id.as_str()) {
            bail!("root contains duplicate thread ID {}", reference.thread_id);
        }
        validate_sha256(&reference.descriptor_sha256)
            .map_err(|_| anyhow::anyhow!("invalid thread descriptor hash"))?;
    }
    Ok(())
}

fn legacy_object_path(repository_root: &Path, sha256: &str) -> Result<PathBuf> {
    validate_sha256(sha256).map_err(|_| anyhow::anyhow!("invalid legacy object hash"))?;
    let digest = sha256.strip_prefix("sha256:").expect("validated prefix");
    Ok(repository_root
        .join("objects/sha256")
        .join(&digest[..2])
        .join(&digest[2..]))
}

fn validate_content_ref(content: &ContentRef) -> Result<()> {
    validate_sha256(&content.logical_sha256)
        .map_err(|_| anyhow::anyhow!("invalid logical content hash"))?;
    if content.byte_length > MAX_LOGICAL_CONTENT_BYTES {
        bail!("content exceeds the logical-length limit");
    }
    let storage_hash = match &content.storage {
        StorageRef::Whole { object_sha256 } => object_sha256,
        StorageRef::Chunked { manifest_sha256 } => manifest_sha256,
    };
    validate_sha256(storage_hash).map_err(|_| anyhow::anyhow!("invalid storage object hash"))?;
    if let StorageRef::Whole { object_sha256 } = &content.storage
        && object_sha256 != &content.logical_sha256
    {
        bail!("whole object hash must equal the logical content hash");
    }
    Ok(())
}

fn validate_storage_object_ref(object: &StorageObjectRef) -> Result<()> {
    validate_sha256(&object.sha256)
        .map_err(|_| anyhow::anyhow!("invalid storage object hash {}", object.sha256))?;
    if object.byte_length > object.kind.max_bytes() {
        bail!("{} object exceeds its size limit", object.kind);
    }
    Ok(())
}

fn content_object(content: &ContentRef) -> ContentObject {
    ContentObject {
        sha256: content.logical_sha256.clone(),
        byte_length: content.byte_length,
        media_type: content
            .media_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string()),
        logical_path: content.logical_path.clone(),
        source_path: None,
        storage: Some(content.storage.clone()),
    }
}

fn regular_file_length(path: &Path) -> Result<u64> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect source file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("source is not a regular file: {}", path.display());
    }
    Ok(metadata.len())
}

fn copy_and_hash(
    source: &Path,
    temporary: &Path,
    control: &OperationControl,
) -> Result<(String, u64)> {
    let result = (|| -> Result<(String, u64)> {
        let mut input = BufReader::new(File::open(source)?);
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temporary)?;
        let mut output = BufWriter::new(output);
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            control.check_cancelled()?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
            total = total
                .checked_add(count as u64)
                .context("content length overflow")?;
        }
        output.flush()?;
        output.get_ref().sync_all()?;
        Ok((format!("sha256:{}", hex::encode(hasher.finalize())), total))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn install_stream<R: Read>(
    root: &Path,
    object: &StorageObjectRef,
    mut reader: R,
    control: &OperationControl,
) -> Result<bool> {
    validate_storage_object_ref(object)?;
    let destination = typed_object_path(root, object.kind, &object.sha256)?;
    if destination.exists() {
        validate_file_hash(
            &destination,
            &object.sha256,
            Some(object.byte_length),
            object.kind.max_bytes(),
        )?;
        return Ok(false);
    }
    fs::create_dir_all(root.join("objects/tmp"))?;
    let temporary = root.join("objects/tmp").join(format!(
        "{}-{}.tmp",
        object.kind.wire_name(),
        Uuid::now_v7()
    ));
    let result = (|| -> Result<bool> {
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut output = BufWriter::new(output);
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            control.check_cancelled()?;
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .context("object length overflow")?;
            if total > object.byte_length {
                bail!("object stream is longer than declared");
            }
            hasher.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        output.flush()?;
        output.get_ref().sync_all()?;
        drop(output);
        let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
        if total != object.byte_length || actual != object.sha256 {
            bail!("object hash or length mismatch");
        }
        install_prepared(root, &temporary, object)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn install_prepared(root: &Path, temporary: &Path, object: &StorageObjectRef) -> Result<bool> {
    let destination = typed_object_path(root, object.kind, &object.sha256)?;
    if destination.exists() {
        validate_file_hash(
            &destination,
            &object.sha256,
            Some(object.byte_length),
            object.kind.max_bytes(),
        )?;
        fs::remove_file(temporary)?;
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(temporary, &destination) {
        Ok(()) => Ok(true),
        Err(error) if destination.exists() => {
            let _ = fs::remove_file(temporary);
            validate_file_hash(
                &destination,
                &object.sha256,
                Some(object.byte_length),
                object.kind.max_bytes(),
            )
            .context(error)?;
            Ok(false)
        }
        Err(error) => Err(error).context("failed to atomically install storage object"),
    }
}

fn copy_validated_part<W: Write>(
    path: &Path,
    expected_hash: &str,
    expected_length: u64,
    output: &mut W,
    logical_hasher: &mut Sha256,
    control: &OperationControl,
) -> Result<()> {
    let mut input = BufReader::new(
        File::open(path).with_context(|| format!("missing storage object {}", path.display()))?,
    );
    let mut part_hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        control.check_cancelled()?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("object length overflow")?;
        if total > expected_length {
            bail!("storage object is longer than declared");
        }
        part_hasher.update(&buffer[..count]);
        logical_hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    let actual = format!("sha256:{}", hex::encode(part_hasher.finalize()));
    if total != expected_length || actual != expected_hash {
        bail!("storage object failed hash or length validation");
    }
    Ok(())
}

fn validate_file_hash(
    path: &Path,
    expected_hash: &str,
    expected_length: Option<u64>,
    max_bytes: u64,
) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("missing storage object {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("invalid or oversized storage object {}", path.display());
    }
    if expected_length.is_some_and(|length| length != metadata.len()) {
        bail!("storage object length mismatch");
    }
    let mut input = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual != expected_hash {
        bail!("storage object hash mismatch");
    }
    Ok(())
}

fn read_bounded_json<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> Result<T> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("JSON object is missing or exceeds {max_bytes} bytes");
    }
    serde_json::from_reader(BufReader::new(File::open(path)?))
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_canonical_json(value: &Value, output: &mut String) -> Result<()> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&serde_json::to_string(value)?),
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
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key)?);
                output.push(':');
                write_canonical_json(value, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tempfile::tempdir;

    use super::*;

    fn write_pattern(path: &Path, length: usize) {
        let bytes = (0..length)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn whole_and_chunked_content_round_trip() {
        let temp = tempdir().unwrap();
        let store = FilesystemContentStore::with_options(temp.path(), 16, 32).unwrap();
        let small = temp.path().join("small");
        let large = temp.path().join("large");
        write_pattern(&small, 15);
        write_pattern(&large, 45);
        let small_ref = store.ingest(&small, &OperationControl::default()).unwrap();
        let large_ref = store.ingest(&large, &OperationControl::default()).unwrap();
        assert!(matches!(small_ref.storage, StorageRef::Whole { .. }));
        assert!(matches!(large_ref.storage, StorageRef::Chunked { .. }));
        let restored = temp.path().join("restored");
        store
            .materialize(&large_ref, &restored, &OperationControl::default())
            .unwrap();
        assert_eq!(fs::read(restored).unwrap(), fs::read(large).unwrap());
    }

    #[test]
    fn appending_reuses_stable_chunks_and_replaces_the_tail() {
        let temp = tempdir().unwrap();
        let store = FilesystemContentStore::with_options(temp.path(), 16, 1).unwrap();
        let source = temp.path().join("source");
        write_pattern(&source, 40);
        let first = store.ingest(&source, &OperationControl::default()).unwrap();
        let first_manifest = match &first.storage {
            StorageRef::Chunked { manifest_sha256 } => {
                store.load_chunk_manifest(manifest_sha256).unwrap()
            }
            _ => panic!("expected chunked content"),
        };
        let mut bytes = fs::read(&source).unwrap();
        bytes.extend_from_slice(b"append");
        fs::write(&source, bytes).unwrap();
        let second = store.ingest(&source, &OperationControl::default()).unwrap();
        let second_manifest = match &second.storage {
            StorageRef::Chunked { manifest_sha256 } => {
                store.load_chunk_manifest(manifest_sha256).unwrap()
            }
            _ => panic!("expected chunked content"),
        };
        assert_eq!(first_manifest.chunks[..2], second_manifest.chunks[..2]);
        assert_ne!(first_manifest.chunks[2], second_manifest.chunks[2]);
    }

    #[test]
    fn materialization_rejects_missing_or_corrupt_chunks_without_installing_destination() {
        let temp = tempdir().unwrap();
        let store = FilesystemContentStore::with_options(temp.path(), 16, 1).unwrap();
        let source = temp.path().join("source");
        write_pattern(&source, 40);
        let content = store.ingest(&source, &OperationControl::default()).unwrap();
        let manifest = match &content.storage {
            StorageRef::Chunked { manifest_sha256 } => {
                store.load_chunk_manifest(manifest_sha256).unwrap()
            }
            _ => panic!("expected chunked content"),
        };
        let chunk_path = store
            .object_path_by_id(StorageObjectKind::Chunk, &manifest.chunks[0].sha256)
            .unwrap();
        fs::write(&chunk_path, b"corrupt").unwrap();
        let destination = temp.path().join("destination");
        assert!(
            store
                .materialize(&content, &destination, &OperationControl::default())
                .is_err()
        );
        assert!(!destination.exists());
    }

    #[test]
    fn typed_objects_with_identical_hashes_remain_separate() {
        let temp = tempdir().unwrap();
        let store = FilesystemContentStore::open(temp.path()).unwrap();
        let bytes = b"same bytes";
        let sha256 = digest_bytes(bytes);
        for kind in [StorageObjectKind::Whole, StorageObjectKind::Chunk] {
            store
                .install(
                    &StorageObjectRef {
                        kind,
                        sha256: sha256.clone(),
                        byte_length: bytes.len() as u64,
                    },
                    Cursor::new(bytes),
                    &OperationControl::default(),
                )
                .unwrap();
        }
        assert_ne!(
            store
                .object_path_by_id(StorageObjectKind::Whole, &sha256)
                .unwrap(),
            store
                .object_path_by_id(StorageObjectKind::Chunk, &sha256)
                .unwrap()
        );
    }

    #[test]
    fn gc_plan_preserves_shared_reachable_chunks_and_quarantines_orphans() {
        let temp = tempdir().unwrap();
        let store = FilesystemContentStore::with_options(temp.path(), 16, 1).unwrap();
        let source = temp.path().join("source");
        write_pattern(&source, 40);
        let content = store.ingest(&source, &OperationControl::default()).unwrap();
        let bundle = ThreadBundle {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: "thread".to_string(),
            title: "Thread".to_string(),
            archived: false,
            created_at_ms: None,
            updated_at_ms: None,
            model_provider: None,
            workspace: WorkspaceRef::default(),
            rollout: ContentObject {
                sha256: content.logical_sha256.clone(),
                byte_length: content.byte_length,
                media_type: "application/x-ndjson".to_string(),
                logical_path: Some("sessions/rollout-thread.jsonl".to_string()),
                source_path: None,
                storage: Some(content.storage.clone()),
            },
            related_records: RelatedRecords::default(),
            attachments: Vec::new(),
        };
        let snapshot = LocalSnapshot {
            schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: Uuid::now_v7().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            threads: vec![bundle],
            warning_count: 0,
        };
        write_v2_snapshot(
            &snapshot,
            &BTreeMap::from([("thread".to_string(), content)]),
            temp.path(),
        )
        .unwrap();
        let orphan = b"orphan";
        let orphan_ref = StorageObjectRef {
            kind: StorageObjectKind::Chunk,
            sha256: digest_bytes(orphan),
            byte_length: orphan.len() as u64,
        };
        store
            .install(
                &orphan_ref,
                Cursor::new(orphan),
                &OperationControl::default(),
            )
            .unwrap();
        let plan = plan_local_gc(temp.path()).unwrap();
        assert_eq!(plan.unreachable_objects, vec![orphan_ref.clone()]);
        let trash = quarantine_local_gc_plan(temp.path(), &plan).unwrap();
        assert!(!store.object_path(&orphan_ref).unwrap().exists());
        assert!(
            trash
                .join("chunks")
                .join(orphan_ref.sha256.strip_prefix("sha256:").unwrap())
                .exists()
        );
    }
}
