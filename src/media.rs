use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use http::StatusCode;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tokio::sync::{watch, Mutex};

use crate::config::{Config, FeedKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub id: String,
    pub title: String,
    pub webpage_url: String,
    pub description: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub content_length: Option<u64>,
}

#[async_trait]
pub trait MediaBackend: Send + Sync + 'static {
    async fn fetch_feed(&self, source_url: &str, feed: FeedKind) -> anyhow::Result<Vec<FeedItem>>;
    async fn download_audio(&self, source_url: &str, output_path: &Path) -> anyhow::Result<()>;
}

pub struct YtDlpBackend {
    downloader: yt_dlp::Downloader,
    ytdlp_path: PathBuf,
    ffmpeg_path: PathBuf,
}

impl YtDlpBackend {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let libs_dir = config.libs_dir();
        let output_dir = config.media_dir();
        fs::create_dir_all(&libs_dir).await?;
        fs::create_dir_all(&output_dir).await?;

        let ytdlp_path = resolve_tool_path(libs_dir.join(executable_name("yt-dlp")), "yt-dlp")?;
        let ffmpeg_path = resolve_tool_path(libs_dir.join(executable_name("ffmpeg")), "ffmpeg")?;
        tracing::info!(
            yt_dlp = %ytdlp_path.display(),
            ffmpeg = %ffmpeg_path.display(),
            "using media tools"
        );

        let libraries =
            yt_dlp::client::deps::Libraries::new(ytdlp_path.clone(), ffmpeg_path.clone());
        let cache_config = yt_dlp::cache::config::CacheConfig::builder()
            .cache_dir(config.metadata_dir())
            .persistent_backend(Some(yt_dlp::cache::PersistentBackendKind::Redb))
            .build();
        let downloader = yt_dlp::Downloader::builder(libraries, output_dir)
            .with_cache_config(cache_config)
            .build()
            .await?;
        Ok(Self {
            downloader,
            ytdlp_path,
            ffmpeg_path,
        })
    }
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn resolve_tool_path(preferred_path: PathBuf, command_name: &str) -> anyhow::Result<PathBuf> {
    if preferred_path.is_file() {
        return Ok(preferred_path);
    }

    find_in_path(command_name).with_context(|| {
        format!(
            "could not find {command_name}; expected {} or a {command_name} executable on PATH",
            preferred_path.display()
        )
    })
}

fn find_in_path(command_name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    find_in_paths(command_name, env::split_paths(&paths))
}

fn find_in_paths(command_name: &str, paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .map(|dir| dir.join(executable_name(command_name)))
        .find(|candidate| candidate.is_file())
}

