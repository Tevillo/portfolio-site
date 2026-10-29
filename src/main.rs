mod audit;
mod handlers;
mod nether;
mod notify;
mod paths;
mod people;
mod portfolio;
mod recent;
mod state;
mod thumbs;
mod views;
mod work;

use std::collections::HashSet;
use std::io::Write as _;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::Redirect;
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
        Some("recent") => recent_cmd(&args[2..]).await,
        Some("notify") => notify_cmd(&args[2..]).await,
        Some("audit") => audit_cmd().await,
        Some("serve") | None => serve().await,
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage:");
            eprintln!("  portfolio-site [serve]               run the web server");
            eprintln!("  portfolio-site warm [--prune]        pre-generate every rendition for every photo");
            eprintln!("                                       (--prune also deletes cached renditions with no source)");
            eprintln!("  portfolio-site prebuild <name>...    build all zip combos for one or more work items");
            eprintln!("  portfolio-site prebuild --all        build all zip combos for every work item");
            eprintln!("  portfolio-site recent show           print the folders currently marked recent");
            eprintln!("  portfolio-site recent set <dir>...   replace the recent set (paths relative to photos/)");
            eprintln!("  portfolio-site notify --dry-run      show what would be sent about the recent set");
            eprintln!("  portfolio-site notify                send it, and record what went out");
            eprintln!("  portfolio-site audit                 print subscriber, download and per-job figures");
            std::process::exit(2);
        }
    }
}

/// Resolve photos/, cache/ and data/ relative to the current working directory.
/// Both the web server and the prebuild command share this layout so a
/// running server and a one-shot prebuild use the same on-disk state.
///
/// `cache/` and `data/` are siblings but not peers: everything under `cache/`
/// can be regenerated from the photos (and `warm --prune` is allowed to delete
/// from it), while `data/` holds the subscriber logs and the API credentials,
/// which exist nowhere else. Keeping them apart is what stops a cache clear
/// from taking the mailing list with it.
fn resolve_roots() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let binding = cwd.parent().context("CANNOT FIND PHOTOS")?;
    let photos_dir = binding.join("photos");
    let cache_dir = cwd.join("cache");
    let data_dir = cwd.join("data");

    std::fs::create_dir_all(&photos_dir)
        .with_context(|| format!("creating {}", photos_dir.display()))?;
    std::fs::create_dir_all(cache_dir.join("thumbs"))
        .with_context(|| format!("creating {}", cache_dir.join("thumbs").display()))?;
    std::fs::create_dir_all(cache_dir.join("preview"))
        .with_context(|| format!("creating {}", cache_dir.join("preview").display()))?;
    std::fs::create_dir_all(cache_dir.join("work"))
        .with_context(|| format!("creating {}", cache_dir.join("work").display()))?;
    // Split in two: `logs/` is the append-only state (subscribers, pending
    // confirmations, what has already been announced), `token/` is the API
    // credentials. See `notify::LOGS_DIR` for why they are kept apart.
    //
    // 0700 on all three: subscriber addresses are other people's contact
    // details, and the credentials can send mail as this domain.
    for dir in [
        data_dir.clone(),
        data_dir.join(notify::LOGS_DIR),
        data_dir.join(notify::TOKENS_DIR),
    ] {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 700 {}", dir.display()))?;
        }
    }

    let photos_root: PathBuf = std::fs::canonicalize(&photos_dir)
        .with_context(|| format!("canonicalizing {}", photos_dir.display()))?;
    let cache_root: PathBuf = std::fs::canonicalize(&cache_dir)
        .with_context(|| format!("canonicalizing {}", cache_dir.display()))?;
    let data_root: PathBuf = std::fs::canonicalize(&data_dir)
        .with_context(|| format!("canonicalizing {}", data_dir.display()))?;
    Ok((photos_root, cache_root, data_root))
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

    let (photos_root, cache_root, data_root) = resolve_roots()?;

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

    let state = AppState::new(photos_root, cache_root, data_root, db_path, nether_root);

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

