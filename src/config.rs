use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::fs;

const PIP_TOOL_UPDATES_ENABLED_ENV: &str = "YT_DLP_FEED_PIP_TOOL_UPDATES_ENABLED";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub cache: CacheConfig,
    pub metadata: MetadataConfig,
    pub downloads: DownloadsConfig,
    pub pip_tool_updates: PipToolUpdatesConfig,
    pub auth: AuthConfig,
    pub users: Vec<UserConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CacheConfig {
    pub data_dir: PathBuf,
    pub media_ttl_minutes: Option<u64>,
    pub media_max_megabytes: Option<u64>,
    pub disconnect_behavior: DisconnectBehavior,
    pub disconnect_grace_seconds: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectBehavior {
    Continue,
    Cancel,
    DelayCancel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuthConfig {
    pub enabled: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MetadataConfig {
    pub refresh_interval_hours: Option<u64>,
    pub refresh_recent_grace_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DownloadsConfig {
    pub max_concurrent: usize,
    pub probe_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PipToolUpdatesConfig {
    pub enabled: bool,
    pub startup_check: bool,
    pub interval_hours: Option<u64>,
    pub pip_package: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserConfig {
    pub name: String,
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceConfig {
    pub kind: ServiceKind,
    #[serde(alias = "account")]
    pub name: String,
    pub profile_url: String,
    #[serde(default = "default_soundcloud_feeds")]
    pub feeds: Vec<SoundCloudFeedKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Soundcloud,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SoundCloudFeedKind {
    Profile,
    Likes,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            cache: CacheConfig::default(),
            metadata: MetadataConfig::default(),
            downloads: DownloadsConfig::default(),
            pip_tool_updates: PipToolUpdatesConfig::default(),
            auth: AuthConfig::default(),
            users: vec![UserConfig {
                name: "derek".to_string(),
                services: vec![
                    ServiceConfig {
                        kind: ServiceKind::Soundcloud,
                        name: "dereknet".to_string(),
                        profile_url: "https://soundcloud.com/dereknet".to_string(),
                        feeds: default_soundcloud_feeds(),
                    },
                    ServiceConfig {
                        kind: ServiceKind::Soundcloud,
                        name: "NTS".to_string(),
                        profile_url: "https://soundcloud.com/user-202286394-991268468".to_string(),
                        feeds: default_soundcloud_feeds(),
                    },
                ],
            }],
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            media_ttl_minutes: Some(360),
            media_max_megabytes: Some(10_240),
            disconnect_behavior: DisconnectBehavior::DelayCancel,
            disconnect_grace_seconds: 15,
        }
    }
}

impl CacheConfig {
    pub fn media_ttl(&self) -> Option<Duration> {
        self.media_ttl_minutes
            .map(|minutes| Duration::from_secs(minutes.saturating_mul(60)))
    }

    pub fn media_max_bytes(&self) -> Option<u64> {
        self.media_max_megabytes
            .map(|megabytes| megabytes.saturating_mul(1024 * 1024))
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            username: None,
            password: None,
        }
    }
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            refresh_interval_hours: Some(24),
            refresh_recent_grace_minutes: 60,
        }
    }
}

impl MetadataConfig {
    pub fn refresh_interval(&self) -> Option<Duration> {
        self.refresh_interval_hours
            .map(|hours| Duration::from_secs(hours.saturating_mul(3600)))
    }

    pub fn refresh_recent_grace(&self) -> chrono::Duration {
        chrono::Duration::minutes(self.refresh_recent_grace_minutes as i64)
    }
}

impl Default for DownloadsConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            probe_timeout_seconds: 300,
        }
    }
}

impl DownloadsConfig {
    pub fn probe_timeout(&self) -> Duration {
        Duration::from_secs(self.probe_timeout_seconds)
    }
}

impl Default for PipToolUpdatesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            startup_check: true,
            interval_hours: Some(168),
            pip_package: "yt-dlp".to_string(),
        }
    }
}

impl PipToolUpdatesConfig {
    pub fn interval(&self) -> Option<Duration> {
        self.interval_hours
            .map(|hours| Duration::from_secs(hours.saturating_mul(3600)))
    }
}

impl Config {
    pub async fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        let mut config = match fs::read_to_string(path).await {
            Ok(contents) => serde_yaml::from_str(&contents).context("invalid YAML config"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }?;
        config.apply_env_overrides();
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(value) = std::env::var(PIP_TOOL_UPDATES_ENABLED_ENV) {
            match parse_bool(&value) {
                Some(enabled) => self.pip_tool_updates.enabled = enabled,
                None => tracing::warn!(
                    env = PIP_TOOL_UPDATES_ENABLED_ENV,
                    value,
                    "ignoring invalid boolean environment value"
                ),
            }
        }
    }

