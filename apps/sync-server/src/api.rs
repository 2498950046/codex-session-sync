use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Serialize;
use sync_core::{
    ChunkManifest, CommitRevisionResponse, CommitRevisionRootRequest, ContentRef,
    CreateNamespaceRequest, HistoryTrashListResponse, NamespaceHeadResponse, NamespaceListResponse,
    ProtocolCapabilities, ProtocolInfoResponse, ProtocolLimits, PutObjectResponse,
    REMOTE_PROTOCOL_VERSION, RenameNamespaceRequest, RestoreHistoryRequest, RevisionListResponse,
    RevisionRootV2, ServerGcPlan, ServerGcQuarantineRequest, ServerGcQuarantineResponse,
    ServerStorageSummary, StorageObjectKind, StorageObjectRef, StorageRef, ThreadDescriptor,
    TruncateHistoryRequest, TypedMissingObjectsRequest, TypedMissingObjectsResponse,
    canonical_json, digest_bytes, validate_sha256,
};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::auth::{AuthState, require_auth};
use crate::config::ServerConfig;
use crate::error::HttpError;
use crate::metadata::{GcQueueEntry, MetadataStore, NewRevisionMetadata};
use crate::object_store::{ObjectStore, PutObjectResult};

const MAX_MISSING_OBJECTS: usize = 10_000;
const REQUEST_ENVELOPE_OVERHEAD_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    metadata: MetadataStore,
    typed_objects: BTreeMap<StorageObjectKind, ObjectStore>,
    data_dir: PathBuf,
    gc_gate: Arc<tokio::sync::RwLock<()>>,
}

