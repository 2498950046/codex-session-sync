use std::collections::BTreeMap;

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
    CommitRevisionRequest, CommitRevisionResponse, CreateNamespaceRequest, MissingObjectsRequest,
    MissingObjectsResponse, NamespaceHeadResponse, NamespaceListResponse, ProtocolInfoResponse,
    PutObjectResponse, REMOTE_PROTOCOL_VERSION, RenameNamespaceRequest, RevisionManifest,
    validate_sha256,
};
use tokio_util::io::ReaderStream;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::auth::{AuthState, require_auth};
use crate::config::ServerConfig;
use crate::error::HttpError;
use crate::metadata::{CommitRevisionOutcome, MetadataError, MetadataStore, NewRevisionMetadata};
use crate::object_store::{ObjectStore, PutObjectResult};
use crate::revision_store::RevisionStore;

const MAX_MISSING_OBJECTS: usize = 10_000;
const REQUEST_ENVELOPE_OVERHEAD_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    metadata: MetadataStore,
    objects: ObjectStore,
    revisions: RevisionStore,
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
        let objects = ObjectStore::open(&config.data_dir, config.max_object_bytes)
            .await
            .context("failed to initialize server object storage")?;
        let revisions = RevisionStore::open(&config.data_dir, config.max_manifest_bytes)
            .await
            .context("failed to initialize server revision storage")?;
        Ok(Self {
            metadata,
            objects,
            revisions,
        })
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
    let protected = Router::new()
        .route("/namespaces", get(list_namespaces).post(create_namespace))
        .route("/namespaces/{namespace_id}", patch(rename_namespace))
        .route("/namespaces/{namespace_id}/head", get(namespace_head))
        .route(
            "/namespaces/{namespace_id}/revisions",
            post(commit_revision),
        )
        .route("/objects/missing", post(missing_objects))
        .route("/objects/{digest}", put(put_object).get(get_object))
        .route("/revisions/{digest}", get(get_revision))
        .layer(DefaultBodyLimit::max(json_limit))
        .layer(middleware::from_fn_with_state(
            AuthState::new(config.token.clone()),
            require_auth,
        ))
        .with_state(state);

    Router::new()
        .route("/health", get(health))
        .route("/api/v1/info", get(info))
        .nest("/api/v1", protected)
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
    })
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
    Ok(Json(NamespaceHeadResponse {
        namespace_id,
        head: state.metadata.get_head(namespace_id).await?,
    }))
}

async fn missing_objects(
    State(state): State<AppState>,
    payload: Result<Json<MissingObjectsRequest>, JsonRejection>,
) -> Result<Json<MissingObjectsResponse>, HttpError> {
    let Json(payload) = parse_json(payload)?;
    if payload.objects.len() > MAX_MISSING_OBJECTS {
        return Err(HttpError::invalid_request(format!(
            "objects must contain no more than {MAX_MISSING_OBJECTS} entries"
        )));
    }
    let mut unique = BTreeMap::new();
    for object in payload.objects {
        match unique.insert(object.sha256.clone(), object.byte_length) {
            Some(existing) if existing != object.byte_length => {
                return Err(HttpError::invalid_request(
                    "the same object hash cannot have different byte lengths",
                ));
            }
            _ => {}
        }
    }
    let objects = unique
        .into_iter()
        .map(|(sha256, byte_length)| sync_core::ObjectDescriptor {
            sha256,
            byte_length,
        })
        .collect::<Vec<_>>();
    Ok(Json(MissingObjectsResponse {
        missing: state.objects.missing(&objects).await?,
    }))
}

async fn put_object(
    State(state): State<AppState>,
    Path(digest): Path<String>,
    request: Request,
) -> Result<impl IntoResponse, HttpError> {
    let sha256 = object_id_from_path(&digest)?;
    let expected_length = parse_content_length(request.headers())?;
    let stream = request.into_body().into_data_stream();
    let result = state
        .objects
        .put_stream(&sha256, expected_length, stream)
        .await?;
    let created = matches!(result, PutObjectResult::Created);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(PutObjectResponse {
            sha256,
            byte_length: expected_length,
            created,
        }),
    ))
}

async fn get_object(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Response, HttpError> {
    let sha256 = object_id_from_path(&digest)?;
    let download = state.objects.open_download(&sha256).await?;
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

async fn get_revision(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Json<RevisionManifest>, HttpError> {
    let revision_id = object_id_from_path(&digest)?;
    state
        .metadata
        .get_revision_metadata(revision_id.clone())
        .await?;
    Ok(Json(state.revisions.get(&revision_id).await?))
}

async fn commit_revision(
    State(state): State<AppState>,
    Path(namespace_id): Path<String>,
    payload: Result<Json<CommitRevisionRequest>, JsonRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let namespace_id = parse_namespace_id(&namespace_id)?;
    let Json(payload) = parse_json(payload)?;
    payload
        .validate()
        .map_err(|error| HttpError::invalid_request(error.to_string()))?;
    if payload.revision.payload.namespace_id != namespace_id {
        return Err(HttpError::invalid_request(
            "revision namespaceId must match the namespace route",
        ));
    }

    let current_head = state.metadata.get_head(namespace_id).await?;
    if current_head != payload.expected_head
        && current_head.as_deref() != Some(payload.revision.revision_id.as_str())
    {
        return Err(MetadataError::HeadMismatch {
            current: current_head,
        }
        .into());
    }

    let metadata = NewRevisionMetadata::from_manifest(&payload.revision)?;
    let missing = state.objects.missing(metadata.objects()).await?;
    if !missing.is_empty() {
        return Err(HttpError::missing_objects(missing));
    }
    state.revisions.put(&payload.revision).await?;
    let outcome = state
        .metadata
        .commit_revision(payload.expected_head, metadata)
        .await?;
    let created = matches!(outcome, CommitRevisionOutcome::Created);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(CommitRevisionResponse {
            namespace_id,
            head: payload.revision.revision_id,
            created,
        }),
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
