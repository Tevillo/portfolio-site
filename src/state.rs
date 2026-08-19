use std::path::PathBuf;
use std::sync::Arc;

use crate::notify::RateLimiter;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    photos_root: PathBuf,
    cache_root: PathBuf,
    /// Durable state that is not a cache: the subscriber logs and the notify
    /// credentials. See `resolve_roots` for why it is separate from `cache/`.
    data_root: PathBuf,
    db_path: Option<PathBuf>,
    nether_root: PathBuf,
    /// Throttles `/notify` submissions per client address.
    notify_limiter: RateLimiter,
}

impl AppState {
    pub fn new(
        photos_root: PathBuf,
        cache_root: PathBuf,
        data_root: PathBuf,
        db_path: Option<PathBuf>,
        nether_root: PathBuf,
    ) -> Self {
        Self(Arc::new(Inner {
            photos_root,
            cache_root,
            data_root,
            db_path,
            nether_root,
            notify_limiter: RateLimiter::new(),
        }))
    }

    pub fn photos_root(&self) -> &PathBuf {
        &self.0.photos_root
    }

    pub fn cache_root(&self) -> &PathBuf {
        &self.0.cache_root
    }

    pub fn data_root(&self) -> &PathBuf {
        &self.0.data_root
    }

    pub fn notify_limiter(&self) -> &RateLimiter {
        &self.0.notify_limiter
    }

    pub fn db_path(&self) -> Option<&PathBuf> {
        self.0.db_path.as_ref()
    }

    pub fn nether_root(&self) -> &PathBuf {
        &self.0.nether_root
    }
}