impl AppState {
    pub async fn initialize(config: &ServerConfig) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&config.data_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to create sync server data directory {}",
                    config.data_dir.display()
                )
            })?;
        let metadata = MetadataStore::new(config.data_dir.join("metadata.sqlite"));
        metadata
            .initialize()
            .await
            .context("failed to initialize server metadata")?;
        let mut typed_objects = BTreeMap::new();
        for kind in StorageObjectKind::ALL {
            let limit = kind.max_bytes().min(config.max_object_bytes);
            typed_objects.insert(
                kind,
                ObjectStore::open(config.data_dir.join("typed").join(kind.directory()), limit)
                    .await
                    .context("failed to initialize typed object storage")?,
            );
        }
        let state = Self {
            metadata,
            typed_objects,
            data_dir: config.data_dir.clone(),
            gc_gate: Arc::new(tokio::sync::RwLock::new(())),
        };
        state
            .resume_pending_gc()
            .await
            .map_err(|_| anyhow::anyhow!("failed to resume server garbage collection"))?;
        Ok(state)
    }

    async fn resume_pending_gc(&self) -> Result<(), HttpError> {
        let entries = self.metadata.pending_gc_entries().await?;
        self.process_gc_entries(entries).await?;
        Ok(())
    }

    async fn process_gc_entries(
        &self,
        entries: Vec<GcQueueEntry>,
    ) -> Result<(usize, u64), HttpError> {
        let mut count = 0_usize;
        let mut bytes = 0_u64;
        for entry in entries {
            if self
                .metadata
                .gc_object_is_reachable(entry.object.clone())
                .await?
            {
                self.metadata
                    .cancel_gc_entry(
                        entry.id,
                        "object became reachable before quarantine".to_string(),
                    )
                    .await?;
                continue;
            }
            let digest = entry
                .object
                .sha256
                .strip_prefix("sha256:")
                .ok_or_else(|| HttpError::invalid_digest("invalid queued object digest"))?;
            let destination = self
                .data_dir
                .join("gc")
                .join(entry.operation_id.to_string())
                .join(entry.object.kind.directory())
                .join(digest);
            typed_store(self, entry.object.kind)?
                .quarantine(&entry.object.sha256, &destination)
                .await?;
            count += 1;
            bytes = bytes.saturating_add(entry.object.byte_length);
            self.metadata.complete_gc_entry(entry).await?;
        }
        Ok((count, bytes))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

pub fn build_router(state: AppState, config: &ServerConfig) -> Router {
    let json_limit = usize::try_from(
        config
            .max_manifest_bytes
            .saturating_add(REQUEST_ENVELOPE_OVERHEAD_BYTES),
    )
    .unwrap_or(usize::MAX);
    let protected_v2 = Router::new()
        .route("/namespaces", get(list_namespaces).post(create_namespace))
        .route("/namespaces/{namespace_id}", patch(rename_namespace))
        .route("/namespaces/{namespace_id}/head", get(namespace_head))
        .route("/namespaces/{namespace_id}/revisions", get(list_revisions))
        .route(
            "/namespaces/{namespace_id}/revisions/commit",
            post(commit_revision_root),
        )
        .route(
            "/namespaces/{namespace_id}/history/truncations",
            post(truncate_history),
        )
        .route("/namespaces/{namespace_id}/trash", get(list_history_trash))
        .route(
            "/namespaces/{namespace_id}/trash/{operation_id}/restore",
            post(restore_history_trash),
        )
        .route("/objects/missing", post(missing_typed_objects))
        .route("/storage", get(server_storage_summary))
        .route("/gc/plan", get(plan_server_gc))
        .route("/gc/quarantine", post(quarantine_server_gc))
        .route(
            "/objects/{kind}/{digest}",
            put(put_typed_object).get(get_typed_object),
        )
        .layer(DefaultBodyLimit::max(json_limit))
        .layer(middleware::from_fn_with_state(
            AuthState::new(config.token.clone()),
            require_auth,
        ))
        .with_state(state.clone());

    Router::new()
        .route("/health", get(health))
        .route("/api/v2/info", get(info))
        .nest("/api/v2", protected_v2)
        .layer(TraceLayer::new_for_http())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "codex-session-sync",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn info() -> Json<ProtocolInfoResponse> {
    Json(ProtocolInfoResponse {
        service: "codex-session-sync".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: REMOTE_PROTOCOL_VERSION,
        capabilities: ProtocolCapabilities {
            chunked_objects: true,
            thread_descriptors: true,
            revision_roots_v2: true,
            garbage_collection: true,
        },
        limits: ProtocolLimits::default(),
    })
}

async fn plan_server_gc(State(state): State<AppState>) -> Result<Json<ServerGcPlan>, HttpError> {
    Ok(Json(state.metadata.plan_gc().await?))
}

async fn quarantine_server_gc(
    State(state): State<AppState>,
    payload: Result<Json<ServerGcQuarantineRequest>, JsonRejection>,
) -> Result<Json<ServerGcQuarantineResponse>, HttpError> {
    let Json(payload) = parse_json(payload)?;
    validate_sha256(&payload.plan_fingerprint)
        .map_err(|_| HttpError::invalid_digest("invalid GC plan fingerprint"))?;
    let _gc_guard = state.gc_gate.write().await;
    let (operation_id, entries) = state.metadata.enqueue_gc(payload.plan_fingerprint).await?;
    let (quarantined_object_count, quarantined_bytes) = state.process_gc_entries(entries).await?;
    Ok(Json(ServerGcQuarantineResponse {
        operation_id,
        quarantined_object_count,
        quarantined_bytes,
    }))
}

async fn server_storage_summary(
    State(state): State<AppState>,
) -> Result<Json<ServerStorageSummary>, HttpError> {
    let plan = state.metadata.plan_gc().await?;
    let repository_physical_bytes = directory_bytes(&state.data_dir.join("typed")).await?;
    let gc_quarantine_bytes = directory_bytes(&state.data_dir.join("gc")).await?;
    Ok(Json(ServerStorageSummary {
        object_count: plan.reachable_object_count + plan.candidates.len(),
        repository_physical_bytes,
        reachable_object_count: plan.reachable_object_count,
        reachable_physical_bytes: repository_physical_bytes.saturating_sub(plan.reclaimable_bytes),
        reclaimable_object_count: plan.candidates.len(),
        reclaimable_bytes: plan.reclaimable_bytes,
        gc_quarantine_bytes,
    }))
}

async fn directory_bytes(root: &std::path::Path) -> Result<u64, HttpError> {
    if !tokio::fs::try_exists(root)
        .await
        .map_err(HttpError::internal)?
    {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .map_err(HttpError::internal)?;
        while let Some(entry) = entries.next_entry().await.map_err(HttpError::internal)? {
            let metadata = entry.metadata().await.map_err(HttpError::internal)?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

async fn missing_typed_objects(
    State(state): State<AppState>,
    payload: Result<Json<TypedMissingObjectsRequest>, JsonRejection>,
) -> Result<Json<TypedMissingObjectsResponse>, HttpError> {
    let Json(payload) = parse_json(payload)?;
    if payload.objects.len() > MAX_MISSING_OBJECTS {
        return Err(HttpError::invalid_request(format!(
            "objects must contain no more than {MAX_MISSING_OBJECTS} entries"
        )));
    }
    let mut unique = BTreeMap::new();
    for object in payload.objects {
        if object.byte_length > object.kind.max_bytes() {
            return Err(HttpError::invalid_request(
                "typed object exceeds its kind limit",
            ));
        }
        let key = (object.kind, object.sha256.clone());
        if let Some(existing) = unique.insert(key, object.byte_length)
            && existing != object.byte_length
        {
            return Err(HttpError::invalid_request(
                "the same typed object cannot have different byte lengths",
            ));
        }
    }
    let mut missing = Vec::new();
    for ((kind, sha256), byte_length) in unique {
        let store = typed_store(&state, kind)?;
        if !store
            .missing(&[sync_core::ObjectDescriptor {
                sha256: sha256.clone(),
                byte_length,
            }])
            .await?
            .is_empty()
        {
            missing.push(StorageObjectRef {
                kind,
                sha256,
                byte_length,
            });
        }
    }
    Ok(Json(TypedMissingObjectsResponse { missing }))
}

async fn put_typed_object(
    State(state): State<AppState>,
    Path((kind, digest)): Path<(String, String)>,
    request: Request,
) -> Result<impl IntoResponse, HttpError> {
    let _gc_guard = state.gc_gate.read().await;
    let kind = parse_object_kind(&kind)?;
    let sha256 = object_id_from_path(&digest)?;
    let expected_length = parse_content_length(request.headers())?;
    if expected_length > kind.max_bytes() {
        return Err(HttpError::payload_too_large(
            "typed object exceeds its kind limit",
        ));
    }
    let result = typed_store(&state, kind)?
        .put_stream(
            &sha256,
            expected_length,
            request.into_body().into_data_stream(),
        )
        .await?;
    let created = matches!(result, PutObjectResult::Created);
    state
        .metadata
        .record_storage_object(StorageObjectRef {
            kind,
            sha256: sha256.clone(),
            byte_length: expected_length,
        })
        .await?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(PutObjectResponse {
            sha256,
            byte_length: expected_length,
            created,
        }),
    ))
}

async fn get_typed_object(
    State(state): State<AppState>,
    Path((kind, digest)): Path<(String, String)>,
) -> Result<Response, HttpError> {
    let kind = parse_object_kind(&kind)?;
    let sha256 = object_id_from_path(&digest)?;
    let download = typed_store(&state, kind)?.open_download(&sha256).await?;
    let mut response = Response::new(Body::from_stream(ReaderStream::new(download.file)));
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&download.byte_length.to_string())
            .map_err(|error| HttpError::invalid_request(error.to_string()))?,
    );
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        ETAG,
        HeaderValue::from_str(&format!("\"{sha256}\""))
            .map_err(|error| HttpError::invalid_request(error.to_string()))?,
    );
    Ok(response)
}

