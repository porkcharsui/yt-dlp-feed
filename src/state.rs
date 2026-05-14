use std::sync::Arc;

use crate::{config::Config, media::DownloadCoordinator, metadata::MetadataCache};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub downloads: Arc<DownloadCoordinator>,
    pub metadata: Arc<MetadataCache>,
}

impl AppState {
    pub fn new(
        config: Config,
        downloads: Arc<DownloadCoordinator>,
        metadata: Arc<MetadataCache>,
    ) -> Self {
        Self {
            config,
            downloads,
            metadata,
        }
    }
}
