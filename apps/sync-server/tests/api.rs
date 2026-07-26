use std::collections::BTreeMap;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use axum::http::{HeaderValue, Method, Request, Response, StatusCode};
use futures_util::stream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sync_core::{
    CommitRevisionRequest, CommitRevisionResponse, ContentObject, CreateNamespaceRequest,
    Namespace, REVISION_SCHEMA_VERSION, RelatedRecords, RevisionManifest, RevisionPayload,
    THREAD_BUNDLE_SCHEMA_VERSION, ThreadBundle, WorkspaceRef,
};
use sync_server::{AppState, ServerConfig, build_router};
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

const TOKEN: &str = "integration-test-token-that-must-stay-secret";

struct TestApp {
    _directory: TempDir,
    config: ServerConfig,
    router: Router,
}

impl TestApp {
    async fn new(max_object_bytes: u64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            data_dir: directory.path().join("server-data"),
            token: TOKEN.to_string(),
            max_object_bytes,
            max_manifest_bytes: 1024 * 1024,
        };
        let state = AppState::initialize(&config).await.unwrap();
        let router = build_router(state, &config);
        Self {
            _directory: directory,
            config,
            router,
        }
    }

    async fn restart(&mut self) {
        let state = AppState::initialize(&self.config).await.unwrap();
        self.router = build_router(state, &self.config);
    }

    async fn send(&self, request: Request<Body>) -> Response<Body> {
        self.router.clone().oneshot(request).await.unwrap()
    }
}

#[tokio::test]
async fn public_endpoints_work_and_data_routes_require_authentication() {
    let app = TestApp::new(1024).await;

    let health = app
        .send(request(Method::GET, "/health", Body::empty()))
        .await;
    assert_eq!(health.status(), StatusCode::OK);
    let health: Value = response_json(health).await;
    assert_eq!(health["status"], "ok");
    assert!(health.get("dataDir").is_none());

    let info = app
        .send(request(Method::GET, "/api/v1/info", Body::empty()))
        .await;
    assert_eq!(info.status(), StatusCode::OK);
    let info: Value = response_json(info).await;
    assert_eq!(info["protocolVersion"], 1);

    let unauthorized_create = app
        .send(json_request(
            Method::POST,
            "/api/v1/namespaces",
            &CreateNamespaceRequest {
                display_name: "Must not be created".to_string(),
            },
            None,
        ))
        .await;
    assert_unauthorized(unauthorized_create).await;

    let wrong_token = app
        .send(json_request(
            Method::POST,
            "/api/v1/namespaces",
            &CreateNamespaceRequest {
                display_name: "Must not be created".to_string(),
            },
            Some("wrong-token"),
        ))
        .await;
    assert_unauthorized(wrong_token).await;

    let unauthorized_content = b"must not be stored";
    let unauthorized_object = digest(unauthorized_content);
    let unauthorized_put = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!(
                    "/api/v1/objects/{}",
                    raw_digest(&unauthorized_object)
                ))
                .header(CONTENT_LENGTH, unauthorized_content.len())
                .body(Body::from(unauthorized_content.to_vec()))
                .unwrap(),
        )
        .await;
    assert_unauthorized(unauthorized_put).await;
    let unauthorized_missing = app
        .send(json_request(
            Method::POST,
            "/api/v1/objects/missing",
            &json!({
                "objects": [{
                    "sha256": unauthorized_object,
                    "byteLength": unauthorized_content.len()
                }]
            }),
            Some(TOKEN),
        ))
        .await;
    let unauthorized_missing: Value = response_json(unauthorized_missing).await;
    assert_eq!(
        unauthorized_missing["missing"],
        json!([unauthorized_object])
    );

    for uri in [
        "/api/v1/namespaces",
        "/api/v1/objects/0000000000000000000000000000000000000000000000000000000000000000",
        "/api/v1/revisions/0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        let response = app.send(request(Method::GET, uri, Body::empty())).await;
        assert_unauthorized(response).await;
    }

    let list = app
        .send(authenticated(request(
            Method::GET,
            "/api/v1/namespaces",
            Body::empty(),
        )))
        .await;
    assert_eq!(list.status(), StatusCode::OK);
    let list: Value = response_json(list).await;
    assert_eq!(list["namespaces"], json!([]));

    let oversized_json = app
        .send(json_request(
            Method::POST,
            "/api/v1/namespaces",
            &json!({"displayName": "x".repeat(2 * 1024 * 1024)}),
            Some(TOKEN),
        ))
        .await;
    assert_error(
        oversized_json,
        StatusCode::PAYLOAD_TOO_LARGE,
        "object_too_large",
    )
    .await;
}