/// 308 rather than 301: it forbids a client from rewriting the method, so a
/// POST to a slashed path stays a POST. None of these routes take a POST today,
/// but a redirect that silently downgrades one is a trap to leave lying around.
/// Search engines treat both as permanent and transfer ranking either way.
async fn permanent(to: &'static str) -> Redirect {
    Redirect::permanent(to)
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
        // One page per `portfolio/*` tag. The section that `/` renders is
        // reached there and nowhere else; its own slug redirects, so no set
        // of photographs is published at two addresses.
        .route("/portfolio", get(handlers::portfolio_root))
        .route("/portfolio/:slug", get(handlers::portfolio_section))
        .route("/about", get(handlers::about))
        .route("/recent", get(handlers::recent_photos))
        .route("/notify", get(handlers::notify_form).post(handlers::notify_subscribe))
        .route("/notify/confirm", get(handlers::notify_confirm))
        .route("/browse", get(handlers::browse_root))
        .route("/browse/*path", get(handlers::browse))
        .route("/all", get(handlers::all_photos))
        .route("/people", get(handlers::people_index))
        .route("/people/:name", get(handlers::person_photos))
        .route("/work", get(handlers::work_index))
        .route("/work/:name", get(handlers::work_detail))
        .route("/work/:name/auth", post(handlers::work_auth))
        .route("/nether", get(nether::root))
        .route("/nether/graph", get(nether::graph))
        .route("/nether/*path", get(nether::note))
        // Trailing-slash spellings used to be second `get` handlers returning
        // the same 200, which is how one page came to have two indexable URLs
        // and earned the "Duplicate without user-selected canonical" report.
        // A permanent redirect collapses them instead: one URL serves the page,
        // the other names it, and any link equity on the slashed form transfers.
        .route("/about/", get(|| permanent("/about")))
        .route("/recent/", get(|| permanent("/recent")))
        .route("/notify/", get(|| permanent("/notify")))
        .route("/portfolio/", get(|| permanent("/")))
        .route("/browse/", get(|| permanent("/browse")))
        .route("/people/", get(|| permanent("/people")))
        .route("/work/", get(|| permanent("/work")))
        .route("/nether/", get(|| permanent("/nether")))
        // On the pages router, and before the layer below, so an unmatched URL
        // gets the same `no-cache` treatment as a real page — and so the assets
        // router, merged in below, contributes no fallback of its own.
        .fallback(handlers::not_found)
        // `private` because /work/:name renders differently pre- vs post-auth
        // off a path-scoped cookie; a shared cache must not reuse the unlocked
        // variant for another visitor. `no-cache` rather than `no-store` so
        // back/forward navigation still restores instantly from bfcache —
        // `no-store` would disable it.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, private"),
        ));

    let mut assets = Router::new()
        // Crawler-facing, and neither is HTML: they set their own hour-long
        // Cache-Control, so they sit with the assets to keep the pages layer's
        // `no-cache` off them.
        .route("/robots.txt", get(handlers::robots_txt))
        .route("/sitemap.xml", get(handlers::sitemap_xml))
        .route("/version", get(handlers::version))
        .route("/work/:name/download", post(handlers::work_download))
        .route(
            "/work/:name/file/*filename",
            post(handlers::work_file_download),
        )
        .route("/image/*path", get(handlers::image))
        // Images embedded in vault notes. Sits with the assets rather than
        // under /nether so it keeps its own ETag/max-age handling.
        .route("/nether-media/*path", get(nether::media))
        .route("/download/*path", get(handlers::download))
        // Keyed by person, not by path: a face is a crop the database chooses,
        // so it cannot ride the `/<rendition>/<photo path>` shape the
        // renditions below share. See `handlers::face`.
        .route("/face/:name", get(handlers::face))
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
    // Derived from `ThumbKind::ALL`, so a new rendition gets its route with
    // nothing to add here.
    for kind in ThumbKind::ALL {
        assets = assets.route(
            &format!("/{}/*path", kind.route()),
            get(move |state, path, headers| handlers::rendition(kind, state, path, headers)),
        );
    }

    pages.merge(assets).with_state(state)
}

