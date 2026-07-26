use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sync_core::{RevisionManifest, RevisionPayload, RevisionValidationError, validate_sha256};
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RevisionStore {
    sha256_dir: PathBuf,
    tmp_dir: PathBuf,
    max_manifest_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutRevisionResult {
    Created,
    AlreadyPresent,
}

#[derive(Debug, Error)]
pub enum RevisionStoreError {
    #[error("invalid SHA-256 revision identifier: {digest}")]
    InvalidDigest { digest: String },
    #[error(transparent)]
    Validation(#[from] RevisionValidationError),
    #[error(
        "revision manifest exceeds the configured {max_bytes} byte limit ({actual_bytes} bytes)"
    )]
    ManifestTooLarge { max_bytes: u64, actual_bytes: u64 },
    #[error("revision hash mismatch: expected {expected}, calculated {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("stored revision is not canonical JSON: {revision_id}")]
    NonCanonical { revision_id: String },
    #[error("immutable revision already exists with different content: {revision_id}")]
    ImmutableConflict { revision_id: String },
    #[error("revision not found: {revision_id}")]
    NotFound { revision_id: String },
    #[error("failed to decode revision {revision_id}: {source}")]
    Decode {
        revision_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl RevisionStore {
    pub async fn open(
        data_dir: impl AsRef<Path>,
        max_manifest_bytes: u64,
    ) -> Result<Self, RevisionStoreError> {
        let revisions_dir = data_dir.as_ref().join("revisions");
        let sha256_dir = revisions_dir.join("sha256");
        let tmp_dir = revisions_dir.join("tmp");
        create_dir_all(&sha256_dir).await?;
        create_dir_all(&tmp_dir).await?;
        cleanup_temp_dir(&tmp_dir).await?;

        Ok(Self {
            sha256_dir,
            tmp_dir,
            max_manifest_bytes,
        })
    }

    pub async fn put(
        &self,
        manifest: &RevisionManifest,
    ) -> Result<PutRevisionResult, RevisionStoreError> {
        self.revision_path(&manifest.revision_id)?;
        manifest.validate()?;
        let canonical = manifest.payload.canonical_json()?;
        let canonical_length = canonical.len() as u64;
        if canonical_length > self.max_manifest_bytes {
            return Err(RevisionStoreError::ManifestTooLarge {
                max_bytes: self.max_manifest_bytes,
                actual_bytes: canonical_length,
            });
        }

        let target_path = self.revision_path(&manifest.revision_id)?;
        let parent = target_path
            .parent()
            .expect("content-addressed revision paths always have a parent");
        create_dir_all(parent).await?;
        let temp_path = self
            .tmp_dir
            .join(format!("{}.json.part", Uuid::now_v7().simple()));
        let mut temp_guard = TempFileGuard::new(temp_path.clone());

        if let Err(error) = write_synced_file(&temp_path, &canonical).await {
            remove_temp(&temp_path).await;
            return Err(error);
        }

        match file_exists(&target_path).await {
            Ok(true) => {
                let existing_result = self.get(&manifest.revision_id).await;
                remove_temp(&temp_path).await;
                let existing = existing_result?;
                if existing.payload.canonical_json()? != canonical {
                    return Err(RevisionStoreError::ImmutableConflict {
                        revision_id: manifest.revision_id.clone(),
                    });
                }
                return Ok(PutRevisionResult::AlreadyPresent);
            }
            Ok(false) => {}
            Err(error) => {
                remove_temp(&temp_path).await;
                return Err(error);
            }
        }

        match fs::rename(&temp_path, &target_path).await {
            Ok(()) => {
                sync_directory(parent).await?;
                sync_directory(&self.sha256_dir).await?;
                temp_guard.disarm();
                Ok(PutRevisionResult::Created)
            }
            Err(rename_error) => {
                // On Windows a concurrent immutable writer causes rename to fail
                // when the destination already exists. Validate that winner fully.
                match file_exists(&target_path).await {
                    Ok(true) => {
                        let existing_result = self.get(&manifest.revision_id).await;
                        remove_temp(&temp_path).await;
                        let existing = existing_result?;
                        if existing.payload.canonical_json()? != canonical {
                            return Err(RevisionStoreError::ImmutableConflict {
                                revision_id: manifest.revision_id.clone(),
                            });
                        }
                        Ok(PutRevisionResult::AlreadyPresent)
                    }
                    Ok(false) => {
                        remove_temp(&temp_path).await;
                        Err(io_error("atomically install", &target_path, rename_error))
                    }
                    Err(error) => {
                        remove_temp(&temp_path).await;
                        Err(error)
                    }
                }
            }
        }
    }

    pub async fn get(&self, revision_id: &str) -> Result<RevisionManifest, RevisionStoreError> {
        let path = self.revision_path(revision_id)?;
        let mut file = match File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RevisionStoreError::NotFound {
                    revision_id: revision_id.to_string(),
                });
            }
            Err(error) => return Err(io_error("open", &path, error)),
        };
        let metadata = file
            .metadata()
            .await
            .map_err(|error| io_error("read metadata for", &path, error))?;
        if !metadata.is_file() {
            return Err(not_a_file(&path));
        }
        if metadata.len() > self.max_manifest_bytes {
            return Err(RevisionStoreError::ManifestTooLarge {
                max_bytes: self.max_manifest_bytes,
                actual_bytes: metadata.len(),
            });
        }

        let capacity = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)
            .await
            .map_err(|error| io_error("read", &path, error))?;
        if bytes.len() as u64 > self.max_manifest_bytes {
            return Err(RevisionStoreError::ManifestTooLarge {
                max_bytes: self.max_manifest_bytes,
                actual_bytes: bytes.len() as u64,
            });
        }

        let actual_id = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        if actual_id != revision_id {
            return Err(RevisionStoreError::HashMismatch {
                expected: revision_id.to_string(),
                actual: actual_id,
            });
        }

        let payload: RevisionPayload =
            serde_json::from_slice(&bytes).map_err(|source| RevisionStoreError::Decode {
                revision_id: revision_id.to_string(),
                source,
            })?;
        let canonical = payload.canonical_json()?;
        if canonical != bytes {
            return Err(RevisionStoreError::NonCanonical {
                revision_id: revision_id.to_string(),
            });
        }

        let manifest = RevisionManifest {
            revision_id: revision_id.to_string(),
            payload,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub async fn contains(&self, revision_id: &str) -> Result<bool, RevisionStoreError> {
        let path = self.revision_path(revision_id)?;
        file_exists(&path).await
    }

    fn revision_path(&self, revision_id: &str) -> Result<PathBuf, RevisionStoreError> {
        validate_sha256(revision_id).map_err(|_| RevisionStoreError::InvalidDigest {
            digest: revision_id.to_string(),
        })?;
        let digest = &revision_id["sha256:".len()..];
        Ok(self
            .sha256_dir
            .join(&digest[..2])
            .join(format!("{}.json", &digest[2..])))
    }
}

async fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<(), RevisionStoreError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .await
        .map_err(|error| io_error("create", path, error))?;
    file.write_all(bytes)
        .await
        .map_err(|error| io_error("write", path, error))?;
    file.flush()
        .await
        .map_err(|error| io_error("flush", path, error))?;
    file.sync_all()
        .await
        .map_err(|error| io_error("sync", path, error))?;
    Ok(())
}

async fn create_dir_all(path: &Path) -> Result<(), RevisionStoreError> {
    fs::create_dir_all(path)
        .await
        .map_err(|error| io_error("create directory", path, error))
}

async fn file_exists(path: &Path) -> Result<bool, RevisionStoreError> {
    match fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(not_a_file(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("read metadata for", path, error)),
    }
}

async fn remove_temp(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

async fn cleanup_temp_dir(path: &Path) -> Result<(), RevisionStoreError> {
    let mut entries = fs::read_dir(path)
        .await
        .map_err(|error| io_error("read temporary directory", path, error))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| io_error("read temporary directory entry", path, error))?
    {
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .map_err(|error| io_error("read temporary file type", &entry_path, error))?;
        if !file_type.is_file() && !file_type.is_symlink() {
            return Err(not_a_file(&entry_path));
        }
        fs::remove_file(&entry_path)
            .await
            .map_err(|error| io_error("remove stale temporary file", &entry_path, error))?;
    }
    Ok(())
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<(), RevisionStoreError> {
    let path = path.to_path_buf();
    let error_path = path.clone();
    tokio::task::spawn_blocking(move || {
        let directory = std::fs::File::open(&path)?;
        directory.sync_all()
    })
    .await
    .map_err(|error| {
        io_error(
            "join directory sync task for",
            &error_path,
            io::Error::other(error),
        )
    })?
    .map_err(|error| io_error("sync directory", &error_path, error))
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> Result<(), RevisionStoreError> {
    Ok(())
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn not_a_file(path: &Path) -> RevisionStoreError {
    io_error(
        "use non-file revision path",
        path,
        io::Error::new(io::ErrorKind::InvalidData, "path is not a regular file"),
    )
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> RevisionStoreError {
    RevisionStoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use sync_core::{REVISION_SCHEMA_VERSION, RevisionPayload};
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    fn manifest() -> RevisionManifest {
        RevisionManifest::from_payload(RevisionPayload {
            schema_version: REVISION_SCHEMA_VERSION,
            namespace_id: Uuid::parse_str("01890f3a-6b4c-7cc2-98c8-0123456789ab").unwrap(),
            parent_revision: None,
            created_at: "2026-07-26T10:30:00Z".to_string(),
            threads: Vec::new(),
            warning_count: 0,
        })
        .unwrap()
    }

    async fn assert_tmp_empty(store: &RevisionStore) {
        let mut entries = fs::read_dir(&store.tmp_dir).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stores_canonical_revision_and_repeated_put_is_idempotent() {
        let root = tempdir().unwrap();
        let store = RevisionStore::open(root.path(), 1024 * 1024).await.unwrap();
        let manifest = manifest();

        assert_eq!(
            store.put(&manifest).await.unwrap(),
            PutRevisionResult::Created
        );
        assert_eq!(store.get(&manifest.revision_id).await.unwrap(), manifest);
        assert!(store.contains(&manifest.revision_id).await.unwrap());
        assert_eq!(
            store.put(&manifest).await.unwrap(),
            PutRevisionResult::AlreadyPresent
        );
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn detects_tampered_revision_bytes() {
        let root = tempdir().unwrap();
        let store = RevisionStore::open(root.path(), 1024 * 1024).await.unwrap();
        let manifest = manifest();
        store.put(&manifest).await.unwrap();

        fs::write(
            store.revision_path(&manifest.revision_id).unwrap(),
            b"{\"tampered\":true}",
        )
        .await
        .unwrap();
        assert!(matches!(
            store.get(&manifest.revision_id).await,
            Err(RevisionStoreError::HashMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_manifest_with_wrong_revision_id() {
        let root = tempdir().unwrap();
        let store = RevisionStore::open(root.path(), 1024 * 1024).await.unwrap();
        let mut manifest = manifest();
        manifest.revision_id = format!("sha256:{}", "0".repeat(64));

        assert!(matches!(
            store.put(&manifest).await,
            Err(RevisionStoreError::Validation(
                RevisionValidationError::RevisionIdMismatch { .. }
            ))
        ));
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn startup_removes_stale_temporary_files() {
        let root = tempdir().unwrap();
        let tmp_dir = root.path().join("revisions").join("tmp");
        fs::create_dir_all(&tmp_dir).await.unwrap();
        fs::write(tmp_dir.join("stale.json.part"), b"stale")
            .await
            .unwrap();

        let store = RevisionStore::open(root.path(), 1024).await.unwrap();
        assert_tmp_empty(&store).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_identical_revisions_are_idempotent() {
        let root = tempdir().unwrap();
        let store = RevisionStore::open(root.path(), 1024 * 1024).await.unwrap();
        let manifest = manifest();
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let manifest = manifest.clone();
            tasks.push(tokio::spawn(async move { store.put(&manifest).await }));
        }

        for task in tasks {
            assert!(matches!(
                task.await.unwrap().unwrap(),
                PutRevisionResult::Created | PutRevisionResult::AlreadyPresent
            ));
        }
        assert_eq!(store.get(&manifest.revision_id).await.unwrap(), manifest);
        assert_tmp_empty(&store).await;
    }
}
