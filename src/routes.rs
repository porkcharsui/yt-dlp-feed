use std::collections::HashMap;
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use httpdate::{fmt_http_date, parse_http_date};

use crate::auth::AuthenticatedUser;
use crate::config::{ServiceKind, SoundCloudFeedKind};
use crate::media::{cache_key, is_fresh, media_path, stream_download, FeedItem};
use crate::metadata::{identity_for, MetadataCacheState, RefreshOutcome, RefreshPriority};
use crate::rss_feed;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let cleanup_config = state.config.clone();
    tokio::spawn(crate::media::cleanup_expired_media(cleanup_config));

    Router::new()
        .route("/", get(index))
        .route("/index.json", get(index_json))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/users/:user/soundcloud/:name/feed.xml", get(profile_feed))
        .route("/users/:user/soundcloud/:name/likes.xml", get(likes_feed))
        .route(
            "/users/:user/soundcloud/:name/items/:item_id/audio.m4a",
            get(download_audio),
        )
        .with_state(state)
}

async fn index(
    State(state): State<AppState>,
    auth_user: Option<Extension<AuthenticatedUser>>,
) -> Response {
    let only_user = authenticated_config_user(&state, auth_user_name(auth_user.as_ref()));
    let index = state.metadata.index_json_for_user(only_user).await;
    Html(crate::html::render_index_for_user(
        &state.config,
        &index,
        only_user,
    ))
    .into_response()
}

async fn index_json(
    State(state): State<AppState>,
    auth_user: Option<Extension<AuthenticatedUser>>,
) -> Response {
    let only_user = authenticated_config_user(&state, auth_user_name(auth_user.as_ref()));
    match serde_json::to_string_pretty(&state.metadata.index_json_for_user(only_user).await) {
        Ok(json) => (
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            json,
        )
            .into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<AppState>) -> Response {
    let missing = state.metadata.ready_missing_count().await;
    if missing == 0 {
        "ready".into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("metadata cache warming: {missing} feed(s) missing"),
        )
            .into_response()
    }
}

fn auth_user_name(auth_user: Option<&Extension<AuthenticatedUser>>) -> Option<&String> {
    auth_user.map(|user| &user.0 .0)
}

fn authenticated_config_user<'a>(
    state: &AppState,
    auth_user: Option<&'a String>,
) -> Option<&'a str> {
    if state.config.auth.enabled {
        auth_user.map(String::as_str)
    } else {
        None
    }
}

fn authorize_config_user(
    state: &AppState,
    auth_user: Option<&String>,
    requested_user: &str,
) -> Result<(), Response> {
    if !state.config.auth.enabled {
        return Ok(());
    }

    match auth_user {
        Some(auth_user) if auth_user == requested_user => Ok(()),
        Some(_) => Err(StatusCode::FORBIDDEN.into_response()),
        None => Err(StatusCode::UNAUTHORIZED.into_response()),
    }
}

async fn profile_feed(
    State(state): State<AppState>,
    auth_user: Option<Extension<AuthenticatedUser>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path((user, name)): Path<(String, String)>,
) -> Response {
    if let Err(response) = authorize_config_user(&state, auth_user_name(auth_user.as_ref()), &user)
    {
        return response;
    }

    render_soundcloud_feed(
        state,
        headers,
        query,
        user,
        name,
        SoundCloudFeedKind::Profile,
    )
    .await
}

async fn likes_feed(
    State(state): State<AppState>,
    auth_user: Option<Extension<AuthenticatedUser>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path((user, name)): Path<(String, String)>,
) -> Response {
    if let Err(response) = authorize_config_user(&state, auth_user_name(auth_user.as_ref()), &user)
    {
        return response;
    }

    render_soundcloud_feed(state, headers, query, user, name, SoundCloudFeedKind::Likes).await
}

