use anyhow::Result;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use rust_agent::acp;
use rust_agent::config::AppConfig;
use rust_agent::mlx_client::MlxClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = AppConfig::from_env();
    info!(
        mlx_url = %config.mlx_url,
        mlx_model = %config.mlx_model,
        "starting rust ACP agent"
    );

    let model = Arc::new(MlxClient::from_config(&config)?);
    acp::run(model).await
}
