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
pub struct UserConfig {
    pub name: String,
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceConfig {
    pub kind: ServiceKind,
    pub account: String,
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
    #[serde(alias = "popular-tracks")]
    PopularTracks,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            cache: CacheConfig::default(),
            auth: AuthConfig::default(),
            users: vec![UserConfig {
                name: "derek".to_string(),
                services: vec![
                    ServiceConfig {
                        kind: ServiceKind::Soundcloud,
                        account: "dereknet".to_string(),
                        profile_url: "https://soundcloud.com/dereknet".to_string(),
                        feeds: default_soundcloud_feeds(),
                    },
                    ServiceConfig {
                        kind: ServiceKind::Soundcloud,
                        account: "NTS".to_string(),
                        profile_url: "https://soundcloud.com/user-202286394-991268468".to_string(),
                        feeds: vec![
                            SoundCloudFeedKind::Profile,
                            SoundCloudFeedKind::PopularTracks,
                        ],
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
            media_ttl_seconds: 86_400,
            disconnect_behavior: DisconnectBehavior::DelayCancel,
            disconnect_grace_seconds: 15,
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

impl SoundCloudFeedKind {
    pub fn as_path(self) -> &'static str {
        match self {
            SoundCloudFeedKind::Profile => "feed.xml",
            SoundCloudFeedKind::Likes => "likes.xml",
            SoundCloudFeedKind::PopularTracks => "popular-tracks.xml",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SoundCloudFeedKind::Profile => "Profile",
            SoundCloudFeedKind::Likes => "Likes",
            SoundCloudFeedKind::PopularTracks => "Popular Tracks",
        }
    }

    pub fn source_url(self, service: &ServiceConfig) -> String {
        match self {
            SoundCloudFeedKind::Profile => service.profile_url.clone(),
            SoundCloudFeedKind::Likes => {
                format!("https://soundcloud.com/{}/likes", service.account)
            }
            SoundCloudFeedKind::PopularTracks => format!("{}/popular-tracks", service.profile_url),
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

        let nts = config
            .service("derek", ServiceKind::Soundcloud, "NTS")
            .expect("default NTS soundcloud service");
        assert_eq!(
            nts.profile_url,
            "https://soundcloud.com/user-202286394-991268468"
        );
        assert_eq!(
            nts.feeds,
            vec![
                SoundCloudFeedKind::Profile,
                SoundCloudFeedKind::PopularTracks
            ]
        );
        assert_eq!(
            SoundCloudFeedKind::PopularTracks.source_url(nts),
            "https://soundcloud.com/user-202286394-991268468/popular-tracks"
        );
    }

    #[test]
    fn parses_yaml_config_with_auth_and_disconnect_behavior() {
        let yaml = r#"
server:
  bind: "0.0.0.0:9090"
cache:
  data_dir: "/tmp/yt-dlp-feed"
  media_ttl_seconds: 42
  disconnect_behavior: "cancel"
  disconnect_grace_seconds: 5
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
      - kind: "soundcloud"
        account: "NTS"
        profile_url: "https://soundcloud.com/user-202286394-991268468"
        feeds:
          - profile
          - popular-tracks
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.server.bind, "0.0.0.0:9090");
        assert_eq!(config.cache.media_ttl_seconds, 42);
        assert_eq!(config.cache.disconnect_behavior, DisconnectBehavior::Cancel);
        assert_eq!(config.cache.disconnect_grace_seconds, 5);
        assert!(config.auth.enabled);
        assert_eq!(config.auth.username.as_deref(), Some("derek"));
        assert!(config
            .service("derek", ServiceKind::Soundcloud, "dereknet")
            .is_some());
        assert_eq!(
            config
                .service("derek", ServiceKind::Soundcloud, "NTS")
                .unwrap()
                .feeds,
            vec![
                SoundCloudFeedKind::Profile,
                SoundCloudFeedKind::PopularTracks
            ]
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
}