async fn render_soundcloud_feed(
    state: AppState,
    headers: HeaderMap,
    query: HashMap<String, String>,
    user: String,
    name: String,
    feed: SoundCloudFeedKind,
) -> Response {
    let Some(service) = state
        .config
        .service(&user, ServiceKind::Soundcloud, &name)
        .cloned()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if !service.feeds.contains(&feed) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let identity = identity_for(&user, &service, feed);
    let refresh_requested = query
        .get("refresh")
        .is_some_and(|value| value == "1" || value == "true");
    let mut refresh_header = None;

    if refresh_requested {
        let handle = state
            .metadata
            .enqueue_refresh(identity.clone(), &service, RefreshPriority::Manual)
            .await;
        let outcome = handle.wait().await;
        refresh_header = Some(match outcome {
            RefreshOutcome::Refreshed => MetadataCacheState::Ready,
            RefreshOutcome::AlreadyRunning(state) => state,
            RefreshOutcome::Failed(state) => state,
        });
    }

    let Some(cached) = state.metadata.get(&identity).await else {
        if !refresh_requested {
            let _ = state
                .metadata
                .enqueue_refresh(identity.clone(), &service, RefreshPriority::Scheduled)
                .await;
        }
        let state = refresh_header.unwrap_or_else(|| {
            if refresh_requested {
                MetadataCacheState::Error
            } else {
                MetadataCacheState::Warming
            }
        });
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            "feed metadata cache is warming; no cached metadata is available yet",
        )
            .into_response();
        if refresh_requested {
            response.headers_mut().insert(
                "x-yt-dlp-feed-refresh",
                state
                    .as_header_value()
                    .parse()
                    .expect("valid refresh header"),
            );
        }
        return response;
    };

    if !refresh_requested && not_modified(&headers, cached.last_successful_refresh) {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let base_url = base_url_from_headers(&headers);

    match rss_feed::render_feed(
        &base_url,
        &user,
        &service,
        feed,
        &cached.items,
        cached.last_successful_refresh,
    ) {
        Ok(xml) => {
            let mut response = (
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response();
            response.headers_mut().insert(
                header::LAST_MODIFIED,
                http_date(cached.last_successful_refresh)
                    .parse()
                    .expect("valid Last-Modified"),
            );
            if let Some(state) = refresh_header {
                response.headers_mut().insert(
                    "x-yt-dlp-feed-refresh",
                    state
                        .as_header_value()
                        .parse()
                        .expect("valid refresh header"),
                );
            }
            response
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn download_audio(
    method: Method,
    State(state): State<AppState>,
    auth_user: Option<Extension<AuthenticatedUser>>,
    headers: HeaderMap,
    Path((user, name, item_id)): Path<(String, String, String)>,
) -> Response {
    if let Err(response) = authorize_config_user(&state, auth_user_name(auth_user.as_ref()), &user)
    {
        return response;
    }

    let Some(service) = state
        .config
        .service(&user, ServiceKind::Soundcloud, &name)
        .cloned()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(item) = find_item(&state, &service, &user, &item_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let key = cache_key(&user, "soundcloud", &name, &item.id);
    let path = media_path(&state.config, &key);
    let ttl = state.config.cache.media_ttl();

    if is_fresh(&path, ttl).await {
        return serve_cached_media(&method, &headers, &path).await;
    }

    if method == Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }

    let active = match state
        .downloads
        .try_ensure_download(key, item.webpage_url, path.clone())
        .await
    {
        Ok(active) => active,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "30")],
                "maximum concurrent media downloads reached",
            )
                .into_response()
        }
    };
    let stream = stream_download(active);
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
    user: &str,
    item_id: &str,
) -> Option<FeedItem> {
    for feed in &service.feeds {
        let identity = identity_for(user, service, *feed);
        if let Some(cached) = state.metadata.get(&identity).await {
            if let Some(item) = cached.items.into_iter().find(|item| item.id == item_id) {
                return Some(item);
            }
        }
    }
    None
}

