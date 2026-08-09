use std::fmt;
use std::fs::File;
use std::io::{Read, Take};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::{Body, Client, RequestBuilder, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sync_core::{
    ApiError, CreateNamespaceRequest, HistoryTrashListResponse, HistoryTrashOperation, Namespace,
    NamespaceHeadResponse, NamespaceListResponse, OperationControl, ProtocolInfoResponseV4,
    PurgeHistoryTrashRequest, PurgeHistoryTrashResponse, PutObjectResponse,
    REMOTE_PROTOCOL_VERSION_V4, RenameNamespaceRequest, RestoreHistoryRequest,
    RevisionCommitRequestV4, RevisionCommitResponseV4, RevisionListResponse, RevisionRootV4,
    RevisionSummary, StorageObjectKindV4, StorageObjectRef, StorageObjectRefV4,
    TruncateHistoryRequest, TypedMissingObjectsRequest, TypedMissingObjectsResponse, digest_bytes,
};
use url::Url;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_JSON_RESPONSE_BYTES: u64 = 72 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: String) -> Result<Self> {
        if value.len() < 16 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            bail!("server token must contain at least 16 visible ASCII characters");
        }
        Ok(Self(value))
    }

    fn authorization_header(&self) -> Result<HeaderValue> {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", self.0))
            .context("server token is not a valid Authorization header value")?;
        value.set_sensitive(true);
        Ok(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([redacted])")
    }
}

#[derive(Clone)]
pub struct RemoteClient {
    client: Client,
    base_url: Url,
}

impl fmt::Debug for RemoteClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl RemoteClient {
    pub fn new(server_url: &str, token: SecretToken) -> Result<Self> {
        let base_url = normalize_server_url(server_url)?;
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, token.authorization_header()?);
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(Policy::none())
            .default_headers(headers)
            .user_agent(concat!("codex-session-sync/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to initialize HTTP client")?;
        Ok(Self { client, base_url })
    }

    pub fn info(&self) -> Result<ProtocolInfoResponseV4> {
        let response = self
            .client
            .get(self.endpoint("api/v4/info")?)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .context("failed to connect to synchronization server")?;
        let info: ProtocolInfoResponseV4 = parse_json_response(response)?;
        if info.service != "codex-session-sync" {
            bail!("server returned an unexpected service identifier");
        }
        if info.protocol_version != REMOTE_PROTOCOL_VERSION_V4 {
            bail!(
                "incompatible synchronization protocol: server {}, client {}",
                info.protocol_version,
                REMOTE_PROTOCOL_VERSION_V4
            );
        }
        Ok(info)
    }