#[tokio::test]
async fn namespace_create_rename_and_head_are_stable() {
    let app = TestApp::new(1024).await;
    let namespace = create_namespace(&app, "Personal").await;

    let rename = app
        .send(json_request(
            Method::PATCH,
            &format!("/api/v1/namespaces/{}", namespace.id),
            &json!({"displayName": "Desktop"}),
            Some(TOKEN),
        ))
        .await;
    assert_eq!(rename.status(), StatusCode::OK);
    let renamed: Namespace = response_json(rename).await;
    assert_eq!(renamed.id, namespace.id);
    assert_eq!(renamed.display_name, "Desktop");

    let head = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/namespaces/{}/head", namespace.id),
            Body::empty(),
        )))
        .await;
    assert_eq!(head.status(), StatusCode::OK);
    let head: Value = response_json(head).await;
    assert_eq!(head["namespaceId"], namespace.id.to_string());
    assert!(head["head"].is_null());
}

#[tokio::test]
async fn object_missing_upload_download_and_validation_work() {
    let app = TestApp::new(64).await;
    let content = b"streamed object";
    let object_id = digest(content);
    let raw_digest = raw_digest(&object_id);

    let missing = app
        .send(json_request(
            Method::POST,
            "/api/v1/objects/missing",
            &json!({"objects": [{"sha256": object_id, "byteLength": content.len()}]}),
            Some(TOKEN),
        ))
        .await;
    assert_eq!(missing.status(), StatusCode::OK);
    let missing: Value = response_json(missing).await;
    assert_eq!(missing["missing"], json!([object_id]));

    let chunks = stream::iter([
        Ok::<_, std::io::Error>(Bytes::from_static(b"streamed ")),
        Ok(Bytes::from_static(b"object")),
    ]);
    let upload = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/v1/objects/{raw_digest}"))
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_LENGTH, content.len())
                .body(Body::from_stream(chunks))
                .unwrap(),
        )
        .await;
    assert_eq!(upload.status(), StatusCode::CREATED);
    let upload: Value = response_json(upload).await;
    assert_eq!(upload["created"], true);

    let repeated = upload_object(&app, content, &object_id).await;
    assert_eq!(repeated.status(), StatusCode::OK);
    let repeated: Value = response_json(repeated).await;
    assert_eq!(repeated["created"], false);

    let missing = app
        .send(json_request(
            Method::POST,
            "/api/v1/objects/missing",
            &json!({"objects": [{"sha256": object_id, "byteLength": content.len()}]}),
            Some(TOKEN),
        ))
        .await;
    let missing: Value = response_json(missing).await;
    assert_eq!(missing["missing"], json!([]));

    let conflicting_lengths = app
        .send(json_request(
            Method::POST,
            "/api/v1/objects/missing",
            &json!({
                "objects": [
                    {"sha256": object_id, "byteLength": content.len()},
                    {"sha256": object_id, "byteLength": content.len() + 1}
                ]
            }),
            Some(TOKEN),
        ))
        .await;
    assert_error(
        conflicting_lengths,
        StatusCode::BAD_REQUEST,
        "invalid_request",
    )
    .await;

    let download = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/objects/{raw_digest}"),
            Body::empty(),
        )))
        .await;
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download.headers().get(ETAG).unwrap(),
        &format!("\"{object_id}\"")
    );
    assert_eq!(response_bytes(download).await.as_ref(), content);

    let invalid_digest = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/v1/objects/{}", "A".repeat(64)))
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_LENGTH, content.len())
                .body(Body::from(content.to_vec()))
                .unwrap(),
        )
        .await;
    assert_error(invalid_digest, StatusCode::BAD_REQUEST, "invalid_digest").await;

    let wrong_hash = upload_object(&app, content, &format!("sha256:{}", "0".repeat(64))).await;
    assert_error(
        wrong_hash,
        StatusCode::UNPROCESSABLE_ENTITY,
        "hash_mismatch",
    )
    .await;

    let missing_length = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/v1/objects/{raw_digest}"))
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from(content.to_vec()))
                .unwrap(),
        )
        .await;
    assert_error(missing_length, StatusCode::BAD_REQUEST, "invalid_request").await;

    let mut duplicate_length = Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/v1/objects/{raw_digest}"))
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_LENGTH, content.len())
        .body(Body::from(content.to_vec()))
        .unwrap();
    duplicate_length.headers_mut().append(
        CONTENT_LENGTH,
        HeaderValue::from_str(&content.len().to_string()).unwrap(),
    );
    let duplicate_length = app.send(duplicate_length).await;
    assert_error(duplicate_length, StatusCode::BAD_REQUEST, "invalid_request").await;

    let wrong_length = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/v1/objects/{raw_digest}"))
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_LENGTH, content.len() - 1)
                .body(Body::from(content.to_vec()))
                .unwrap(),
        )
        .await;
    assert_error(
        wrong_length,
        StatusCode::UNPROCESSABLE_ENTITY,
        "length_mismatch",
    )
    .await;

    let invalid_length = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/v1/objects/{raw_digest}"))
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_LENGTH, "not-a-number")
                .body(Body::from(content.to_vec()))
                .unwrap(),
        )
        .await;
    assert_error(invalid_length, StatusCode::BAD_REQUEST, "invalid_request").await;

    let short_body = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/v1/objects/{raw_digest}"))
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_LENGTH, content.len() + 1)
                .body(Body::from(content.to_vec()))
                .unwrap(),
        )
        .await;
    assert_error(
        short_body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "length_mismatch",
    )
    .await;

    let empty_id = digest(b"");
    let empty_upload = upload_object(&app, b"", &empty_id).await;
    assert_eq!(empty_upload.status(), StatusCode::CREATED);

    let oversized_content = vec![b'x'; 65];
    let oversized_id = digest(&oversized_content);
    let oversized = upload_object(&app, &oversized_content, &oversized_id).await;
    assert_error(oversized, StatusCode::PAYLOAD_TOO_LARGE, "object_too_large").await;
}