async fn serve_cached_media(
    method: &Method,
    headers: &HeaderMap,
    path: &std::path::Path,
) -> Response {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };
    let len = metadata.len();
    let modified = metadata.modified().ok();

    if method == Method::GET && cached_media_not_modified(headers, modified) {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_byte_range(value, len));

    if headers.get(header::RANGE).is_some() && range.is_none() {
        return range_not_satisfiable(len);
    }

    let (status, start, end) = if let Some((start, end)) = range {
        (StatusCode::PARTIAL_CONTENT, start, end)
    } else {
        (StatusCode::OK, 0, len.saturating_sub(1))
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/mp4")
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(modified) = modified {
        builder = builder.header(header::LAST_MODIFIED, fmt_http_date(modified));
    }

    if len == 0 {
        builder = builder.header(header::CONTENT_LENGTH, "0");
        return builder
            .body(Body::empty())
            .expect("valid empty media response");
    }

    let content_len = end - start + 1;
    builder = builder.header(header::CONTENT_LENGTH, content_len.to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }

    if method == Method::HEAD {
        return builder.body(Body::empty()).expect("valid HEAD response");
    }

    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let slice = bytes
                .get(start as usize..=end as usize)
                .map(Bytes::copy_from_slice)
                .unwrap_or_default();
            builder
                .body(Body::from(slice))
                .expect("valid media response")
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

fn range_not_satisfiable(len: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{len}"))
        .body(Body::empty())
        .expect("valid range response")
}

fn parse_byte_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix);
        return Some((start, len - 1));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        len - 1
    } else {
        end.parse::<u64>().ok()?
    };
    if start >= len || end < start {
        return None;
    }
    Some((start, end.min(len - 1)))
}

fn not_modified(headers: &HeaderMap, last_successful_refresh: DateTime<Utc>) -> bool {
    headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_http_date(value).ok())
        .is_some_and(|since| DateTime::<Utc>::from(since) >= last_successful_refresh)
}

fn cached_media_not_modified(headers: &HeaderMap, modified: Option<SystemTime>) -> bool {
    let Some(modified) = modified else {
        return false;
    };
    headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_http_date(value).ok())
        .is_some_and(|since| since >= modified)
}