fn typed_store(state: &AppState, kind: StorageObjectKind) -> Result<&ObjectStore, HttpError> {
    state
        .typed_objects
        .get(&kind)
        .ok_or_else(|| HttpError::invalid_request("unsupported storage object kind"))
}

fn parse_object_kind(value: &str) -> Result<StorageObjectKind, HttpError> {
    value
        .parse()
        .map_err(|_| HttpError::invalid_request("unsupported storage object kind"))
}

async fn list_namespaces(
    State(state): State<AppState>,
) -> Result<Json<NamespaceListResponse>, HttpError> {
    Ok(Json(NamespaceListResponse {
        namespaces: state.metadata.list_namespaces().await?,
    }))
}

async fn create_namespace(
    State(state): State<AppState>,
    payload: Result<Json<CreateNamespaceRequest>, JsonRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let Json(payload) = parse_json(payload)?;
    let namespace = state
        .metadata
        .create_namespace(payload.display_name)
        .await?;
    Ok((StatusCode::CREATED, Json(namespace)))
}

async fn rename_namespace(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
    payload: Result<Json<RenameNamespaceRequest>, JsonRejection>,
) -> Result<Json<sync_core::Namespace>, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    let Json(payload) = parse_json(payload)?;
    Ok(Json(
        state
            .metadata
            .rename_namespace(namespace_id, payload.display_name)
            .await?,
    ))
}

