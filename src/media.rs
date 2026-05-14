use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, watch, Mutex, Semaphore};

use crate::config::{Config, DisconnectBehavior, SoundCloudFeedKind};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeedItem {
    pub id: String,
    pub title: String,
    pub webpage_url: String,
    pub description: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub content_length: Option<u64>,
    pub thumbnail_url: Option<String>,
}

#[async_trait]
pub trait MediaBackend: Send + Sync + 'static {
    async fn fetch_feed(
        &self,
        source_url: &str,
        feed: SoundCloudFeedKind,
    ) -> anyhow::Result<Vec<FeedItem>>;
    async fn download_audio(
        &self,
        source_url: &str,
        output_path: &Path,
        chunks: broadcast::Sender<DownloadChunk>,
        shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
pub struct DownloadChunk {
    pub offset: u64,
    pub bytes: Bytes,
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
    async fn fetch_feed(
        &self,
        source_url: &str,
        feed: SoundCloudFeedKind,
    ) -> anyhow::Result<Vec<FeedItem>> {
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

        let mut skipped_playlist_items = 0usize;
        let items = playlist
            .entries
            .into_iter()
            .filter(|entry| {
                let is_playlist = is_soundcloud_playlist_item_url(&entry.url);
                if is_playlist {
                    skipped_playlist_items += 1;
                }
                !is_playlist
            })
            .map(|entry| FeedItem {
                id: entry.id,
                title: entry.title,
                webpage_url: entry.url,
                description: entry
                    .uploader
                    .map(|uploader| format!("Uploaded by {uploader}")),
                published_at: None,
                content_length: None,
                thumbnail_url: entry.thumbnail,
            })
            .collect();

        if skipped_playlist_items > 0 {
            tracing::debug!(
                %source_url,
                ?feed,
                skipped_playlist_items,
                "skipped SoundCloud set/playlist entries"
            );
        }

        Ok(items)
    }

    async fn download_audio(
        &self,
        source_url: &str,
        output_path: &Path,
        chunks: broadcast::Sender<DownloadChunk>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
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
        let progress_logging = tracing::enabled!(tracing::Level::DEBUG);
        let mut child = Command::new(&self.ytdlp_path)
            .args(ytdlp_download_args(
                &self.ffmpeg_path,
                "-",
                source_url,
                progress_logging,
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute yt-dlp for {source_url}"))?;

        let stdout = child
            .stdout
            .take()
            .context("yt-dlp stdout was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("yt-dlp stderr was not captured")?;

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(log_ytdlp_stderr(
            source_url.to_string(),
            Arc::clone(&stderr_tail),
            stderr,
        ));

        let mut cache_file = File::create(&temp_path)
            .await
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        let mut stdout = stdout;
        let mut offset = 0;
        let mut buffer = vec![0; 64 * 1024];
        let mut cancelled = false;

        loop {
            let read = tokio::select! {
                read = stdout.read(&mut buffer) => {
                    read.with_context(|| format!("failed to read yt-dlp stdout for {source_url}"))?
                },
                _ = wait_for_shutdown(&mut shutdown) => {
                    cancelled = true;
                    tracing::debug!(
                        %source_url,
                        output_path = %output_path.display(),
                        temp_path = %temp_path.display(),
                        "yt-dlp passthrough audio download cancelling for shutdown"
                    );
                    let _ = child.start_kill();
                    break;
                },
            };
            if read == 0 {
                break;
            }

            cache_file
                .write_all(&buffer[..read])
                .await
                .with_context(|| format!("failed to write {}", temp_path.display()))?;
            let bytes = Bytes::copy_from_slice(&buffer[..read]);
            let _ = chunks.send(DownloadChunk { offset, bytes });
            offset += read as u64;
        }

        cache_file
            .flush()
            .await
            .with_context(|| format!("failed to flush {}", temp_path.display()))?;
        drop(cache_file);

        let status = child
            .wait()
            .await
            .with_context(|| format!("failed to wait for yt-dlp for {source_url}"))?;
        let _ = stderr_task.await;

        if cancelled {
            tracing::debug!(
                %source_url,
                output_path = %output_path.display(),
                temp_path = %temp_path.display(),
                elapsed_ms = started.elapsed().as_millis(),
                status = ?status.code(),
                "yt-dlp passthrough audio download cancelled for shutdown"
            );
            remove_stale_file(&temp_path).await?;
            return Err(anyhow::anyhow!("yt-dlp cancelled during shutdown"));
        }

        if !status.success() {
            let stderr = stderr_tail.lock().await.join("\n");
            tracing::debug!(
                %source_url,
                output_path = %output_path.display(),
                temp_path = %temp_path.display(),
                elapsed_ms = started.elapsed().as_millis(),
                status = ?status.code(),
                stderr = %stderr,
                "yt-dlp passthrough audio download failed"
            );
            remove_stale_file(&temp_path).await?;
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

fn is_soundcloud_playlist_item_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };

    let host = host
        .split('@')
        .next_back()
        .and_then(|host| host.split(':').next())
        .unwrap_or(host);
    let path = path.split(['?', '#']).next().unwrap_or(path);

    match host {
        "soundcloud.com" | "www.soundcloud.com" => path.split('/').any(|part| part == "sets"),
        "api-v2.soundcloud.com" => path
            .split('/')
            .next()
            .is_some_and(|part| part == "playlists"),
        _ => false,
    }
}

fn ytdlp_download_args(
    ffmpeg_path: &Path,
    output_template: &str,
    source_url: &str,
    progress_logging: bool,
) -> Vec<String> {
    let mut args = vec![
        "--no-playlist".to_string(),
        "--playlist-items".to_string(),
        "1".to_string(),
        "--no-part".to_string(),
        "--ffmpeg-location".to_string(),
        ffmpeg_path.display().to_string(),
        "-f".to_string(),
        "bestaudio[ext=m4a]".to_string(),
        "-o".to_string(),
        output_template.to_string(),
    ];

    if progress_logging {
        args.extend([
            "--newline".to_string(),
            "--progress".to_string(),
            "--progress-delta".to_string(),
            "1".to_string(),
            "--progress-template".to_string(),
            "download:yt-dlp-rss-progress|%(progress.status|)s|%(progress.downloaded_bytes|)s|%(progress.total_bytes|)s|%(progress.total_bytes_estimate|)s|%(progress.speed|)s|%(progress.eta|)s".to_string(),
        ]);
    } else {
        args.push("--no-progress".to_string());
    }

    args.push(source_url.to_string());
    args
}

#[derive(Debug, PartialEq)]
struct YtDlpProgress {
    status: Option<String>,
    downloaded_bytes: Option<u64>,
    total_bytes: Option<u64>,
    total_bytes_estimate: Option<u64>,
    speed_bytes_per_second: Option<f64>,
    eta_seconds: Option<u64>,
}

impl YtDlpProgress {
    fn total_or_estimate(&self) -> Option<u64> {
        self.total_bytes.or(self.total_bytes_estimate)
    }

    fn effective_eta_seconds(&self) -> Option<u64> {
        self.eta_seconds.or_else(|| {
            let remaining = self
                .total_or_estimate()?
                .checked_sub(self.downloaded_bytes?)?;
            let speed = self.speed_bytes_per_second?;
            if speed <= 0.0 {
                return None;
            }
            Some((remaining as f64 / speed).ceil() as u64)
        })
    }

    fn downloaded_display(&self) -> Option<String> {
        self.downloaded_bytes.map(format_bytes)
    }

    fn total_display(&self) -> Option<String> {
        self.total_or_estimate().map(format_bytes)
    }

    fn speed_display(&self) -> Option<String> {
        self.speed_bytes_per_second.map(format_rate)
    }

    fn eta_display(&self) -> Option<String> {
        self.effective_eta_seconds().map(format_duration)
    }
}

fn parse_ytdlp_progress(line: &str) -> Option<YtDlpProgress> {
    let payload = line.strip_prefix("yt-dlp-rss-progress|")?;
    let mut parts = payload.split('|');
    Some(YtDlpProgress {
        status: empty_to_none(parts.next()).map(ToOwned::to_owned),
        downloaded_bytes: parse_u64_field(parts.next()),
        total_bytes: parse_u64_field(parts.next()),
        total_bytes_estimate: parse_u64_field(parts.next()),
        speed_bytes_per_second: parse_f64_field(parts.next()),
        eta_seconds: parse_u64_field(parts.next()),
    })
}

fn empty_to_none(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty() && value != "NA").then_some(value)
    })
}

fn parse_u64_field(value: Option<&str>) -> Option<u64> {
    empty_to_none(value)?.parse().ok()
}

fn parse_f64_field(value: Option<&str>) -> Option<f64> {
    empty_to_none(value)?.parse().ok()
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for candidate in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = candidate;
    }
    format_human_number(value, unit)
}

fn format_rate(bytes_per_second: f64) -> String {
    if bytes_per_second <= 0.0 {
        return "0 B/s".to_string();
    }
    format!("{}/s", format_bytes(bytes_per_second.round() as u64))
}

fn format_human_number(value: f64, unit: &str) -> String {
    if unit == "B" {
        format!("{} {unit}", value.round() as u64)
    } else if value >= 100.0 {
        format!("{value:.0} {unit}")
    } else if value >= 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

async fn log_ytdlp_stderr(
    source_url: String,
    tail: Arc<Mutex<Vec<String>>>,
    stderr: tokio::process::ChildStderr,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        {
            let mut tail = tail.lock().await;
            tail.push(line.clone());
            if tail.len() > 20 {
                tail.remove(0);
            }
        }

        if let Some(progress) = parse_ytdlp_progress(&line) {
            tracing::debug!(
                %source_url,
                status = progress.status.as_deref(),
                downloaded = progress.downloaded_display().as_deref(),
                total = progress.total_display().as_deref(),
                speed = progress.speed_display().as_deref(),
                eta = progress.eta_display().as_deref(),
                downloaded_bytes = progress.downloaded_bytes,
                total_bytes = progress.total_bytes,
                total_bytes_estimate = progress.total_bytes_estimate,
                speed_bytes_per_second = progress.speed_bytes_per_second,
                eta_seconds = progress.effective_eta_seconds(),
                "yt-dlp download progress"
            );
        } else {
            tracing::debug!(%source_url, line = %line, "yt-dlp stderr");
        }
    }
}

pub struct DownloadCoordinator {
    backend: Arc<dyn MediaBackend>,
    in_flight: Arc<Mutex<HashMap<String, ActiveDownload>>>,
    download_slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
    disconnect_behavior: DisconnectBehavior,
    disconnect_grace: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadLimitError;

#[derive(Clone)]
pub struct ActiveDownload {
    temp_path: PathBuf,
    completion: watch::Receiver<Option<Result<(), String>>>,
    chunks: broadcast::Sender<DownloadChunk>,
    shutdown: watch::Receiver<bool>,
    lifecycle: Arc<DownloadLifecycle>,
}

struct DownloadLifecycle {
    key: String,
    source_url: String,
    temp_path: PathBuf,
    clients: AtomicUsize,
    generation: AtomicU64,
    completed: AtomicBool,
    behavior: DisconnectBehavior,
    grace: Duration,
    cancel_tx: watch::Sender<bool>,
}

struct ClientAttachment {
    lifecycle: Option<Arc<DownloadLifecycle>>,
}

impl ActiveDownload {
    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    pub fn completion(&self) -> watch::Receiver<Option<Result<(), String>>> {
        self.completion.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DownloadChunk> {
        self.chunks.subscribe()
    }

    pub fn shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.clone()
    }

    fn attach_client(&self) -> ClientAttachment {
        self.lifecycle.attach_client()
    }
}

impl DownloadLifecycle {
    fn attach_client(self: &Arc<Self>) -> ClientAttachment {
        let clients = self.clients.fetch_add(1, Ordering::SeqCst) + 1;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::debug!(
            key = %self.key,
            source_url = %self.source_url,
            clients,
            generation,
            "media stream client attached"
        );

        ClientAttachment {
            lifecycle: Some(Arc::clone(self)),
        }
    }

    fn detach_client(self: Arc<Self>) {
        let previous = self.clients.fetch_sub(1, Ordering::SeqCst);
        if previous == 0 {
            return;
        }

        let clients = previous - 1;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::debug!(
            key = %self.key,
            source_url = %self.source_url,
            clients,
            generation,
            "media stream client detached"
        );

        if clients != 0 {
            return;
        }
        if self.completed.load(Ordering::SeqCst) {
            return;
        }

        match self.behavior {
            DisconnectBehavior::Continue => tracing::debug!(
                key = %self.key,
                source_url = %self.source_url,
                "media download continuing after last client disconnected"
            ),
            DisconnectBehavior::Cancel => self.cancel("last client disconnected"),
            DisconnectBehavior::DelayCancel => {
                let lifecycle = Arc::clone(&self);
                tokio::spawn(async move {
                    tracing::debug!(
                        key = %lifecycle.key,
                        source_url = %lifecycle.source_url,
                        grace_ms = lifecycle.grace.as_millis(),
                        generation,
                        "media download orphan cancellation grace started"
                    );
                    tokio::time::sleep(lifecycle.grace).await;
                    let still_orphaned = lifecycle.clients.load(Ordering::SeqCst) == 0;
                    let same_generation = lifecycle.generation.load(Ordering::SeqCst) == generation;
                    if still_orphaned && same_generation {
                        lifecycle.cancel("last client disconnected after grace period");
                    } else {
                        tracing::debug!(
                            key = %lifecycle.key,
                            source_url = %lifecycle.source_url,
                            clients = lifecycle.clients.load(Ordering::SeqCst),
                            generation = lifecycle.generation.load(Ordering::SeqCst),
                            "media download orphan cancellation skipped"
                        );
                    }
                });
            }
        }
    }

    fn cancel(&self, reason: &'static str) {
        if self.completed.load(Ordering::SeqCst) {
            return;
        }
        tracing::debug!(
            key = %self.key,
            source_url = %self.source_url,
            temp_path = %self.temp_path.display(),
            reason,
            "media download cancellation requested"
        );
        let _ = self.cancel_tx.send(true);
    }

    fn mark_completed(&self) {
        self.completed.store(true, Ordering::SeqCst);
    }
}

impl Drop for ClientAttachment {
    fn drop(&mut self) {
        if let Some(lifecycle) = self.lifecycle.take() {
            lifecycle.detach_client();
        }
    }
}

impl DownloadCoordinator {
    pub fn new<B>(backend: B) -> Self
    where
        B: MediaBackend,
    {
        Self {
            backend: Arc::new(backend),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            download_slots: Arc::new(Semaphore::new(3)),
            shutdown: default_shutdown_receiver(),
            disconnect_behavior: DisconnectBehavior::DelayCancel,
            disconnect_grace: Duration::from_secs(15),
        }
    }

    pub fn with_shutdown<B>(backend: B, shutdown: watch::Receiver<bool>) -> Self
    where
        B: MediaBackend,
    {
        Self::with_shutdown_and_disconnect(
            backend,
            shutdown,
            DisconnectBehavior::DelayCancel,
            Duration::from_secs(15),
        )
    }

    pub fn with_shutdown_and_disconnect<B>(
        backend: B,
        shutdown: watch::Receiver<bool>,
        disconnect_behavior: DisconnectBehavior,
        disconnect_grace: Duration,
    ) -> Self
    where
        B: MediaBackend,
    {
        Self {
            backend: Arc::new(backend),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            download_slots: Arc::new(Semaphore::new(3)),
            shutdown,
            disconnect_behavior,
            disconnect_grace,
        }
    }

    pub fn from_arc(backend: Arc<dyn MediaBackend>) -> Self {
        Self {
            backend,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            download_slots: Arc::new(Semaphore::new(3)),
            shutdown: default_shutdown_receiver(),
            disconnect_behavior: DisconnectBehavior::DelayCancel,
            disconnect_grace: Duration::from_secs(15),
        }
    }

    pub fn from_arc_with_shutdown(
        backend: Arc<dyn MediaBackend>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self::from_arc_with_shutdown_and_disconnect(
            backend,
            shutdown,
            DisconnectBehavior::DelayCancel,
            Duration::from_secs(15),
        )
    }

    pub fn from_arc_with_disconnect(
        backend: Arc<dyn MediaBackend>,
        disconnect_behavior: DisconnectBehavior,
        disconnect_grace: Duration,
    ) -> Self {
        Self::from_arc_with_shutdown_and_disconnect(
            backend,
            default_shutdown_receiver(),
            disconnect_behavior,
            disconnect_grace,
        )
    }

    pub fn from_arc_with_shutdown_and_disconnect(
        backend: Arc<dyn MediaBackend>,
        shutdown: watch::Receiver<bool>,
        disconnect_behavior: DisconnectBehavior,
        disconnect_grace: Duration,
    ) -> Self {
        Self::from_arc_with_shutdown_disconnect_and_limit(
            backend,
            shutdown,
            disconnect_behavior,
            disconnect_grace,
            3,
        )
    }

    pub fn from_arc_with_limit(backend: Arc<dyn MediaBackend>, max_concurrent: usize) -> Self {
        Self::from_arc_with_shutdown_disconnect_and_limit(
            backend,
            default_shutdown_receiver(),
            DisconnectBehavior::DelayCancel,
            Duration::from_secs(15),
            max_concurrent,
        )
    }

    pub fn from_arc_with_shutdown_disconnect_and_limit(
        backend: Arc<dyn MediaBackend>,
        shutdown: watch::Receiver<bool>,
        disconnect_behavior: DisconnectBehavior,
        disconnect_grace: Duration,
        max_concurrent: usize,
    ) -> Self {
        Self {
            backend,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            download_slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
            shutdown,
            disconnect_behavior,
            disconnect_grace,
        }
    }

    pub async fn fetch_feed(
        &self,
        source_url: &str,
        feed: SoundCloudFeedKind,
    ) -> anyhow::Result<Vec<FeedItem>> {
        tracing::debug!(%source_url, ?feed, "feed metadata request entering backend");
        self.backend.fetch_feed(source_url, feed).await
    }

    pub async fn ensure_download(
        &self,
        key: String,
        source_url: String,
        output_path: PathBuf,
    ) -> ActiveDownload {
        self.try_ensure_download(key, source_url, output_path)
            .await
            .unwrap_or_else(|_| active_download_error("maximum concurrent media downloads reached"))
    }

    pub async fn try_ensure_download(
        &self,
        key: String,
        source_url: String,
        output_path: PathBuf,
    ) -> Result<ActiveDownload, DownloadLimitError> {
        let mut in_flight = self.in_flight.lock().await;
        if let Some(existing) = in_flight.get(&key) {
            tracing::debug!(
                %key,
                %source_url,
                output_path = %output_path.display(),
                "joining existing media download"
            );
            return Ok(existing.clone());
        }

        let permit = self
            .download_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| DownloadLimitError)?;

        let temp_path = match temp_download_path(&output_path) {
            Ok(path) => path,
            Err(err) => {
                let (completion_tx, completion) = watch::channel(Some(Err(err.to_string())));
                drop(completion_tx);
                let (chunks, _) = broadcast::channel(1);
                let (cancel_tx, cancel_rx) = watch::channel(false);
                return Ok(ActiveDownload {
                    temp_path: output_path,
                    completion,
                    chunks,
                    shutdown: cancel_rx,
                    lifecycle: Arc::new(DownloadLifecycle {
                        key,
                        source_url,
                        temp_path: PathBuf::new(),
                        clients: AtomicUsize::new(0),
                        generation: AtomicU64::new(0),
                        completed: AtomicBool::new(true),
                        behavior: self.disconnect_behavior,
                        grace: self.disconnect_grace,
                        cancel_tx,
                    }),
                });
            }
        };
        tracing::debug!(
            %key,
            %source_url,
            output_path = %output_path.display(),
            temp_path = %temp_path.display(),
            "starting new media download"
        );
        let (tx, rx) = watch::channel(None);
        let (chunks, _) = broadcast::channel(64);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let lifecycle = Arc::new(DownloadLifecycle {
            key: key.clone(),
            source_url: source_url.clone(),
            temp_path: temp_path.clone(),
            clients: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            completed: AtomicBool::new(false),
            behavior: self.disconnect_behavior,
            grace: self.disconnect_grace,
            cancel_tx: cancel_tx.clone(),
        });
        let active = ActiveDownload {
            temp_path: temp_path.clone(),
            completion: rx.clone(),
            chunks: chunks.clone(),
            shutdown: cancel_rx.clone(),
            lifecycle,
        };
        in_flight.insert(key.clone(), active.clone());
        let backend = Arc::clone(&self.backend);
        let map = Arc::clone(&self.in_flight);
        let mut server_shutdown = self.shutdown.clone();
        let cancel_for_shutdown = cancel_tx.clone();
        let lifecycle_for_completion = Arc::clone(&active.lifecycle);

        tokio::spawn(async move {
            wait_for_shutdown(&mut server_shutdown).await;
            let _ = cancel_for_shutdown.send(true);
        });

        tokio::spawn(async move {
            let _permit = permit;
            let result = backend
                .download_audio(&source_url, &output_path, chunks, cancel_rx)
                .await
                .map_err(|err| err.to_string());
            lifecycle_for_completion.mark_completed();
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

        Ok(active)
    }
}

fn active_download_error(message: &str) -> ActiveDownload {
    let (completion_tx, completion) = watch::channel(Some(Err(message.to_string())));
    drop(completion_tx);
    let (chunks, _) = broadcast::channel(1);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    ActiveDownload {
        temp_path: PathBuf::new(),
        completion,
        chunks,
        shutdown: cancel_rx,
        lifecycle: Arc::new(DownloadLifecycle {
            key: "download-limit".to_string(),
            source_url: String::new(),
            temp_path: PathBuf::new(),
            clients: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            completed: AtomicBool::new(true),
            behavior: DisconnectBehavior::Cancel,
            grace: Duration::from_secs(0),
            cancel_tx,
        }),
    }
}

fn default_shutdown_receiver() -> watch::Receiver<bool> {
    let (_tx, rx) = watch::channel(false);
    rx
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

pub async fn is_fresh(path: &Path, ttl: Option<Duration>) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let Some(ttl) = ttl else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified.elapsed().map(|age| age <= ttl).unwrap_or(false)
}

pub fn stream_download(
    active: ActiveDownload,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::try_stream! {
        let _client = active.attach_client();
        let mut offset = 0;
        let path = active.temp_path().to_path_buf();
        let mut completion = active.completion();
        let mut chunks = active.subscribe();
        let mut shutdown = active.shutdown();

        loop {
            if let Some(bytes) = read_available_bytes(&path, &mut offset).await? {
                yield bytes;
                continue;
            }

            let completed = { completion.borrow().clone() };
            if let Some(result) = completed {
                if let Err(message) = result {
                    Err(std::io::Error::new(std::io::ErrorKind::Other, message))?;
                }
                break;
            }

            tokio::select! {
                chunk = chunks.recv() => {
                    match chunk {
                        Ok(chunk) => {
                            if let Some(bytes) = trim_chunk_to_offset(chunk, &mut offset) {
                                yield bytes;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {}
                    }
                },
                changed = completion.changed() => {
                    if changed.is_err() {
                        break;
                    }
                },
                _ = wait_for_shutdown(&mut shutdown) => {
                    tracing::debug!(
                        path = %path.display(),
                        "ending active media stream for shutdown"
                    );
                    let _ = remove_stale_file(&path).await;
                    break;
                },
                _ = tokio::time::sleep(Duration::from_millis(150)) => {},
            }
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }

    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }

    std::future::pending::<()>().await;
}

async fn read_available_bytes(
    path: &Path,
    offset: &mut u64,
) -> Result<Option<Bytes>, std::io::Error> {
    match File::open(path).await {
        Ok(mut file) => {
            file.seek(std::io::SeekFrom::Start(*offset)).await?;
            let mut buffer = vec![0; 64 * 1024];
            let read = file.read(&mut buffer).await?;
            if read > 0 {
                *offset += read as u64;
                buffer.truncate(read);
                return Ok(Some(Bytes::from(buffer)));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    Ok(None)
}

fn trim_chunk_to_offset(chunk: DownloadChunk, offset: &mut u64) -> Option<Bytes> {
    let chunk_end = chunk.offset + chunk.bytes.len() as u64;
    if chunk_end <= *offset {
        return None;
    }

    let skip = offset.saturating_sub(chunk.offset) as usize;
    let bytes = if skip == 0 {
        chunk.bytes
    } else {
        chunk.bytes.slice(skip..)
    };
    *offset += bytes.len() as u64;
    Some(bytes)
}

pub async fn cleanup_expired_media(config: Config) {
    loop {
        if let Err(err) = cleanup_once(
            &config.media_dir(),
            config.cache.media_ttl(),
            config.cache.media_max_bytes(),
        )
        .await
        {
            tracing::warn!(error = %err, "media cleanup failed");
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

pub async fn cleanup_once(
    media_dir: &Path,
    ttl: Option<Duration>,
    max_bytes: Option<u64>,
) -> anyhow::Result<()> {
    let mut entries = match fs::read_dir(media_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut cached_files = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let metadata = entry.metadata().await?;
        let path = entry.path();
        if !metadata.is_file() || !is_completed_media_file(&path) {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if ttl
            .map(|ttl| modified.elapsed().unwrap_or_default() > ttl)
            .unwrap_or(false)
        {
            fs::remove_file(path).await?;
            continue;
        }
        cached_files.push(CachedMediaFile {
            path,
            modified,
            bytes: metadata.len(),
        });
    }

    let Some(max_bytes) = max_bytes else {
        return Ok(());
    };

    let mut total_bytes = cached_files
        .iter()
        .map(|file| file.bytes)
        .fold(0_u64, u64::saturating_add);
    cached_files.sort_by_key(|file| file.modified);

    for file in cached_files {
        if total_bytes <= max_bytes {
            break;
        }
        fs::remove_file(&file.path).await?;
        total_bytes = total_bytes.saturating_sub(file.bytes);
    }

    Ok(())
}

struct CachedMediaFile {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

fn is_completed_media_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    file_name.ends_with(".m4a") && !file_name.ends_with(".download.m4a")
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
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::Notify;

    struct MockBackend {
        downloads: AtomicUsize,
        fail: bool,
        hold_open: bool,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl MediaBackend for MockBackend {
        async fn fetch_feed(
            &self,
            _source_url: &str,
            _feed: SoundCloudFeedKind,
        ) -> anyhow::Result<Vec<FeedItem>> {
            Ok(vec![])
        }

        async fn download_audio(
            &self,
            _source_url: &str,
            output_path: &Path,
            chunks: broadcast::Sender<DownloadChunk>,
            mut shutdown: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let temp_path = temp_download_path(output_path)?;
            fs::write(&temp_path, b"au").await?;
            let _ = chunks.send(DownloadChunk {
                offset: 0,
                bytes: Bytes::from_static(b"au"),
            });
            if self.hold_open {
                tokio::select! {
                    _ = self.release.notified() => {},
                    _ = wait_for_shutdown(&mut shutdown) => {
                        remove_stale_file(&temp_path).await?;
                        return Err(anyhow::anyhow!("mock download cancelled"));
                    },
                }
            }
            fs::write(&temp_path, b"audio").await?;
            let _ = chunks.send(DownloadChunk {
                offset: 2,
                bytes: Bytes::from_static(b"dio"),
            });
            if self.fail {
                remove_stale_file(&temp_path).await?;
                return Err(anyhow::anyhow!("mock download failed"));
            }
            fs::rename(temp_path, output_path).await?;
            Ok(())
        }
    }

    impl MockBackend {
        fn shared(hold_open: bool, fail: bool) -> (Arc<Self>, Arc<Notify>) {
            let release = Arc::new(Notify::new());
            (
                Arc::new(Self {
                    downloads: AtomicUsize::new(0),
                    fail,
                    hold_open,
                    release: Arc::clone(&release),
                }),
                release,
            )
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
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc(backend.clone());

        let active1 = coordinator
            .ensure_download("same".to_string(), "url".to_string(), path.clone())
            .await;
        let active2 = coordinator
            .ensure_download("same".to_string(), "url".to_string(), path)
            .await;

        release.notify_one();
        wait_done(active1.completion()).await.unwrap();
        wait_done(active2.completion()).await.unwrap();
        assert_eq!(backend.downloads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn new_downloads_respect_concurrency_limit() {
        let dir = tempdir().unwrap();
        let path1 = dir.path().join("item-1.m4a");
        let path2 = dir.path().join("item-2.m4a");
        let (backend, _release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_limit(backend, 1);

        let _active = coordinator
            .try_ensure_download("one".to_string(), "url-1".to_string(), path1)
            .await
            .unwrap();
        let limited = coordinator
            .try_ensure_download("two".to_string(), "url-2".to_string(), path2)
            .await;

        assert!(limited.is_err());
    }

    #[tokio::test]
    async fn concurrency_limit_allows_joining_existing_download() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_limit(backend.clone(), 1);

        let active1 = coordinator
            .try_ensure_download("same".to_string(), "url".to_string(), path.clone())
            .await
            .unwrap();
        let active2 = coordinator
            .try_ensure_download("same".to_string(), "url".to_string(), path)
            .await
            .unwrap();

        release.notify_one();
        wait_done(active1.completion()).await.unwrap();
        wait_done(active2.completion()).await.unwrap();
        assert_eq!(backend.downloads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_download_yields_bytes_before_completion() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc(backend);
        let active = coordinator
            .ensure_download("first".to_string(), "url".to_string(), path)
            .await;

        let mut stream = Box::pin(stream_download(active));
        let first = stream.next().await.unwrap().unwrap();
        release.notify_one();

        assert_eq!(first, Bytes::from_static(b"au"));
    }

    #[tokio::test]
    async fn stream_download_replays_temp_bytes_for_concurrent_reader() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc(backend);
        let active1 = coordinator
            .ensure_download("replay".to_string(), "url".to_string(), path)
            .await;
        let active2 = coordinator
            .ensure_download(
                "replay".to_string(),
                "url".to_string(),
                active1.temp_path().with_file_name("item.m4a"),
            )
            .await;

        let mut stream1 = Box::pin(stream_download(active1));
        assert_eq!(
            stream1.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );

        let mut stream2 = Box::pin(stream_download(active2.clone()));
        assert_eq!(
            stream2.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );

        release.notify_one();
        wait_done(active2.completion()).await.unwrap();
    }

    #[tokio::test]
    async fn successful_download_promotes_temp_cache() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc(backend);
        let active = coordinator
            .ensure_download("promote".to_string(), "url".to_string(), path.clone())
            .await;

        release.notify_one();
        wait_done(active.completion()).await.unwrap();

        assert_eq!(fs::read(&path).await.unwrap(), b"audio");
        assert!(!active.temp_path().exists());
    }

    #[tokio::test]
    async fn failed_download_does_not_promote_partial_cache() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, true);
        let coordinator = DownloadCoordinator::from_arc(backend);
        let active = coordinator
            .ensure_download("fail".to_string(), "url".to_string(), path.clone())
            .await;

        release.notify_one();
        assert!(wait_done(active.completion()).await.is_err());

        assert!(!path.exists());
        assert!(!active.temp_path().exists());
    }

    #[tokio::test]
    async fn immediate_disconnect_cancel_stops_download_and_removes_partial_cache() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, _release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_disconnect(
            backend,
            DisconnectBehavior::Cancel,
            Duration::from_secs(0),
        );
        let active = coordinator
            .ensure_download("disconnect".to_string(), "url".to_string(), path.clone())
            .await;
        let completion = active.completion();
        let temp_path = active.temp_path().to_path_buf();
        let mut stream = Box::pin(stream_download(active));

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        drop(stream);

        assert!(wait_done(completion).await.is_err());
        assert!(!path.exists());
        assert!(!temp_path.exists());
    }

    #[tokio::test]
    async fn delayed_disconnect_cancel_waits_for_grace_period() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_disconnect(
            backend,
            DisconnectBehavior::DelayCancel,
            Duration::from_millis(200),
        );
        let active = coordinator
            .ensure_download("delay".to_string(), "url".to_string(), path.clone())
            .await;
        let completion = active.completion();
        let temp_path = active.temp_path().to_path_buf();
        let mut stream = Box::pin(stream_download(active));

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        drop(stream);
        tokio::time::sleep(Duration::from_millis(25)).await;

        assert!(completion.borrow().is_none());
        assert!(temp_path.exists());

        release.notify_one();
        wait_done(completion).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn delayed_disconnect_cancel_stops_download_after_grace_period() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, _release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_disconnect(
            backend,
            DisconnectBehavior::DelayCancel,
            Duration::from_millis(10),
        );
        let active = coordinator
            .ensure_download("delay-expire".to_string(), "url".to_string(), path.clone())
            .await;
        let completion = active.completion();
        let temp_path = active.temp_path().to_path_buf();
        let mut stream = Box::pin(stream_download(active));

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        drop(stream);

        assert!(wait_done(completion).await.is_err());
        assert!(!path.exists());
        assert!(!temp_path.exists());
    }

    #[tokio::test]
    async fn reconnect_during_grace_prevents_disconnect_cancel() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_disconnect(
            backend,
            DisconnectBehavior::DelayCancel,
            Duration::from_millis(80),
        );
        let active1 = coordinator
            .ensure_download("reconnect".to_string(), "url".to_string(), path.clone())
            .await;
        let completion = active1.completion();
        let mut stream1 = Box::pin(stream_download(active1));

        assert_eq!(
            stream1.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        drop(stream1);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let active2 = coordinator
            .ensure_download("reconnect".to_string(), "url".to_string(), path.clone())
            .await;
        let mut stream2 = Box::pin(stream_download(active2));
        assert_eq!(
            stream2.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        tokio::time::sleep(Duration::from_millis(90)).await;

        assert!(completion.borrow().is_none());
        release.notify_one();
        wait_done(completion).await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_cancel_waits_for_all_clients_to_detach() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_disconnect(
            backend,
            DisconnectBehavior::Cancel,
            Duration::from_secs(0),
        );
        let active1 = coordinator
            .ensure_download("multi".to_string(), "url".to_string(), path.clone())
            .await;
        let completion = active1.completion();
        let active2 = coordinator
            .ensure_download("multi".to_string(), "url".to_string(), path.clone())
            .await;
        let mut stream1 = Box::pin(stream_download(active1));
        let mut stream2 = Box::pin(stream_download(active2));

        assert_eq!(
            stream1.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        assert_eq!(
            stream2.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        drop(stream1);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(completion.borrow().is_none());

        release.notify_one();
        wait_done(completion).await.unwrap();
        assert!(path.exists());
        drop(stream2);
    }

    #[tokio::test]
    async fn continue_disconnect_behavior_completes_cache_without_clients() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, release) = MockBackend::shared(true, false);
        let coordinator = DownloadCoordinator::from_arc_with_disconnect(
            backend,
            DisconnectBehavior::Continue,
            Duration::from_secs(0),
        );
        let active = coordinator
            .ensure_download("continue".to_string(), "url".to_string(), path.clone())
            .await;
        let completion = active.completion();
        let mut stream = Box::pin(stream_download(active));

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );
        drop(stream);
        release.notify_one();

        wait_done(completion).await.unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"audio");
    }

    #[tokio::test]
    async fn shutdown_cancels_active_download_and_removes_partial_cache() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item.m4a");
        let (backend, _release) = MockBackend::shared(true, false);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let coordinator = DownloadCoordinator::from_arc_with_shutdown(backend, shutdown_rx);
        let active = coordinator
            .ensure_download("shutdown".to_string(), "url".to_string(), path.clone())
            .await;

        let mut stream = Box::pin(stream_download(active.clone()));
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"au")
        );

        shutdown_tx.send(true).unwrap();
        assert!(wait_done(active.completion()).await.is_err());

        assert!(!path.exists());
        assert!(!active.temp_path().exists());
        match stream.next().await {
            Some(Err(_)) | None => {}
            Some(Ok(bytes)) => panic!("unexpected bytes after shutdown: {bytes:?}"),
        }
    }

    #[tokio::test]
    async fn cleanup_removes_expired_media_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.m4a");
        fs::write(&path, b"old").await.unwrap();

        cleanup_once(dir.path(), Some(Duration::from_secs(0)), None)
            .await
            .unwrap();

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cleanup_keeps_media_when_ttl_is_disabled() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("kept.m4a");
        fs::write(&path, b"kept").await.unwrap();

        cleanup_once(dir.path(), None, None).await.unwrap();

        assert!(path.exists());
    }

    #[tokio::test]
    async fn cleanup_trims_oldest_media_to_max_size() {
        let dir = tempdir().unwrap();
        let oldest = dir.path().join("oldest.m4a");
        let newest = dir.path().join("newest.m4a");
        fs::write(&oldest, b"old").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        fs::write(&newest, b"new").await.unwrap();

        cleanup_once(dir.path(), None, Some(3)).await.unwrap();

        assert!(!oldest.exists());
        assert!(newest.exists());
    }

    #[tokio::test]
    async fn cleanup_does_not_count_in_progress_downloads_against_max_size() {
        let dir = tempdir().unwrap();
        let complete = dir.path().join("complete.m4a");
        let partial = dir.path().join("complete.m4a.download.m4a");
        fs::write(&complete, b"complete").await.unwrap();
        fs::write(&partial, b"partial-download").await.unwrap();

        cleanup_once(dir.path(), None, Some(8)).await.unwrap();

        assert!(complete.exists());
        assert!(partial.exists());
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
            "-",
            "https://soundcloud.com/example/item",
            false,
        );

        assert!(args.contains(&"bestaudio[ext=m4a]".to_string()));
        assert!(args.contains(&"-".to_string()));
        assert!(args.contains(&"--no-part".to_string()));
        assert!(args_contains_pair(&args, "--playlist-items", "1"));
        assert!(args.contains(&"--ffmpeg-location".to_string()));
        assert!(args.contains(&"--no-progress".to_string()));
    }

    #[test]
    fn ytdlp_download_args_limit_playlist_to_single_item() {
        let args = ytdlp_download_args(
            Path::new("/usr/bin/ffmpeg"),
            "-",
            "https://soundcloud.com/example/sets/album",
            false,
        );

        assert!(args_contains_pair(&args, "--playlist-items", "1"));
    }

    #[test]
    fn detects_soundcloud_playlist_item_urls() {
        assert!(is_soundcloud_playlist_item_url(
            "https://soundcloud.com/dereknet/sets/bumpin"
        ));
        assert!(is_soundcloud_playlist_item_url(
            "https://api-v2.soundcloud.com/playlists/13564113"
        ));
        assert!(is_soundcloud_playlist_item_url(
            "https://www.soundcloud.com/dereknet/sets/bumpin?utm_source=feed"
        ));
    }

    #[test]
    fn keeps_soundcloud_track_item_urls() {
        assert!(!is_soundcloud_playlist_item_url(
            "https://soundcloud.com/disclosuremusic/apollo"
        ));
        assert!(!is_soundcloud_playlist_item_url(
            "https://api-v2.soundcloud.com/tracks/117448277"
        ));
        assert!(!is_soundcloud_playlist_item_url(
            "https://example.test/dereknet/sets/bumpin"
        ));
    }

    #[test]
    fn ytdlp_download_args_enable_debug_progress() {
        let args = ytdlp_download_args(
            Path::new("/usr/bin/ffmpeg"),
            "-",
            "https://soundcloud.com/example/item",
            true,
        );

        assert!(args.contains(&"--progress".to_string()));
        assert!(args.contains(&"--newline".to_string()));
        assert!(args.contains(&"--progress-delta".to_string()));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("download:yt-dlp-rss-progress|")));
    }

    #[test]
    fn parses_ytdlp_progress_with_eta() {
        let progress =
            parse_ytdlp_progress("yt-dlp-rss-progress|downloading|1024|4096||512|6").unwrap();

        assert_eq!(progress.status.as_deref(), Some("downloading"));
        assert_eq!(progress.downloaded_bytes, Some(1024));
        assert_eq!(progress.total_bytes, Some(4096));
        assert_eq!(progress.speed_bytes_per_second, Some(512.0));
        assert_eq!(progress.effective_eta_seconds(), Some(6));
    }

    #[test]
    fn parses_ytdlp_progress_with_computed_eta() {
        let progress =
            parse_ytdlp_progress("yt-dlp-rss-progress|downloading|1024||4096|512|").unwrap();

        assert_eq!(progress.total_bytes_estimate, Some(4096));
        assert_eq!(progress.effective_eta_seconds(), Some(6));
    }

    #[test]
    fn parses_ytdlp_progress_with_unknown_speed() {
        let progress =
            parse_ytdlp_progress("yt-dlp-rss-progress|downloading|1024||4096||").unwrap();

        assert_eq!(progress.effective_eta_seconds(), None);
    }

    #[test]
    fn ignores_non_progress_stderr_lines() {
        assert!(parse_ytdlp_progress("[download] Destination: item.m4a").is_none());
    }

    #[test]
    fn formats_progress_values_for_humans() {
        let progress = parse_ytdlp_progress(
            "yt-dlp-rss-progress|downloading|3030476|4305764||401394.85616020963|4",
        )
        .unwrap();

        assert_eq!(progress.downloaded_display().as_deref(), Some("2.89 MiB"));
        assert_eq!(progress.total_display().as_deref(), Some("4.11 MiB"));
        assert_eq!(progress.speed_display().as_deref(), Some("392 KiB/s"));
        assert_eq!(progress.eta_display().as_deref(), Some("4s"));
    }

    #[test]
    fn formats_computed_eta_for_humans() {
        let progress =
            parse_ytdlp_progress("yt-dlp-rss-progress|downloading|1048576||7340032|1048576|")
                .unwrap();

        assert_eq!(progress.downloaded_display().as_deref(), Some("1.00 MiB"));
        assert_eq!(progress.total_display().as_deref(), Some("7.00 MiB"));
        assert_eq!(progress.speed_display().as_deref(), Some("1.00 MiB/s"));
        assert_eq!(progress.eta_display().as_deref(), Some("6s"));
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

    fn args_contains_pair(args: &[String], key: &str, value: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == key && window[1] == value)
    }
}
