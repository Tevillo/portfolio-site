mod handlers;
mod nether;
mod paths;
mod people;
mod portfolio;
mod state;
mod thumbs;
mod views;
mod work;

use std::collections::HashSet;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use axum::Router;
use axum::http::{HeaderValue, header};
use axum::routing::{get, post};
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;
use crate::thumbs::ThumbKind;
use crate::work::{DownloadKind, Scope};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("prebuild") => prebuild_cmd(&args[2..]).await,
        Some("warm") => warm_cmd(&args[2..]).await,
        Some("serve") | None => serve().await,
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage:");
            eprintln!("  portfolio-site [serve]               run the web server");
            eprintln!("  portfolio-site warm [--prune]        pre-generate grid + preview renditions for every photo");
            eprintln!("                                       (--prune also deletes cached renditions with no source)");
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

    let app = build_router(state, static_dir)
        // HTML/CSS/JS go out compressed; the default predicate already skips
        // image/* so JPEG responses aren't recompressed for nothing.
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// The route table, split by cache policy.
///
/// `pages` are the HTML views: they must never be reused without checking with
/// the server, so a deploy is visible on the next navigation.
///
/// `assets` set their own `Cache-Control` (images `max-age=3600` + ETag, static
/// files `immutable`) and must be left alone. The header layer is deliberately
/// *not* applied router-wide: `/image`, `/thumb` and `/preview` answer
/// `If-None-Match` with a 304, and per RFC 9111 §4.3.4 a cache updates its
/// stored entry's headers from that 304. Stamping `no-cache` onto those bare
/// 304s would rewrite the cached image's `max-age=3600`, turning every one of
/// the ~1400 thumbnails on `/all` into a revalidation on every page load.
fn build_router(state: AppState, static_dir: PathBuf) -> Router {
    let pages = Router::new()
        .route("/", get(handlers::index))
        .route("/about", get(handlers::about))
        .route("/about/", get(handlers::about))
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
        .route("/nether", get(nether::root))
        .route("/nether/", get(nether::root))
        .route("/nether/graph", get(nether::graph))
        .route("/nether/*path", get(nether::note))
        // `private` because /work/:name renders differently pre- vs post-auth
        // off a path-scoped cookie; a shared cache must not reuse the unlocked
        // variant for another visitor. `no-cache` rather than `no-store` so
        // back/forward navigation still restores instantly from bfcache —
        // `no-store` would disable it.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, private"),
        ));

    let assets = Router::new()
        .route("/version", get(handlers::version))
        .route("/work/:name/download", post(handlers::work_download))
        .route(
            "/work/:name/file/*filename",
            post(handlers::work_file_download),
        )
        .route("/image/*path", get(handlers::image))
        .route("/download/*path", get(handlers::download))
        .route("/thumb/*path", get(handlers::thumb))
        .route("/preview/*path", get(handlers::preview))
        .nest_service(
            "/static",
            // Asset URLs carry a `?v=<build id>` stamp (see views::asset), so a
            // given URL's bytes never change and the response can be cached
            // hard instead of revalidated on every page load. A deploy mints a
            // new stamp, so returning visitors pick up new CSS/JS immediately.
            ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=31536000, immutable"),
                ))
                .service(ServeDir::new(static_dir)),
        );

    pages.merge(assets).with_state(state)
}

