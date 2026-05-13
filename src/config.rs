use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub cache: CacheConfig,
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
    pub media_ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuthConfig {
    pub enabled: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserConfig {
    pub name: String,
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceConfig {
    pub kind: ServiceKind,
    pub account: String,
    pub profile_url: String,
    #[serde(default = "default_feeds")]
    pub feeds: Vec<FeedKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Soundcloud,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    Profile,
    Likes,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            cache: CacheConfig::default(),
            auth: AuthConfig::default(),
            users: vec![UserConfig {
                name: "derek".to_string(),
                services: vec![ServiceConfig {
                    kind: ServiceKind::Soundcloud,
                    account: "dereknet".to_string(),
                    profile_url: "https://soundcloud.com/dereknet".to_string(),
                    feeds: default_feeds(),
                }],
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
            media_ttl_seconds: 86_400,
        }
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

impl Config {
    pub async fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        match fs::read_to_string(path).await {
            Ok(contents) => serde_yaml::from_str(&contents).context("invalid YAML config"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub async fn ensure_directories(&self) -> anyhow::Result<()> {
        fs::create_dir_all(self.metadata_dir()).await?;
        fs::create_dir_all(self.media_dir()).await?;
        fs::create_dir_all(self.libs_dir()).await?;
        Ok(())
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.cache.data_dir.join("metadata")
    }

    pub fn media_dir(&self) -> PathBuf {
        self.cache.data_dir.join("media")
    }

    pub fn libs_dir(&self) -> PathBuf {
        self.cache.data_dir.join("libs")
    }

    pub fn service(
        &self,
        user: &str,
        service: ServiceKind,
        account: &str,
    ) -> Option<&ServiceConfig> {
        self.users
            .iter()
            .find(|candidate| candidate.name == user)
            .and_then(|user| {
                user.services
                    .iter()
                    .find(|candidate| candidate.kind == service && candidate.account == account)
            })
    }
}

impl ServiceKind {
    pub fn as_path(self) -> &'static str {
        match self {
            ServiceKind::Soundcloud => "soundcloud",
        }
    }
}

impl FeedKind {
    pub fn as_path(self) -> &'static str {
        match self {
            FeedKind::Profile => "feed.xml",
            FeedKind::Likes => "likes.xml",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FeedKind::Profile => "Profile",
            FeedKind::Likes => "Likes",
        }
    }

    pub fn source_url(self, service: &ServiceConfig) -> String {
        match self {
            FeedKind::Profile => service.profile_url.clone(),
            FeedKind::Likes => format!("https://soundcloud.com/{}/likes", service.account),
        }
    }
}

fn default_feeds() -> Vec<FeedKind> {
    vec![FeedKind::Profile, FeedKind::Likes]
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
        assert_eq!(service.feeds, vec![FeedKind::Profile, FeedKind::Likes]);
    }

    #[test]
    fn parses_yaml_config_with_auth() {
        let yaml = r#"
server:
  bind: "0.0.0.0:9090"
cache:
  data_dir: "/tmp/yt-dlp-feed"
  media_ttl_seconds: 42
auth:
  enabled: true
  username: "derek"
  password: "secret"
users:
  - name: "derek"
    services:
      - kind: "soundcloud"
        account: "dereknet"
        profile_url: "https://soundcloud.com/dereknet"
        feeds:
          - profile
          - likes
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.server.bind, "0.0.0.0:9090");
        assert_eq!(config.cache.media_ttl_seconds, 42);
        assert!(config.auth.enabled);
        assert_eq!(config.auth.username.as_deref(), Some("derek"));
        assert!(config
            .service("derek", ServiceKind::Soundcloud, "dereknet")
            .is_some());
    }
}
