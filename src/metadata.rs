use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Mutex, Notify};

use crate::config::{Config, ServiceConfig, ServiceKind, SoundCloudFeedKind};
use crate::media::{DownloadCoordinator, FeedItem};

const SCHEMA_VERSION: u32 = 1;
const LAST_ERROR_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FeedIdentity {
    pub user: String,
    pub service: ServiceKind,
    #[serde(alias = "account")]
    pub name: String,
    pub feed: SoundCloudFeedKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFeedMetadata {
    pub schema_version: u32,
    pub user: String,
    pub service: ServiceKind,
    #[serde(alias = "account")]
    pub name: String,
    pub feed: SoundCloudFeedKind,
    pub source_url: String,
    pub last_successful_refresh: DateTime<Utc>,
    pub items: Vec<FeedItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataCacheState {
    Missing,
    Warming,
    Ready,
    Refreshing,
    Stale,
    Error,
}

impl MetadataCacheState {
    pub fn as_header_value(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Warming => "warming",
            Self::Ready => "ready",
            Self::Refreshing => "refreshing",
            Self::Stale => "stale",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedStatus {
    pub user: String,
    pub service: String,
    pub name: String,
    pub feed: String,
    pub title: String,
    pub source_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<usize>,
    pub metadata_cache: MetadataCacheStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetadataCacheStatus {
    pub state: MetadataCacheState,
    pub last_successful_refresh: Option<DateTime<Utc>>,
    pub refresh_in_progress: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexJson {
    pub generated_at: DateTime<Utc>,
    pub summary: IndexSummary,
    pub feeds: Vec<FeedStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    pub feeds_total: usize,
    pub feeds_ready: usize,
    pub feeds_missing: usize,
    pub refreshes_in_progress: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPriority {
    Manual,
    Scheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Refreshed,
    AlreadyRunning(MetadataCacheState),
    Failed(MetadataCacheState),
}

#[derive(Debug, Clone)]
pub struct RefreshHandle {
    receiver: watch::Receiver<Option<RefreshOutcome>>,
    already_running: Option<MetadataCacheState>,
}

impl RefreshHandle {
    pub fn already_running(&self) -> Option<MetadataCacheState> {
        self.already_running
    }

    pub async fn wait(mut self) -> RefreshOutcome {
        if let Some(state) = self.already_running {
            return RefreshOutcome::AlreadyRunning(state);
        }

        loop {
            if let Some(outcome) = *self.receiver.borrow() {
                return outcome;
            }
            if self.receiver.changed().await.is_err() {
                return RefreshOutcome::Failed(MetadataCacheState::Error);
            }
        }
    }
}

#[derive(Clone)]
pub struct MetadataCache {
    config: Config,
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
}

#[derive(Default)]
struct Inner {
    memory: HashMap<FeedIdentity, CachedFeedMetadata>,
    statuses: HashMap<FeedIdentity, RuntimeStatus>,
    manual_queue: VecDeque<RefreshJob>,
    scheduled_queue: VecDeque<RefreshJob>,
    worker_started: bool,
}

#[derive(Debug, Clone, Default)]
struct RuntimeStatus {
    refreshing: bool,
    last_error: Option<String>,
    waiters: Vec<watch::Sender<Option<RefreshOutcome>>>,
}

#[derive(Debug, Clone)]
struct RefreshJob {
    identity: FeedIdentity,
    source_url: String,
}

impl MetadataCache {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(config.feed_metadata_dir()).await?;
        Ok(Self {
            config,
            inner: Arc::new(Mutex::new(Inner::default())),
            notify: Arc::new(Notify::new()),
        })
    }

    pub async fn get(&self, identity: &FeedIdentity) -> Option<CachedFeedMetadata> {
        if let Some(cached) = self.inner.lock().await.memory.get(identity).cloned() {
            return Some(cached);
        }

        match self.read_from_disk(identity).await {
            Ok(Some(cached)) => {
                self.inner
                    .lock()
                    .await
                    .memory
                    .insert(identity.clone(), cached.clone());
                Some(cached)
            }
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(?identity, error = %err, "metadata cache read failed");
                None
            }
        }
    }

    pub async fn status(
        &self,
        identity: &FeedIdentity,
        service: &ServiceConfig,
    ) -> MetadataCacheStatus {
        let cached = self.get(identity).await;
        self.status_from_cached(identity, service, cached).await
    }

    async fn status_from_cached(
        &self,
        identity: &FeedIdentity,
        service: &ServiceConfig,
        cached: Option<CachedFeedMetadata>,
    ) -> MetadataCacheStatus {
        let inner = self.inner.lock().await;
        let runtime = inner.statuses.get(identity);
        let refreshing = runtime.map(|status| status.refreshing).unwrap_or(false);
        let last_error = runtime.and_then(|status| status.last_error.clone());
        let state = match (cached.is_some(), refreshing, last_error.is_some()) {
            (false, true, _) => MetadataCacheState::Warming,
            (false, false, true) => MetadataCacheState::Error,
            (false, false, false) => MetadataCacheState::Missing,
            (true, true, _) => MetadataCacheState::Refreshing,
            (true, false, true) => MetadataCacheState::Stale,
            (true, false, false) => MetadataCacheState::Ready,
        };

        let last_successful_refresh = cached.as_ref().map(|cache| cache.last_successful_refresh);
        drop(inner);

        tracing::trace!(
            user = %identity.user,
            service = %identity.service.as_path(),
            name = %identity.name,
            feed = %identity.feed.slug(),
            source_url = %identity.feed.source_url(service),
            state = ?state,
            "metadata cache status checked"
        );

        MetadataCacheStatus {
            state,
            last_successful_refresh,
            refresh_in_progress: refreshing,
            last_error,
        }
    }

    pub async fn index_json(&self) -> IndexJson {
        self.index_json_for_user(None).await
    }

    pub async fn index_json_for_user(&self, only_user: Option<&str>) -> IndexJson {
        let mut feeds = Vec::new();

        for (identity, service) in self.configured_feeds() {
            if only_user.is_some_and(|user| identity.user != user) {
                continue;
            }
            let cached = self.get(&identity).await;
            let item_count = cached
                .as_ref()
                .map(|cache| cache.items.len())
                .filter(|count| *count > 0);
            let status = self.status_from_cached(&identity, &service, cached).await;
            feeds.push(FeedStatus {
                user: identity.user.clone(),
                service: identity.service.as_path().to_string(),
                name: identity.name.clone(),
                feed: identity.feed.slug().to_string(),
                title: feed_title(&service, identity.feed),
                source_url: identity.feed.source_url(&service),
                item_count,
                metadata_cache: status,
            });
        }

        let feeds_total = feeds.len();
        let feeds_ready = feeds
            .iter()
            .filter(|feed| {
                matches!(
                    feed.metadata_cache.state,
                    MetadataCacheState::Ready
                        | MetadataCacheState::Refreshing
                        | MetadataCacheState::Stale
                )
            })
            .count();
        let refreshes_in_progress = feeds
            .iter()
            .filter(|feed| feed.metadata_cache.refresh_in_progress)
            .count();

        IndexJson {
            generated_at: Utc::now(),
            summary: IndexSummary {
                feeds_total,
                feeds_ready,
                feeds_missing: feeds_total.saturating_sub(feeds_ready),
                refreshes_in_progress,
            },
            feeds,
        }
    }

    pub async fn ready_missing_count(&self) -> usize {
        let mut missing = 0;
        for (identity, _) in self.configured_feeds() {
            if self.get(&identity).await.is_none() {
                missing += 1;
            }
        }
        missing
    }

    pub async fn store_success_for_test(
        &self,
        identity: FeedIdentity,
        service: &ServiceConfig,
        items: Vec<FeedItem>,
    ) -> anyhow::Result<()> {
        let cached = CachedFeedMetadata {
            schema_version: SCHEMA_VERSION,
            user: identity.user.clone(),
            service: identity.service,
            name: identity.name.clone(),
            feed: identity.feed,
            source_url: identity.feed.source_url(service),
            last_successful_refresh: Utc::now(),
            items,
        };
        self.write_to_disk(&identity, &cached).await?;
        self.inner.lock().await.memory.insert(identity, cached);
        Ok(())
    }

    pub async fn enqueue_refresh(
        &self,
        identity: FeedIdentity,
        service: &ServiceConfig,
        priority: RefreshPriority,
    ) -> RefreshHandle {
        let mut inner = self.inner.lock().await;
        let cached_exists = inner.memory.contains_key(&identity);
        let status = inner.statuses.entry(identity.clone()).or_default();
        let (tx, rx) = watch::channel(None);

        if status.refreshing {
            let state = if cached_exists {
                MetadataCacheState::Refreshing
            } else {
                MetadataCacheState::Warming
            };
            return RefreshHandle {
                receiver: rx,
                already_running: Some(state),
            };
        }

        status.refreshing = true;
        status.waiters.push(tx);
        let job = RefreshJob {
            identity: identity.clone(),
            source_url: identity.feed.source_url(service),
        };

        match priority {
            RefreshPriority::Manual => inner.manual_queue.push_back(job),
            RefreshPriority::Scheduled => inner.scheduled_queue.push_back(job),
        }

        let queue_size = inner.manual_queue.len() + inner.scheduled_queue.len();
        tracing::info!(
            user = %identity.user,
            service = %identity.service.as_path(),
            name = %identity.name,
            feed = %identity.feed.slug(),
            priority = ?priority,
            queue_size,
            "metadata refresh queued"
        );
        drop(inner);
        self.notify.notify_one();

        RefreshHandle {
            receiver: rx,
            already_running: None,
        }
    }

    pub async fn enqueue_missing(&self) {
        for (identity, service) in self.configured_feeds() {
            if self.get(&identity).await.is_none() {
                self.enqueue_refresh(identity, &service, RefreshPriority::Scheduled)
                    .await;
            }
        }
    }

    pub async fn enqueue_due_scheduled(&self) {
        let recent_grace = self.config.metadata.refresh_recent_grace();
        for (identity, service) in self.configured_feeds() {
            let cached = self.get(&identity).await;
            if cached
                .as_ref()
                .is_some_and(|cache| Utc::now() - cache.last_successful_refresh < recent_grace)
            {
                tracing::debug!(
                    user = %identity.user,
                    service = %identity.service.as_path(),
                    name = %identity.name,
                    feed = %identity.feed.slug(),
                    "metadata scheduled refresh skipped because cache is recent"
                );
                continue;
            }
            self.enqueue_refresh(identity, &service, RefreshPriority::Scheduled)
                .await;
        }
    }

    pub fn start_worker(&self, downloads: Arc<DownloadCoordinator>) {
        let cache = self.clone();
        tokio::spawn(async move {
            cache.worker_loop(downloads).await;
        });
    }

    pub fn start_startup_warming(&self) {
        let cache = self.clone();
        tokio::spawn(async move {
            cache.enqueue_missing().await;
        });
    }

    pub fn start_scheduled_refresh(&self) {
        let Some(interval) = self.config.metadata.refresh_interval() else {
            tracing::warn!("metadata scheduled refresh disabled");
            return;
        };
        let cache = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                cache.enqueue_due_scheduled().await;
            }
        });
    }

    async fn worker_loop(&self, downloads: Arc<DownloadCoordinator>) {
        {
            let mut inner = self.inner.lock().await;
            if inner.worker_started {
                return;
            }
            inner.worker_started = true;
        }

        loop {
            let job = loop {
                if let Some(job) = self.next_job().await {
                    break job;
                }
                self.notify.notified().await;
            };

            let queue_size = {
                let inner = self.inner.lock().await;
                inner.manual_queue.len() + inner.scheduled_queue.len()
            };
            tracing::info!(
                user = %job.identity.user,
                service = %job.identity.service.as_path(),
                name = %job.identity.name,
                feed = %job.identity.feed.slug(),
                queue_size,
                "metadata refresh started"
            );

            let result = downloads
                .fetch_feed(&job.source_url, job.identity.feed)
                .await
                .map(|items| CachedFeedMetadata {
                    schema_version: SCHEMA_VERSION,
                    user: job.identity.user.clone(),
                    service: job.identity.service,
                    name: job.identity.name.clone(),
                    feed: job.identity.feed,
                    source_url: job.source_url.clone(),
                    last_successful_refresh: Utc::now(),
                    items,
                });

            match result {
                Ok(cached) => {
                    let write_result = self.write_to_disk(&job.identity, &cached).await;
                    match write_result {
                        Ok(()) => {
                            self.finish_refresh(
                                &job.identity,
                                Some(cached),
                                None,
                                RefreshOutcome::Refreshed,
                            )
                            .await;
                        }
                        Err(err) => {
                            let state = if self.get(&job.identity).await.is_some() {
                                MetadataCacheState::Stale
                            } else {
                                MetadataCacheState::Error
                            };
                            self.finish_refresh(
                                &job.identity,
                                None,
                                Some(err.to_string()),
                                RefreshOutcome::Failed(state),
                            )
                            .await;
                        }
                    }
                }
                Err(err) => {
                    let state = if self.get(&job.identity).await.is_some() {
                        MetadataCacheState::Stale
                    } else {
                        MetadataCacheState::Error
                    };
                    self.finish_refresh(
                        &job.identity,
                        None,
                        Some(err.to_string()),
                        RefreshOutcome::Failed(state),
                    )
                    .await;
                }
            }
        }
    }

    async fn next_job(&self) -> Option<RefreshJob> {
        let mut inner = self.inner.lock().await;
        inner
            .manual_queue
            .pop_front()
            .or_else(|| inner.scheduled_queue.pop_front())
    }

    async fn finish_refresh(
        &self,
        identity: &FeedIdentity,
        cached: Option<CachedFeedMetadata>,
        error: Option<String>,
        outcome: RefreshOutcome,
    ) {
        let mut inner = self.inner.lock().await;
        if let Some(cached) = cached {
            inner.memory.insert(identity.clone(), cached);
        }

        let queue_size = inner.manual_queue.len() + inner.scheduled_queue.len();
        let status = inner.statuses.entry(identity.clone()).or_default();
        status.refreshing = false;
        status.last_error = error.map(|message| truncate_error(&message));
        let waiters = std::mem::take(&mut status.waiters);

        for waiter in waiters {
            let _ = waiter.send(Some(outcome));
        }

        match outcome {
            RefreshOutcome::Refreshed => tracing::info!(
                user = %identity.user,
                service = %identity.service.as_path(),
                name = %identity.name,
                feed = %identity.feed.slug(),
                queue_size,
                "metadata refresh completed"
            ),
            RefreshOutcome::Failed(state) => tracing::warn!(
                user = %identity.user,
                service = %identity.service.as_path(),
                name = %identity.name,
                feed = %identity.feed.slug(),
                state = state.as_header_value(),
                queue_size,
                error = status.last_error.as_deref(),
                "metadata refresh failed"
            ),
            RefreshOutcome::AlreadyRunning(_) => {}
        }
    }

    fn configured_feeds(&self) -> Vec<(FeedIdentity, ServiceConfig)> {
        let mut feeds = Vec::new();
        for user in &self.config.users {
            for service in &user.services {
                for feed in &service.feeds {
                    feeds.push((
                        FeedIdentity {
                            user: user.name.clone(),
                            service: service.kind,
                            name: service.name.clone(),
                            feed: *feed,
                        },
                        service.clone(),
                    ));
                }
            }
        }
        feeds
    }

    async fn read_from_disk(
        &self,
        identity: &FeedIdentity,
    ) -> anyhow::Result<Option<CachedFeedMetadata>> {
        let path = self.cache_path(identity);
        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        let cached: CachedFeedMetadata = serde_json::from_str(&contents)
            .with_context(|| format!("invalid metadata cache JSON {}", path.display()))?;
        if cached.schema_version != SCHEMA_VERSION {
            tracing::warn!(
                path = %path.display(),
                schema_version = cached.schema_version,
                "ignoring metadata cache with unknown schema version"
            );
            return Ok(None);
        }
        Ok(Some(cached))
    }

    async fn write_to_disk(
        &self,
        identity: &FeedIdentity,
        cached: &CachedFeedMetadata,
    ) -> anyhow::Result<()> {
        let path = self.cache_path(identity);
        let temp_path = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(cached)?;
        tokio::fs::write(&temp_path, json)
            .await
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        tokio::fs::rename(&temp_path, &path)
            .await
            .with_context(|| {
                format!(
                    "failed to promote metadata cache {} to {}",
                    temp_path.display(),
                    path.display()
                )
            })?;
        Ok(())
    }

    fn cache_path(&self, identity: &FeedIdentity) -> PathBuf {
        self.config
            .feed_metadata_dir()
            .join(format!("{}.json", cache_file_stem(identity)))
    }
}

pub fn identity_for(user: &str, service: &ServiceConfig, feed: SoundCloudFeedKind) -> FeedIdentity {
    FeedIdentity {
        user: user.to_string(),
        service: service.kind,
        name: service.name.clone(),
        feed,
    }
}

pub fn feed_title(service: &ServiceConfig, feed: SoundCloudFeedKind) -> String {
    format!(
        "{}: {} / {}",
        service_display_name(service.kind.as_path()),
        service.name,
        feed.label()
    )
}

fn service_display_name(service: &str) -> &str {
    match service {
        "soundcloud" => "SoundCloud",
        _ => service,
    }
}

fn cache_file_stem(identity: &FeedIdentity) -> String {
    let readable = format!(
        "{}__{}__{}__{}",
        safe_part(&identity.user),
        safe_part(identity.service.as_path()),
        safe_part(&identity.name),
        safe_part(identity.feed.slug())
    );
    format!("{}__{}", readable, &identity_hash(identity)[..12])
}

fn identity_hash(identity: &FeedIdentity) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.user.as_bytes());
    hasher.update([0]);
    hasher.update(identity.service.as_path().as_bytes());
    hasher.update([0]);
    hasher.update(identity.name.as_bytes());
    hasher.update([0]);
    hasher.update(identity.feed.slug().as_bytes());
    hex(&hasher.finalize())
}

fn safe_part(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "empty".to_string()
    } else {
        output
    }
}

fn truncate_error(input: &str) -> String {
    input.chars().take(LAST_ERROR_LIMIT).collect()
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

pub fn namespace_map() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "media".to_string(),
            "http://search.yahoo.com/mrss/".to_string(),
        ),
        (
            "itunes".to_string(),
            "http://www.itunes.com/dtds/podcast-1.0.dtd".to_string(),
        ),
        (
            "atom".to_string(),
            "http://www.w3.org/2005/Atom".to_string(),
        ),
    ])
}