/// `portfolio-site warm [--prune]`
///
/// Walks every servable JPEG under the photos root and forces both the grid
/// (400px) and preview (1600px) renditions into the cache, so the first
/// visitor to any folder never pays the on-demand decode/downscale cost.
///
/// Already-fresh renditions are detected by `ensure_thumb` (mtime check) and
/// skipped cheaply, so this is safe to re-run after every deploy. Writes are
/// atomic (temp file + rename) so it can run against a live server.
///
/// With `--prune`, after warming it also deletes any cached rendition whose
/// source photo is no longer servable (deleted, renamed, now hidden, or moved
/// into a skipped dir) so the cache doesn't accumulate orphans over time.
async fn warm_cmd(args: &[String]) -> Result<()> {
    let prune = args.iter().any(|a| a == "--prune");
    let (photos_root, cache_root) = resolve_roots()?;

    println!("scanning {} for photos...", photos_root.display());
    let jpegs = collect_jpegs(&photos_root).await;
    let total = jpegs.len();
    if total == 0 {
        println!("no photos found under {}", photos_root.display());
        return Ok(());
    }

    // Renditions are CPU-bound (JPEG decode + downscale + encode), so cap
    // concurrency near the core count rather than flooding the blocking pool.
    let concurrency = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("found {total} photos; warming grid + preview cache ({concurrency} at a time)");

    let jpegs = Arc::new(jpegs);
    let next = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let start = std::time::Instant::now();

    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let jpegs = jpegs.clone();
        let next = next.clone();
        let built = built.clone();
        let failed = failed.clone();
        let photos_root = photos_root.clone();
        let cache_root = cache_root.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= jpegs.len() {
                    break;
                }
                let src = &jpegs[idx];
                for kind in [ThumbKind::Grid, ThumbKind::Preview] {
                    match thumbs::ensure_thumb(src, &photos_root, &cache_root, kind).await {
                        Ok(info) => {
                            if info.rebuilt {
                                built.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            eprintln!("  FAILED {} ({kind:?}): {e:#}", src.display());
                        }
                    }
                }
                let done = idx + 1;
                if done % 25 == 0 || done == jpegs.len() {
                    println!("  [{done}/{}] photos processed", jpegs.len());
                }
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }

    println!(
        "done in {:.1}s — {} renditions generated, {} already fresh, {} failed",
        start.elapsed().as_secs_f32(),
        built.load(Ordering::Relaxed),
        total * 2 - built.load(Ordering::Relaxed) - failed.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
    );

    if prune {
        // Relative paths the server can serve; any cached rendition outside
        // this set is an orphan. Mirrors the `cache/<kind>/<rel>` layout that
        // `ensure_thumb` writes, so a cache file's rel maps 1:1 to a source rel.
        let servable: HashSet<PathBuf> = jpegs
            .iter()
            .filter_map(|p| p.strip_prefix(&photos_root).ok().map(Path::to_path_buf))
            .collect();
        prune_orphans(&cache_root, &servable).await;
    }
    Ok(())
}

/// Remove every file under `cache/thumbs` and `cache/preview` whose
/// root-relative path is not in `servable`. In-progress `.<name>.tmp` files
/// (written by a concurrent render) are left alone. Reports how much was freed.
async fn prune_orphans(cache_root: &Path, servable: &HashSet<PathBuf>) {
    let mut removed = 0usize;
    let mut freed = 0u64;
    for subdir in [ThumbKind::Grid.subdir(), ThumbKind::Preview.subdir()] {
        let base = cache_root.join(subdir);
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let mut read = match tokio::fs::read_dir(&dir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(entry)) = read.next_entry().await {
                let ftype = match entry.file_type().await {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let path = entry.path();
                if ftype.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !ftype.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') && name.ends_with(".tmp") {
                    continue;
                }
                let rel = match path.strip_prefix(&base) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if !servable.contains(&rel) {
                    let sz = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                    match tokio::fs::remove_file(&path).await {
                        Ok(_) => {
                            removed += 1;
                            freed += sz;
                        }
                        Err(e) => eprintln!("  prune failed {}: {e}", path.display()),
                    }
                }
            }
        }
    }
    println!(
        "pruned {removed} orphan renditions ({} freed)",
        human_bytes(freed)
    );
}

