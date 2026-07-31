use std::collections::BTreeMap;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{Method, Request, Response, StatusCode};
use rusqlite::params;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sync_core::{
    CommitRevisionResponse, CommitRevisionRootRequest, ContentRef, CreateNamespaceRequest,
    HistoryTrashListResponse, Namespace, ProtocolInfoResponse, RelatedRecords,
    RestoreHistoryRequest, RevisionListResponse, RevisionRootV2, ServerGcPlan,
    ServerGcQuarantineRequest, ServerGcQuarantineResponse, StorageObjectKind, StorageRef,
    THREAD_DESCRIPTOR_SCHEMA_VERSION, ThreadDescriptor, ThreadRef, TruncateHistoryRequest,
    WorkspaceRef, canonical_json, digest_bytes,
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
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            data_dir: directory.path().join("server-data"),
            token: TOKEN.to_string(),
            max_object_bytes: 2 * 1024 * 1024,
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

    async fn create_namespace(&self, name: &str) -> Namespace {
        let response = self
            .send(json_request(
                Method::POST,
                "/api/v2/namespaces",
                &CreateNamespaceRequest {
                    display_name: name.to_string(),
                },
            ))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response_json(response).await
    }

    async fn upload(&self, kind: StorageObjectKind, bytes: &[u8]) -> String {
        let sha256 = digest_bytes(bytes);
        let digest = sha256.strip_prefix("sha256:").unwrap();
        let response = self
            .send(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/v2/objects/{}/{digest}", kind.wire_name()))
                    .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(CONTENT_LENGTH, bytes.len())
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(bytes.to_vec()))
                    .unwrap(),
            )
            .await;
        assert!(matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::OK
        ));
        sha256
    }

    async fn install_revision(
        &self,
        namespace_id: Uuid,
        parent_revision: Option<String>,
        content: &[u8],
        thread_id: &str,
    ) -> RevisionRootV2 {
        let whole_sha256 = self.upload(StorageObjectKind::Whole, content).await;
        let descriptor = ThreadDescriptor {
            schema_version: THREAD_DESCRIPTOR_SCHEMA_VERSION,
            thread_id: thread_id.to_string(),
            title: format!("Thread {thread_id}"),
            archived: false,
            created_at_ms: Some(1_700_000_000_000),
            updated_at_ms: Some(1_700_000_100_000),
            model_provider: Some("openai".to_string()),
            workspace: WorkspaceRef::default(),
            rollout: ContentRef {
                logical_sha256: whole_sha256.clone(),
                byte_length: content.len() as u64,
                storage: StorageRef::Whole {
                    object_sha256: whole_sha256,
                },
                media_type: Some("application/x-ndjson".to_string()),
                logical_path: Some(format!("sessions/rollout-{thread_id}.jsonl")),
            },
            related_records: RelatedRecords {
                source_database: None,
                tables: BTreeMap::new(),
            },
            attachments: Vec::new(),
        };
        let descriptor_bytes = canonical_json(&descriptor).unwrap();
        let descriptor_sha256 = self
            .upload(StorageObjectKind::Thread, &descriptor_bytes)
            .await;
        let root = RevisionRootV2 {
            schema_version: sync_core::REVISION_ROOT_V2_SCHEMA_VERSION,
            namespace_id,
            parent_revision,
            created_at: chrono::Utc::now().to_rfc3339(),
            threads: vec![ThreadRef {
                thread_id: thread_id.to_string(),
                descriptor_sha256,
            }],
            warning_count: 0,
        };
        let root_bytes = canonical_json(&root).unwrap();
        let root_sha256 = self
            .upload(StorageObjectKind::RevisionRoot, &root_bytes)
            .await;
        assert_eq!(root.revision_id().unwrap(), root_sha256);
        root
    }

    async fn commit(
        &self,
        namespace_id: Uuid,
        expected_head: Option<String>,
        expected_epoch: u64,
        root: &RevisionRootV2,
    ) -> Response<Body> {
        self.send(json_request(
            Method::POST,
            &format!("/api/v2/namespaces/{namespace_id}/revisions/commit"),
            &CommitRevisionRootRequest {
                expected_head,
                expected_namespace_epoch: expected_epoch,
                revision_root_sha256: root.revision_id().unwrap(),
            },
        ))
        .await
    }

    fn backdate_all_objects(&self) {
        let connection =
            rusqlite::Connection::open(self.config.data_dir.join("metadata.sqlite")).unwrap();
        connection
            .execute(
                "UPDATE storage_objects SET created_at = '2020-01-01T00:00:00.000Z'",
                [],
            )
            .unwrap();
    }
}

