use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::process::Command;
use tokio::sync::RwLock;

use crate::config::PipToolUpdatesConfig;

pub const YT_DLP_VENV_DIR: &str = "/opt/yt-dlp";

#[derive(Clone)]
pub struct PipToolUpdateGate {
    inner: Arc<RwLock<()>>,
}

impl PipToolUpdateGate {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(())),
        }
    }

    pub async fn read_owned(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.inner.clone().read_owned().await
    }

    async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        self.inner.write().await
    }
}

impl Default for PipToolUpdateGate {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct PipToolUpdater {
    config: PipToolUpdatesConfig,
    gate: PipToolUpdateGate,
}

impl PipToolUpdater {
    pub fn new(config: PipToolUpdatesConfig, gate: PipToolUpdateGate) -> Self {
        Self { config, gate }
    }

    pub fn ytdlp_path() -> PathBuf {
        Path::new(YT_DLP_VENV_DIR).join("bin").join("yt-dlp")
    }

    pub fn python_path() -> PathBuf {
        Path::new(YT_DLP_VENV_DIR).join("bin").join("python")
    }

    pub fn verify_install() -> anyhow::Result<()> {
        let python = Self::python_path();
        let ytdlp = Self::ytdlp_path();
        if !python.is_file() {
            anyhow::bail!("missing {}", python.display());
        }
        if !ytdlp.is_file() {
            anyhow::bail!("missing {}", ytdlp.display());
        }
        Ok(())
    }

    pub async fn run_startup_check(&self) {
        if !self.config.enabled || !self.config.startup_check {
            return;
        }
        if let Err(err) = self.update_once().await {
            tracing::warn!(error = %err, "yt-dlp startup update failed; keeping existing install");
        }
    }

    pub fn start_periodic(self) {
        if !self.config.enabled {
            return;
        }
        let Some(interval) = self.config.interval() else {
            tracing::warn!("yt-dlp periodic updates disabled");
            return;
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(err) = self.update_once().await {
                    tracing::warn!(error = %err, "yt-dlp periodic update failed; keeping existing install");
                }
            }
        });
    }

    pub async fn update_once(&self) -> anyhow::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        let _guard = self.gate.write().await;
        Self::verify_install()?;

        let before = ytdlp_version()
            .await
            .unwrap_or_else(|err| format!("unknown ({err})"));
        tracing::info!(
            version = %before,
            package = %self.config.pip_package,
            "yt-dlp update check starting"
        );

        let status = Command::new(Self::python_path())
            .args([
                "-m",
                "pip",
                "install",
                "--upgrade",
                &self.config.pip_package,
            ])
            .status()
            .await
            .context("failed to run pip install --upgrade yt-dlp")?;

        if !status.success() {
            anyhow::bail!(
                "pip install --upgrade {} exited with {status}",
                self.config.pip_package
            );
        }

        let after = ytdlp_version()
            .await
            .unwrap_or_else(|err| format!("unknown ({err})"));
        tracing::info!(before = %before, after = %after, "yt-dlp update check completed");
        Ok(())
    }
}

pub async fn ytdlp_version() -> anyhow::Result<String> {
    let output = Command::new(PipToolUpdater::ytdlp_path())
        .arg("--version")
        .output()
        .await
        .context("failed to run yt-dlp --version")?;
    if !output.status.success() {
        anyhow::bail!("yt-dlp --version exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn docker_tool_paths_are_hardcoded() {
        assert_eq!(
            PipToolUpdater::python_path(),
            Path::new("/opt/yt-dlp/bin/python")
        );
        assert_eq!(
            PipToolUpdater::ytdlp_path(),
            Path::new("/opt/yt-dlp/bin/yt-dlp")
        );
    }

    #[tokio::test]
    async fn update_gate_blocks_new_readers_while_writer_is_active() {
        let gate = PipToolUpdateGate::new();
        let write_guard = gate.write().await;
        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_for_task = Arc::clone(&acquired);
        let gate_for_task = gate.clone();

        let task = tokio::spawn(async move {
            let _read_guard = gate_for_task.read_owned().await;
            acquired_for_task.store(true, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!acquired.load(Ordering::SeqCst));
        drop(write_guard);
        task.await.unwrap();
        assert!(acquired.load(Ordering::SeqCst));
    }
}