/// Depth-first walk of `root` collecting every servable JPEG, applying the
/// same visibility rules the request handlers use: skip dotfiles, skip files
/// whose name marks them hidden, and skip `negative` directories.
async fn collect_jpegs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut read = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  read_dir {} failed: {e}", dir.display());
                continue;
            }
        };
        while let Ok(Some(entry)) = read.next_entry().await {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            let ftype = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ftype.is_dir() {
                if !handlers::is_skipped_dir(&name) {
                    stack.push(entry.path());
                }
            } else if ftype.is_file() && handlers::is_jpeg(&name) && !handlers::is_hidden(&name) {
                out.push(entry.path());
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    //! Cache-policy tests.
    //!
    //! The split in `build_router` is exactly the kind of thing that regresses
    //! silently: moving the `Cache-Control` layer up one level still compiles,
    //! still serves every page correctly, and quietly destroys image caching
    //! for every visitor. These assertions pin the four cases that matter.

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    /// Minimal photos/cache/static tree with one real JPEG in it.
    struct Fixture {
        _dir: tempfile::TempDir,
        router: Router,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let photos = root.join("photos");
        let cache = root.join("cache");
        let static_dir = root.join("static");
        std::fs::create_dir_all(photos.join("portfolio")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&static_dir).unwrap();
        std::fs::write(static_dir.join("style.css"), b"body{}").unwrap();

        // A real JPEG, so the thumbnail pipeline actually runs.
        image::RgbImage::from_pixel(16, 16, image::Rgb([120, 140, 100]))
            .save(photos.join("portfolio/test.jpg"))
            .expect("write test jpeg");

        let state = AppState::new(photos, cache, None, root.join("nether"));
        Fixture {
            router: build_router(state, static_dir),
            _dir: dir,
        }
    }

    async fn get(router: &Router, uri: &str) -> axum::response::Response {
        router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn cache_control(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn html_pages_are_revalidated() {
        let f = fixture();
        for uri in ["/", "/about", "/browse", "/all", "/work"] {
            let cc = cache_control(&get(&f.router, uri).await);
            assert!(cc.contains("no-cache"), "{uri} cache-control was {cc:?}");
            assert!(cc.contains("private"), "{uri} cache-control was {cc:?}");
        }
    }

    #[tokio::test]
    async fn static_assets_stay_immutable() {
        let f = fixture();
        let cc = cache_control(&get(&f.router, "/static/style.css").await);
        assert!(cc.contains("immutable"), "was {cc:?}");
        // The page layer must not reach the static service.
        assert!(!cc.contains("no-cache"), "was {cc:?}");
    }

    #[tokio::test]
    async fn thumb_200_is_cacheable() {
        let f = fixture();
        let resp = get(&f.router, "/thumb/portfolio/test.jpg").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(cache_control(&resp).contains("max-age=3600"));
        assert!(resp.headers().contains_key(header::ETAG));
    }

    /// The regression this whole split exists to prevent. A cache updates its
    /// stored entry's headers from a 304 (RFC 9111 §4.3.4), so a `no-cache`
    /// leaking onto this response would rewrite the cached image's `max-age`
    /// and force every thumbnail to revalidate on every page load.
    #[tokio::test]
    async fn thumb_304_preserves_cache_control() {
        let f = fixture();
        let first = get(&f.router, "/thumb/portfolio/test.jpg").await;
        let etag = first.headers()[header::ETAG].clone();

        let resp = f
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/thumb/portfolio/test.jpg")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        let cc = cache_control(&resp);
        assert!(cc.contains("max-age=3600"), "304 cache-control was {cc:?}");
        assert!(!cc.contains("no-cache"), "304 cache-control was {cc:?}");
        assert_eq!(resp.headers().get(header::ETAG), Some(&etag));
    }

    #[tokio::test]
    async fn version_is_never_cached() {
        let f = fixture();
        let resp = get(&f.router, "/version").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(cache_control(&resp).contains("no-store"));

        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        let id = String::from_utf8(body.to_vec()).unwrap();
        // version.js only accepts lowercase hex; anything else is treated as a
        // proxy error page and ignored.
        assert!(
            !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "build id {id:?} would not match version.js's regex"
        );
    }

    /// The page's own meta tag and the endpoint must agree, or every client
    /// would reload on first focus.
    #[tokio::test]
    async fn page_meta_matches_version_endpoint() {
        let f = fixture();
        let resp = get(&f.router, "/version").await;
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        let id = String::from_utf8(body.to_vec()).unwrap();

        let page = get(&f.router, "/").await;
        let html = axum::body::to_bytes(page.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&html);
        assert!(
            html.contains(&format!(r#"<meta name="build-version" content="{id}">"#)),
            "page meta did not carry build id {id:?}"
        );
    }
}