/// `portfolio-site warm [--prune]`
///
/// Walks every servable JPEG under the photos root and forces every rendition
/// in [`ThumbKind::ALL`] into the cache, so the first visitor to any folder
/// never pays the on-demand decode/downscale cost. Adding a variant to that
/// constant is all it takes to have this build it too — the counts and the
/// progress line below are derived from it rather than written out, because
/// they were not, and going from two renditions to three made the summary
/// print a number that had underflowed.
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
    let (photos_root, cache_root, _data_root) = resolve_roots()?;

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
    let rendition_names = ThumbKind::ALL.map(ThumbKind::subdir).join(", ");
    println!(
        "found {total} photos; warming {rendition_names} ({concurrency} at a time, \
         {} renditions each)",
        ThumbKind::ALL.len(),
    );

    let jpegs = Arc::new(jpegs);
    let next = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let skipped = Arc::new(AtomicUsize::new(0));
    let start = std::time::Instant::now();

    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let jpegs = jpegs.clone();
        let next = next.clone();
        let built = built.clone();
        let failed = failed.clone();
        let skipped = skipped.clone();
        let photos_root = photos_root.clone();
        let cache_root = cache_root.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= jpegs.len() {
                    break;
                }
                let src = &jpegs[idx];
                // One JPEG-header read, no decode. It exists so `Wide` can be
                // skipped for everything that is not a panorama; see
                // `rendition_wanted`.
                let dims = {
                    let src = src.clone();
                    tokio::task::spawn_blocking(move || thumbs::oriented_dimensions(&src).ok())
                        .await
                        .unwrap_or(None)
                };
                for kind in ThumbKind::ALL {
                    if !rendition_wanted(kind, dims) {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
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
                if done.is_multiple_of(25) || done == jpegs.len() {
                    println!("  [{done}/{}] photos processed", jpegs.len());
                }
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }

    // One attempt per photo per rendition. `saturating_sub` rather than plain
    // arithmetic: these are `usize`, and the hardcoded `total * 2` this replaces
    // went negative the moment a third rendition existed, wrapping the "already
    // fresh" figure to about eighteen quintillion.
    let skipped = skipped.load(Ordering::Relaxed);
    let attempted = (total * ThumbKind::ALL.len()).saturating_sub(skipped);
    let built = built.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    println!(
        "done in {:.1}s — {built} renditions generated, {} already fresh, {failed} failed, \
         {skipped} not wanted",
        start.elapsed().as_secs_f32(),
        attempted.saturating_sub(built).saturating_sub(failed),
    );

    let faces = warm_faces(&photos_root, &cache_root).await;

    if prune {
        // Relative paths the server can serve; any cached rendition outside
        // this set is an orphan. Mirrors the `cache/<kind>/<rel>` layout that
        // `ensure_thumb` writes, so a cache file's rel maps 1:1 to a source rel.
        let servable: HashSet<PathBuf> = jpegs
            .iter()
            .filter_map(|p| p.strip_prefix(&photos_root).ok().map(Path::to_path_buf))
            .collect();
        prune_orphans(&cache_root, &servable).await;
        prune_faces(&cache_root, &faces).await;
    }
    Ok(())
}

/// Build the face crop for every person on /people, and return the cache
/// filenames the current database asks for.
///
/// A separate pass from the rendition warm above, because a face is not one of
/// the renditions: it is keyed by person rather than by path, there is one per
/// person rather than one per photograph, and it exists only when the database
/// is there to say where a face is. A missing or unreadable database is not a
/// failure here — every other command works without one, and the People page
/// itself reports the absence in plain text.
///
/// Sequential, unlike the rendition pass. There are ~30 of them against
/// thousands of photographs, and every one is a full decode of a frame the
/// pass above has already pulled through the page cache.
async fn warm_faces(photos_root: &Path, cache_root: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let db_path = photos_root.join("digikam4.db");
    if !db_path.is_file() {
        return names;
    }
    let people = match people::list_people(db_path).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  faces skipped: {e:#}");
            return names;
        }
    };

    let mut built = 0usize;
    let mut failed = 0usize;
    let mut faceless = 0usize;
    for person in &people {
        let Some(face) = &person.face else {
            faceless += 1;
            continue;
        };
        let rect = (face.rect.x, face.rect.y, face.rect.w, face.rect.h);
        let cache_name = thumbs::face_cache_name(&person.name, &face.rel, rect);
        let source = photos_root.join(&face.rel);
        match thumbs::ensure_face(&source, cache_root, rect, &cache_name).await {
            Ok(info) => {
                if info.rebuilt {
                    built += 1;
                }
                names.insert(cache_name);
            }
            Err(e) => {
                failed += 1;
                eprintln!("  FAILED face for {}: {e:#}", person.name);
                eprintln!("    from {}", face.rel);
            }
        }
    }
    println!(
        "faces: {built} built, {} already fresh, {failed} failed, {faceless} with no tagged face",
        people.len().saturating_sub(built + failed + faceless),
    );
    // Said plainly, because every failure here is the same fixable mistake and
    // the tile it belongs to is a broken image until it is fixed.
    if failed > 0 {
        eprintln!(
            "  ^ {failed} of these are set on a file the site cannot render. \
             In digiKam, set that person's tag thumbnail on the JPEG."
        );
    }
    names
}