#[tokio::test]
async fn revision_commit_requires_objects_and_fast_forwards() {
    let app = TestApp::new(1024).await;
    let namespace = create_namespace(&app, "Personal").await;
    let first_content = b"first rollout";
    let first = revision(namespace.id, None, "first", first_content);
    let first_request = CommitRevisionRequest {
        expected_head: None,
        revision: first.clone(),
    };

    let missing = commit(&app, namespace.id, &first_request).await;
    assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let missing: Value = response_json(missing).await;
    assert_eq!(missing["code"], "missing_objects");
    assert_eq!(missing["missingObjects"], json!([digest(first_content)]));

    assert_eq!(
        upload_object(&app, first_content, &digest(first_content))
            .await
            .status(),
        StatusCode::CREATED
    );
    let committed = commit(&app, namespace.id, &first_request).await;
    assert_eq!(committed.status(), StatusCode::CREATED);
    let committed: CommitRevisionResponse = response_json(committed).await;
    assert!(committed.created);
    assert_eq!(committed.head, first.revision_id);

    let repeated = commit(&app, namespace.id, &first_request).await;
    assert_eq!(repeated.status(), StatusCode::OK);
    let repeated: CommitRevisionResponse = response_json(repeated).await;
    assert!(!repeated.created);

    let fetched = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/revisions/{}", raw_digest(&first.revision_id)),
            Body::empty(),
        )))
        .await;
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched: RevisionManifest = response_json(fetched).await;
    assert_eq!(fetched, first);

    let second_content = b"second rollout";
    assert_eq!(
        upload_object(&app, second_content, &digest(second_content))
            .await
            .status(),
        StatusCode::CREATED
    );
    let second = revision(
        namespace.id,
        Some(first.revision_id.clone()),
        "second",
        second_content,
    );
    let second_request = CommitRevisionRequest {
        expected_head: Some(first.revision_id.clone()),
        revision: second.clone(),
    };
    assert_eq!(
        commit(&app, namespace.id, &second_request).await.status(),
        StatusCode::CREATED
    );
    assert_eq!(namespace_head(&app, namespace.id).await, second.revision_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_and_concurrent_pushes_cannot_overwrite_head() {
    let app = TestApp::new(1024).await;
    let namespace = create_namespace(&app, "Personal").await;
    let base_content = b"base rollout";
    upload_object(&app, base_content, &digest(base_content)).await;
    let base = revision(namespace.id, None, "base", base_content);
    assert_eq!(
        commit(
            &app,
            namespace.id,
            &CommitRevisionRequest {
                expected_head: None,
                revision: base.clone(),
            },
        )
        .await
        .status(),
        StatusCode::CREATED
    );

    let left_content = b"left rollout";
    let right_content = b"right rollout";
    upload_object(&app, left_content, &digest(left_content)).await;
    upload_object(&app, right_content, &digest(right_content)).await;
    let left = revision(
        namespace.id,
        Some(base.revision_id.clone()),
        "left",
        left_content,
    );
    let right = revision(
        namespace.id,
        Some(base.revision_id.clone()),
        "right",
        right_content,
    );
    let left_request = CommitRevisionRequest {
        expected_head: Some(base.revision_id.clone()),
        revision: left.clone(),
    };
    let right_request = CommitRevisionRequest {
        expected_head: Some(base.revision_id.clone()),
        revision: right.clone(),
    };

    let left_http = json_request(
        Method::POST,
        &format!("/api/v1/namespaces/{}/revisions", namespace.id),
        &left_request,
        Some(TOKEN),
    );
    let right_http = json_request(
        Method::POST,
        &format!("/api/v1/namespaces/{}/revisions", namespace.id),
        &right_request,
        Some(TOKEN),
    );
    let (left_response, right_response) = tokio::join!(
        app.router.clone().oneshot(left_http),
        app.router.clone().oneshot(right_http)
    );
    let left_response = left_response.unwrap();
    let right_response = right_response.unwrap();
    let statuses = [left_response.status(), right_response.status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1
    );

    let (winner, loser, loser_response) = if left_response.status() == StatusCode::CREATED {
        (left, right, right_response)
    } else {
        (right, left, left_response)
    };
    let conflict: Value = response_json(loser_response).await;
    assert_eq!(conflict["code"], "head_mismatch");
    assert_eq!(conflict["currentHead"], winner.revision_id);
    assert_eq!(namespace_head(&app, namespace.id).await, winner.revision_id);

    let orphan = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/revisions/{}", raw_digest(&loser.revision_id)),
            Body::empty(),
        )))
        .await;
    assert_error(orphan, StatusCode::NOT_FOUND, "revision_not_found").await;
}