#[tokio::test]
async fn exposes_only_v2_and_requires_auth_for_data_routes() {
    let app = TestApp::new().await;
    assert_eq!(
        app.send(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status(),
        StatusCode::OK
    );
    let info = app
        .send(
            Request::builder()
                .uri("/api/v2/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let info: ProtocolInfoResponse = response_json(info).await;
    assert_eq!(info.protocol_version, 2);
    assert!(info.capabilities.garbage_collection);
    assert_eq!(
        app.send(
            Request::builder()
                .uri("/api/v1/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.send(
            Request::builder()
                .uri("/api/v2/namespaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn compact_revision_commit_is_idempotent_and_rejects_stale_head_and_epoch() {
    let app = TestApp::new().await;
    let namespace = app.create_namespace("Personal").await;
    let root = app
        .install_revision(namespace.id, None, b"first revision\n", "thread-one")
        .await;
    let first = app.commit(namespace.id, None, 0, &root).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let committed: CommitRevisionResponse = response_json(first).await;
    assert_eq!(committed.head, root.revision_id().unwrap());
    let retry = app.commit(namespace.id, None, 0, &root).await;
    assert_eq!(retry.status(), StatusCode::OK);

    let stale = app
        .install_revision(namespace.id, None, b"stale revision\n", "thread-stale")
        .await;
    assert_eq!(
        app.commit(namespace.id, None, 0, &stale).await.status(),
        StatusCode::CONFLICT
    );
    let stale_epoch = app
        .install_revision(
            namespace.id,
            Some(committed.head.clone()),
            b"stale epoch\n",
            "thread-epoch",
        )
        .await;
    assert_eq!(
        app.commit(namespace.id, Some(committed.head.clone()), 1, &stale_epoch,)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let revisions = app
        .send(auth_request(
            Method::GET,
            &format!("/api/v2/namespaces/{}/revisions", namespace.id),
            Body::empty(),
        ))
        .await;
    let revisions: RevisionListResponse = response_json(revisions).await;
    assert_eq!(revisions.revisions.len(), 1);
}

#[tokio::test]
async fn history_truncation_is_recoverable_and_epoch_cas_protected() {
    let app = TestApp::new().await;
    let namespace = app.create_namespace("History").await;
    let first = app
        .install_revision(namespace.id, None, b"first\n", "thread")
        .await;
    let first_id = first.revision_id().unwrap();
    assert_eq!(
        app.commit(namespace.id, None, 0, &first).await.status(),
        StatusCode::CREATED
    );
    let second = app
        .install_revision(namespace.id, Some(first_id.clone()), b"second\n", "thread")
        .await;
    let second_id = second.revision_id().unwrap();
    assert_eq!(
        app.commit(namespace.id, Some(first_id.clone()), 0, &second)
            .await
            .status(),
        StatusCode::CREATED
    );
    let truncated = app
        .send(json_request(
            Method::POST,
            &format!("/api/v2/namespaces/{}/history/truncations", namespace.id),
            &TruncateHistoryRequest {
                expected_head: Some(second_id),
                expected_namespace_epoch: 0,
                new_head: Some(first_id.clone()),
            },
        ))
        .await;
    assert_eq!(truncated.status(), StatusCode::OK);
    let operation: sync_core::HistoryTrashOperation = response_json(truncated).await;
    assert_eq!(operation.epoch_after, 1);
    let trash = app
        .send(auth_request(
            Method::GET,
            &format!("/api/v2/namespaces/{}/trash", namespace.id),
            Body::empty(),
        ))
        .await;
    let trash: HistoryTrashListResponse = response_json(trash).await;
    assert_eq!(trash.operations.len(), 1);
    let restored = app
        .send(json_request(
            Method::POST,
            &format!(
                "/api/v2/namespaces/{}/trash/{}/restore",
                namespace.id, operation.operation_id
            ),
            &RestoreHistoryRequest {
                expected_head: Some(first_id),
                expected_namespace_epoch: 1,
            },
        ))
        .await;
    assert_eq!(restored.status(), StatusCode::OK);
}

#[tokio::test]
async fn gc_quarantines_only_globally_unreachable_objects_and_resumes_after_restart() {
    let mut app = TestApp::new().await;
    let namespace = app.create_namespace("Protected").await;
    let root = app
        .install_revision(namespace.id, None, b"shared content\n", "shared")
        .await;
    assert_eq!(
        app.commit(namespace.id, None, 0, &root).await.status(),
        StatusCode::CREATED
    );
    let orphan = app
        .upload(StorageObjectKind::Whole, b"orphan content")
        .await;
    app.backdate_all_objects();

    let plan = app
        .send(auth_request(Method::GET, "/api/v2/gc/plan", Body::empty()))
        .await;
    let plan: ServerGcPlan = response_json(plan).await;
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].sha256, orphan);
    let quarantined = app
        .send(json_request(
            Method::POST,
            "/api/v2/gc/quarantine",
            &ServerGcQuarantineRequest {
                plan_fingerprint: plan.plan_fingerprint,
            },
        ))
        .await;
    let quarantined: ServerGcQuarantineResponse = response_json(quarantined).await;
    assert_eq!(quarantined.quarantined_object_count, 1);
    let orphan_digest = orphan.strip_prefix("sha256:").unwrap();
    assert_eq!(
        app.send(auth_request(
            Method::GET,
            &format!("/api/v2/objects/whole/{orphan_digest}"),
            Body::empty(),
        ))
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let descriptor = &root.threads[0].descriptor_sha256;
    assert_eq!(
        app.send(auth_request(
            Method::GET,
            &format!(
                "/api/v2/objects/thread/{}",
                descriptor.strip_prefix("sha256:").unwrap()
            ),
            Body::empty(),
        ))
        .await
        .status(),
        StatusCode::OK
    );

    let restart_orphan = app
        .upload(StorageObjectKind::Whole, b"restart orphan")
        .await;
    let queue_id = Uuid::now_v7();
    let operation_id = Uuid::now_v7();
    let connection =
        rusqlite::Connection::open(app.config.data_dir.join("metadata.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO gc_queue
         (id, operation_id, object_kind, object_sha256, expected_length, state, created_at)
         VALUES (?1, ?2, 'whole', ?3, ?4, 'pending', '2020-01-01T00:00:00.000Z')",
            params![
                queue_id.to_string(),
                operation_id.to_string(),
                restart_orphan,
                14_i64
            ],
        )
        .unwrap();
    drop(connection);
    app.restart().await;
    let connection =
        rusqlite::Connection::open(app.config.data_dir.join("metadata.sqlite")).unwrap();
    let state: String = connection
        .query_row(
            "SELECT state FROM gc_queue WHERE id = ?1",
            [queue_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "quarantined");
}

fn auth_request(method: Method, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(body)
        .unwrap()
}

fn json_request<T: Serialize>(method: Method, uri: &str, value: &T) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(value).unwrap()))
        .unwrap()
}

async fn response_json<T: DeserializeOwned>(response: Response<Body>) -> T {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "failed to decode response {status}: {error}; body={}",
            String::from_utf8_lossy(&bytes)
        )
    })
}
