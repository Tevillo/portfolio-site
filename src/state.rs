use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    photos_root: PathBuf,
    cache_root: PathBuf,
    db_path: Option<PathBuf>,
}

impl AppState {
    pub fn new(photos_root: PathBuf, cache_root: PathBuf, db_path: Option<PathBuf>) -> Self {
        Self(Arc::new(Inner {
            photos_root,
            cache_root,
            db_path,
        }))
    }

    pub fn photos_root(&self) -> &PathBuf {
        &self.0.photos_root
    }

    pub fn cache_root(&self) -> &PathBuf {
        &self.0.cache_root
    }

    pub fn db_path(&self) -> Option<&PathBuf> {
        self.0.db_path.as_ref()
    }
}
