use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    photos_root: PathBuf,
    cache_root: PathBuf,
}

impl AppState {
    pub fn new(photos_root: PathBuf, cache_root: PathBuf) -> Self {
        Self(Arc::new(Inner {
            photos_root,
            cache_root,
        }))
    }

    pub fn photos_root(&self) -> &PathBuf {
        &self.0.photos_root
    }

    pub fn cache_root(&self) -> &PathBuf {
        &self.0.cache_root
    }
}
