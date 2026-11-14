use std::path::PathBuf;
use std::sync::Arc;

use crate::notify::RateLimiter;
use crate::tags::{self, TagIndex};

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
    /// The `Archive` tag tree, reloaded when the database changes.
    tags: tags::Cache,
    /// Throttles `/archive?k=` attempts per client address.
    archive_limiter: RateLimiter,
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
            tags: tags::Cache::default(),
            archive_limiter: RateLimiter::new(),
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

    pub fn archive_limiter(&self) -> &RateLimiter {
        &self.0.archive_limiter
    }

    pub fn db_path(&self) -> Option<&PathBuf> {
        self.0.db_path.as_ref()
    }

    /// What the public site withholds and what the archive shows. See
    /// `tags.rs`. Empty when there is no database.
    pub async fn tag_index(&self) -> Arc<TagIndex> {
        self.0.tags.get(self.0.db_path.as_ref()).await
    }

    pub fn nether_root(&self) -> &PathBuf {
        &self.0.nether_root
    }
}