/// Delete cached face crops the database no longer asks for.
///
/// Two ways one goes stale, and neither is a deleted photograph: a face
/// re-tagged onto a different frame mints a different filename and abandons the
/// old one, and a person removed from the archive stops asking for theirs at
/// all. So this prunes against the *current pick set* rather than against the
/// servable photographs `prune_orphans` uses — a crop whose source is still on
/// disk can be an orphan here.
async fn prune_faces(cache_root: &Path, wanted: &HashSet<String>) {
    let base = cache_root.join(thumbs::FACE_SUBDIR);
    let mut read = match tokio::fs::read_dir(&base).await {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut removed = 0usize;
    let mut freed = 0u64;
    while let Ok(Some(entry)) = read.next_entry().await {
        if !entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // A concurrent render's temp file, same exemption `prune_orphans` makes.
        if name.starts_with('.') && name.ends_with(".tmp") {
            continue;
        }
        if wanted.contains(name.as_ref()) {
            continue;
        }
        let sz = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        match tokio::fs::remove_file(entry.path()).await {
            Ok(_) => {
                removed += 1;
                freed += sz;
            }
            Err(e) => eprintln!("  prune failed {}: {e}", entry.path().display()),
        }
    }
    println!(
        "pruned {removed} orphan face crops ({} freed)",
        human_bytes(freed)
    );
}

/// Whether `kind` is worth building for a photograph of these dimensions.
///
/// Only [`ThumbKind::Wide`] ever says no. It exists for the portfolio's
/// full-width panoramas, which are a handful of frames in this archive, and it
/// is the largest rendition there is — building it for everything would roughly
/// double the cache to serve a file no page would link. The threshold is the
/// layout's own (`views::is_wide_ratio`), so the warm pass and the page cannot
/// disagree about what a panorama is.
///
/// Unmeasurable dimensions count as not-wide. `ensure_thumb` still builds the
/// rendition on demand if a page links it anyway, so the only cost of guessing
/// wrong here is the first visitor waiting for one downscale.
fn rendition_wanted(kind: ThumbKind, dims: Option<(u32, u32)>) -> bool {
    !matches!(kind, ThumbKind::Wide) || dims.is_some_and(views::is_wide_ratio)
}

/// Remove every file under every `cache/<rendition>/` directory whose
/// root-relative path is not in `servable`. In-progress `.<name>.tmp` files
/// (written by a concurrent render) are left alone. Reports how much was freed.
async fn prune_orphans(cache_root: &Path, servable: &HashSet<PathBuf>) {
    let mut removed = 0usize;
    let mut freed = 0u64;
    for subdir in ThumbKind::ALL.map(ThumbKind::subdir) {
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

/// Every servable JPEG under `root`, by the same visibility rules the request
/// handlers use — see [`paths::walk_visible_photos`]. Unreadable directories
/// go to stderr, since the CLI commands that call this run without tracing.
pub(crate) async fn collect_jpegs(root: &Path) -> Vec<PathBuf> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        paths::walk_visible_photos(
            &root,
            |photo| {
                out.push(photo);
                ControlFlow::Continue(())
            },
            |dir, e| eprintln!("  read_dir {} failed: {e}", dir.display()),
        );
        out
    })
    .await
    .unwrap_or_default()
}