fn http_date(timestamp: DateTime<Utc>) -> String {
    fmt_http_date(timestamp.into())
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
    use crate::html::feed_path;
    use crate::media::{DownloadChunk, DownloadCoordinator, MediaBackend};
    use crate::metadata::{identity_for, MetadataCache};

    struct MockBackend;

    #[async_trait]
    impl MediaBackend for MockBackend {
        async fn fetch_feed(
            &self,
            _source_url: &str,
            _feed: SoundCloudFeedKind,
        ) -> anyhow::Result<Vec<FeedItem>> {
            Ok(vec![
                FeedItem {
                    id: "track-2".to_string(),
                    title: "Track Two".to_string(),
                    webpage_url: "https://soundcloud.com/dereknet/track-two".to_string(),
                    description: Some("Second in source order".to_string()),
                    published_at: None,
                    content_length: Some(12),
                    thumbnail_url: Some("https://example.test/art-2.jpg".to_string()),
                },
                FeedItem {
                    id: "track-1".to_string(),
                    title: "Track One".to_string(),
                    webpage_url: "https://soundcloud.com/dereknet/track-one".to_string(),
                    description: Some("First in title, second in source order".to_string()),
                    published_at: None,
                    content_length: Some(12),
                    thumbnail_url: Some("https://example.test/art-1.jpg".to_string()),
                },
            ])
        }

        async fn download_audio(
            &self,
            _source_url: &str,
            output_path: &FsPath,
            chunks: tokio::sync::broadcast::Sender<DownloadChunk>,
            _shutdown: tokio::sync::watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let temp_path = output_path.with_file_name("test.download.m4a");
            tokio::fs::write(&temp_path, b"audio").await?;
            let _ = chunks.send(DownloadChunk {
                offset: 0,
                bytes: bytes::Bytes::from_static(b"audio"),
            });
            tokio::fs::rename(temp_path, output_path).await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn index_lists_configured_feed_links() {
        let app = test_router().await;
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();

        assert!(html.contains(&configured_feed_path(
            "dereknet",
            SoundCloudFeedKind::Profile
        )));
        assert!(html.contains(&configured_feed_path("dereknet", SoundCloudFeedKind::Likes)));
        assert!(html.contains(&configured_feed_path("NTS", SoundCloudFeedKind::Profile)));
        assert!(html.contains(&configured_feed_path("NTS", SoundCloudFeedKind::Likes)));
    }

    #[tokio::test]
    async fn healthz_responds_ok() {
        let app = test_router().await;
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
        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(configured_feed_path(
                        "dereknet",
                        SoundCloudFeedKind::Profile,
                    ))
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
        assert!(xml.contains(&configured_media_url("dereknet", "track-1")));
        assert!(xml.contains("audio/mp4"));
        assert!(xml.contains("media:thumbnail"));
        assert!(xml.contains("itunes:image"));
        assert!(xml.find("Track Two").unwrap() < xml.find("Track One").unwrap());
    }

    #[tokio::test]
    async fn authenticated_user_must_match_route_user() {
        let mut config = Config::default();
        config.auth.enabled = true;
        config.auth.username = Some("derek".to_string());
        config.auth.password = Some("secret".to_string());
        let app = test_router_with_config(config).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/users/someone-else/soundcloud/dereknet/feed.xml")
                    .extension(AuthenticatedUser("derek".to_string()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn index_json_contains_metadata_status_summary() {
        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/index.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["summary"]["feeds_missing"], 0);
        assert_eq!(json["feeds"][0]["metadata_cache"]["state"], "ready");
        assert!(json["feeds"][0].get("name").is_some());
        assert_eq!(json["feeds"][0]["item_count"], 2);
        assert!(json["feeds"][0].get("account").is_none());
        assert!(json["feeds"][0].get("rss_path").is_none());
        assert!(json["feeds"][0].get("rss_url").is_none());
    }

    #[tokio::test]
    async fn readyz_reports_ready_when_all_feeds_have_metadata() {
        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cached_media_supports_byte_ranges() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        tokio::fs::write(&path, b"abcdef").await.unwrap();
        let headers = HeaderMap::from_iter([(
            header::RANGE,
            "bytes=2-4".parse().expect("valid range header"),
        )]);

        let response = serve_cached_media(&Method::GET, &headers, &path).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 2-4/6"
        );
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"cde");
    }

    #[tokio::test]
    async fn cached_media_head_includes_lengths_without_body() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        tokio::fs::write(&path, b"abcdef").await.unwrap();

        let response = serve_cached_media(&Method::HEAD, &HeaderMap::new(), &path).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(header::CONTENT_LENGTH).unwrap(), "6");
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    async fn test_router() -> Router {
        test_router_with_config(Config::default()).await
    }

    async fn test_router_with_config(mut config: Config) -> Router {
        let dir = tempdir().unwrap();
        config.cache.data_dir = dir.keep();
        let coordinator = Arc::new(DownloadCoordinator::from_arc(Arc::new(MockBackend)));
        let metadata = Arc::new(MetadataCache::new(config.clone()).await.unwrap());
        for user in &config.users {
            for service in &user.services {
                for feed in &service.feeds {
                    let items = coordinator
                        .fetch_feed(&feed.source_url(service), *feed)
                        .await
                        .unwrap();
                    metadata
                        .store_success_for_test(
                            identity_for(&user.name, service, *feed),
                            service,
                            items,
                        )
                        .await
                        .unwrap();
                }
            }
        }
        router(AppState::new(config, coordinator, metadata))
    }

    fn configured_feed_path(name: &str, feed: SoundCloudFeedKind) -> String {
        let config = Config::default();
        let user = &config.users[0];
        let service = config
            .service(&user.name, ServiceKind::Soundcloud, name)
            .expect("configured test service");
        feed_path(&user.name, service.kind.as_path(), &service.name, feed)
    }

    fn configured_media_url(name: &str, item_id: &str) -> String {
        let config = Config::default();
        let user = &config.users[0];
        let service = config
            .service(&user.name, ServiceKind::Soundcloud, name)
            .expect("configured test service");
        format!(
            "http://example.test/users/{}/{}/{}/items/{}/audio.m4a",
            urlencoding::encode(&user.name),
            urlencoding::encode(service.kind.as_path()),
            urlencoding::encode(&service.name),
            urlencoding::encode(item_id)
        )
    }
}