async fn namespace_head(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
) -> Result<Json<NamespaceHeadResponse>, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    let (head, namespace_epoch) = state.metadata.get_head_state(namespace_id).await?;
    Ok(Json(NamespaceHeadResponse {
        namespace_id,
        head,
        namespace_epoch,
    }))
}

async fn list_revisions(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
) -> Result<Json<RevisionListResponse>, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    Ok(Json(RevisionListResponse {
        revisions: state.metadata.list_revisions(namespace_id, 200).await?,
        next_cursor: None,
    }))
}

async fn truncate_history(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
    payload: Result<Json<TruncateHistoryRequest>, JsonRejection>,
) -> Result<Json<sync_core::HistoryTrashOperation>, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    let Json(payload) = parse_json(payload)?;
    if let Some(head) = &payload.expected_head {
        validate_sha256(head).map_err(|_| HttpError::invalid_digest("invalid expected Head"))?;
    }
    if let Some(head) = &payload.new_head {
        validate_sha256(head).map_err(|_| HttpError::invalid_digest("invalid new Head"))?;
    }
    Ok(Json(
        state
            .metadata
            .truncate_history(
                namespace_id,
                payload.expected_head,
                payload.expected_namespace_epoch,
                payload.new_head,
            )
            .await?,
    ))
}

async fn list_history_trash(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
) -> Result<Json<HistoryTrashListResponse>, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    Ok(Json(HistoryTrashListResponse {
        operations: state.metadata.list_history_trash(namespace_id).await?,
    }))
}

async fn restore_history_trash(
    State(state): State<AppState>,
    Path((namespace_id, operation_id)): Path<(String, String)>,
    payload: Result<Json<RestoreHistoryRequest>, JsonRejection>,
) -> Result<Json<sync_core::HistoryTrashOperation>, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    let operation_id = Uuid::parse_str(&operation_id)
        .map_err(|_| HttpError::invalid_request("invalid trash operation ID"))?;
    let Json(payload) = parse_json(payload)?;
    if let Some(head) = &payload.expected_head {
        validate_sha256(head).map_err(|_| HttpError::invalid_digest("invalid expected Head"))?;
    }
    Ok(Json(
        state
            .metadata
            .restore_history_trash(
                namespace_id,
                operation_id,
                payload.expected_head,
                payload.expected_namespace_epoch,
            )
            .await?,
    ))
}