    pub async fn ensure_directories(&self) -> anyhow::Result<()> {
        fs::create_dir_all(self.metadata_dir()).await?;
        fs::create_dir_all(self.feed_metadata_dir()).await?;
        fs::create_dir_all(self.media_dir()).await?;
        fs::create_dir_all(self.libs_dir()).await?;
        Ok(())
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.cache.data_dir.join("metadata")
    }

    pub fn feed_metadata_dir(&self) -> PathBuf {
        self.cache.data_dir.join("feed-metadata")
    }

    pub fn media_dir(&self) -> PathBuf {
        self.cache.data_dir.join("media")
    }

    pub fn libs_dir(&self) -> PathBuf {
        self.cache.data_dir.join("libs")
    }

    pub fn service(&self, user: &str, service: ServiceKind, name: &str) -> Option<&ServiceConfig> {
        self.users
            .iter()
            .find(|candidate| candidate.name == user)
            .and_then(|user| {
                user.services
                    .iter()
                    .find(|candidate| candidate.kind == service && candidate.name == name)
            })
    }

    pub fn lint_and_repair(&mut self) {
        if self.downloads.max_concurrent == 0 {
            tracing::warn!(
                "downloads.max_concurrent must be greater than 0; using default {}",
                DownloadsConfig::default().max_concurrent
            );
            self.downloads.max_concurrent = DownloadsConfig::default().max_concurrent;
        }

        if self.downloads.probe_timeout_seconds == 0 {
            tracing::warn!(
                "downloads.probe_timeout_seconds must be greater than 0; using default {}",
                DownloadsConfig::default().probe_timeout_seconds
            );
            self.downloads.probe_timeout_seconds = DownloadsConfig::default().probe_timeout_seconds;
        }

        if self.metadata.refresh_interval_hours == Some(0) {
            tracing::warn!("metadata.refresh_interval_hours=0 is invalid; using default 24");
            self.metadata.refresh_interval_hours = MetadataConfig::default().refresh_interval_hours;
        }

        if self.pip_tool_updates.interval_hours == Some(0) {
            tracing::warn!("pip_tool_updates.interval_hours=0 is invalid; using default 168");
            self.pip_tool_updates.interval_hours = PipToolUpdatesConfig::default().interval_hours;
        }

        if self.pip_tool_updates.pip_package.trim().is_empty() {
            tracing::warn!("pip_tool_updates.pip_package is empty; using default yt-dlp");
            self.pip_tool_updates.pip_package = PipToolUpdatesConfig::default().pip_package;
        }

        if self.auth.enabled
            && (self.auth.username.as_deref().unwrap_or_default().is_empty()
                || self.auth.password.as_deref().unwrap_or_default().is_empty())
        {
            tracing::warn!("auth.enabled=true but username or password is empty");
        }

        if !self.auth.enabled && self.server.bind.starts_with("0.0.0.0") {
            tracing::warn!(
                bind = %self.server.bind,
                "auth is disabled while binding all interfaces; use only on a trusted network"
            );
        }

        if self.downloads.max_concurrent > 10 {
            tracing::warn!(
                max_concurrent = self.downloads.max_concurrent,
                "downloads.max_concurrent is high for yt-dlp-backed media downloads"
            );
        }

        if matches!(self.cache.media_ttl_minutes, Some(minutes) if minutes < 10) {
            tracing::warn!(
                media_ttl_minutes = self.cache.media_ttl_minutes,
                "media TTL is very small and may cause repeated downloads"
            );
        }

        if self.metadata.refresh_interval_hours.is_none() {
            tracing::warn!("metadata scheduled refresh is disabled");
        }

        let mut service_keys = std::collections::HashSet::new();
        for user in &self.users {
            if user.name.trim().is_empty() {
                tracing::warn!("configured user has an empty name");
            }
            for service in &user.services {
                if service.name.trim().is_empty() {
                    tracing::warn!(user = %user.name, "configured service has an empty name");
                }
                if service.profile_url.trim().is_empty() {
                    tracing::warn!(
                        user = %user.name,
                        name = %service.name,
                        "configured service has an empty profile_url"
                    );
                } else if !service.profile_url.starts_with("http://")
                    && !service.profile_url.starts_with("https://")
                {
                    tracing::warn!(
                        user = %user.name,
                        name = %service.name,
                        profile_url = %service.profile_url,
                        "configured profile_url does not start with http:// or https://"
                    );
                }

                let key = (user.name.as_str(), service.kind, service.name.as_str());
                if !service_keys.insert(key) {
                    tracing::warn!(
                        user = %user.name,
                        service = %service.kind.as_path(),
                        name = %service.name,
                        "duplicate configured service route"
                    );
                }

                let mut feed_kinds = std::collections::HashSet::new();
                for feed in &service.feeds {
                    if !feed_kinds.insert(*feed) {
                        tracing::warn!(
                            user = %user.name,
                            service = %service.kind.as_path(),
                            name = %service.name,
                            feed = %feed.slug(),
                            "duplicate feed kind configured"
                        );
                    }
                }
            }
        }
    }