    pub fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let response = self
            .client
            .get(self.endpoint("api/v4/namespaces")?)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .context("failed to list remote namespaces")?;
        Ok(parse_json_response::<NamespaceListResponse>(response)?.namespaces)
    }

    pub fn create_namespace(&self, display_name: String) -> Result<Namespace> {
        self.send_json(
            self.client.post(self.endpoint("api/v4/namespaces")?),
            &CreateNamespaceRequest { display_name },
        )
    }

    pub fn rename_namespace(&self, namespace_id: Uuid, display_name: String) -> Result<Namespace> {
        self.send_json(
            self.client
                .patch(self.endpoint(&format!("api/v4/namespaces/{namespace_id}"))?),
            &RenameNamespaceRequest { display_name },
        )
    }

    pub fn namespace_head(&self, namespace_id: Uuid) -> Result<Option<String>> {
        let response = self
            .client
            .get(self.endpoint(&format!("api/v4/namespaces/{namespace_id}/head"))?)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .context("failed to read namespace head")?;
        let response: NamespaceHeadResponse = parse_json_response(response)?;
        if response.namespace_id != namespace_id {
            bail!("server returned a head for the wrong namespace");
        }
        Ok(response.head)
    }

    pub fn namespace_head_state(&self, namespace_id: Uuid) -> Result<NamespaceHeadResponse> {
        let response = self
            .client
            .get(self.endpoint(&format!("api/v4/namespaces/{namespace_id}/head"))?)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .context("failed to read namespace head")?;
        let response: NamespaceHeadResponse = parse_json_response(response)?;
        if response.namespace_id != namespace_id {
            bail!("server returned a head for the wrong namespace");
        }
        Ok(response)
    }

    pub fn list_revisions(&self, namespace_id: Uuid) -> Result<Vec<RevisionSummary>> {
        let response = self
            .client
            .get(self.endpoint(&format!("api/v4/namespaces/{namespace_id}/revisions"))?)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .context("failed to list remote revisions")?;
        Ok(parse_json_response::<RevisionListResponse>(response)?.revisions)
    }

    pub fn truncate_history(
        &self,
        namespace_id: Uuid,
        request: &TruncateHistoryRequest,
    ) -> Result<HistoryTrashOperation> {
        self.send_json(
            self.client.post(self.endpoint(&format!(
                "api/v4/namespaces/{namespace_id}/history/truncations"
            ))?),
            request,
        )
    }

    pub fn list_history_trash(&self, namespace_id: Uuid) -> Result<Vec<HistoryTrashOperation>> {
        let response = self
            .client
            .get(self.endpoint(&format!("api/v4/namespaces/{namespace_id}/trash"))?)
            .timeout(REQUEST_TIMEOUT)
            .send()?;
        Ok(parse_json_response::<HistoryTrashListResponse>(response)?.operations)
    }

    pub fn restore_history_trash(
        &self,
        namespace_id: Uuid,
        operation_id: Uuid,
        request: &RestoreHistoryRequest,
    ) -> Result<HistoryTrashOperation> {
        self.send_json(
            self.client.post(self.endpoint(&format!(
                "api/v4/namespaces/{namespace_id}/trash/{operation_id}/restore"
            ))?),
            request,
        )
    }

    pub fn purge_history_trash(
        &self,
        namespace_id: Uuid,
        request: &PurgeHistoryTrashRequest,
    ) -> Result<PurgeHistoryTrashResponse> {
        self.send_json(
            self.client
                .post(self.endpoint(&format!("api/v4/namespaces/{namespace_id}/trash/purge"))?),
            request,
        )
    }

    pub fn missing_typed_objects(
        &self,
        objects: Vec<StorageObjectRef>,
    ) -> Result<Vec<StorageObjectRef>> {
        let response: TypedMissingObjectsResponse = self.send_json(
            self.client.post(self.endpoint("api/v4/objects/missing")?),
            &TypedMissingObjectsRequest { objects },
        )?;
        Ok(response.missing)
    }

    #[cfg(test)]
    pub fn upload_typed_object(
        &self,
        object: &StorageObjectRef,
        path: &Path,
        control: &OperationControl,
    ) -> Result<bool> {
        self.upload_typed_object_with_progress(
            object,
            path,
            control,
            Arc::new(AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
    }

    pub fn upload_typed_object_with_progress(
        &self,
        object: &StorageObjectRef,
        path: &Path,
        control: &OperationControl,
        internal_stop: Arc<AtomicBool>,
        on_bytes: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> Result<bool> {
        let digest = raw_digest(&object.sha256)?;
        if std::fs::metadata(path)?.len() != object.byte_length {
            bail!("typed object length changed before upload");
        }
        let file = File::open(path)?;
        let response = self
            .client
            .put(self.endpoint(&format!(
                "api/v4/objects/{}/{digest}",
                object.kind.wire_name()
            ))?)
            .header(CONTENT_LENGTH, object.byte_length)
            .body(Body::sized(
                CancellableReader {
                    inner: file,
                    control: control.clone(),
                    internal_stop,
                    on_bytes,
                },
                object.byte_length,
            ))
            .send()?;
        let response: PutObjectResponse = parse_json_response(response)?;
        if response.sha256 != object.sha256 || response.byte_length != object.byte_length {
            bail!("server acknowledged a different typed object");
        }
        Ok(response.created)
    }

    pub fn download_typed_object(&self, object: &StorageObjectRef) -> Result<Response> {
        let digest = raw_digest(&object.sha256)?;
        ensure_success(
            self.client
                .get(self.endpoint(&format!(
                    "api/v4/objects/{}/{digest}",
                    object.kind.wire_name()
                ))?)
                .send()?,
        )
    }

    pub fn missing_v4_objects(
        &self,
        objects: Vec<StorageObjectRefV4>,
    ) -> Result<Vec<StorageObjectRefV4>> {
        let legacy = objects
            .into_iter()
            .map(v4_object_to_transport)
            .collect::<Result<Vec<_>>>()?;
        self.missing_typed_objects(legacy)?
            .into_iter()
            .map(transport_object_to_v4)
            .collect()
    }

    #[cfg(test)]
    pub fn upload_v4_object(
        &self,
        object: &StorageObjectRefV4,
        path: &Path,
        control: &OperationControl,
    ) -> Result<bool> {
        self.upload_typed_object(&v4_object_to_transport(object.clone())?, path, control)
    }

    pub fn upload_v4_object_with_progress(
        &self,
        object: &StorageObjectRefV4,
        path: &Path,
        control: &OperationControl,
        internal_stop: Arc<AtomicBool>,
        on_bytes: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> Result<bool> {
        self.upload_typed_object_with_progress(
            &v4_object_to_transport(object.clone())?,
            path,
            control,
            internal_stop,
            on_bytes,
        )
    }

    pub fn download_v4_object(&self, object: &StorageObjectRefV4) -> Result<Response> {
        self.download_typed_object(&v4_object_to_transport(object.clone())?)
    }

    pub fn revision_root_v4(&self, revision_id: &str) -> Result<RevisionRootV4> {
        let object = StorageObjectRef {
            kind: sync_core::StorageObjectKind::RevisionRoot,
            sha256: revision_id.to_string(),
            byte_length: 0,
        };
        let bytes = self.download_typed_object(&object)?.bytes()?;
        if digest_bytes(&bytes) != revision_id {
            bail!("server returned a corrupt v4 revision root");
        }
        let root: RevisionRootV4 = serde_json::from_slice(&bytes)?;
        root.validate()?;
        if root.revision_id()? != revision_id {
            bail!("server returned a non-canonical v4 revision root");
        }
        Ok(root)
    }

    pub fn commit_revision_v4(
        &self,
        namespace_id: Uuid,
        request: &RevisionCommitRequestV4,
    ) -> Result<RevisionCommitResponseV4> {
        self.send_json(
            self.client.post(self.endpoint(&format!(
                "api/v4/namespaces/{namespace_id}/revisions/commit"
            ))?),
            request,
        )
    }

    fn endpoint(&self, relative: &str) -> Result<Url> {
        self.base_url
            .join(relative)
            .with_context(|| format!("failed to construct server endpoint {relative}"))
    }

    fn send_json<T: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        payload: &T,
    ) -> Result<R> {
        let response = request
            .timeout(REQUEST_TIMEOUT)
            .json(payload)
            .send()
            .context("synchronization server request failed")?;
        parse_json_response(response)
    }
}

fn v4_object_to_transport(object: StorageObjectRefV4) -> Result<StorageObjectRef> {
    let kind = match object.kind {
        StorageObjectKindV4::Whole => sync_core::StorageObjectKind::Whole,
        StorageObjectKindV4::Chunk => sync_core::StorageObjectKind::Chunk,
        StorageObjectKindV4::ChunkManifest => sync_core::StorageObjectKind::ChunkManifest,
        StorageObjectKindV4::Thread => sync_core::StorageObjectKind::Thread,
        StorageObjectKindV4::RevisionRoot => sync_core::StorageObjectKind::RevisionRoot,
        StorageObjectKindV4::SnapshotOverlay => bail!("snapshot overlays are local-only"),
    };
    Ok(StorageObjectRef {
        kind,
        sha256: object.sha256,
        byte_length: object.byte_length,
    })
}

fn transport_object_to_v4(object: StorageObjectRef) -> Result<StorageObjectRefV4> {
    let kind = match object.kind {
        sync_core::StorageObjectKind::Whole => StorageObjectKindV4::Whole,
        sync_core::StorageObjectKind::Chunk => StorageObjectKindV4::Chunk,
        sync_core::StorageObjectKind::ChunkManifest => StorageObjectKindV4::ChunkManifest,
        sync_core::StorageObjectKind::Thread => StorageObjectKindV4::Thread,
        sync_core::StorageObjectKind::RevisionRoot => StorageObjectKindV4::RevisionRoot,
    };
    Ok(StorageObjectRefV4 {
        kind,
        sha256: object.sha256,
        byte_length: object.byte_length,
    })
}

struct CancellableReader<R> {
    inner: R,
    control: OperationControl,
    internal_stop: Arc<AtomicBool>,
    on_bytes: Arc<dyn Fn(u64) + Send + Sync>,
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.control.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "operation cancelled",
            ));
        }
        if self.internal_stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "parallel upload stopped after another object failed",
            ));
        }
        let count = self.inner.read(buffer)?;
        if count > 0 {
            (self.on_bytes)(count as u64);
        }
        Ok(count)
    }
}

