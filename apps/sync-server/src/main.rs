use anyhow::Context;
use sync_server::{AppState, ServerConfig, build_router};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sync_server=info,tower_http=info".into()),
        )
        .init();

    let config = ServerConfig::from_env()?;
    let bind = config.bind.clone();
    let state = AppState::initialize(&config).await?;
    let app = build_router(state, &config);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind sync server to {bind}"))?;
    info!(address = %bind, "sync server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
