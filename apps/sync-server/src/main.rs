use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Clone)]
struct AppState {
    data_dir: Arc<PathBuf>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    data_dir: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sync_server=info,tower_http=info".into()),
        )
        .init();

    let bind = std::env::var("SYNC_SERVER_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let data_dir = std::env::var_os("SYNC_SERVER_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./data"));
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create server data dir {}", data_dir.display()))?;

    let state = AppState {
        data_dir: Arc::new(data_dir),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/info", get(health))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind sync server to {bind}"))?;
    info!(address = %bind, "sync server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "codex-session-sync",
        version: env!("CARGO_PKG_VERSION"),
        data_dir: state.data_dir.to_string_lossy().into_owned(),
    })
}