#[tokio::test]
async fn namespace_head_survives_server_restart() {
    let mut app = TestApp::new(1024).await;
    let namespace = create_namespace(&app, "Personal").await;
    let content = b"persistent rollout";
    upload_object(&app, content, &digest(content)).await;
    let revision = revision(namespace.id, None, "persistent", content);
    assert_eq!(
        commit(
            &app,
            namespace.id,
            &CommitRevisionRequest {
                expected_head: None,
                revision: revision.clone(),
            },
        )
        .await
        .status(),
        StatusCode::CREATED
    );

    app.restart().await;
    assert_eq!(
        namespace_head(&app, namespace.id).await,
        revision.revision_id
    );

    let fetched_revision = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/revisions/{}", raw_digest(&revision.revision_id)),
            Body::empty(),
        )))
        .await;
    assert_eq!(fetched_revision.status(), StatusCode::OK);
    let fetched_revision: RevisionManifest = response_json(fetched_revision).await;
    assert_eq!(fetched_revision, revision);

    let fetched_object = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/objects/{}", raw_digest(&digest(content))),
            Body::empty(),
        )))
        .await;
    assert_eq!(fetched_object.status(), StatusCode::OK);
    assert_eq!(response_bytes(fetched_object).await.as_ref(), content);
}

async fn create_namespace(app: &TestApp, display_name: &str) -> Namespace {
    let response = app
        .send(json_request(
            Method::POST,
            "/api/v1/namespaces",
            &CreateNamespaceRequest {
                display_name: display_name.to_string(),
            },
            Some(TOKEN),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    response_json(response).await
}

async fn upload_object(app: &TestApp, content: &[u8], object_id: &str) -> Response<Body> {
    app.send(
        Request::builder()
            .method(Method::PUT)
            .uri(format!("/api/v1/objects/{}", raw_digest(object_id)))
            .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(CONTENT_LENGTH, content.len())
            .body(Body::from(content.to_vec()))
            .unwrap(),
    )
    .await
}

async fn commit(
    app: &TestApp,
    namespace_id: Uuid,
    commit: &CommitRevisionRequest,
) -> Response<Body> {
    app.send(json_request(
        Method::POST,
        &format!("/api/v1/namespaces/{namespace_id}/revisions"),
        commit,
        Some(TOKEN),
    ))
    .await
}

async fn namespace_head(app: &TestApp, namespace_id: Uuid) -> String {
    let response = app
        .send(authenticated(request(
            Method::GET,
            &format!("/api/v1/namespaces/{namespace_id}/head"),
            Body::empty(),
        )))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response_json(response).await;
    body["head"].as_str().unwrap().to_string()
}

fn revision(
    namespace_id: Uuid,
    parent_revision: Option<String>,
    label: &str,
    content: &[u8],
) -> RevisionManifest {
    RevisionManifest::from_payload(RevisionPayload {
        schema_version: REVISION_SCHEMA_VERSION,
        namespace_id,
        parent_revision,
        created_at: "2026-07-26T10:30:00Z".to_string(),
        threads: vec![ThreadBundle {
            schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
            thread_id: format!("thread-{label}"),
            title: format!("Thread {label}"),
            archived: false,
            created_at_ms: None,
            updated_at_ms: None,
            model_provider: Some("openai".to_string()),
            workspace: WorkspaceRef::default(),
            rollout: ContentObject {
                sha256: digest(content),
                byte_length: content.len() as u64,
                media_type: "application/x-ndjson".to_string(),
                logical_path: Some(format!("sessions/rollout-{label}.jsonl")),
                source_path: None,
            },
            related_records: RelatedRecords {
                source_database: None,
                tables: BTreeMap::new(),
            },
            attachments: Vec::new(),
        }],
        warning_count: 0,
    })
    .unwrap()
}

fn digest(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}

fn raw_digest(object_id: &str) -> &str {
    object_id.strip_prefix("sha256:").unwrap()
}

fn request(method: Method, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap()
}

fn authenticated(mut request: Request<Body>) -> Request<Body> {
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {TOKEN}").parse().unwrap());
    request
}

fn json_request<T: Serialize>(
    method: Method,
    uri: &str,
    payload: &T,
    token: Option<&str>,
) -> Request<Body> {
    let body = serde_json::to_vec(payload).unwrap();
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).unwrap()
}

async fn response_json<T: DeserializeOwned>(response: Response<Body>) -> T {
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

async fn response_bytes(response: Response<Body>) -> Bytes {
    to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap()
}

async fn assert_unauthorized(response: Response<Body>) {
    assert_error(response, StatusCode::UNAUTHORIZED, "unauthorized").await;
}

async fn assert_error(response: Response<Body>, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let body: Value = response_json(response).await;
    assert_eq!(body["code"], code);
}
