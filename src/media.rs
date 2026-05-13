use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use http::StatusCode;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
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
}

impl YtDlpBackend {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let libs_dir = config.libs_dir();
        let output_dir = config.media_dir();
        fs::create_dir_all(&libs_dir).await?;
        fs::create_dir_all(&output_dir).await?;
        let libraries =
            yt_dlp::client::deps::Libraries::new(libs_dir.join("yt-dlp"), libs_dir.join("ffmpeg"));
        let cache_config = yt_dlp::cache::config::CacheConfig::builder()
            .cache_dir(config.metadata_dir())
            .persistent_backend(Some(yt_dlp::cache::PersistentBackendKind::Redb))
            .build();
        let downloader = yt_dlp::Downloader::builder(libraries, output_dir)
            .with_cache_config(cache_config)
            .build()
            .await?;
        Ok(Self { downloader })
    }
}

#[async_trait]
impl MediaBackend for YtDlpBackend {
    async fn fetch_feed(&self, source_url: &str, feed: FeedKind) -> anyhow::Result<Vec<FeedItem>> {
        let playlist = self
            .downloader
            .fetch_playlist_infos(source_url.to_string())
            .await
            .with_context(|| format!("failed to fetch {feed:?} metadata for {source_url}"))?;

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
        let video = self
            .downloader
            .fetch_video_infos(source_url.to_string())
            .await
            .with_context(|| format!("failed to fetch media metadata for {source_url}"))?;
        let file_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("download path has no valid file name")?;

        self.downloader
            .download_audio_stream_with_quality(
                &video,
                file_name,
                yt_dlp::model::selector::AudioQuality::Best,
                yt_dlp::model::selector::AudioCodecPreference::AAC,
            )
            .await
            .with_context(|| format!("failed to download AAC audio for {source_url}"))?;

        Ok(())
    }
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
            return existing.clone();
        }

        let (tx, rx) = watch::channel(None);
        in_flight.insert(key.clone(), rx.clone());
        let backend = Arc::clone(&self.backend);
        let map = Arc::clone(&self.in_flight);

        tokio::spawn(async move {
            let result = backend
                .download_audio(&source_url, &output_path)
                .await
                .map_err(|err| err.to_string());
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

    async fn wait_done(mut rx: watch::Receiver<Option<Result<(), String>>>) -> Result<(), String> {
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            rx.changed().await.unwrap();
        }
    }
}
