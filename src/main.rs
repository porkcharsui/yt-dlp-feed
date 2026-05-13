use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use clap::{ArgAction, Parser};
use tokio::net::TcpListener;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing_subscriber::{filter::EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use yt_dlp_feed::auth::AuthLayer;
use yt_dlp_feed::config::Config;
use yt_dlp_feed::media::{DownloadCoordinator, YtDlpBackend};
use yt_dlp_feed::state::AppState;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long, env = "YT_DLP_FEED_CONFIG", default_value = "config.yaml")]
    config: PathBuf,

    #[arg(long, env = "YT_DLP_FEED_DEBUG", action = ArgAction::SetTrue)]
    debug: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::registry()
        .with(log_filter(args.debug))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::load_or_default(&args.config)
        .await
        .with_context(|| format!("failed to load config from {}", args.config.display()))?;
    config.ensure_directories().await?;

    let backend = YtDlpBackend::new(&config).await?;
    let state = AppState::new(config.clone(), Arc::new(DownloadCoordinator::new(backend)));
    let app = build_router(state);
    let addr: SocketAddr = config.server.bind.parse().context("invalid server.bind")?;
    let listener = TcpListener::bind(addr).await?;

    tracing::info!(%addr, "yt-dlp-feed listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn log_filter(debug: bool) -> EnvFilter {
    if debug {
        "yt_dlp_feed=debug,yt_dlp=debug,tower_http=debug".into()
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "yt_dlp_feed=info,tower_http=info".into())
    }
}

fn build_router(state: AppState) -> Router {
    let auth = AuthLayer::new(state.config.auth.clone());
    yt_dlp_feed::routes::router(state).layer(auth).layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::DEBUG))
            .on_request(DefaultOnRequest::new().level(Level::DEBUG))
            .on_response(DefaultOnResponse::new().level(Level::DEBUG)),
    )
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
