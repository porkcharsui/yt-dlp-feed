use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::config::{FeedKind, ServiceKind};
use crate::media::{cache_key, is_fresh, media_path, stream_growing_file, FeedItem};
use crate::rss_feed;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let cleanup_config = state.config.clone();
    tokio::spawn(crate::media::cleanup_expired_media(cleanup_config));

    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route(
            "/users/:user/soundcloud/:account/feed.xml",
            get(profile_feed),
        )
        .route(
            "/users/:user/soundcloud/:account/likes.xml",
            get(likes_feed),
        )
        .route(
            "/users/:user/soundcloud/:account/items/:item_id/audio.m4a",
            get(download_audio),
        )
        .with_state(state)
}

async fn index(State(state): State<AppState>) -> Html<String> {
    Html(crate::html::render_index(&state.config))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn profile_feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((user, account)): Path<(String, String)>,
) -> Response {
    render_soundcloud_feed(state, headers, user, account, FeedKind::Profile).await
}

async fn likes_feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((user, account)): Path<(String, String)>,
) -> Response {
    render_soundcloud_feed(state, headers, user, account, FeedKind::Likes).await
}

async fn render_soundcloud_feed(
    state: AppState,
    headers: HeaderMap,
    user: String,
    account: String,
    feed: FeedKind,
) -> Response {
    let Some(service) = state
        .config
        .service(&user, ServiceKind::Soundcloud, &account)
        .cloned()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if !service.feeds.contains(&feed) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let source_url = feed.source_url(&service);
    let items = match state.downloads.fetch_feed(&source_url, feed).await {
        Ok(items) => items,
        Err(err) => return (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    };
    let base_url = base_url_from_headers(&headers);

    match rss_feed::render_feed(&base_url, &user, &service, feed, &items) {
        Ok(xml) => (
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn download_audio(
    State(state): State<AppState>,
    Path((user, account, item_id)): Path<(String, String, String)>,
) -> Response {
    let Some(service) = state
        .config
        .service(&user, ServiceKind::Soundcloud, &account)
        .cloned()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(item) = find_item(&state, &service, &item_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let key = cache_key(&user, "soundcloud", &account, &item.id);
    let path = media_path(&state.config, &key);
    let ttl = Duration::from_secs(state.config.cache.media_ttl_seconds);

    if is_fresh(&path, ttl).await {
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                return ([(header::CONTENT_TYPE, "audio/mp4")], bytes).into_response();
            }
            Err(err) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
            }
        }
    }

    let completion = state
        .downloads
        .ensure_download(key, item.webpage_url, path.clone())
        .await;
    let stream = stream_growing_file(path, completion);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mp4")
        .body(body)
        .expect("valid audio stream response")
}

async fn find_item(
    state: &AppState,
    service: &crate::config::ServiceConfig,
    item_id: &str,
) -> Option<FeedItem> {
    for feed in &service.feeds {
        let source_url = feed.source_url(service);
        if let Ok(items) = state.downloads.fetch_feed(&source_url, *feed).await {
            if let Some(item) = items.into_iter().find(|item| item.id == item_id) {
                return Some(item);
            }
        }
    }
    None
}

fn base_url_from_headers(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("{proto}://{host}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body;
    use http::Request;
    use std::path::Path as FsPath;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::media::{DownloadCoordinator, MediaBackend};

    struct MockBackend;

    #[async_trait]
    impl MediaBackend for MockBackend {
        async fn fetch_feed(
            &self,
            _source_url: &str,
            _feed: FeedKind,
        ) -> anyhow::Result<Vec<FeedItem>> {
            Ok(vec![FeedItem {
                id: "track-1".to_string(),
                title: "Track One".to_string(),
                webpage_url: "https://soundcloud.com/dereknet/track-one".to_string(),
                description: Some("A track".to_string()),
                published_at: None,
                content_length: Some(12),
            }])
        }

        async fn download_audio(
            &self,
            _source_url: &str,
            output_path: &FsPath,
        ) -> anyhow::Result<()> {
            tokio::fs::write(output_path, b"audio").await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn index_lists_configured_feed_links() {
        let app = test_router();
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();

        assert!(html.contains("/users/derek/soundcloud/dereknet/feed.xml"));
        assert!(html.contains("/users/derek/soundcloud/dereknet/likes.xml"));
    }

    #[tokio::test]
    async fn healthz_responds_ok() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rss_contains_stable_audio_enclosure() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/users/derek/soundcloud/dereknet/feed.xml")
                    .header(header::HOST, "example.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let xml = String::from_utf8(body.to_vec()).unwrap();

        assert!(xml.contains("Track One"));
        assert!(xml.contains(
            "http://example.test/users/derek/soundcloud/dereknet/items/track-1/audio.m4a"
        ));
        assert!(xml.contains("audio/mp4"));
    }

    fn test_router() -> Router {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.cache.data_dir = dir.path().to_path_buf();
        let coordinator = DownloadCoordinator::from_arc(Arc::new(MockBackend));
        router(AppState::new(config, Arc::new(coordinator)))
    }
}
