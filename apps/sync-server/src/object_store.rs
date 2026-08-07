use std::collections::BTreeSet;
use std::fmt::Display;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use sync_core::{ObjectDescriptor, validate_sha256};
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ObjectStore {
    sha256_dir: PathBuf,
    tmp_dir: PathBuf,
    max_object_bytes: u64,
    metrics: Arc<ObjectStoreMetrics>,
}

#[derive(Debug, Default)]
struct ObjectStoreMetrics {
    file_sync_count: AtomicU64,
    file_sync_bytes: AtomicU64,
    file_sync_nanos: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutObjectResult {
    Created,
    AlreadyPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectorySyncReport {
    pub directory_count: usize,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectStoreIoMetrics {
    pub file_sync_count: u64,
    pub file_sync_bytes: u64,
    pub file_sync_nanos: u64,
}

#[derive(Debug)]
pub struct ObjectDownload {
    pub file: File,
    pub byte_length: u64,
}

#[derive(Debug, Error)]
pub enum ObjectStoreError {
    #[error("invalid SHA-256 object identifier: {digest}")]
    InvalidDigest { digest: String },
    #[error("object stream failed: {message}")]
    Stream { message: String },
    #[error("object exceeds the configured {max_bytes} byte limit (received {actual_bytes} bytes)")]
    ObjectTooLarge { max_bytes: u64, actual_bytes: u64 },
    #[error(
        "object length mismatch: expected {expected_bytes} bytes, received {actual_bytes} bytes"
    )]
    LengthMismatch {
        expected_bytes: u64,
        actual_bytes: u64,
    },
    #[error("object hash mismatch: expected {expected}, calculated {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("object not found: {digest}")]
    NotFound { digest: String },
    #[error("failed to {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl ObjectStore {
    pub async fn open(
        data_dir: impl AsRef<Path>,
        max_object_bytes: u64,
    ) -> Result<Self, ObjectStoreError> {
        let objects_dir = data_dir.as_ref().join("objects");
        let sha256_dir = objects_dir.join("sha256");
        let tmp_dir = objects_dir.join("tmp");
        create_dir_all(&sha256_dir).await?;
        create_dir_all(&tmp_dir).await?;
        cleanup_temp_dir(&tmp_dir).await?;

        Ok(Self {
            sha256_dir,
            tmp_dir,
            max_object_bytes,
            metrics: Arc::new(ObjectStoreMetrics::default()),
        })
    }

    pub async fn put_stream<S, E>(
        &self,
        expected_digest: &str,
        expected_length: u64,
        stream: S,
    ) -> Result<PutObjectResult, ObjectStoreError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send,
        E: Display + Send + Sync + 'static,
    {
        let target_path = self.object_path(expected_digest)?;
        if expected_length > self.max_object_bytes {
            return Err(ObjectStoreError::ObjectTooLarge {
                max_bytes: self.max_object_bytes,
                actual_bytes: expected_length,
            });
        }

        let parent = target_path
            .parent()
            .expect("content-addressed object paths always have a parent");
        create_dir_all(parent).await?;

        let temp_path = self
            .tmp_dir
            .join(format!("{}.part", Uuid::now_v7().simple()));
        let mut temp_guard = TempFileGuard::new(temp_path.clone());
        let write_result = self
            .write_verified_temp(&temp_path, expected_digest, expected_length, stream)
            .await;
        if let Err(error) = write_result {
            remove_temp(&temp_path).await;
            return Err(error);
        }

        match file_exists(&target_path).await {
            Ok(true) => {
                let verify_result = self
                    .verify_file(&target_path, expected_digest, Some(expected_length))
                    .await;
                remove_temp(&temp_path).await;
                verify_result?;
                return Ok(PutObjectResult::AlreadyPresent);
            }
            Ok(false) => {}
            Err(error) => {
                remove_temp(&temp_path).await;
                return Err(error);
            }
        }

        match fs::rename(&temp_path, &target_path).await {
            Ok(()) => {
                temp_guard.disarm();
                Ok(PutObjectResult::Created)
            }
            Err(rename_error) => {
                // Windows rename does not replace an existing target. A concurrent
                // writer may therefore win between the existence check and rename.
                match file_exists(&target_path).await {
                    Ok(true) => {
                        let verify_result = self
                            .verify_file(&target_path, expected_digest, Some(expected_length))
                            .await;
                        remove_temp(&temp_path).await;
                        verify_result?;
                        Ok(PutObjectResult::AlreadyPresent)
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

    pub async fn sync_object_directories(
        &self,
        digests: &[String],
    ) -> Result<DirectorySyncReport, ObjectStoreError> {
        let mut directories = BTreeSet::new();
        for digest in digests {
            let path = self.object_path(digest)?;
            directories.insert(
                path.parent()
                    .expect("content-addressed object paths always have a parent")
                    .to_path_buf(),
            );
        }
        let started = Instant::now();
        for directory in &directories {
            sync_directory(directory).await?;
        }
        if !directories.is_empty() {
            sync_directory(&self.sha256_dir).await?;
        }
        Ok(DirectorySyncReport {
            directory_count: directories.len() + usize::from(!directories.is_empty()),
            elapsed: started.elapsed(),
        })
    }

    pub async fn contains(&self, digest: &str) -> Result<bool, ObjectStoreError> {
        let path = self.object_path(digest)?;
        file_exists(&path).await
    }

    pub fn io_metrics(&self) -> ObjectStoreIoMetrics {
        ObjectStoreIoMetrics {
            file_sync_count: self.metrics.file_sync_count.load(Ordering::Relaxed),
            file_sync_bytes: self.metrics.file_sync_bytes.load(Ordering::Relaxed),
            file_sync_nanos: self.metrics.file_sync_nanos.load(Ordering::Relaxed),
        }
    }

    pub async fn missing(
        &self,
        objects: &[ObjectDescriptor],
    ) -> Result<Vec<String>, ObjectStoreError> {
        let mut missing = Vec::new();
        for object in objects {
            let path = self.object_path(&object.sha256)?;
            if object.byte_length > self.max_object_bytes {
                return Err(ObjectStoreError::ObjectTooLarge {
                    max_bytes: self.max_object_bytes,
                    actual_bytes: object.byte_length,
                });
            }

            match fs::metadata(&path).await {
                Ok(metadata) if metadata.is_file() => {
                    if metadata.len() != object.byte_length {
                        return Err(ObjectStoreError::LengthMismatch {
                            expected_bytes: object.byte_length,
                            actual_bytes: metadata.len(),
                        });
                    }
                }
                Ok(_) => return Err(not_a_file(&path)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing.push(object.sha256.clone());
                }
                Err(error) => return Err(io_error("read metadata for", &path, error)),
            }
        }
        Ok(missing)
    }

    pub async fn open_download(&self, digest: &str) -> Result<ObjectDownload, ObjectStoreError> {
        let path = self.object_path(digest)?;
        let file = match File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ObjectStoreError::NotFound {
                    digest: digest.to_string(),
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
        if metadata.len() > self.max_object_bytes {
            return Err(ObjectStoreError::ObjectTooLarge {
                max_bytes: self.max_object_bytes,
                actual_bytes: metadata.len(),
            });
        }

        Ok(ObjectDownload {
            file,
            byte_length: metadata.len(),
        })
    }

    pub async fn quarantine(
        &self,
        digest: &str,
        destination: &Path,
    ) -> Result<bool, ObjectStoreError> {
        let source = self.object_path(digest)?;
        if !file_exists(&source).await? {
            return file_exists(destination).await;
        }
        if let Some(parent) = destination.parent() {
            create_dir_all(parent).await?;
        }
        match tokio::fs::rename(&source, destination).await {
            Ok(()) => {
                if let Some(parent) = source.parent() {
                    sync_directory(parent).await?;
                }
                if let Some(parent) = destination.parent() {
                    sync_directory(parent).await?;
                }
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(file_exists(destination).await?)
            }
            Err(source_error) => Err(io_error("quarantine object", &source, source_error)),
        }
    }

    async fn write_verified_temp<S, E>(
        &self,
        temp_path: &Path,
        expected_digest: &str,
        expected_length: u64,
        stream: S,
    ) -> Result<(), ObjectStoreError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send,
        E: Display + Send + Sync + 'static,
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temp_path)
            .await
            .map_err(|error| io_error("create", temp_path, error))?;
        let mut hasher = Sha256::new();
        let mut actual_length = 0_u64;
        futures_util::pin_mut!(stream);

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ObjectStoreError::Stream {
                message: error.to_string(),
            })?;
            let chunk_length = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            actual_length = actual_length.checked_add(chunk_length).ok_or(
                ObjectStoreError::ObjectTooLarge {
                    max_bytes: self.max_object_bytes,
                    actual_bytes: u64::MAX,
                },
            )?;
            if actual_length > self.max_object_bytes {
                return Err(ObjectStoreError::ObjectTooLarge {
                    max_bytes: self.max_object_bytes,
                    actual_bytes: actual_length,
                });
            }
            if actual_length > expected_length {
                return Err(ObjectStoreError::LengthMismatch {
                    expected_bytes: expected_length,
                    actual_bytes: actual_length,
                });
            }

            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| io_error("write", temp_path, error))?;
        }

        if actual_length != expected_length {
            return Err(ObjectStoreError::LengthMismatch {
                expected_bytes: expected_length,
                actual_bytes: actual_length,
            });
        }
        let actual_digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        if actual_digest != expected_digest {
            return Err(ObjectStoreError::HashMismatch {
                expected: expected_digest.to_string(),
                actual: actual_digest,
            });
        }

        file.flush()
            .await
            .map_err(|error| io_error("flush", temp_path, error))?;
        let sync_started = Instant::now();
        file.sync_all()
            .await
            .map_err(|error| io_error("sync", temp_path, error))?;
        let sync_elapsed = sync_started.elapsed();
        self.metrics.file_sync_count.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .file_sync_bytes
            .fetch_add(expected_length, Ordering::Relaxed);
        self.metrics.file_sync_nanos.fetch_add(
            u64::try_from(sync_elapsed.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        tracing::debug!(
            object_bytes = expected_length,
            file_sync_ms = duration_ms(sync_elapsed),
            "synchronized uploaded object data"
        );
        Ok(())
    }

    async fn verify_file(
        &self,
        path: &Path,
        expected_digest: &str,
        expected_length: Option<u64>,
    ) -> Result<u64, ObjectStoreError> {
        let mut file = File::open(path)
            .await
            .map_err(|error| io_error("open", path, error))?;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut hasher = Sha256::new();
        let mut actual_length = 0_u64;

        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| io_error("read", path, error))?;
            if read == 0 {
                break;
            }
            actual_length =
                actual_length
                    .checked_add(read as u64)
                    .ok_or(ObjectStoreError::ObjectTooLarge {
                        max_bytes: self.max_object_bytes,
                        actual_bytes: u64::MAX,
                    })?;
            if actual_length > self.max_object_bytes {
                return Err(ObjectStoreError::ObjectTooLarge {
                    max_bytes: self.max_object_bytes,
                    actual_bytes: actual_length,
                });
            }
            hasher.update(&buffer[..read]);
        }

        if let Some(expected_length) = expected_length
            && actual_length != expected_length
        {
            return Err(ObjectStoreError::LengthMismatch {
                expected_bytes: expected_length,
                actual_bytes: actual_length,
            });
        }
        let actual_digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        if actual_digest != expected_digest {
            return Err(ObjectStoreError::HashMismatch {
                expected: expected_digest.to_string(),
                actual: actual_digest,
            });
        }
        Ok(actual_length)
    }

    fn object_path(&self, digest: &str) -> Result<PathBuf, ObjectStoreError> {
        validate_sha256(digest).map_err(|_| ObjectStoreError::InvalidDigest {
            digest: digest.to_string(),
        })?;
        let hex_digest = &digest["sha256:".len()..];
        Ok(self
            .sha256_dir
            .join(&hex_digest[..2])
            .join(&hex_digest[2..]))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn create_dir_all(path: &Path) -> Result<(), ObjectStoreError> {
    fs::create_dir_all(path)
        .await
        .map_err(|error| io_error("create directory", path, error))
}

async fn file_exists(path: &Path) -> Result<bool, ObjectStoreError> {
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

async fn cleanup_temp_dir(path: &Path) -> Result<(), ObjectStoreError> {
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
async fn sync_directory(path: &Path) -> Result<(), ObjectStoreError> {
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
async fn sync_directory(_path: &Path) -> Result<(), ObjectStoreError> {
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

fn not_a_file(path: &Path) -> ObjectStoreError {
    io_error(
        "use non-file object path",
        path,
        io::Error::new(io::ErrorKind::InvalidData, "path is not a regular file"),
    )
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> ObjectStoreError {
    ObjectStoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::io;

    use futures_util::stream;
    use tempfile::tempdir;
    use tokio::io::AsyncReadExt;

    use super::*;

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    fn byte_stream(
        chunks: impl IntoIterator<Item = &'static [u8]>,
    ) -> impl Stream<Item = Result<Bytes, Infallible>> {
        stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk))),
        )
    }

    async fn assert_tmp_empty(store: &ObjectStore) {
        let mut entries = fs::read_dir(&store.tmp_dir).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stores_stream_and_supports_lookup_and_download() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let content = b"hello world";
        let object_digest = digest(content);

        let result = store
            .put_stream(
                &object_digest,
                content.len() as u64,
                byte_stream([&b"hello "[..], &b"world"[..]]),
            )
            .await
            .unwrap();
        assert_eq!(result, PutObjectResult::Created);
        assert!(store.contains(&object_digest).await.unwrap());
        assert!(
            store
                .missing(&[ObjectDescriptor {
                    sha256: object_digest.clone(),
                    byte_length: content.len() as u64,
                }])
                .await
                .unwrap()
                .is_empty()
        );

        let mut download = store.open_download(&object_digest).await.unwrap();
        assert_eq!(download.byte_length, content.len() as u64);
        let mut downloaded = Vec::new();
        download.file.read_to_end(&mut downloaded).await.unwrap();
        assert_eq!(downloaded, content);
        let metrics = store.io_metrics();
        assert_eq!(metrics.file_sync_count, 1);
        assert_eq!(metrics.file_sync_bytes, content.len() as u64);
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn rejects_hash_length_and_size_mismatches_and_cleans_temp_files() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 5).await.unwrap();
        let content = b"hello";

        let hash_error = store
            .put_stream(
                &format!("sha256:{}", "0".repeat(64)),
                content.len() as u64,
                byte_stream([&content[..]]),
            )
            .await
            .unwrap_err();
        assert!(matches!(hash_error, ObjectStoreError::HashMismatch { .. }));
        assert_tmp_empty(&store).await;

        let length_error = store
            .put_stream(&digest(content), 4, byte_stream([&content[..]]))
            .await
            .unwrap_err();
        assert!(matches!(
            length_error,
            ObjectStoreError::LengthMismatch { .. }
        ));
        assert_tmp_empty(&store).await;

        let size_error = store
            .put_stream(&digest(b"123456"), 6, byte_stream([&b"123456"[..]]))
            .await
            .unwrap_err();
        assert!(matches!(
            size_error,
            ObjectStoreError::ObjectTooLarge { .. }
        ));
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn repeated_upload_is_idempotent_and_validates_existing_file() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let content = b"same object";
        let object_digest = digest(content);

        assert_eq!(
            store
                .put_stream(
                    &object_digest,
                    content.len() as u64,
                    byte_stream([&content[..]])
                )
                .await
                .unwrap(),
            PutObjectResult::Created
        );
        assert_eq!(
            store
                .put_stream(
                    &object_digest,
                    content.len() as u64,
                    byte_stream([&content[..]])
                )
                .await
                .unwrap(),
            PutObjectResult::AlreadyPresent
        );

        fs::write(store.object_path(&object_digest).unwrap(), b"corrupt")
            .await
            .unwrap();
        let error = store
            .put_stream(
                &object_digest,
                content.len() as u64,
                byte_stream([&content[..]]),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ObjectStoreError::LengthMismatch { .. } | ObjectStoreError::HashMismatch { .. }
        ));
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn rejects_noncanonical_digest_and_reports_missing_objects() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let invalid = format!("sha256:{}", "A".repeat(64));
        assert!(matches!(
            store.contains(&invalid).await,
            Err(ObjectStoreError::InvalidDigest { .. })
        ));

        let missing_digest = digest(b"missing");
        assert_eq!(
            store
                .missing(&[ObjectDescriptor {
                    sha256: missing_digest.clone(),
                    byte_length: 7,
                }])
                .await
                .unwrap(),
            vec![missing_digest]
        );
    }

    #[tokio::test]
    async fn stream_failure_cleans_temporary_file() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let stream = stream::iter([
            Ok(Bytes::from_static(b"partial")),
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "test failure")),
        ]);

        assert!(matches!(
            store.put_stream(&digest(b"partial"), 7, stream).await,
            Err(ObjectStoreError::Stream { .. })
        ));
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn cancelled_upload_cleans_temporary_file() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let stream = stream::once(async { Ok::<_, io::Error>(Bytes::from_static(b"partial")) })
            .chain(stream::pending());
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .put_stream(&digest(b"partial-and-more"), 16, stream)
                .await
        });

        for _ in 0..100 {
            let mut entries = fs::read_dir(&store.tmp_dir).await.unwrap();
            if entries.next_entry().await.unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn startup_removes_stale_temporary_files() {
        let root = tempdir().unwrap();
        let tmp_dir = root.path().join("objects").join("tmp");
        fs::create_dir_all(&tmp_dir).await.unwrap();
        fs::write(tmp_dir.join("stale.part"), b"stale")
            .await
            .unwrap();

        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        assert_tmp_empty(&store).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_identical_uploads_are_idempotent() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let content = b"concurrent object";
        let object_digest = digest(content);
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let object_digest = object_digest.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .put_stream(
                        &object_digest,
                        content.len() as u64,
                        byte_stream([&content[..]]),
                    )
                    .await
            }));
        }

        for task in tasks {
            assert!(matches!(
                task.await.unwrap().unwrap(),
                PutObjectResult::Created | PutObjectResult::AlreadyPresent
            ));
        }
        let mut download = store.open_download(&object_digest).await.unwrap();
        let mut downloaded = Vec::new();
        download.file.read_to_end(&mut downloaded).await.unwrap();
        assert_eq!(downloaded, content);
        assert_tmp_empty(&store).await;
    }

    #[tokio::test]
    async fn commit_barrier_deduplicates_object_prefix_directories() {
        let root = tempdir().unwrap();
        let store = ObjectStore::open(root.path(), 1024).await.unwrap();
        let first = digest(b"same-prefix-a");
        let prefix = &first["sha256:".len()..][..2];
        let second = (0_u64..)
            .map(|index| digest(format!("same-prefix-{index}").as_bytes()))
            .find(|candidate| &candidate["sha256:".len()..][..2] == prefix && candidate != &first)
            .unwrap();
        let different = (0_u64..)
            .map(|index| digest(format!("different-prefix-{index}").as_bytes()))
            .find(|candidate| &candidate["sha256:".len()..][..2] != prefix)
            .unwrap();

        for value in [&first, &second, &different] {
            let path = store.object_path(value).unwrap();
            fs::create_dir_all(path.parent().unwrap()).await.unwrap();
            fs::write(path, b"present").await.unwrap();
        }
        let report = store
            .sync_object_directories(&[first, second, different])
            .await
            .unwrap();

        assert_eq!(report.directory_count, 3);
    }
}
