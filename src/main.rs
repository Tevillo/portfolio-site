mod handlers;
mod nether;
mod paths;
mod people;
mod state;
mod thumbs;
mod views;
mod work;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{get, post};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;
use crate::work::{DownloadKind, Scope};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("prebuild") => prebuild_cmd(&args[2..]).await,
        Some("serve") | None => serve().await,
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage:");
            eprintln!("  portfolio-site [serve]               run the web server");
            eprintln!("  portfolio-site prebuild <name>...    build all zip combos for one or more work items");
            eprintln!("  portfolio-site prebuild --all        build all zip combos for every work item");
            std::process::exit(2);
        }
    }
}

/// Resolve photos/ and cache/ relative to the current working directory.
/// Both the web server and the prebuild command share this layout so a
/// running server and a one-shot prebuild use the same on-disk state.
fn resolve_roots() -> Result<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let binding = cwd.parent().context("CANNOT FIND PHOTOS")?;
    let photos_dir = binding.join("photos");
    let cache_dir = cwd.join("cache");

    std::fs::create_dir_all(&photos_dir)
        .with_context(|| format!("creating {}", photos_dir.display()))?;
    std::fs::create_dir_all(cache_dir.join("thumbs"))
        .with_context(|| format!("creating {}", cache_dir.join("thumbs").display()))?;
    std::fs::create_dir_all(cache_dir.join("preview"))
        .with_context(|| format!("creating {}", cache_dir.join("preview").display()))?;
    std::fs::create_dir_all(cache_dir.join("work"))
        .with_context(|| format!("creating {}", cache_dir.join("work").display()))?;

    let photos_root: PathBuf = std::fs::canonicalize(&photos_dir)
        .with_context(|| format!("canonicalizing {}", photos_dir.display()))?;
    let cache_root: PathBuf = std::fs::canonicalize(&cache_dir)
        .with_context(|| format!("canonicalizing {}", cache_dir.display()))?;
    Ok((photos_root, cache_root))
}

async fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cwd = std::env::current_dir()?;
    let binding = cwd.parent().expect("CANNOT FIND PHOTOS");
    let static_dir = cwd.join("static");
    let nether_dir = binding.join("nether");

    let (photos_root, cache_root) = resolve_roots()?;

    let db_candidate = photos_root.join("digikam4.db");
    let db_path: Option<PathBuf> = if db_candidate.is_file() {
        Some(db_candidate)
    } else {
        None
    };

    // The Obsidian vault is optional; fall back to the uncanonicalized path so
    // the handlers simply 404 if it is absent rather than failing startup.
    let nether_root: PathBuf = std::fs::canonicalize(&nether_dir).unwrap_or(nether_dir);

    info!(
        photos = %photos_root.display(),
        cache = %cache_root.display(),
        db = ?db_path.as_ref().map(|p| p.display().to_string()),
        nether = %nether_root.display(),
        "roots",
    );

    let state = AppState::new(photos_root, cache_root, db_path, nether_root);

    let app = Router::new()
        .route("/", get(handlers::index))
        .route("/browse", get(handlers::browse_root))
        .route("/browse/", get(handlers::browse_root))
        .route("/browse/*path", get(handlers::browse))
        .route("/all", get(handlers::all_photos))
        .route("/people", get(handlers::people_index))
        .route("/people/", get(handlers::people_index))
        .route("/people/:name", get(handlers::person_photos))
        .route("/work", get(handlers::work_index))
        .route("/work/", get(handlers::work_index))
        .route("/work/:name", get(handlers::work_detail))
        .route("/work/:name/auth", post(handlers::work_auth))
        .route("/work/:name/download", post(handlers::work_download))
        .route(
            "/work/:name/file/*filename",
            post(handlers::work_file_download),
        )
        .route("/image/*path", get(handlers::image))
        .route("/thumb/*path", get(handlers::thumb))
        .route("/preview/*path", get(handlers::preview))
        .route("/nether", get(nether::root))
        .route("/nether/", get(nether::root))
        .route("/nether/graph", get(nether::graph))
        .route("/nether/*path", get(nether::note))
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `portfolio-site prebuild <name>... | --all`
///
/// Walks every (scope, kind) combination for the named work item(s) and
/// drives `work::build_or_get_zip` to either build the cached archive or
/// confirm it's already fresh. Safe to run concurrently with a live server
/// — `write_zip` writes to a temp file then renames, so the server only
/// ever sees a complete archive.
async fn prebuild_cmd(args: &[String]) -> Result<()> {
    let (photos_root, cache_root) = resolve_roots()?;
    let all_mode = args.iter().any(|a| a == "--all");
    let names: Vec<String> = if all_mode {
        let summaries = work::list_work(photos_root.clone()).await?;
        if summaries.is_empty() {
            eprintln!("no work items found under {}", photos_root.join("work").display());
            return Ok(());
        }
        summaries.into_iter().map(|s| s.name).collect()
    } else {
        let names: Vec<String> = args
            .iter()
            .filter(|a| !a.starts_with("--"))
            .cloned()
            .collect();
        if names.is_empty() {
            eprintln!("usage: portfolio-site prebuild <name>... | --all");
            std::process::exit(2);
        }
        names
    };

    let scopes = [Scope::All, Scope::Edited, Scope::Original];
    // The Both button downloads a single merged archive, so prebuild it
    // alongside the per-kind zips to keep the first click instant.
    let kinds = [DownloadKind::Jpeg, DownloadKind::Raw, DownloadKind::Both];
    let total_combos = scopes.len() * kinds.len();

    for name in &names {
        println!("prebuilding {name}");
        let mut step = 0;
        for &scope in &scopes {
            for &kind in &kinds {
                step += 1;
                let label = format!("{}-{}", scope.slug(), kind.slug());
                print!("  [{step}/{total_combos}] {label:<14} ");
                std::io::stdout().flush().ok();
                let start = std::time::Instant::now();
                let result = work::build_or_get_zip(
                    photos_root.clone(),
                    cache_root.clone(),
                    name.clone(),
                    scope,
                    kind,
                )
                .await;
                match result {
                    Ok(path) => {
                        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        println!(
                            "ok in {:.1}s ({})",
                            start.elapsed().as_secs_f32(),
                            human_bytes(size)
                        );
                    }
                    Err(e) => {
                        // Empty scope/kind combos (e.g. a work item with no
                        // RAW files) come back as this anyhow error; treat
                        // them as a no-op rather than a hard failure so
                        // --all keeps marching.
                        let msg = format!("{e:#}");
                        if msg.contains("no files for scope") {
                            println!("skip (no files)");
                        } else {
                            println!("FAILED: {msg}");
                        }
                    }
                }
            }
        }
        if let Some(zip_dir) = cache_root.join("work").to_str() {
            // Show total disk used by this work item's prebuilt archives.
            if let Ok(total) = du_prefix(Path::new(zip_dir), name) {
                println!("  total cached for {name}: {}", human_bytes(total));
            }
        }
    }
    Ok(())
}

fn du_prefix(dir: &Path, name_prefix: &str) -> Result<u64> {
    let safe = sanitize(name_prefix);
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let s = match file_name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if s.starts_with(&safe) && s.ends_with(".zip") {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", n)
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}