#[async_trait]
impl MediaBackend for YtDlpBackend {
    async fn fetch_feed(&self, source_url: &str, feed: FeedKind) -> anyhow::Result<Vec<FeedItem>> {
        tracing::debug!(%source_url, ?feed, "yt-dlp playlist metadata fetch starting");
        let started = Instant::now();
        let playlist_result = self
            .downloader
            .fetch_playlist_infos(source_url.to_string())
            .await;

        let playlist = match playlist_result {
            Ok(playlist) => {
                tracing::debug!(
                    %source_url,
                    ?feed,
                    playlist_id = %playlist.id,
                    playlist_title = %playlist.title,
                    item_count = playlist.entries.len(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "yt-dlp playlist metadata fetch completed"
                );
                playlist
            }
            Err(err) => {
                tracing::debug!(
                    %source_url,
                    ?feed,
                    elapsed_ms = started.elapsed().as_millis(),
                    error = %err,
                    "yt-dlp playlist metadata fetch failed"
                );
                return Err(err).with_context(|| {
                    format!("failed to fetch {feed:?} metadata for {source_url}")
                });
            }
        };

        Ok(playlist
            .entries
            .into_iter()
            .map(|entry| FeedItem {
                id: entry.id,
                title: entry.title,
                webpage_url: entry.url,
                description: entry
                    .uploader
                    .map(|uploader| format!("Uploaded by {uploader}")),
                published_at: None,
                content_length: None,
            })
            .collect())
    }

    async fn download_audio(&self, source_url: &str, output_path: &Path) -> anyhow::Result<()> {
        let parent = output_path
            .parent()
            .context("download path has no parent")?;
        fs::create_dir_all(parent).await?;
        remove_stale_file(output_path).await?;
        let temp_path = temp_download_path(output_path)?;
        remove_stale_file(&temp_path).await?;

        tracing::debug!(
            %source_url,
            output_path = %output_path.display(),
            temp_path = %temp_path.display(),
            yt_dlp = %self.ytdlp_path.display(),
            ffmpeg = %self.ffmpeg_path.display(),
            "yt-dlp passthrough audio download starting"
        );
        let started = Instant::now();
        let output = Command::new(&self.ytdlp_path)
            .args(ytdlp_download_args(
                &self.ffmpeg_path,
                &temp_path,
                source_url,
            ))
            .output()
            .await
            .with_context(|| format!("failed to execute yt-dlp for {source_url}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(
                %source_url,
                output_path = %output_path.display(),
                temp_path = %temp_path.display(),
                elapsed_ms = started.elapsed().as_millis(),
                status = ?output.status.code(),
                stderr = %stderr,
                "yt-dlp passthrough audio download failed"
            );
            return Err(anyhow::anyhow!(
                "yt-dlp failed for {source_url}: {}",
                stderr.trim()
            ));
        }

        fs::rename(&temp_path, output_path).await.with_context(|| {
            format!(
                "failed to move {} to {}",
                temp_path.display(),
                output_path.display()
            )
        })?;
        let downloaded_bytes = fs::metadata(output_path)
            .await
            .ok()
            .map(|metadata| metadata.len());
        tracing::debug!(
            %source_url,
            output_path = %output_path.display(),
            elapsed_ms = started.elapsed().as_millis(),
            bytes = downloaded_bytes,
            "yt-dlp passthrough audio download completed"
        );

        Ok(())
    }
}

fn temp_download_path(output_path: &Path) -> anyhow::Result<PathBuf> {
    let file_name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("download path has no valid file name")?;
    Ok(output_path.with_file_name(format!("{file_name}.download.m4a")))
}

async fn remove_stale_file(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn ytdlp_download_args(ffmpeg_path: &Path, output_path: &Path, source_url: &str) -> Vec<String> {
    vec![
        "--no-playlist".to_string(),
        "--no-part".to_string(),
        "--no-progress".to_string(),
        "--ffmpeg-location".to_string(),
        ffmpeg_path.display().to_string(),
        "-f".to_string(),
        "bestaudio[ext=m4a]".to_string(),
        "-o".to_string(),
        output_path.display().to_string(),
        source_url.to_string(),
    ]
}

pub struct DownloadCoordinator {
    backend: Arc<dyn MediaBackend>,
    in_flight: Arc<Mutex<HashMap<String, watch::Receiver<Option<Result<(), String>>>>>>,
}

impl DownloadCoordinator {
    pub fn new<B>(backend: B) -> Self
    where
        B: MediaBackend,
    {
        Self {
            backend: Arc::new(backend),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn from_arc(backend: Arc<dyn MediaBackend>) -> Self {
        Self {
            backend,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn fetch_feed(
        &self,
        source_url: &str,
        feed: FeedKind,
    ) -> anyhow::Result<Vec<FeedItem>> {
        tracing::debug!(%source_url, ?feed, "feed metadata request entering backend");
        self.backend.fetch_feed(source_url, feed).await
    }

    pub async fn ensure_download(
        &self,
        key: String,
        source_url: String,
        output_path: PathBuf,
    ) -> watch::Receiver<Option<Result<(), String>>> {
        let mut in_flight = self.in_flight.lock().await;
        if let Some(existing) = in_flight.get(&key) {
            tracing::debug!(
                %key,
                %source_url,
                output_path = %output_path.display(),
                "joining existing media download"
            );
            return existing.clone();
        }

        tracing::debug!(
            %key,
            %source_url,
            output_path = %output_path.display(),
            "starting new media download"
        );
        let (tx, rx) = watch::channel(None);
        in_flight.insert(key.clone(), rx.clone());
        let backend = Arc::clone(&self.backend);
        let map = Arc::clone(&self.in_flight);

        tokio::spawn(async move {
            let result = backend
                .download_audio(&source_url, &output_path)
                .await
                .map_err(|err| err.to_string());
            match &result {
                Ok(()) => tracing::debug!(
                    %key,
                    %source_url,
                    output_path = %output_path.display(),
                    "media download job completed"
                ),
                Err(err) => tracing::debug!(
                    %key,
                    %source_url,
                    output_path = %output_path.display(),
                    error = %err,
                    "media download job failed"
                ),
            }
            let _ = tx.send(Some(result));
            map.lock().await.remove(&key);
        });

        rx
    }
}

pub fn cache_key(user: &str, service: &str, account: &str, item_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user.as_bytes());
    hasher.update([0]);
    hasher.update(service.as_bytes());
    hasher.update([0]);
    hasher.update(account.as_bytes());
    hasher.update([0]);
    hasher.update(item_id.as_bytes());
    hex(&hasher.finalize())
}

pub fn media_path(config: &Config, key: &str) -> PathBuf {
    config.media_dir().join(format!("{key}.m4a"))
}

pub async fn is_fresh(path: &Path, ttl: Duration) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified.elapsed().map(|age| age <= ttl).unwrap_or(false)
}

pub fn stream_growing_file(
    path: PathBuf,
    mut completion: watch::Receiver<Option<Result<(), String>>>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::try_stream! {
        let mut offset = 0;

        loop {
            match File::open(&path).await {
                Ok(mut file) => {
                    file.seek(std::io::SeekFrom::Start(offset)).await?;
                    let mut buffer = vec![0; 64 * 1024];
                    let read = file.read(&mut buffer).await?;
                    if read > 0 {
                        offset += read as u64;
                        buffer.truncate(read);
                        yield Bytes::from(buffer);
                        continue;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => Err(err)?,
            }

            let completed = { completion.borrow().clone() };
            if let Some(result) = completed {
                if let Err(message) = result {
                    Err(std::io::Error::new(std::io::ErrorKind::Other, message))?;
                }
                break;
            }

            tokio::select! {
                _ = completion.changed() => {},
                _ = tokio::time::sleep(Duration::from_millis(150)) => {},
            }
        }
    }
}

pub async fn cleanup_expired_media(config: Config) {
    let ttl = Duration::from_secs(config.cache.media_ttl_seconds);
    loop {
        if let Err(err) = cleanup_once(&config.media_dir(), ttl).await {
            tracing::warn!(error = %err, "media cleanup failed");
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

pub async fn cleanup_once(media_dir: &Path, ttl: Duration) -> anyhow::Result<()> {
    let mut entries = match fs::read_dir(media_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    while let Some(entry) = entries.next_entry().await? {
        let metadata = entry.metadata().await?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if modified.elapsed().unwrap_or_default() > ttl {
            fs::remove_file(entry.path()).await?;
        }
    }

    Ok(())
}

pub fn download_error_response(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, err.to_string())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    struct MockBackend {
        downloads: AtomicUsize,
    }

    #[async_trait]
    impl MediaBackend for MockBackend {
        async fn fetch_feed(
            &self,
            _source_url: &str,
            _feed: FeedKind,
        ) -> anyhow::Result<Vec<FeedItem>> {
            Ok(vec![])
        }

        async fn download_audio(
            &self,
            _source_url: &str,
            output_path: &Path,
        ) -> anyhow::Result<()> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            fs::write(output_path, b"audio").await?;
            Ok(())
        }
    }

    #[test]
    fn cache_keys_are_stable() {
        assert_eq!(
            cache_key("derek", "soundcloud", "dereknet", "abc"),
            cache_key("derek", "soundcloud", "dereknet", "abc")
        );
        assert_ne!(
            cache_key("derek", "soundcloud", "dereknet", "abc"),
            cache_key("derek", "soundcloud", "dereknet", "def")
        );
    }

    #[tokio::test]
    async fn concurrent_download_requests_share_job() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let backend = Arc::new(MockBackend {
            downloads: AtomicUsize::new(0),
        });
        let coordinator = DownloadCoordinator::from_arc(backend.clone());

        let rx1 = coordinator
            .ensure_download("same".to_string(), "url".to_string(), path.clone())
            .await;
        let rx2 = coordinator
            .ensure_download("same".to_string(), "url".to_string(), path)
            .await;

        wait_done(rx1).await.unwrap();
        wait_done(rx2).await.unwrap();
        assert_eq!(backend.downloads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cleanup_removes_expired_media_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.m4a");
        fs::write(&path, b"old").await.unwrap();

        cleanup_once(dir.path(), Duration::from_secs(0))
            .await
            .unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn resolve_tool_path_prefers_managed_binary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(executable_name("yt-dlp"));
        std::fs::write(&path, b"tool").unwrap();

        assert_eq!(
            resolve_tool_path(path.clone(), "missing-tool").unwrap(),
            path
        );
    }

    #[test]
    fn find_in_paths_locates_system_binary_candidate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(executable_name("yt-dlp"));
        std::fs::write(&path, b"tool").unwrap();

        assert_eq!(
            find_in_paths("yt-dlp", std::iter::once(dir.path().to_path_buf())),
            Some(path)
        );
    }

    #[test]
    fn ytdlp_download_args_pin_m4a_output() {
        let args = ytdlp_download_args(
            Path::new("/usr/bin/ffmpeg"),
            Path::new("/tmp/item.m4a"),
            "https://soundcloud.com/example/item",
        );

        assert!(args.contains(&"bestaudio[ext=m4a]".to_string()));
        assert!(args.contains(&"/tmp/item.m4a".to_string()));
        assert!(args.contains(&"--no-part".to_string()));
        assert!(args.contains(&"--ffmpeg-location".to_string()));
    }

    #[test]
    fn temp_download_path_keeps_m4a_extension() {
        assert_eq!(
            temp_download_path(Path::new("/tmp/item.m4a")).unwrap(),
            PathBuf::from("/tmp/item.m4a.download.m4a")
        );
    }

    async fn wait_done(mut rx: watch::Receiver<Option<Result<(), String>>>) -> Result<(), String> {
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            rx.changed().await.unwrap();
        }
    }
}