async fn commit_revision_root(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
    payload: Result<Json<CommitRevisionRootRequest>, JsonRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let _gc_guard = state.gc_gate.read().await;
    let namespace_id = parse_namespace_id(&namespace_id)?;
    let Json(payload) = parse_json(payload)?;
    payload
        .validate()
        .map_err(|error| HttpError::invalid_request(error.to_string()))?;
    let (root, graph) = validate_revision_graph(&state, &payload.revision_root_sha256).await?;
    if root.namespace_id != namespace_id {
        return Err(HttpError::invalid_request(
            "revision root namespaceId must match the namespace route",
        ));
    }
    if root.parent_revision != payload.expected_head {
        return Err(HttpError::invalid_request(
            "revision root parent must equal expectedHead",
        ));
    }
    let root_length = graph
        .iter()
        .find(|object| object.kind == StorageObjectKind::RevisionRoot)
        .map(|object| object.byte_length)
        .ok_or_else(|| HttpError::invalid_request("validated graph has no revision root"))?;
    let metadata = NewRevisionMetadata::from_revision_root(&root, root_length, &graph)?;
    let (outcome, namespace_epoch) = state
        .metadata
        .commit_revision_root(
            payload.expected_head,
            payload.expected_namespace_epoch,
            metadata,
        )
        .await?;
    let created = outcome.created();
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(CommitRevisionResponse {
            namespace_id,
            head: payload.revision_root_sha256,
            created,
            namespace_epoch,
        }),
    ))
}

async fn validate_revision_graph(
    state: &AppState,
    revision_id: &str,
) -> Result<(RevisionRootV2, Vec<StorageObjectRef>), HttpError> {
    let (root, root_object) =
        read_typed_json::<RevisionRootV2>(state, StorageObjectKind::RevisionRoot, revision_id)
            .await?;
    root.validate()
        .map_err(|error| HttpError::invalid_request(error.to_string()))?;
    if root
        .revision_id()
        .map_err(|error| HttpError::invalid_request(error.to_string()))?
        != revision_id
    {
        return Err(HttpError::invalid_request(
            "revision root hash is not canonical",
        ));
    }
    let mut graph = BTreeMap::from([(
        (root_object.kind, root_object.sha256.clone()),
        root_object.clone(),
    )]);
    let mut root_targets = Vec::new();
    for thread_ref in &root.threads {
        let (descriptor, descriptor_object) = read_typed_json::<ThreadDescriptor>(
            state,
            StorageObjectKind::Thread,
            &thread_ref.descriptor_sha256,
        )
        .await?;
        descriptor
            .validate()
            .map_err(|error| HttpError::invalid_request(error.to_string()))?;
        if descriptor.thread_id != thread_ref.thread_id {
            return Err(HttpError::invalid_request(
                "thread reference ID does not match its descriptor",
            ));
        }
        root_targets.push(descriptor_object.clone());
        graph.insert(
            (descriptor_object.kind, descriptor_object.sha256.clone()),
            descriptor_object.clone(),
        );
        let mut descriptor_targets = Vec::new();
        for content in std::iter::once(&descriptor.rollout).chain(descriptor.attachments.iter()) {
            let targets = validate_content_graph(state, content).await?;
            descriptor_targets.extend(targets.iter().cloned());
            for object in targets {
                graph.insert((object.kind, object.sha256.clone()), object);
            }
        }
        state
            .metadata
            .replace_object_edges(descriptor_object, descriptor_targets)
            .await?;
    }
    state
        .metadata
        .replace_object_edges(root_object, root_targets)
        .await?;
    Ok((root, graph.into_values().collect()))
}