/// `portfolio-site notify [--dry-run]`
///
/// Tells every confirmed subscriber about the folders in the recent set that
/// they have not already heard about. Run after `update_db.sh`, once the new
/// photos are actually live.
///
/// **Always dry-run first.** The recent set is whatever `.recent` currently
/// says, and `notified.log` is the only thing standing between a mistake there
/// and a mail to every subscriber about the entire back catalogue.
///
/// One recipient's failure is logged and skipped rather than aborting: a
/// Discord user who has closed their DMs should not stop the emails going out.
/// Nothing is recorded for a recipient whose send failed, so a re-run picks up
/// exactly what did not arrive.
async fn notify_cmd(args: &[String]) -> Result<()> {
    let dry_run = args.iter().any(|a| a == "--dry-run" || a == "-n");
    let (photos_root, _cache_root, data_root) = resolve_roots()?;
    let db_candidate = photos_root.join("digikam4.db");
    let db_path = db_candidate.is_file().then_some(db_candidate);

    let digests = notify::plan(&photos_root, db_path.as_ref(), &data_root).await?;
    if digests.is_empty() {
        println!("nothing to send: no subscriber has unseen photos in the recent set");
        return Ok(());
    }

    if dry_run {
        println!("dry run — {} message(s) would be sent\n", digests.len());
        for digest in &digests {
            let (subject, body) = notify::compose(digest);
            println!("to: {} ({})", digest.handle, digest.channel.as_str());
            println!("subject: {subject}");
            for line in body.lines() {
                println!("  {line}");
            }
            println!();
        }
        println!("re-run without --dry-run to send");
        return Ok(());
    }

    let sender = notify::Sender::load(&data_root).await?;
    let mut sent = 0usize;
    let mut failed = 0usize;
    for digest in &digests {
        let (subject, body) = notify::compose(digest);
        match sender
            .send(digest.channel, &digest.handle, &subject, &body)
            .await
        {
            Ok(()) => {
                // Recorded only now, so a failure above leaves the folder
                // unannounced and the next run retries it.
                notify::record_sent(&data_root, digest).await?;
                sent += 1;
                println!("sent to {} ({})", digest.handle, digest.channel.as_str());
            }
            Err(e) => {
                failed += 1;
                eprintln!(
                    "FAILED {} ({}): {e:#}",
                    digest.handle,
                    digest.channel.as_str()
                );
            }
        }
    }
    println!("{sent} sent, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// `portfolio-site audit`
///
/// Prints what the logs know: who is subscribed, what has been downloaded, and
/// per work item whether the client collected their photos. Read-only — it
/// opens nothing but `data/logs/` and the photo tree, so it is safe to run
/// against the live server.
///
/// A command rather than a web route on purpose. The figures include
/// subscriber handles and client delivery activity, and a page showing them
/// would need a password to hold, a session to get wrong, and a URL that
/// exists whether or not anyone is looking at it.
async fn audit_cmd() -> Result<()> {
    let (photos_root, _cache_root, data_root) = resolve_roots()?;
    audit::report(&photos_root, &data_root).await
}

/// `portfolio-site recent show | set <dir>...`
///
/// Maintains `photos/.recent`, the declaration of which folders make up the
/// current drop. `set` replaces the whole set rather than appending, which is
/// what "remove the last set and include the new ones" means in practice.
///
/// Validation is strict here on purpose: `recent::load` skips a bad entry so a
/// renamed folder cannot take the page down, which means a typo would otherwise
/// surface as a roll quietly missing from `/recent`. Catching it at the moment
/// the set is written is the only place the mistake is cheap.
async fn recent_cmd(args: &[String]) -> Result<()> {
    let (photos_root, _cache_root, _data_root) = resolve_roots()?;
    match args.first().map(String::as_str) {
        Some("show") | None => {
            let folders = recent::load(&photos_root).await;
            if folders.is_empty() {
                println!(
                    "no recent folders set ({} is missing or empty)",
                    recent::file_path(&photos_root).display()
                );
            } else {
                for folder in &folders {
                    println!("{folder}");
                }
            }
            Ok(())
        }
        Some("set") => {
            let folders: Vec<String> = args[1..]
                .iter()
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .collect();
            if folders.is_empty() {
                eprintln!("usage: portfolio-site recent set <dir>...");
                eprintln!("  (to clear the set, delete {})", recent::file_path(&photos_root).display());
                std::process::exit(2);
            }
            let validated = recent::validate(&photos_root, &folders).await?;
            recent::write(&photos_root, &validated).await?;
            println!("wrote {}", recent::file_path(&photos_root).display());
            for folder in &validated {
                println!("  {folder}");
            }
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown recent subcommand: {other}");
            eprintln!("usage: portfolio-site recent show | set <dir>...");
            std::process::exit(2);
        }
    }
}

/// `portfolio-site prebuild <name>... | --all`
///
/// Walks every (scope, kind) combination for the named work item(s) and
/// drives `work::build_or_get_zip` to either build the cached archive or
/// confirm it's already fresh. Safe to run concurrently with a live server
/// — `write_zip` writes to a temp file then renames, so the server only
/// ever sees a complete archive.
async fn prebuild_cmd(args: &[String]) -> Result<()> {
    let (photos_root, cache_root, _data_root) = resolve_roots()?;
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
        // Show total disk used by this work item's prebuilt archives. Summed
        // by exact filename rather than by prefix, which would also count
        // `smith-jones`'s archives towards `smith`.
        let zip_dir = cache_root.join("work");
        let total: u64 = scopes
            .iter()
            .flat_map(|&scope| kinds.iter().map(move |&kind| (scope, kind)))
            .filter_map(|(scope, kind)| {
                std::fs::metadata(zip_dir.join(work::zip_file_name(name, scope, kind))).ok()
            })
            .map(|m| m.len())
            .sum();
        println!("  total cached for {name}: {}", human_bytes(total));
    }
    Ok(())
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
mod tests;
