mod handlers;
mod paths;
mod state;
mod thumbs;
mod views;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cwd = std::env::current_dir()?;
    let binding = cwd.parent().expect("CANNOT FIND PHOTOS");
    let photos_dir = binding.join("photos");
    let cache_dir = cwd.join("cache").join("thumbs");
    let static_dir = cwd.join("static");

    std::fs::create_dir_all(&photos_dir)
        .with_context(|| format!("creating {}", photos_dir.display()))?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating {}", cache_dir.display()))?;

    let photos_root: PathBuf = std::fs::canonicalize(&photos_dir)
        .with_context(|| format!("canonicalizing {}", photos_dir.display()))?;
    let cache_root: PathBuf = std::fs::canonicalize(&cache_dir)
        .with_context(|| format!("canonicalizing {}", cache_dir.display()))?;

    info!(photos = %photos_root.display(), cache = %cache_root.display(), "roots");

    let state = AppState::new(photos_root, cache_root);

    let app = Router::new()
        .route("/", get(handlers::index))
        .route("/browse", get(handlers::browse_root))
        .route("/browse/", get(handlers::browse_root))
        .route("/browse/*path", get(handlers::browse))
        .route("/image/*path", get(handlers::image))
        .route("/thumb/*path", get(handlers::thumb))
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