    pub fn log_startup_summary(&self) {
        let feed_count: usize = self
            .users
            .iter()
            .flat_map(|user| &user.services)
            .map(|service| service.feeds.len())
            .sum();

        tracing::info!(
            bind = %self.server.bind,
            data_dir = %self.cache.data_dir.display(),
            feed_metadata_dir = %self.feed_metadata_dir().display(),
            media_dir = %self.media_dir().display(),
            auth_enabled = self.auth.enabled,
            users = self.users.len(),
            feeds = feed_count,
            metadata_refresh_interval_hours = ?self.metadata.refresh_interval_hours,
            metadata_recent_grace_minutes = self.metadata.refresh_recent_grace_minutes,
            max_concurrent_downloads = self.downloads.max_concurrent,
            pip_tool_updates_enabled = self.pip_tool_updates.enabled,
            pip_tool_updates_interval_hours = ?self.pip_tool_updates.interval_hours,
            probe_timeout_seconds = self.downloads.probe_timeout_seconds,
            "yt-dlp-feed startup summary"
        );
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl ServiceKind {
    pub fn as_path(self) -> &'static str {
        match self {
            ServiceKind::Soundcloud => "soundcloud",
        }
    }
}

impl SoundCloudFeedKind {
    pub fn as_path(self) -> &'static str {
        match self {
            SoundCloudFeedKind::Profile => "feed.xml",
            SoundCloudFeedKind::Likes => "likes.xml",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SoundCloudFeedKind::Profile => "Profile",
            SoundCloudFeedKind::Likes => "Likes",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            SoundCloudFeedKind::Profile => "profile",
            SoundCloudFeedKind::Likes => "likes",
        }
    }

    pub fn source_url(self, service: &ServiceConfig) -> String {
        match self {
            SoundCloudFeedKind::Profile => service.profile_url.clone(),
            SoundCloudFeedKind::Likes => {
                format!("{}/likes", service.profile_url.trim_end_matches('/'))
            }
        }
    }
}

