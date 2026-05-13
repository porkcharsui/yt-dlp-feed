use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use clap::Parser;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use yt_dlp_rss::auth::AuthLayer;
use yt_dlp_rss::config::Config;
use yt_dlp_rss::media::{DownloadCoordinator, YtDlpBackend};
use yt_dlp_rss::state::AppState;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long, env = "YT_DLP_RSS_CONFIG", default_value = "config.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "yt_dlp_rss=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    let config = Config::load_or_default(&args.config)
        .await
        .with_context(|| format!("failed to load config from {}", args.config.display()))?;
    config.ensure_directories().await?;

    let backend = YtDlpBackend::new(&config).await?;
    let state = AppState::new(config.clone(), Arc::new(DownloadCoordinator::new(backend)));
    let app = build_router(state);
    let addr: SocketAddr = config.server.bind.parse().context("invalid server.bind")?;
    let listener = TcpListener::bind(addr).await?;

    tracing::info!(%addr, "yt-dlp-rss listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn build_router(state: AppState) -> Router {
    let auth = AuthLayer::new(state.config.auth.clone());
    yt_dlp_rss::routes::router(state)
        .layer(auth)
        .layer(TraceLayer::new_for_http())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
