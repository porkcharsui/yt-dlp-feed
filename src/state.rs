use std::sync::Arc;

use crate::{config::Config, media::DownloadCoordinator};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub downloads: Arc<DownloadCoordinator>,
}

impl AppState {
    pub fn new(config: Config, downloads: Arc<DownloadCoordinator>) -> Self {
        Self { config, downloads }
    }
}