fn default_soundcloud_feeds() -> Vec<SoundCloudFeedKind> {
    vec![SoundCloudFeedKind::Profile, SoundCloudFeedKind::Likes]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_include_derek_soundcloud_feeds() {
        let config = Config::default();
        let service = config
            .service("derek", ServiceKind::Soundcloud, "dereknet")
            .expect("default derek soundcloud service");

        assert_eq!(service.profile_url, "https://soundcloud.com/dereknet");
        assert_eq!(
            service.feeds,
            vec![SoundCloudFeedKind::Profile, SoundCloudFeedKind::Likes]
        );
        assert_eq!(
            config.cache.disconnect_behavior,
            DisconnectBehavior::DelayCancel
        );
        assert_eq!(config.cache.disconnect_grace_seconds, 15);
        assert_eq!(config.cache.media_ttl_minutes, Some(360));
        assert_eq!(config.cache.media_ttl(), Some(Duration::from_secs(21_600)));
        assert_eq!(config.cache.media_max_megabytes, Some(10_240));
        assert_eq!(config.cache.media_max_bytes(), Some(10_737_418_240));
        assert!(!config.pip_tool_updates.enabled);
        assert!(config.pip_tool_updates.startup_check);
        assert_eq!(config.pip_tool_updates.interval_hours, Some(168));
        assert_eq!(config.pip_tool_updates.pip_package, "yt-dlp");
        assert_eq!(config.downloads.max_concurrent, 3);
        assert_eq!(config.downloads.probe_timeout_seconds, 300);
        assert_eq!(config.downloads.probe_timeout(), Duration::from_secs(300));

        let nts = config
            .service("derek", ServiceKind::Soundcloud, "NTS")
            .expect("default NTS soundcloud service");
        assert_eq!(
            nts.profile_url,
            "https://soundcloud.com/user-202286394-991268468"
        );
        assert_eq!(
            nts.feeds,
            vec![SoundCloudFeedKind::Profile, SoundCloudFeedKind::Likes]
        );
        assert_eq!(
            SoundCloudFeedKind::Likes.source_url(nts),
            "https://soundcloud.com/user-202286394-991268468/likes"
        );
    }

    #[test]
    fn parses_yaml_config_with_auth_and_disconnect_behavior() {
        let yaml = r#"
server:
  bind: "0.0.0.0:9090"
cache:
  data_dir: "/tmp/yt-dlp-feed"
  media_ttl_minutes: 42
  media_max_megabytes: 1024
  disconnect_behavior: "cancel"
  disconnect_grace_seconds: 5
downloads:
  max_concurrent: 4
  probe_timeout_seconds: 42
auth:
  enabled: true
  username: "derek"
  password: "secret"
pip_tool_updates:
  enabled: true
  startup_check: false
  interval_hours: 24
  pip_package: "yt-dlp-nightly"
users:
  - name: "derek"
    services:
      - kind: "soundcloud"
        name: "dereknet"
        profile_url: "https://soundcloud.com/dereknet"
        feeds:
          - profile
          - likes
      - kind: "soundcloud"
        name: "NTS"
        profile_url: "https://soundcloud.com/user-202286394-991268468"
        feeds:
          - profile
          - likes
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.server.bind, "0.0.0.0:9090");
        assert_eq!(config.cache.media_ttl_minutes, Some(42));
        assert_eq!(config.cache.media_ttl(), Some(Duration::from_secs(2_520)));
        assert_eq!(config.cache.media_max_megabytes, Some(1024));
        assert_eq!(config.cache.media_max_bytes(), Some(1_073_741_824));
        assert_eq!(config.cache.disconnect_behavior, DisconnectBehavior::Cancel);
        assert_eq!(config.cache.disconnect_grace_seconds, 5);
        assert_eq!(config.downloads.max_concurrent, 4);
        assert_eq!(config.downloads.probe_timeout_seconds, 42);
        assert_eq!(config.downloads.probe_timeout(), Duration::from_secs(42));
        assert!(config.auth.enabled);
        assert_eq!(config.auth.username.as_deref(), Some("derek"));
        assert!(config.pip_tool_updates.enabled);
        assert!(!config.pip_tool_updates.startup_check);
        assert_eq!(config.pip_tool_updates.interval_hours, Some(24));
        assert_eq!(
            config.pip_tool_updates.interval(),
            Some(Duration::from_secs(86_400))
        );
        assert_eq!(config.pip_tool_updates.pip_package, "yt-dlp-nightly");
        assert!(config
            .service("derek", ServiceKind::Soundcloud, "dereknet")
            .is_some());
        assert_eq!(
            config
                .service("derek", ServiceKind::Soundcloud, "NTS")
                .unwrap()
                .feeds,
            vec![SoundCloudFeedKind::Profile, SoundCloudFeedKind::Likes]
        );
    }

    #[test]
    fn parses_legacy_account_as_service_name() {
        let yaml = r#"
users:
  - name: "derek"
    services:
      - kind: "soundcloud"
        account: "NTS"
        profile_url: "https://soundcloud.com/user-202286394-991268468"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let service = config
            .service("derek", ServiceKind::Soundcloud, "NTS")
            .expect("legacy account alias should become service name");

        assert_eq!(service.name, "NTS");
        assert_eq!(
            SoundCloudFeedKind::Likes.source_url(service),
            "https://soundcloud.com/user-202286394-991268468/likes"
        );
    }

    #[test]
    fn parses_all_disconnect_behaviors() {
        for (yaml_value, expected) in [
            ("continue", DisconnectBehavior::Continue),
            ("cancel", DisconnectBehavior::Cancel),
            ("delay_cancel", DisconnectBehavior::DelayCancel),
        ] {
            let yaml = format!(
                r#"
cache:
  disconnect_behavior: "{yaml_value}"
"#
            );
            let config: Config = serde_yaml::from_str(&yaml).unwrap();

            assert_eq!(config.cache.disconnect_behavior, expected);
        }
    }

    #[test]
    fn parses_disabled_media_ttl_with_max_cache_size() {
        let yaml = r#"
cache:
  media_ttl_minutes: null
  media_max_megabytes: 2048
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.cache.media_ttl_minutes, None);
        assert_eq!(config.cache.media_ttl(), None);
        assert_eq!(config.cache.media_max_megabytes, Some(2048));
        assert_eq!(config.cache.media_max_bytes(), Some(2_147_483_648));
    }

    #[test]
    fn parses_bool_env_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("wat"), None);
    }
}