async fn validate_content_graph(
    state: &AppState,
    content: &ContentRef,
) -> Result<Vec<StorageObjectRef>, HttpError> {
    match &content.storage {
        StorageRef::Whole { object_sha256 } => {
            let object = StorageObjectRef {
                kind: StorageObjectKind::Whole,
                sha256: object_sha256.clone(),
                byte_length: content.byte_length,
            };
            ensure_typed_object(state, &object).await?;
            Ok(vec![object])
        }
        StorageRef::Chunked { manifest_sha256 } => {
            let (manifest, manifest_object) = read_typed_json::<ChunkManifest>(
                state,
                StorageObjectKind::ChunkManifest,
                manifest_sha256,
            )
            .await?;
            manifest
                .validate()
                .map_err(|error| HttpError::invalid_request(error.to_string()))?;
            if manifest.logical_sha256 != content.logical_sha256
                || manifest.byte_length != content.byte_length
            {
                return Err(HttpError::invalid_request(
                    "chunk manifest does not match content identity",
                ));
            }
            let chunks = manifest
                .chunks
                .iter()
                .map(|chunk| StorageObjectRef {
                    kind: StorageObjectKind::Chunk,
                    sha256: chunk.sha256.clone(),
                    byte_length: chunk.byte_length,
                })
                .collect::<Vec<_>>();
            for chunk in &chunks {
                ensure_typed_object(state, chunk).await?;
            }
            state
                .metadata
                .replace_object_edges(manifest_object.clone(), chunks.clone())
                .await?;
            let mut result = vec![manifest_object];
            result.extend(chunks);
            Ok(result)
        }
    }
}

async fn ensure_typed_object(state: &AppState, object: &StorageObjectRef) -> Result<(), HttpError> {
    let missing = typed_store(state, object.kind)?
        .missing(&[sync_core::ObjectDescriptor {
            sha256: object.sha256.clone(),
            byte_length: object.byte_length,
        }])
        .await?;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(HttpError::missing_objects(missing))
    }
}

async fn read_typed_json<T: serde::de::DeserializeOwned + serde::Serialize>(
    state: &AppState,
    kind: StorageObjectKind,
    sha256: &str,
) -> Result<(T, StorageObjectRef), HttpError> {
    let mut download = typed_store(state, kind)?.open_download(sha256).await?;
    let mut bytes = Vec::with_capacity(download.byte_length as usize);
    download
        .file
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| HttpError::invalid_request(error.to_string()))?;
    if digest_bytes(&bytes) != sha256 {
        return Err(HttpError::invalid_request(
            "typed structured object hash mismatch",
        ));
    }
    let value: T = serde_json::from_slice(&bytes)
        .map_err(|error| HttpError::invalid_request(error.to_string()))?;
    if canonical_json(&value).map_err(|error| HttpError::invalid_request(error.to_string()))?
        != bytes
    {
        return Err(HttpError::invalid_request(
            "typed structured object is not canonical JSON",
        ));
    }
    Ok((
        value,
        StorageObjectRef {
            kind,
            sha256: sha256.to_string(),
            byte_length: bytes.len() as u64,
        },
    ))
}

fn parse_json<T>(payload: Result<Json<T>, JsonRejection>) -> Result<Json<T>, HttpError> {
    payload.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            HttpError::payload_too_large("JSON request body exceeds the configured limit")
        } else {
            HttpError::invalid_request(error.body_text())
        }
    })
}

fn parse_namespace_id(value: &str) -> Result<Uuid, HttpError> {
    Uuid::parse_str(value).map_err(|_| HttpError::invalid_request("invalid namespace ID"))
}

fn object_id_from_path(digest: &str) -> Result<String, HttpError> {
    let sha256 = format!("sha256:{digest}");
    validate_sha256(&sha256).map_err(|_| HttpError::invalid_digest("invalid SHA-256 digest"))?;
    Ok(sha256)
}

fn parse_content_length(headers: &HeaderMap) -> Result<u64, HttpError> {
    if headers.get_all(CONTENT_LENGTH).iter().count() != 1 {
        return Err(HttpError::invalid_request(
            "object upload requires exactly one Content-Length header",
        ));
    }
    let value = headers
        .get(CONTENT_LENGTH)
        .ok_or_else(|| HttpError::invalid_request("missing Content-Length header"))?
        .to_str()
        .map_err(|_| HttpError::invalid_request("invalid Content-Length header"))?;
    value
        .parse::<u64>()
        .map_err(|_| HttpError::invalid_request("invalid Content-Length header"))
}