pub fn normalize_server_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value.trim()).context("server URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("server URL must use http or https");
    }
    if url.host_str().is_none() {
        bail!("server URL must include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("server URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("server URL must not contain a query or fragment");
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn raw_digest(value: &str) -> Result<&str> {
    sync_core::validate_sha256(value)
        .map_err(|_| anyhow::anyhow!("invalid SHA-256 identifier {value}"))?;
    value
        .strip_prefix("sha256:")
        .context("SHA-256 identifier has no algorithm prefix")
}

fn parse_json_response<T: DeserializeOwned>(response: Response) -> Result<T> {
    let response = ensure_success(response)?;
    let bytes = read_limited(
        response.take(MAX_JSON_RESPONSE_BYTES + 1),
        MAX_JSON_RESPONSE_BYTES,
    )?;
    serde_json::from_slice(&bytes).context("server returned invalid JSON")
}

fn ensure_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let bytes = read_limited(
        response.take(MAX_ERROR_RESPONSE_BYTES + 1),
        MAX_ERROR_RESPONSE_BYTES,
    )
    .unwrap_or_default();
    if let Ok(error) = serde_json::from_slice::<ApiError>(&bytes) {
        bail!("remote error {}: {}", api_error_code(&error), error.message);
    }
    bail!("synchronization server returned HTTP {status}")
}

fn api_error_code(error: &ApiError) -> String {
    serde_json::to_value(error.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn read_limited(mut reader: Take<Response>, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("server response exceeds the configured size limit");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use sha2::{Digest, Sha256};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicU64;
    use std::thread;
    use sync_core::{StorageObjectKind, StorageObjectRef};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn token_debug_is_redacted() {
        let value = "server-token-123456".to_string();
        let token = SecretToken::new(value.clone()).unwrap();
        let rendered = format!("{token:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains(&value));
    }

    #[test]
    fn server_url_is_normalized_and_rejects_embedded_credentials() {
        assert_eq!(
            normalize_server_url("https://example.test/sync")
                .unwrap()
                .as_str(),
            "https://example.test/sync/"
        );
        assert!(normalize_server_url("ftp://example.test").is_err());
        assert!(normalize_server_url("https://user:secret@example.test").is_err());
        assert!(normalize_server_url("https://example.test?token=secret").is_err());
    }

    #[test]
    fn object_download_can_exceed_the_timeout_window_while_the_stream_keeps_making_progress() {
        let content = b"slow-but-active-object";
        let sha256 = format!("sha256:{}", hex::encode(Sha256::digest(content)));
        let descriptor = StorageObjectRef {
            kind: StorageObjectKind::Whole,
            sha256,
            byte_length: content.len() as u64,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                content.len()
            )
            .unwrap();
            stream.flush().unwrap();
            for chunk in content.chunks(5) {
                thread::sleep(Duration::from_millis(70));
                if stream.write_all(chunk).is_err() {
                    break;
                }
                let _ = stream.flush();
            }
        });

        let client = RemoteClient::new(
            &format!("http://{address}"),
            SecretToken::new("test-token-that-is-long-enough-for-auth".to_string()).unwrap(),
        )
        .unwrap();
        let downloaded = client
            .download_typed_object(&descriptor)
            .unwrap()
            .bytes()
            .unwrap();
        server.join().unwrap();

        assert_eq!(downloaded.as_ref(), content);
    }

    #[test]
    fn client_runs_the_authenticated_v4_namespace_and_typed_object_flow() {
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
        assert_eq!(
            client.info().unwrap().protocol_version,
            REMOTE_PROTOCOL_VERSION_V4
        );
        assert!(client.list_namespaces().unwrap().is_empty());
        let _namespace = client.create_namespace("Personal".to_string()).unwrap();

        let content = b"remote-client-object";
        let sha256 = format!("sha256:{}", hex::encode(Sha256::digest(content)));
        let descriptor = StorageObjectRef {
            kind: StorageObjectKind::Whole,
            sha256: sha256.clone(),
            byte_length: content.len() as u64,
        };
        assert_eq!(
            client
                .missing_typed_objects(vec![descriptor.clone()])
                .unwrap(),
            vec![descriptor.clone()]
        );
        let object_path = temp.path().join("object");
        std::fs::write(&object_path, content).unwrap();
        let transferred = Arc::new(AtomicU64::new(0));
        let captured = transferred.clone();
        assert!(
            client
                .upload_typed_object_with_progress(
                    &descriptor,
                    &object_path,
                    &OperationControl::default(),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(move |count| {
                        captured.fetch_add(count, Ordering::Relaxed);
                    }),
                )
                .unwrap()
        );
        assert_eq!(transferred.load(Ordering::Relaxed), content.len() as u64);
        assert!(
            client
                .missing_typed_objects(vec![descriptor.clone()])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            client
                .download_typed_object(&descriptor)
                .unwrap()
                .bytes()
                .unwrap()
                .as_ref(),
            content
        );

        task.abort();
        runtime.block_on(async {
            let _ = task.await;
        });
    }
}
