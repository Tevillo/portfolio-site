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

    let assets = Router::new()
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
        .route("/thumb/*path", get(handlers::thumb))
        .route("/medium/*path", get(handlers::medium))
        .route("/preview/*path", get(handlers::preview))
        .route("/wide/*path", get(handlers::wide))
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
                if done % 25 == 0 || done == jpegs.len() {
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

/// Depth-first walk of `root` collecting every servable JPEG, applying the
/// same visibility rules the request handlers use: skip dotfiles, skip files
/// whose name marks them hidden, and skip `negative` directories.
pub(crate) async fn collect_jpegs(root: &Path) -> Vec<PathBuf> {
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
    use std::time::{Duration, UNIX_EPOCH};
    use tower::ServiceExt;

    /// Minimal photos/cache/static tree with one real JPEG in it.
    struct Fixture {
        _dir: tempfile::TempDir,
        router: Router,
        /// Kept so the download-log tests can read what the handlers wrote.
        data: PathBuf,
        /// Kept so a test can add a file to the tree mid-run — `title.txt` is
        /// read per request, so writing one between two `get`s is how the
        /// fallback and the override are compared on one fixture.
        photos: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let photos = root.join("photos");
        let cache = root.join("cache");
        let static_dir = root.join("static");
        std::fs::create_dir_all(photos.join("portfolio")).unwrap();
        std::fs::create_dir_all(photos.join("2024")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&static_dir).unwrap();
        std::fs::write(static_dir.join("style.css"), b"body{}").unwrap();

        // Real JPEGs, so the thumbnail pipeline actually runs.
        //
        // Two top-level folders rather than one on purpose: `render_dir`
        // collapses a folder holding no images and exactly one subfolder by
        // redirecting into it, so a single-folder tree makes `/browse` a 303 and
        // every assertion about the listing it renders untestable.
        std::fs::create_dir_all(photos.join("2024/favs")).unwrap();
        for rel in ["portfolio/test.jpg", "2024/roll.jpg"] {
            image::RgbImage::from_pixel(16, 16, image::Rgb([120, 140, 100]))
                .save(photos.join(rel))
                .expect("write test jpeg");
        }
        // One source larger than every rendition cap, so the ladder is actually
        // exercised: at 16px the Grid and Medium renditions are the same bytes
        // (`scale_to` never enlarges) and no `srcset` is emitted at all. Kept to
        // one file because the others only ever need to decode.
        image::RgbImage::from_pixel(900, 600, image::Rgb([120, 140, 100]))
            .save(photos.join("2024/favs/pick.jpg"))
            .expect("write large test jpeg");

        // One password-protected work item, so the tests can exercise the
        // auth path and the two work download routes.
        std::fs::create_dir_all(photos.join("work/smith/edited")).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([90, 90, 90]))
            .save(photos.join("work/smith/edited/A.jpg"))
            .expect("write work jpeg");
        std::fs::write(photos.join("work/smith/.password"), b"hunter2").unwrap();

        // Declares `2024` as the current drop and leaves `portfolio` out, so
        // `/recent` renders a strict subset and the tests can tell the two
        // apart. The trailing junk lines pin the parser's tolerance.
        std::fs::write(
            photos.join(recent::FILE_NAME),
            b"# the current drop\n2024\n\n../escape\n",
        )
        .unwrap();

        let state = AppState::new(photos.clone(), cache, data.clone(), None, root.join("nether"));
        Fixture {
            router: build_router(state, static_dir),
            _dir: dir,
            data,
            photos,
        }
    }

    /// Every line the download log holds, or an empty vec if it was never
    /// written. Reads the file rather than the parsed rows so a test can assert
    /// on what is *not* in a line as well as what is.
    fn download_log(f: &Fixture) -> Vec<String> {
        let path = f.data.join(notify::LOGS_DIR).join(audit::DOWNLOADS_LOG);
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    async fn get(router: &Router, uri: &str) -> axum::response::Response {
        router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn cache_control(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }

    /// A fixture whose photos root carries a `digikam4.db` holding one
    /// `portfolio/<label>` tag per entry in `sections`, with that entry's
    /// photographs — one per [`Photo`], carrying its rating and its shape.
    ///
    /// Separate from [`fixture`] rather than folded into it: adding a tag
    /// database changes what `/`, `/people` and `/people/:name` do, and every
    /// existing test here is written against the no-database state.
    ///
    /// Section `i`'s photograph `j` is `portfolio/sec<i>-<j>.jpg`, which is how a
    /// test says "this page should show that photograph and no other" and how the
    /// ordering tests name an expected sequence. Ratings are digiKam's own
    /// values, so `-1` (never rated) is expressible alongside `0`.
    fn portfolio_fixture_shaped(sections: &[(&str, &[Photo])]) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let photos = root.join("photos");
        let cache = root.join("cache");
        let data = root.join("data");
        let static_dir = root.join("static");
        for p in [&photos, &cache, &data, &static_dir] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::create_dir_all(photos.join("portfolio")).unwrap();
        std::fs::write(static_dir.join("style.css"), b"body{}").unwrap();
        for (i, (_, shots)) in sections.iter().enumerate() {
            for (j, shot) in shots.iter().enumerate() {
                image::RgbImage::from_pixel(shot.w, shot.h, image::Rgb([120, 140, 100]))
                    .save(photos.join(format!("portfolio/sec{i}-{j}.jpg")))
                    .expect("write test jpeg");
            }
        }

        write_portfolio_db(&photos.join("digikam4.db"), sections);

        let state = AppState::new(
            photos.clone(),
            cache,
            data.clone(),
            Some(photos.join("digikam4.db")),
            root.join("nether"),
        );
        Fixture {
            router: build_router(state, static_dir),
            _dir: dir,
            data,
            photos,
        }
    }

    /// One test photograph: its star rating and its pixel dimensions.
    ///
    /// The dimensions matter because the grid promotes a photograph wide enough
    /// to earn the whole page onto its own row, so a test about that has to be
    /// able to make a panorama. 900x600 is the default shape — ratio 1.5, the
    /// widest thing in the real archive that is *not* promoted.
    #[derive(Clone, Copy)]
    struct Photo {
        rating: i64,
        w: u32,
        h: u32,
    }

    impl Photo {
        /// A 900x600 (ratio 1.5) photograph at `rating` stars.
        const fn rated(rating: i64) -> Self {
            Self {
                rating,
                w: 900,
                h: 600,
            }
        }

        /// A photograph of a given aspect ratio, unrated. Height is fixed at 600
        /// so the Preview rendition is always a real downscale.
        fn shaped(ratio: f64) -> Self {
            Self {
                rating: 0,
                w: (600.0 * ratio).round() as u32,
                h: 600,
            }
        }
    }

    /// Ratings only, every photograph 900x600.
    fn portfolio_fixture_rated(sections: &[(&str, &[i64])]) -> Fixture {
        let shots: Vec<(&str, Vec<Photo>)> = sections
            .iter()
            .map(|(l, rs)| (*l, rs.iter().map(|r| Photo::rated(*r)).collect()))
            .collect();
        let refs: Vec<(&str, &[Photo])> = shots.iter().map(|(l, v)| (*l, &v[..])).collect();
        portfolio_fixture_shaped(&refs)
    }

    /// One photograph per label, all unrated — the shape most of these tests
    /// want, where the section set is what matters and the ordering inside a
    /// section does not.
    fn portfolio_fixture_with(labels: &[&str]) -> Fixture {
        let sections: Vec<(&str, &[i64])> = labels.iter().map(|l| (*l, &[0i64][..])).collect();
        portfolio_fixture_rated(&sections)
    }

    /// The default three-section portfolio.
    ///
    /// `Tags.name COLLATE NOCASE ASC` puts these in the order `Dust & Grain`,
    /// `halation`, `sepia`, and none of the three is named in `SECTION_ORDER`,
    /// so the front door is the alphabetical leader `Dust & Grain` — slug
    /// `dust-grain`, which is also the case that proves a slug is not merely a
    /// lowercased label.
    ///
    /// The labels are deliberately tags this archive does not have. These tests
    /// are about the routing rule — leader at `/`, everyone else at their slug —
    /// and naming a real section here would make them fail the next time the
    /// owner reorders `SECTION_ORDER`, which is a display decision and not a
    /// change in behaviour. `portfolio.rs`'s own unit tests cover the ordering
    /// itself, against orders they pass in explicitly.
    fn portfolio_fixture() -> Fixture {
        portfolio_fixture_with(&["sepia", "halation", "Dust & Grain"])
    }

    /// The (only) photograph belonging to `label` in a [`portfolio_fixture_with`]
    /// set.
    fn section_photo(labels: &[&str], label: &str) -> String {
        let i = labels.iter().position(|l| *l == label).expect("unknown label");
        format!("sec{i}-0.jpg")
    }

    /// The five digiKam tables the portfolio query touches, with one tagged
    /// photograph per rating in each section.
    ///
    /// Only the columns the query reads are declared. A real digiKam database has
    /// dozens more; adding them here would be transcription, not coverage.
    ///
    /// `ImageInformation` is joined for `rating` alone, and a photograph rated
    /// `i64::MIN` is given *no* row at all — that is how a test reaches the
    /// missing-row case the query's `COALESCE` exists for.
    fn write_portfolio_db(path: &Path, sections: &[(&str, &[Photo])]) {
        let conn = rusqlite::Connection::open(path).expect("create test db");
        conn.execute_batch(
            "
            CREATE TABLE Tags (id INTEGER PRIMARY KEY, pid INTEGER, name TEXT);
            CREATE TABLE Albums (id INTEGER PRIMARY KEY, relativePath TEXT);
            CREATE TABLE Images (id INTEGER PRIMARY KEY, album INTEGER, name TEXT, status INTEGER);
            CREATE TABLE ImageTags (tagid INTEGER, imageid INTEGER);
            CREATE TABLE ImageInformation (imageid INTEGER PRIMARY KEY, rating INTEGER);

            INSERT INTO Tags (id, pid, name) VALUES (1, 0, 'portfolio');
            INSERT INTO Albums (id, relativePath) VALUES (1, '/portfolio');
            ",
        )
        .expect("create test db schema");
        let mut image = 0i64;
        for (i, (label, shots)) in sections.iter().enumerate() {
            // Tag ids start at 2, since 1 is the root `portfolio` tag.
            let tag = i as i64 + 2;
            conn.execute(
                "INSERT INTO Tags (id, pid, name) VALUES (?1, 1, ?2)",
                rusqlite::params![tag, label],
            )
            .expect("insert tag");
            for (j, shot) in shots.iter().enumerate() {
                image += 1;
                conn.execute(
                    "INSERT INTO Images (id, album, name, status) VALUES (?1, 1, ?2, 1)",
                    rusqlite::params![image, format!("sec{i}-{j}.jpg")],
                )
                .expect("insert image");
                conn.execute(
                    "INSERT INTO ImageTags (tagid, imageid) VALUES (?1, ?2)",
                    rusqlite::params![tag, image],
                )
                .expect("insert image tag");
                if shot.rating != i64::MIN {
                    conn.execute(
                        "INSERT INTO ImageInformation (imageid, rating) VALUES (?1, ?2)",
                        rusqlite::params![image, shot.rating],
                    )
                    .expect("insert image information");
                }
            }
        }
    }

    /// The order photographs *read* in a rendered portfolio grid, by filename.
    ///
    /// Not document order. The grid groups tiles by column, so the markup runs
    /// column-major (1, 4, 7, 2, 5, 8, ...) while the page reads 1, 2, 3 across
    /// the first band. `data-seq` is the reading index each anchor carries for
    /// exactly this reason, and sorting by it is what makes an assertion here
    /// about "higher up the page" mean what it says.
    fn mosaic_order(html: &str) -> Vec<String> {
        let mut out: Vec<(u32, String)> = Vec::new();
        for chunk in html.split("data-seq=\"").skip(1) {
            let (seq, rest) = chunk.split_once('"').expect("unterminated data-seq");
            let name = rest
                .split_once("data-name=\"")
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(n, _)| n.to_string())
                .expect("anchor has data-seq but no data-name");
            out.push((seq.parse().expect("data-seq is not a number"), name));
        }
        out.sort_by_key(|(seq, _)| *seq);
        out.into_iter().map(|(_, name)| name).collect()
    }

    /// Which column each photograph was assigned to, keyed by filename.
    fn tile_columns(html: &str) -> Vec<Vec<String>> {
        let mut cols = Vec::new();
        for col in html.split("<ul class=\"mcol\"").skip(1) {
            let body = col.split("</ul>").next().unwrap_or("");
            cols.push(
                body.split("data-name=\"")
                    .skip(1)
                    .filter_map(|c| c.split_once('"').map(|(n, _)| n.to_string()))
                    .collect(),
            );
        }
        cols
    }

    /// A panorama takes the whole width, on a row of its own.
    ///
    /// Equal-width columns fix the width and let the ratio decide the height,
    /// which is what makes portraits large here and what would squash a 2:1
    /// frame into a strip a third of the page wide. `caldwell-utopia-wide-07` in
    /// the real archive is 2.05 and the reason this exists.
    #[tokio::test]
    async fn a_panorama_is_promoted_to_the_full_width() {
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo::shaped(0.7),  // 0 portrait
                Photo::shaped(2.05), // 1 panorama, like caldwell-utopia-wide-07
                Photo::shaped(1.5),  // 2 landscape
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;

        // The panorama is a full-width row and is in no column.
        assert!(
            html.contains("<ul class=\"mwide\""),
            "no full-width row was emitted"
        );
        assert!(
            html.contains("class=\"mtile mtile-wide\""),
            "the panorama did not get the wide tile class"
        );
        let in_columns: Vec<String> = tile_columns(&html).into_iter().flatten().collect();
        assert!(
            !in_columns.contains(&"sec0-1.jpg".to_string()),
            "the panorama is still sitting in a column"
        );
        // Its neighbours are not promoted.
        assert_eq!(html.matches("<ul class=\"mwide\"").count(), 1);
        for stays in ["sec0-0.jpg", "sec0-2.jpg"] {
            assert!(
                in_columns.contains(&stays.to_string()),
                "{stays} should still be a column tile"
            );
        }
        // The lone portrait that preceded it could not fill a row, so the
        // panorama took priority and leads the page. See
        // `a_panorama_outranks_a_band_too_short_to_fill_a_row`.
        assert_eq!(
            mosaic_order(&html),
            ["sec0-1.jpg", "sec0-0.jpg", "sec0-2.jpg"]
        );
    }

    /// The threshold sits above 16:9, so a cinematic crop stays a column tile
    /// and only a genuine panorama is promoted.
    ///
    /// Measured on the archive the ratios jump from 1.50 straight to 2.05, so
    /// the boundary cases that matter are the ones checked here rather than
    /// anything in that gap.
    #[tokio::test]
    async fn only_genuinely_wide_photographs_are_promoted() {
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo::shaped(1.50), // 3:2 landscape, the archive's widest column tile
                Photo::shaped(1.78), // 16:9
                Photo::shaped(1.90), // exactly at the threshold
                Photo::shaped(2.83), // 6x17
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;
        let in_columns: Vec<String> = tile_columns(&html).into_iter().flatten().collect();

        for stays in ["sec0-0.jpg", "sec0-1.jpg"] {
            assert!(
                in_columns.contains(&stays.to_string()),
                "{stays} was promoted but should not have been"
            );
        }
        // At or above the threshold, so both promoted.
        assert_eq!(html.matches("<ul class=\"mwide\"").count(), 2);
        for promoted in ["sec0-2.jpg", "sec0-3.jpg"] {
            assert!(
                !in_columns.contains(&promoted.to_string()),
                "{promoted} should have been promoted"
            );
        }
    }

    /// A panorama cuts the band it lands in, when that band can fill a row.
    ///
    /// Cutting is what preserves the sequence: everything before the panorama is
    /// one band, everything after is the next. A band is a three-column group of
    /// any length, not a single row, so the four photographs before the panorama
    /// stay one band and its columns simply end at different heights.
    #[tokio::test]
    async fn a_panorama_cuts_the_band_around_it() {
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo::shaped(0.7), // 0 \
                Photo::shaped(0.7), // 1  | band of four: fills a row, so it stands
                Photo::shaped(0.7), // 2  |
                Photo::shaped(0.7), // 3 /
                Photo::shaped(2.2), // 4 -- panorama
                Photo::shaped(0.7), // 5 \
                Photo::shaped(0.7), // 6  | band of three
                Photo::shaped(0.7), // 7 /
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;

        assert_eq!(
            html.matches("<div class=\"mcols\">").count(),
            2,
            "the panorama did not cut the band"
        );
        // Four photographs round-robin into three columns, so the first column
        // takes two; the second band deals one each.
        assert_eq!(
            tile_columns(&html),
            vec![
                vec!["sec0-0.jpg".to_string(), "sec0-3.jpg".to_string()],
                vec!["sec0-1.jpg".to_string()],
                vec!["sec0-2.jpg".to_string()],
                vec!["sec0-5.jpg".to_string()],
                vec!["sec0-6.jpg".to_string()],
                vec!["sec0-7.jpg".to_string()],
            ]
        );
        // The band filled its row, so nothing moved: order is untouched.
        assert_eq!(
            mosaic_order(&html),
            [
                "sec0-0.jpg", "sec0-1.jpg", "sec0-2.jpg", "sec0-3.jpg",
                "sec0-4.jpg", "sec0-5.jpg", "sec0-6.jpg", "sec0-7.jpg",
            ]
        );
    }

    /// A panorama outranks a band too short to fill a row, and moves above it.
    ///
    /// Cutting naively stranded whatever preceded the panorama. In the real
    /// archive a five-star portrait ranks first and the panorama second, so
    /// `/portfolio/portraits` opened on one lone portrait at a third of the width
    /// with two thirds of the row empty, and the panorama underneath it.
    #[tokio::test]
    async fn a_panorama_outranks_a_band_too_short_to_fill_a_row() {
        // The shape of the real portraits section: one portrait, the panorama,
        // then the rest.
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo::shaped(0.78), // 0 alone before the panorama
                Photo::shaped(2.05), // 1 panorama
                Photo::shaped(0.67), // 2
                Photo::shaped(0.78), // 3
                Photo::shaped(0.81), // 4
                Photo::shaped(0.75), // 5
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;

        // One band, not two, and the panorama is above it.
        assert_eq!(
            html.matches("<div class=\"mcols\">").count(),
            1,
            "the lone portrait was left in a band of its own"
        );
        let wide_at = html.find("<ul class=\"mwide\"").expect("no panorama row");
        let band_at = html.find("<div class=\"mcols\">").expect("no band");
        assert!(wide_at < band_at, "the panorama is not above the band");

        // The stranded portrait joined the following band, so no column tile is
        // ever alone: every column holds at least one and the band fills a row.
        let cols = tile_columns(&html);
        assert_eq!(cols.len(), 3, "the band is not three columns wide");
        assert!(
            cols.iter().all(|c| !c.is_empty()),
            "a column came out empty: {cols:?}"
        );
        assert_eq!(
            cols,
            vec![
                vec!["sec0-0.jpg".to_string(), "sec0-4.jpg".to_string()],
                vec!["sec0-2.jpg".to_string(), "sec0-5.jpg".to_string()],
                vec!["sec0-3.jpg".to_string()],
            ]
        );

        // Display order leads with the panorama, and everything else keeps its
        // relative sequence behind it.
        assert_eq!(
            mosaic_order(&html),
            [
                "sec0-1.jpg", // the panorama, moved up
                "sec0-0.jpg", "sec0-2.jpg", "sec0-3.jpg", "sec0-4.jpg", "sec0-5.jpg",
            ]
        );
    }

    /// The reordering is bounded: a panorama can pass fewer than three
    /// photographs and never a full band.
    ///
    /// Otherwise a weakly-rated panorama could climb the page past better work,
    /// which would quietly undo the star ranking the sections are sorted by.
    #[tokio::test]
    async fn a_panorama_never_climbs_past_a_full_band() {
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo::shaped(0.7), // 0 \
                Photo::shaped(0.7), // 1  | exactly enough to fill a row
                Photo::shaped(0.7), // 2 /
                Photo::shaped(2.4), // 3 -- panorama, stays put
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;

        let band_at = html.find("<div class=\"mcols\">").expect("no band");
        let wide_at = html.find("<ul class=\"mwide\"").expect("no panorama row");
        assert!(
            band_at < wide_at,
            "the panorama climbed above a band that already filled a row"
        );
        assert_eq!(
            mosaic_order(&html),
            ["sec0-0.jpg", "sec0-1.jpg", "sec0-2.jpg", "sec0-3.jpg"]
        );
    }

    /// Consecutive panoramas stack, and the photographs they would have stranded
    /// collect into one band below them.
    #[tokio::test]
    async fn back_to_back_panoramas_stack_above_the_band() {
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo::shaped(0.7), // 0 stranded by the first panorama
                Photo::shaped(2.1), // 1 panorama
                Photo::shaped(0.7), // 2 stranded by the second
                Photo::shaped(2.6), // 3 panorama
                Photo::shaped(0.7), // 4
                Photo::shaped(0.7), // 5
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;

        assert_eq!(html.matches("<ul class=\"mwide\"").count(), 2);
        assert_eq!(
            html.matches("<div class=\"mcols\">").count(),
            1,
            "the stranded photographs did not collect into one band"
        );
        assert_eq!(
            mosaic_order(&html),
            [
                "sec0-1.jpg", "sec0-3.jpg", // both panoramas, in their own order
                "sec0-0.jpg", "sec0-2.jpg", "sec0-4.jpg", "sec0-5.jpg",
            ]
        );
    }

    /// A panorama spans the whole page, where the 1600px Preview every other
    /// tile links is smaller than the slot, so it offers the 3200px rendition as
    /// well. The column tiles beside it do not: their slot is a third of the
    /// page and the Preview covers it twice over.
    #[tokio::test]
    async fn a_panorama_offers_the_higher_resolution_rendition() {
        let f = portfolio_fixture_shaped(&[(
            "solo",
            &[
                Photo {
                    rating: 0,
                    w: 4000,
                    h: 1900,
                },
                Photo::rated(0),
                Photo::rated(0),
                Photo::rated(0),
            ],
        )]);
        let html = body_string(get(&f.router, "/").await).await;

        let srcsets: Vec<&str> = html
            .split("srcset=\"")
            .skip(1)
            .map(|s| s.split('"').next().unwrap())
            .collect();
        assert_eq!(
            srcsets.len(),
            1,
            "exactly one tile should carry a srcset: {srcsets:?}"
        );
        let srcset = srcsets[0];
        assert!(
            srcset.contains("/preview/portfolio/sec0-0.jpg 1600w"),
            "small candidate wrong: {srcset}"
        );
        assert!(
            srcset.contains("/wide/portfolio/sec0-0.jpg 3200w"),
            "large candidate wrong: {srcset}"
        );
        assert!(
            html.contains("sizes=\"100vw\""),
            "the full-width tile declared no slot width"
        );
        // The `src` stays the Preview, so the intrinsic size attributes still
        // describe what a browser without srcset support fetches.
        assert!(html.contains("src=\"/preview/portfolio/sec0-0.jpg\" alt=\"sec0-0.jpg\""));
    }

    /// A panorama the Preview already covers gets no second candidate. Both
    /// routes would downscale the same source to the same width, and a srcset
    /// offering one file at two widths tells the browser nothing.
    #[tokio::test]
    async fn a_small_panorama_offers_no_second_candidate() {
        let f = portfolio_fixture_shaped(&[("solo", &[Photo::shaped(2.2)])]);
        let html = body_string(get(&f.router, "/").await).await;

        assert!(html.contains("class=\"mtile mtile-wide\""), "not promoted");
        assert!(
            !html.contains("srcset="),
            "a source smaller than the Preview cap offered a second candidate"
        );
    }

    /// The warm pass builds the biggest rendition only for the photographs the
    /// page would link it for, and never skips any other rendition.
    #[test]
    fn only_panoramas_are_warmed_at_the_wide_size() {
        let panorama = Some((4000, 1900));
        let portrait = Some((2000, 3000));
        for kind in ThumbKind::ALL {
            assert!(
                rendition_wanted(kind, panorama),
                "{kind:?} skipped for a panorama"
            );
        }
        for dims in [portrait, None] {
            assert!(!rendition_wanted(ThumbKind::Wide, dims), "{dims:?} warmed wide");
            for kind in [ThumbKind::Grid, ThumbKind::Medium, ThumbKind::Preview] {
                assert!(rendition_wanted(kind, dims), "{kind:?} skipped for {dims:?}");
            }
        }
    }

    /// The top band of the page shows the three best photographs, not the best    /// The top band of the page shows the three best photographs, not the best
    /// beside two of the weakest.
    ///
    /// This is the whole reason the columns are assigned on the server. CSS
    /// `columns: 3` fills top-to-bottom, so a nine-photo section would put 1,2,3
    /// down the left column and the first thing a visitor sees would be
    /// photographs 1, 4 and 7 — the five-star shot flanked by two of the worst.
    /// Round-robin on the reading index puts 1, 2, 3 across instead.
    #[tokio::test]
    async fn the_top_band_is_the_top_ranked_photographs() {
        // Nine photographs, rated 9 down to 1, so reading order is also rank
        // order and any scrambling is visible.
        let f = portfolio_fixture_rated(&[("solo", &[5, 5, 5, 3, 3, 3, 0, 0, 0])]);
        let html = body_string(get(&f.router, "/").await).await;

        let cols = tile_columns(&html);
        assert_eq!(cols.len(), 3, "expected three columns");
        // The first entry of each column is the top band, left to right.
        let band: Vec<&str> = cols.iter().map(|c| c[0].as_str()).collect();
        assert_eq!(
            band,
            ["sec0-0.jpg", "sec0-1.jpg", "sec0-2.jpg"],
            "the top band is not the first three photographs in reading order"
        );
        // Which is the same as saying every five-star photo is in the top band.
        assert_eq!(
            cols.iter().map(|c| c.len()).collect::<Vec<_>>(),
            [3, 3, 3],
            "photographs were not dealt evenly across the columns"
        );
    }

    /// Document order is column-major, and that is fine as long as everything
    /// that cares about reading order is told what it is.
    ///
    /// A regression here would be invisible on a desktop and wrong on a phone,
    /// where `order: var(--i)` is the only thing restoring the sequence.
    #[tokio::test]
    async fn tiles_carry_a_reading_index_that_disagrees_with_document_order() {
        let f = portfolio_fixture_rated(&[("solo", &[0, 0, 0, 0, 0, 0])]);
        let html = body_string(get(&f.router, "/").await).await;

        // Document order: column 0 (0, 3), column 1 (1, 4), column 2 (2, 5).
        let document: Vec<String> = html
            .split("data-name=\"")
            .skip(1)
            .filter_map(|c| c.split_once('"').map(|(n, _)| n.to_string()))
            .collect();
        assert_eq!(
            document,
            [
                "sec0-0.jpg", "sec0-3.jpg", // column 0
                "sec0-1.jpg", "sec0-4.jpg", // column 1
                "sec0-2.jpg", "sec0-5.jpg", // column 2
            ]
        );

        // Reading order, recovered from `data-seq`, is the original sequence.
        assert_eq!(
            mosaic_order(&html),
            [
                "sec0-0.jpg", "sec0-1.jpg", "sec0-2.jpg",
                "sec0-3.jpg", "sec0-4.jpg", "sec0-5.jpg",
            ]
        );
        assert_ne!(document, mosaic_order(&html), "the test proves nothing");

        // And every tile declares its reading index for the phone layout.
        for i in 0..6 {
            assert!(html.contains(&format!("--i:{i}")), "tile {i} has no --i");
        }
    }

    /// Stars order a section: more stars, higher up the page.
    ///
    /// The fixture rates five photographs 3, 0, 5, -1, 1 in filename order, so
    /// the star order and the filename order share no prefix — a page that
    /// ignored the rating entirely would come out `0,1,2,3,4` and fail here.
    #[tokio::test]
    async fn stars_order_a_section_highest_first() {
        let f = portfolio_fixture_rated(&[("solo", &[3, 0, 5, -1, 1])]);
        let html = body_string(get(&f.router, "/").await).await;
        assert_eq!(
            mosaic_order(&html),
            [
                "sec0-2.jpg", // 5 stars
                "sec0-0.jpg", // 3
                "sec0-4.jpg", // 1
                // 0 stars and never-rated tie, and fall back to filename order.
                "sec0-1.jpg", // 0
                "sec0-3.jpg", // -1
            ]
        );
    }

    /// digiKam's `-1` means "never rated", not "below zero stars", and a photo
    /// can have no `ImageInformation` row at all. All three are the same thing to
    /// a visitor — no stars — so they rank together and sort by filename, rather
    /// than by which of the three digiKam happened to record.
    ///
    /// This is a real state in the archive, not a hypothetical: one section there
    /// holds a mix of `-1` and `0` with no difference in intent behind it.
    #[tokio::test]
    async fn unrated_zero_star_and_missing_rows_rank_together() {
        // Ratings in filename order: no row, 0, -1, no row, 2.
        let f = portfolio_fixture_rated(&[("solo", &[i64::MIN, 0, -1, i64::MIN, 2])]);
        let html = body_string(get(&f.router, "/").await).await;
        assert_eq!(
            mosaic_order(&html),
            [
                "sec0-4.jpg", // the only starred photo leads
                // Everything else ranks 0 and holds filename order.
                "sec0-0.jpg",
                "sec0-1.jpg",
                "sec0-2.jpg",
                "sec0-3.jpg",
            ]
        );
    }

    /// Stars rank photographs *within* a section, never across them.
    ///
    /// The label has to sort first or the section-cutting loop in
    /// `list_sections_blocking` shatters: it starts a new section every time the
    /// label changes, so a five-star photograph sorting ahead of another
    /// section's rows would split that section into fragments. Here the
    /// five-star photo lives in the section that sorts *last*, which is exactly
    /// the case that would break.
    #[tokio::test]
    async fn stars_do_not_reorder_or_split_sections() {
        let f = portfolio_fixture_rated(&[("alpha", &[0, 0]), ("beta", &[5, 0])]);

        // Section order is untouched: `alpha` still leads and is served at `/`.
        let front = body_string(get(&f.router, "/").await).await;
        assert_eq!(mosaic_order(&front), ["sec0-0.jpg", "sec0-1.jpg"]);

        // `beta` is intact — both of its photographs on one page, starred first.
        let beta = body_string(get(&f.router, "/portfolio/beta").await).await;
        assert_eq!(mosaic_order(&beta), ["sec1-0.jpg", "sec1-1.jpg"]);

        // And there are exactly two sections, not three or four.
        let tabs = front.matches("<a href=\"/portfolio/").count() + 1;
        assert_eq!(tabs, 2, "sections were split by the star sort");
    }

    /// The portfolio's pages come from the database and nowhere else.
    ///
    /// There is one route pattern, `/portfolio/:slug`, and the set of slugs it
    /// accepts is resolved per request from the children of the `portfolio` tag.
    /// So tagging a new sub-tag in digiKam publishes a page, a sub-tab and a
    /// sitemap entry with no code change and no restart — and a tag that goes
    /// away takes its page with it.
    ///
    /// Asserted with labels that appear nowhere in `src/`, which is the point: if
    /// any section name were baked into the routing, these would 404.
    #[tokio::test]
    async fn routes_follow_the_database_not_the_code() {
        let labels = ["Dust & Scratches", "kodachrome 64", "Zone V"];
        let f = portfolio_fixture_with(&labels);

        // NOCASE alphabetical: "Dust & Scratches", "kodachrome 64", "Zone V".
        // The first is the front door and is served at "/" alone.
        let front = body_string(get(&f.router, "/").await).await;
        assert!(front.contains(&section_photo(&labels, "Dust & Scratches")));
        assert!(front.contains("href=\"/portfolio/kodachrome-64\""));
        assert!(front.contains("href=\"/portfolio/zone-v\""));

        for (slug, label) in [("kodachrome-64", "kodachrome 64"), ("zone-v", "Zone V")] {
            let uri = format!("/portfolio/{slug}");
            let resp = get(&f.router, &uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri} status");
            let html = body_string(resp).await;
            assert!(
                html.contains(&section_photo(&labels, label)),
                "{uri} is missing its photograph"
            );
        }

        // And the previous section set's slugs are not routes here — there is no
        // accumulated table of names anywhere.
        for gone in ["/portfolio/sepia", "/portfolio/halation", "/portfolio/dust-grain"] {
            assert_eq!(
                get(&f.router, gone).await.status(),
                StatusCode::NOT_FOUND,
                "{gone} resolved against a database that does not contain it"
            );
        }
    }

    /// `/` is the leading section and nothing else: the whole point of one
    /// section per route is that no set of photographs is published twice.
    #[tokio::test]
    async fn front_door_renders_only_the_leading_section() {
        let labels = ["sepia", "halation", "Dust & Grain"];
        let f = portfolio_fixture_with(&labels);
        let resp = get(&f.router, "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        // `Dust & Grain` leads, so its photograph is here and the other two are
        // not — the whole point of one section per route.
        assert!(
            html.contains(&section_photo(&labels, "Dust & Grain")),
            "leading section's photograph is missing"
        );
        for other in ["sepia", "halation"] {
            assert!(
                !html.contains(&section_photo(&labels, other)),
                "{other}'s photograph leaked onto /"
            );
        }
    }

    /// Every non-leading section is its own page, reachable at its slug.
    #[tokio::test]
    async fn each_section_has_its_own_page() {
        let labels = ["sepia", "halation", "Dust & Grain"];
        let f = portfolio_fixture_with(&labels);
        for (slug, label) in [("sepia", "sepia"), ("halation", "halation")] {
            let uri = format!("/portfolio/{slug}");
            let resp = get(&f.router, &uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri} status");
            let html = body_string(resp).await;
            let own = section_photo(&labels, label);
            assert!(html.contains(&own), "{uri} is missing {own}");
            assert!(
                !html.contains(&section_photo(&labels, "Dust & Grain")),
                "{uri} leaked the leading section"
            );
        }
    }

    /// The leading section's own slug is a second address for `/`, which is the
    /// duplicate this layout exists to avoid. It redirects instead.
    #[tokio::test]
    async fn the_leading_sections_slug_redirects_to_the_front_door() {
        let f = portfolio_fixture();
        let resp = get(&f.router, "/portfolio/dust-grain").await;
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
            "/"
        );
    }

    /// `/portfolio` names no section, and is not a page of its own.
    #[tokio::test]
    async fn portfolio_root_and_trailing_slash_redirect() {
        let f = portfolio_fixture();
        for uri in ["/portfolio", "/portfolio/"] {
            let resp = get(&f.router, uri).await;
            assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT, "{uri} status");
            assert_eq!(
                resp.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
                "/",
                "{uri} location"
            );
        }
    }

    /// A slug naming no tag is a miss, and renders the site's 404 rather than a
    /// blank page — same contract as every other page route.
    #[tokio::test]
    async fn an_unknown_section_slug_is_a_404_page() {
        let f = portfolio_fixture();
        let resp = get(&f.router, "/portfolio/no-such-section").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let html = body_string(resp).await;
        assert!(html.contains("notfound-code"), "did not render the 404 page");
    }

    /// The sub-tab strip lists every section, links the leading one at `/`, and
    /// marks the page you are on. It belongs to the portfolio alone — `/recent`
    /// is an archive listing and keeps the header it always had.
    #[tokio::test]
    async fn the_sub_tab_strip_is_portfolio_only() {
        let f = portfolio_fixture();
        let front = body_string(get(&f.router, "/").await).await;
        assert!(front.contains("class=\"subnav\""), "/ has no sub-tab strip");
        assert!(front.contains("href=\"/portfolio/sepia\""), "/ is missing a tab");
        assert!(front.contains("href=\"/portfolio/halation\""), "/ is missing a tab");
        // The leading section's tab points at `/`, not at its own slug.
        assert!(
            !front.contains("href=\"/portfolio/dust-grain\""),
            "/ links the leading section at its redirecting slug"
        );

        let section = body_string(get(&f.router, "/portfolio/sepia").await).await;
        assert!(section.contains("class=\"subnav\""), "section page has no strip");
        assert!(
            section.contains("href=\"/portfolio/sepia\" aria-current=\"page\""),
            "the active tab is not marked"
        );

        let recent = body_string(get(&f.router, "/recent").await).await;
        assert!(!recent.contains("class=\"subnav\""), "/recent grew a sub-tab strip");
    }

    /// The portfolio dropped the archive's collapsing sections; the archive
    /// pages kept them. Asserted through the script tag, which is the whole
    /// mechanism.
    #[tokio::test]
    async fn only_the_archive_pages_load_the_collapse_script() {
        let f = portfolio_fixture();
        for uri in ["/", "/portfolio/sepia"] {
            let html = body_string(get(&f.router, uri).await).await;
            assert!(!html.contains("collapse.js"), "{uri} still loads collapse.js");
            assert!(html.contains("lightbox.js"), "{uri} lost the lightbox");
        }
        for uri in ["/recent", "/all"] {
            let html = body_string(get(&f.router, uri).await).await;
            assert!(html.contains("collapse.js"), "{uri} lost collapse.js");
        }
    }

    /// Each tile carries the photo's aspect ratio, which is the single input the
    /// justified rows are built from — without it the mosaic collapses.
    #[tokio::test]
    async fn mosaic_tiles_carry_an_aspect_ratio() {
        let f = portfolio_fixture();
        let html = body_string(get(&f.router, "/").await).await;
        // The test JPEGs are 900x600.
        assert!(html.contains("--ar:1.5000"), "no --ar on the tiles");
        // The reading index the phone layout and the lightbox both order by.
        assert!(html.contains("--i:0"), "no --i on the tiles");
    }

    /// The mosaic's tiles are wired to the lightbox.
    ///
    /// `lightbox.js` binds by selector pair, and the portfolio's grid uses its
    /// own classes (`ul.mosaic` / `li.mtile`) rather than the square grid's
    /// `ul.grid` / `li.tile`. When it was added those classes matched no pair, so
    /// every tile was a plain link and a click navigated to the bare JPEG instead
    /// of opening the lightbox — the markup was right and the binding was
    /// missing, which is invisible to any assertion about the HTML alone.
    ///
    /// So this reads the script. It cannot prove the handler works, but it does
    /// catch the two classes drifting apart again, which is the failure that
    /// actually happened.
    #[tokio::test]
    async fn the_mosaic_classes_are_ones_the_lightbox_binds_to() {
        let js = std::fs::read_to_string("static/lightbox.js").expect("read lightbox.js");
        assert!(js.contains("'.mgrid'"), "lightbox.js does not bind .mgrid");
        assert!(js.contains("'li.mtile a'"), "lightbox.js does not bind li.mtile a");
        // The group selector must be the container, not a single column, or
        // prev/next would stop at the bottom of whichever column was clicked.
        for narrower in ["'ul.mcol'", "'.mcols'", "'.mwide'"] {
            assert!(
                !js.contains(narrower),
                "lightbox.js binds {narrower} rather than the whole grid, so \
                 prev/next would stop at a column or a panorama"
            );
        }
        // And it has to walk the reading order the tiles declare.
        assert!(js.contains("dataset.seq"), "lightbox.js ignores data-seq");

        let f = portfolio_fixture();
        let html = body_string(get(&f.router, "/").await).await;
        assert!(html.contains("<div class=\"mgrid\">"), "the grid is not div.mgrid");
        assert!(html.contains("<li class=\"mtile\""), "the tiles are not li.mtile");
        assert!(html.contains("data-seq=\"0\""), "the tiles carry no reading index");
    }

    /// One canonical per address, and the `Person` block on the one page that is
    /// the person's address rather than on all three.
    #[tokio::test]
    async fn section_pages_carry_their_own_canonical_and_no_person_block() {
        let f = portfolio_fixture();
        let front = body_string(get(&f.router, "/").await).await;
        assert!(front.contains("rel=\"canonical\" href=\"https://paulborrego.com/\""));
        assert!(front.contains("application/ld+json"), "/ lost the Person block");

        let section = body_string(get(&f.router, "/portfolio/sepia").await).await;
        assert!(
            section.contains("rel=\"canonical\" href=\"https://paulborrego.com/portfolio/sepia\""),
            "section canonical is wrong"
        );
        assert!(
            !section.contains("application/ld+json"),
            "the Person block is repeated on a section page"
        );
        assert!(
            section.contains("<title>sepia — Paul Borrego</title>"),
            "section title is wrong"
        );
    }

    /// The photographs run straight to the footer: the closing prose block is
    /// gone, and the footer every other page carries is the last thing on the
    /// page here too.
    #[tokio::test]
    async fn the_photographs_run_to_the_footer() {
        let f = portfolio_fixture();
        for uri in ["/", "/portfolio/sepia"] {
            let html = body_string(get(&f.router, uri).await).await;
            let grid = html.find("class=\"mgrid\"").expect("no photo grid");
            assert!(
                !html.contains("portfolio-note"),
                "{uri} still carries the closing prose block"
            );
            let footer = html
                .find("site-footer")
                .unwrap_or_else(|| panic!("{uri} has no footer"));
            assert!(footer > grid, "{uri} put the footer above the photographs");
        }

        for uri in ["/all", "/recent", "/about", "/work"] {
            let html = body_string(get(&f.router, uri).await).await;
            assert!(html.contains("site-footer"), "{uri} lost its footer");
        }
    }

    /// Every non-leading section is crawlable; the leading one is already listed
    /// as `/`, and listing its slug too would advertise a URL that redirects.
    #[tokio::test]
    async fn the_sitemap_lists_sections_but_not_the_redirecting_slug() {
        let f = portfolio_fixture();
        let xml = body_string(get(&f.router, "/sitemap.xml").await).await;
        assert!(xml.contains("<loc>https://paulborrego.com/portfolio/sepia</loc>"));
        assert!(xml.contains("<loc>https://paulborrego.com/portfolio/halation</loc>"));
        assert!(
            !xml.contains("/portfolio/dust-grain"),
            "the sitemap advertises the redirecting slug"
        );
    }

    /// No database, no tags, nothing on disk: the front page says so quietly
    /// rather than reporting an infrastructure problem, and a named section is a
    /// miss because the URL claimed something that is not there.
    #[tokio::test]
    async fn an_empty_portfolio_still_renders_the_front_page() {
        let f = fixture();
        let resp = get(&f.router, "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("Nothing here yet."));
        assert!(html.contains("<header class=\"site\""), "no nav on the empty page");
        assert!(!html.contains("class=\"subnav\""), "an empty portfolio drew tabs");

        let miss = get(&f.router, "/portfolio/sepia").await;
        assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn html_pages_are_revalidated() {
        let f = fixture();
        for uri in ["/", "/about", "/browse", "/recent", "/all", "/work"] {
            let cc = cache_control(&get(&f.router, uri).await);
            assert!(cc.contains("no-cache"), "{uri} cache-control was {cc:?}");
            assert!(cc.contains("private"), "{uri} cache-control was {cc:?}");
        }
    }

    /// A miss on any page route renders the site rather than a blank page, and
    /// keeps saying 404 while it does. Both halves matter: an HTML body with a
    /// 200 on it would tell a crawler the rotted `/browse` URL is still a page.
    #[tokio::test]
    async fn misses_render_the_404_page() {
        let f = fixture();
        for uri in [
            "/no-such-page",
            "/browse/2024/no-such-roll",
            // Not `/people/:name`: with no digiKam database the route reports
            // that state in plain text rather than reporting a missing person,
            // and this fixture has none. The miss branch itself goes through
            // the same `not_found_response`.
            "/work/no-such-job",
            "/nether/no-such-note",
        ] {
            let resp = get(&f.router, uri).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri} status");
            let html = body_string(resp).await;
            assert!(html.contains("notfound-code"), "{uri} did not render the page");
            assert!(html.contains("<header class=\"site\""), "{uri} has no nav back");
        }
    }

    /// The 404 is `noindex` and names no canonical: it exists at no address, so
    /// stating one would file the miss under a URL that does exist.
    #[tokio::test]
    async fn the_404_page_is_not_indexable() {
        let f = fixture();
        let html = body_string(get(&f.router, "/no-such-page").await).await;
        assert!(html.contains("noindex"), "missing robots noindex");
        assert!(!html.contains("rel=\"canonical\""), "404 named a canonical");
        assert!(!html.contains("og:url"), "404 named an og:url");
    }

    /// The fallback sits on the pages router, so an unmatched URL carries the
    /// same `no-cache` as a real page. Off the pages layer it would inherit
    /// nothing and a shared cache could store the miss.
    #[tokio::test]
    async fn the_404_is_revalidated_like_a_page() {
        let f = fixture();
        let cc = cache_control(&get(&f.router, "/no-such-page").await);
        assert!(cc.contains("no-cache"), "was {cc:?}");
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

    /// Every rendition in `ThumbKind::ALL` has a route that serves it. The
    /// warm and prune passes iterate that constant, so a variant added without
    /// a route would be rendered to disk and then be unreachable.
    #[tokio::test]
    async fn every_rendition_has_a_route() {
        let f = fixture();
        for kind in ThumbKind::ALL {
            let uri = format!("/{}/portfolio/test.jpg", kind.route());
            let resp = get(&f.router, &uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri} did not serve");
            assert!(resp.headers().contains_key(header::ETAG), "{uri} has no ETag");
        }
    }

    /// The regression this whole split exists to prevent. A cache updates its
    /// stored entry's headers from a 304 (RFC 9111 §4.3.4), so a `no-cache`
    /// leaking onto this response would rewrite the cached image's `max-age`
    /// and force every thumbnail to revalidate on every page load.
    /// The declared set is the whole of `/recent` — a folder that exists and
    /// holds photos but is not named in `.recent` must not leak onto the page.
    /// This is the assertion that stops `/recent` quietly drifting into a
    /// second `/all`.
    /// `/all` sorts on two different clocks, and this pins both at once.
    ///
    /// Years descend because a year is when the photographs were *taken*. Rolls
    /// inside a year descend on the newest photograph beneath them, because a
    /// roll's name says nothing about time and what matters there is when it was
    /// *published*. The fixture is built so neither rule can pass by accident:
    /// the newest photo on disk by a wide margin sits in the older year (so
    /// recency must not reach the top level), and within 2026 the recency order
    /// is the reverse of the alphabetical one it replaced.
    ///
    /// `alpha` earns its place on a photo in its `favs/` rather than its own, so
    /// a version that read only each folder's direct images would put it last.
    #[tokio::test]
    async fn all_reads_newest_year_first_then_most_recently_published_roll() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let photos = root.join("photos");
        let stamp = |rel: &str, mtime: u64| {
            let abs = photos.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]))
                .save(&abs)
                .expect("write test jpeg");
            std::fs::File::options()
                .write(true)
                .open(&abs)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(mtime))
                .unwrap();
        };
        stamp("2026/alpha/a.jpg", 1_000);
        stamp("2026/alpha/favs/f.jpg", 4_000);
        stamp("2026/beta/b.jpg", 2_000);
        stamp("2026/gamma/g.jpg", 9_000);
        stamp("2025/older/o.jpg", 99_000);

        let state = AppState::new(
            photos,
            root.join("cache"),
            root.join("data"),
            None,
            root.join("nether"),
        );
        let router = build_router(state, root.join("static"));
        let html = body_string(get(&router, "/all").await).await;

        let sections = data_paths(&html, "<section class=\"gallery");
        assert_eq!(
            sections,
            vec![
                "",
                "2026",
                "2026/gamma",
                "2026/alpha",
                "2026/alpha/favs",
                "2026/beta",
                "2025",
                "2025/older",
            ],
            "full page was: {html}"
        );
        // The sidebar is built from the same tree and its rows carry the DOM ids
        // the sections were minted with, so the two orders have to be the one
        // order. Sorting after `finish` would leave this pair disagreeing and
        // every sidebar row scrolling to the wrong section.
        assert_eq!(
            data_paths(&html, "<li class=\"tree-node\""),
            sections,
            "sidebar and sections disagree"
        );
    }

    /// The `data-path` of every element whose opening tag starts with `tag`, in
    /// document order.
    fn data_paths(html: &str, tag: &str) -> Vec<String> {
        html.match_indices(tag)
            .filter_map(|(i, _)| {
                let open = &html[i..i + html[i..].find('>')?];
                let key = "data-path=\"";
                let val = &open[open.find(key)? + key.len()..];
                Some(val[..val.find('"')?].to_string())
            })
            .collect()
    }

    #[tokio::test]
    async fn recent_renders_only_the_declared_folders() {
        let f = fixture();
        let resp = get(&f.router, "/recent").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("/browse/2024"), "declared folder is missing");
        assert!(
            !html.contains("/browse/portfolio"),
            "undeclared folder leaked onto /recent"
        );
    }

    /// `/recent` lays its tiles out five-up at ~210px, so it links the 400px
    /// Grid rendition, not the 1600px Preview one. It used to link Preview,
    /// which cost 19.7 MB a page for tiles the same size as `/all`'s 23 KB ones.
    ///
    /// The intrinsic dimensions are the other half of the same change and are
    /// asserted here too: the tiles carry no CSS `aspect-ratio`, so without
    /// `<img width height>` the grid lays out at zero height, the browser
    /// believes all of it is in the viewport, and `loading="lazy"` fetches every
    /// file up front — undoing the saving this test exists to protect.
    #[tokio::test]
    async fn recent_serves_grid_renditions_with_intrinsic_dimensions() {
        let f = fixture();
        let html = body_string(get(&f.router, "/recent").await).await;
        // Scoped to <img src>, because the page's og:image is a `/preview/`
        // URL on purpose (see `share_cards_use_the_preview_rendition`) and a
        // document-wide search for it would fail on that alone.
        assert!(
            html.contains("<img src=\"/thumb/"),
            "no grid renditions on /recent"
        );
        assert!(
            !html.contains("<img src=\"/preview/"),
            "/recent is back on the 1600px preview rendition"
        );
        assert!(
            html.contains(" width=\"") && html.contains(" height=\""),
            "tiles lost their intrinsic dimensions"
        );
    }

    /// The `srcset` that closes the gap the Grid-only version of `/recent` left
    /// on phones: its single column is the *widest* tile the page ever lays out
    /// (327 CSS px measured, against 252 at a 1920 viewport), so the 400px
    /// rendition that is right for the dense desktop grid is the one case it
    /// cannot cover.
    ///
    /// Both descriptors have to be present and have to differ — a `srcset` whose
    /// candidates claim the same width silently degrades to picking the first.
    #[tokio::test]
    async fn recent_tiles_offer_a_larger_srcset_candidate() {
        let f = fixture();
        let html = body_string(get(&f.router, "/recent").await).await;
        let tile = html
            .split("<img ")
            .find(|t| t.contains("srcset="))
            .expect("no tile carried a srcset");
        let srcset = tile
            .split("srcset=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("srcset was empty");
        assert!(srcset.contains("/thumb/"), "small candidate missing: {srcset}");
        assert!(srcset.contains("/medium/"), "large candidate missing: {srcset}");
        let widths: Vec<u32> = srcset
            .split(',')
            .filter_map(|c| c.trim().rsplit(' ').next())
            .filter_map(|w| w.trim_end_matches('w').parse().ok())
            .collect();
        assert_eq!(widths.len(), 2, "expected two descriptors: {srcset}");
        assert!(
            widths[1] > widths[0],
            "the larger candidate is not larger: {srcset}"
        );
        // Without `sizes` the browser assumes the tile is the full viewport
        // width and takes the biggest candidate every time, on every device.
        assert!(tile.contains("sizes=\""), "srcset without a sizes hint");

        // The other side of the guard: the fixture's 16px photo is smaller than
        // the Grid cap, so both renditions are identical and it must offer no
        // candidates rather than two equal ones.
        let tiny = html
            .split("<img ")
            .find(|t| t.contains("/thumb/2024/roll.jpg"))
            .expect("the small photo is missing from /recent");
        assert!(
            !tiny.contains("srcset="),
            "a source smaller than the grid cap still emitted a srcset"
        );
    }

    /// A share card points at the preview rendition. The original is the wrong
    /// file for the job — the home page's is 3.4 MB, and a scraper that caps
    /// below that renders no card at all.
    #[tokio::test]
    async fn share_cards_use_the_preview_rendition() {
        let f = fixture();
        let mut checked = 0;
        for path in ["/", "/recent", "/browse/2024"] {
            let html = body_string(get(&f.router, path).await).await;
            let Some(rest) = html.split("property=\"og:image\" content=\"").nth(1) else {
                continue;
            };
            let url = rest.split('"').next().unwrap_or_default();
            // The icon is the documented fallback for a page with no photo to
            // show — the fixture's home page has no tagged portfolio, so it
            // takes that branch and there is nothing to assert about it.
            if url.ends_with("/static/icon.png") {
                continue;
            }
            assert!(
                url.contains("/preview/"),
                "{path} og:image is not a preview rendition: {url}"
            );
            assert!(
                !url.contains("/image/"),
                "{path} og:image is still the full-size original: {url}"
            );
            checked += 1;
        }
        assert!(checked > 0, "no page produced a photo-derived og:image");
    }

    /// On `/recent` a roll is one section with its favorites at the front and a
    /// rule between them and the rest — not the two sections the folder tree
    /// would suggest, which is what `/all` still shows.
    #[tokio::test]
    async fn recent_folds_favs_into_the_roll_with_a_divider() {
        let f = fixture();
        let html = body_string(get(&f.router, "/recent").await).await;
        assert_eq!(
            html.matches(r#"data-path="2024""#).count(),
            1,
            "the roll should render as a single section"
        );
        assert!(
            !html.contains(r#"data-path="2024/favs""#),
            "favs should not be a section of its own"
        );
        assert_eq!(
            html.matches("fav-divider").count(),
            1,
            "one rule between the favorites and the rest"
        );
        // The favorite leads, so it appears before the roll's own photo.
        let fav = html.find("favs/pick.jpg").expect("favorite is missing");
        let rule = html.find("fav-divider").unwrap();
        let rest = html.find("2024/roll.jpg").expect("roll photo is missing");
        assert!(fav < rule && rule < rest, "order should be fav, rule, rest");
    }

    /// `../escape` in the fixture's `.recent` is skipped rather than served or
    /// fatal: `recent::load` runs every line through the same containment check
    /// as a URL path, and a bad line drops out of the set on its own.
    #[tokio::test]
    async fn recent_ignores_entries_outside_the_photos_root() {
        let f = fixture();
        let resp = get(&f.router, "/recent").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(!html.contains("escape"), "escaping entry reached the page");
    }

    /// The Browse tab is gone from the nav, but its URLs are indexed, linked
    /// from the `/all` crumbs, and named in every notification message. Dropping
    /// the routes with the tab would 404 all of them.
    #[tokio::test]
    async fn browse_still_serves_after_losing_its_tab() {
        let f = fixture();
        assert_eq!(get(&f.router, "/browse").await.status(), StatusCode::OK);
        assert_eq!(
            get(&f.router, "/browse/2024").await.status(),
            StatusCode::OK
        );
        let nav = body_string(get(&f.router, "/").await).await;
        assert!(
            !nav.contains(">Browse<"),
            "Browse tab is still rendered in the nav"
        );
    }

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

    // -- Canonicalisation ---------------------------------------------------
    //
    // Search Console reported "Duplicate without user-selected canonical": the
    // same page was reachable at more than one URL with nothing declaring which
    // was the original, so it indexed none of them. Two mechanisms fix it and
    // both regress invisibly — a missing tag and a wrong tag look identical
    // from inside the app, and neither breaks a page.

    async fn body_of(router: &Router, uri: &str) -> String {
        let resp = get(router, uri).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Every page must name itself as its own canonical. A tag pointing at the
    /// wrong URL is worse than none: it asks the crawler to drop this page in
    /// favour of another.
    #[tokio::test]
    async fn pages_declare_themselves_canonical() {
        let f = fixture();
        for (uri, path) in [
            ("/", "/"),
            ("/about", "/about"),
            ("/browse", "/browse"),
            ("/all", "/all"),
            ("/work", "/work"),
            // Query strings are not part of the identity of any page here, so
            // the canonical must ignore them rather than mint a URL per link
            // someone shares with a tracking parameter attached.
            ("/about?utm_source=newsletter", "/about"),
        ] {
            let html = body_of(&f.router, uri).await;
            let expected = format!(
                r#"<link rel="canonical" href="{}">"#,
                views::abs_url(path)
            );
            assert!(
                html.contains(&expected),
                "{uri} did not declare {path} canonical"
            );
        }
    }

    /// The trailing-slash spellings used to be duplicate 200s. They must
    /// redirect, not render.
    #[tokio::test]
    async fn trailing_slash_redirects_to_the_canonical_path() {
        let f = fixture();
        for (from, to) in [
            ("/about/", "/about"),
            ("/browse/", "/browse"),
            ("/people/", "/people"),
            ("/work/", "/work"),
            ("/nether/", "/nether"),
        ] {
            let resp = get(&f.router, from).await;
            assert_eq!(
                resp.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{from} should redirect permanently"
            );
            let loc = resp
                .headers()
                .get(header::LOCATION)
                .expect("redirect had no Location")
                .to_str()
                .unwrap();
            assert_eq!(loc, to, "{from} redirected to the wrong place");
        }
    }

    /// A sitemap of relative or wrongly-hosted URLs is rejected wholesale, and
    /// robots.txt is the only place that tells a crawler the sitemap exists.
    #[tokio::test]
    async fn robots_and_sitemap_are_absolute_and_agree() {
        let f = fixture();

        let robots = body_of(&f.router, "/robots.txt").await;
        assert!(
            robots.contains(&format!("Sitemap: {}", views::abs_url("/sitemap.xml"))),
            "robots.txt did not point at the sitemap: {robots:?}"
        );
        // Google Images is a real discovery path for a photography site, so the
        // image routes must stay crawlable.
        for route in ["/image/", "/thumb/", "/medium/", "/preview/", "/wide/"] {
            assert!(
                !robots.contains(&format!("Disallow: {route}")),
                "robots.txt blocked {route}, which hides the photographs"
            );
        }

        let sitemap = body_of(&f.router, "/sitemap.xml").await;
        assert!(sitemap.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(sitemap.contains(&format!("<loc>{}</loc>", views::abs_url("/"))));
        assert!(sitemap.contains(&format!("<loc>{}</loc>", views::abs_url("/about"))));
        for loc in sitemap.split("<loc>").skip(1) {
            let url = loc.split("</loc>").next().unwrap();
            assert!(
                url.starts_with(views::SITE_ORIGIN),
                "sitemap entry {url:?} is not an absolute URL on this origin"
            );
        }
    }

    /// The client deliveries are password-gated: to a crawler each is the same
    /// login stub under a different name. They must be excluded by `noindex`
    /// and left out of the sitemap.
    #[tokio::test]
    async fn client_galleries_are_excluded_from_search() {
        let f = fixture();
        let sitemap = body_of(&f.router, "/sitemap.xml").await;
        assert!(
            !sitemap.contains("/work/"),
            "a client delivery URL leaked into the sitemap"
        );
        // The index itself is public and should be listed.
        assert!(sitemap.contains(&format!("<loc>{}</loc>", views::abs_url("/work"))));
    }

    /// One `<h1>` per page, naming what the page holds.
    #[tokio::test]
    async fn listing_pages_have_exactly_one_heading() {
        let f = fixture();
        for uri in ["/", "/about", "/browse", "/recent", "/all", "/work"] {
            let html = body_of(&f.router, uri).await;
            let n = html.matches("<h1").count();
            assert_eq!(n, 1, "{uri} had {n} <h1> elements, expected 1");
        }
    }

    async fn post(router: &Router, uri: &str, form: &str) -> axum::response::Response {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(
                        header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(Body::from(form.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn auth_failure_log(f: &Fixture) -> Vec<String> {
        let path = f.data.join(notify::LOGS_DIR).join(audit::AUTH_FAILURES_LOG);
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    /// A work job with several folders under it, for the tab-strip tests.
    ///
    /// Its own fixture rather than more folders under `fixture()`'s `work/smith`:
    /// the download and audit tests count files in that job, and a set fixture
    /// that grows whenever a layout question comes up would keep moving their
    /// numbers underneath them.
    ///
    /// Shape mirrors the real archive — an edited folder and its original beside
    /// it, plus a second family of film scans one level deeper, plus a photograph
    /// sitting at the job root.
    fn work_sets_fixture() -> Fixture {
        work_sets_fixture_tagged(None)
    }

    /// As above, plus a digiKam database in which every `(album, filename)` pair
    /// carries the `thumbnail` tag. `None` writes no database at all, which is
    /// the state the site is in until a photograph is tagged.
    fn work_sets_fixture_tagged(tagged: Option<&[(&str, &str)]>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let photos = root.join("photos");
        let cache = root.join("cache");
        let data = root.join("data");
        let static_dir = root.join("static");
        for d in [&photos, &cache, &data, &static_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(static_dir.join("style.css"), b"body{}").unwrap();
        for rel in [
            "work/big-job/loose_shot.jpg",
            "work/big-job/digital/edited/digital_frame-01.jpg",
            "work/big-job/digital/edited/digital_frame-02.jpg",
            "work/big-job/digital/original/digital_frame-01.jpg",
            "work/big-job/digital/original/digital_frame-02.jpg",
            "work/big-job/digital/original/digital_frame-03.jpg",
            "work/big-job/medium-format/positive/edited/mf_frame-01.jpg",
        ] {
            let at = photos.join(rel);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            image::RgbImage::from_pixel(900, 600, image::Rgb([120, 140, 100]))
                .save(&at)
                .expect("write work jpeg");
        }
        std::fs::write(photos.join("work/big-job/.password"), b"hunter2").unwrap();
        let db = tagged.map(|rows| {
            let at = photos.join("digikam4.db");
            write_thumbnail_db(&at, rows);
            at
        });
        let state = AppState::new(
            photos.clone(),
            cache,
            data.clone(),
            db,
            root.join("nether"),
        );
        Fixture {
            router: build_router(state, static_dir),
            _dir: dir,
            data,
            photos,
        }
    }

    /// Write a digiKam database holding one `thumbnail` tag over the given
    /// (album path, filename) pairs. Mirrors the shape `write_portfolio_db`
    /// creates, minus the parts only the portfolio query reads.
    fn write_thumbnail_db(path: &Path, tagged: &[(&str, &str)]) {
        let conn = rusqlite::Connection::open(path).expect("create test db");
        conn.execute_batch(
            "
            CREATE TABLE Tags (id INTEGER PRIMARY KEY, pid INTEGER, name TEXT);
            CREATE TABLE Albums (id INTEGER PRIMARY KEY, relativePath TEXT);
            CREATE TABLE Images (id INTEGER PRIMARY KEY, album INTEGER, name TEXT, status INTEGER);
            CREATE TABLE ImageTags (tagid INTEGER, imageid INTEGER);
            CREATE TABLE ImageInformation (imageid INTEGER PRIMARY KEY, rating INTEGER);

            INSERT INTO Tags (id, pid, name) VALUES (1, 0, 'thumbnail');
            ",
        )
        .expect("create test db schema");
        for (i, (album, file)) in tagged.iter().enumerate() {
            let id = i as i64 + 1;
            conn.execute(
                "INSERT INTO Albums (id, relativePath) VALUES (?1, ?2)",
                rusqlite::params![id, album],
            )
            .expect("insert album");
            conn.execute(
                "INSERT INTO Images (id, album, name, status) VALUES (?1, ?2, ?3, 1)",
                rusqlite::params![id, id, file],
            )
            .expect("insert image");
            conn.execute(
                "INSERT INTO ImageTags (tagid, imageid) VALUES (1, ?1)",
                rusqlite::params![id],
            )
            .expect("insert image tag");
        }
    }

    /// A job's card carries the photograph tagged `thumbnail` in digiKam.
    ///
    /// Which frame represents a wedding is a judgement, and it lives where the
    /// portfolio's judgements live — the tag database — rather than on disk.
    #[tokio::test]
    async fn a_work_card_shows_the_photograph_tagged_thumbnail() {
        let f = work_sets_fixture();

        // No database at all: cards render, without covers. This is the state
        // the fixture ships in, and the one the site is in until a tag exists.
        let plain = body_string(get(&f.router, "/work").await).await;
        assert!(plain.contains(">big job<"), "{plain}");
        assert!(!plain.contains("work-card-thumb"), "a cover appeared from nowhere");

        // Tag the JPEG and its RAW together, the way a pair is tagged in
        // digiKam. Only the JPEG can be rendered, and only it is picked.
        let f = work_sets_fixture_tagged(Some(&[
            ("/work/big-job/digital/edited", "digital_frame-02.jpg"),
            ("/work/big-job/digital/edited", "digital_frame-02.ARW"),
        ]));

        let html = body_string(get(&f.router, "/work").await).await;
        assert!(
            html.contains(
                r#"<img class="work-card-thumb" src="/preview/work/big-job/digital/edited/digital_frame-02.jpg""#
            ),
            "no cover on the card: {html}"
        );
        assert!(!html.contains(".ARW"), "the tagged raw was offered as an image: {html}");
        // Decorative: the card's own text already names the job.
        assert!(html.contains(r#"alt=""#), "the cover has no alt attribute");
        assert_eq!(
            html.matches("work-card-thumb").count(),
            1,
            "more than one cover for one job: {html}"
        );
    }

    /// A tag on something that is not a job, or on a file digiKam has lost,
    /// puts no cover on any card.
    #[tokio::test]
    async fn only_a_live_file_inside_a_job_becomes_a_cover() {
        let f = work_sets_fixture_tagged(Some(&[
            // `/work` itself is not a job.
            ("/work", "loose.jpg"),
            // Outside the work tree entirely.
            ("/2024/roll", "elsewhere.jpg"),
            // A negative scan, which the shared visibility filter excludes.
            ("/work/big-job/film/negative", "neg.jpg"),
        ]));
        let db = f.photos.join("digikam4.db");
        let html = body_string(get(&f.router, "/work").await).await;
        assert!(!html.contains("work-card-thumb"), "a stray tag became a cover: {html}");

        // And a row digiKam kept for a file it can no longer find (status 3)
        // would point a card at a path that is not there.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO Albums (id, relativePath) VALUES (90, '/work/big-job/digital/edited')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Images (id, album, name, status) VALUES (90, 90, 'gone.jpg', 3)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO ImageTags (tagid, imageid) VALUES (1, 90)", [])
            .unwrap();
        drop(conn);
        let html = body_string(get(&f.router, "/work").await).await;
        assert!(!html.contains("work-card-thumb"), "a lost file became a cover: {html}");
    }

    /// The tab strip offers the edited sets and the job root, and nothing else.
    ///
    /// An `original` folder is the same frames unedited — in the real archive it
    /// is 330 of 382 JPEGs — so it is download-only, reachable through the bulk
    /// bar's "Download original" row rather than as a gallery a client picks
    /// from. This is the assertion that stops those coming back as tabs.
    #[tokio::test]
    async fn work_tabs_offer_the_edited_sets_and_not_the_originals() {
        let f = work_sets_fixture();
        let html = body_string(get(&f.router, "/work/big-job").await).await;
        assert!(
            html.contains(r#"<nav class="subnav" aria-label="Photo sets">"#),
            "no tab strip: {html}"
        );
        // The tabs and the download control share one bar, and the control is
        // there whether or not the tabs are.
        assert!(html.contains(r#"<div class="work-bar">"#), "no bar: {html}");
        assert!(
            html.contains(r#"popovertarget="work-downloads""#),
            "nothing opens the download panel: {html}"
        );
        // The job-root set takes the job's own name, `digital/edited` drops the
        // stage segment, and the film set keeps every segment that is not a
        // stage. See `views::work_set_label`.
        for label in [">big job<", ">digital<", ">medium format<"] {
            assert!(html.contains(label), "no tab {label}");
        }
        assert!(
            !html.contains("original"),
            "an original folder reached the tab strip: {html}"
        );
        // Two edited sets plus the root, and no fourth for the originals.
        assert_eq!(
            html.matches(r#"<a href="/work/big-job?set="#).count(),
            3,
            "wrong number of tabs: {html}"
        );
    }

    /// `?set=` picks the gallery, and an unknown slug falls back to the first
    /// set rather than 404ing — a client following a stale link should get their
    /// photographs, not an error.
    #[tokio::test]
    async fn the_set_query_picks_the_gallery_and_tolerates_a_stale_slug() {
        let f = work_sets_fixture();
        let tiles = |html: &str| html.matches(r#"<li class="mtile""#).count();

        let digital = body_string(get(&f.router, "/work/big-job?set=digital").await).await;
        assert_eq!(tiles(&digital), 2, "wrong tile count for digital");
        assert!(digital.contains(r#"aria-current="page">digital<"#));

        let film = body_string(get(&f.router, "/work/big-job?set=medium-format").await).await;
        assert_eq!(tiles(&film), 1, "wrong tile count for the film set");

        // The job root leads the order, so both of these render it.
        let default = body_string(get(&f.router, "/work/big-job").await).await;
        let stale = body_string(get(&f.router, "/work/big-job?set=no-such-set").await).await;
        assert_eq!(tiles(&default), 1, "wrong tile count for the default set");
        assert_eq!(tiles(&stale), tiles(&default), "a stale slug did not fall back");
    }

    /// Every name a client reads has its filesystem separators spelled out.
    ///
    /// The job is `big-job` in the URL and on disk; the frames are
    /// `digital_frame-01.jpg`. Those hyphens and underscores are there because a
    /// filesystem is easier to work in without spaces, which is not something a
    /// paying client should have to read around.
    #[tokio::test]
    async fn client_facing_names_have_their_separators_spelled_out() {
        let f = work_sets_fixture();
        let index = body_string(get(&f.router, "/work").await).await;
        assert!(index.contains(">big job<"), "index card not humanized: {index}");
        // The URL is untouched — this is display only.
        assert!(index.contains(r#"href="/work/big-job""#), "the URL was rewritten");

        let page = body_string(get(&f.router, "/work/big-job?set=digital").await).await;
        assert!(page.contains(r#"<h1 class="site-title">big job</h1>"#), "{page}");
        assert!(
            page.contains(r#"data-name="digital frame 01.jpg""#),
            "photo name not humanized: {page}"
        );
        assert!(
            page.contains(r#"alt="digital frame 01.jpg""#),
            "alt text not humanized: {page}"
        );
        // And the file on disk is still addressed by its real name.
        assert!(page.contains("/preview/work/big-job/digital/edited/digital_frame-01.jpg"));
    }

    /// The delivery tiles carry the POST download route, not the GET one.
    ///
    /// `/work/:name/file/*` is a POST — authorization rides on the path-scoped
    /// job cookie — and `lightbox.js` reads two different attributes for two
    /// different affordances: `data-download` it submits as a one-shot form,
    /// `data-jpg` it puts straight into an `<a href>`. Wiring a work tile to
    /// `data-jpg` renders a download link that answers 405, which is invisible
    /// to any assertion about the tile markup alone. That is the mistake this
    /// catches.
    #[tokio::test]
    async fn work_tiles_carry_the_post_download_route_and_no_get_link() {
        let js = std::fs::read_to_string("static/lightbox.js").expect("read lightbox.js");
        assert!(
            js.contains("dataset.download"),
            "lightbox.js no longer reads data-download"
        );
        assert!(
            js.contains("main.work .mgrid li.mtile a"),
            "lightbox.js prefetches a selector the work grid no longer emits"
        );

        let f = work_sets_fixture();
        let unlocked = post(&f.router, "/work/big-job/auth", "password=hunter2").await;
        let cookie = unlocked
            .headers()
            .get(header::SET_COOKIE)
            .expect("no auth cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let resp = f
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/work/big-job?set=digital")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_string(resp).await;
        assert!(
            html.contains(
                r#"data-download="/work/big-job/file/digital/edited/digital_frame-01.jpg""#
            ),
            "no POST download action on the tiles: {html}"
        );
        // The GET menu has nothing to offer here, and an empty value is what
        // keeps it hidden.
        assert!(
            html.contains(r#"data-jpg="""#),
            "a work tile offered a GET download link: {html}"
        );

        // Pre-auth neither affordance is wired at all.
        let locked = body_string(get(&f.router, "/work/big-job?set=digital").await).await;
        assert!(
            !locked.contains("data-download="),
            "a locked page handed out download routes: {locked}"
        );
    }

    /// Everything that opens or closes the download panel agrees on its `id`.
    ///
    /// The panel is declarative — `popovertarget` names the element it acts on —
    /// so a mismatched `id` is not an error anywhere. The button simply does
    /// nothing, the client cannot reach their files, and nothing in the console
    /// says why. `work.js` looks the element up the same way and fails the same
    /// silent way, which is what this covers that a rendering assertion cannot.
    #[tokio::test]
    async fn the_download_panel_and_everything_that_opens_it_agree_on_one_id() {
        let f = work_sets_fixture();
        let html = body_string(get(&f.router, "/work/big-job").await).await;

        let id = "work-downloads";
        assert!(html.contains(&format!(r#"id="{id}""#)), "no panel: {html}");
        assert!(html.contains(r#"popover="auto""#), "the panel is not a popover");
        // The bar's button opens it and the panel's own button closes it.
        assert_eq!(
            html.matches(&format!(r#"popovertarget="{id}""#)).count(),
            2,
            "expected exactly an opener and a closer: {html}"
        );
        assert!(html.contains(r#"popovertargetaction="hide""#), "nothing closes it");

        let js = std::fs::read_to_string("static/work.js").expect("read work.js");
        assert!(
            js.contains(&format!("'{id}'")),
            "work.js looks up a different id than the page renders"
        );

        // `data-auto-open` is the flag work.js keys on, and it is set on the
        // error re-render and nowhere else — an unlocked page that popped its
        // own download panel open on every visit would be worse than the block
        // this replaced.
        assert!(
            !html.contains("data-auto-open"),
            "a page with no error still asks for the panel to open: {html}"
        );
        let refused = post(&f.router, "/work/big-job/auth", "password=wrong").await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        let refused = body_string(refused).await;
        assert!(
            refused.contains(r#"data-auto-open="true""#),
            "a refused password left the panel shut: {refused}"
        );
        // The refusal sits inside the panel, under the field it is about, and
        // is wired to that field for a screen reader.
        let panel = refused.find(&format!(r#"id="{id}""#)).expect("no panel");
        let message = refused.find("dl-error").expect("no error message");
        assert!(message > panel, "the refusal was left outside the panel");
        let field = refused.find(r#"id="work-password""#).expect("no field");
        assert!(field < message, "the refusal was put above the field");
        assert!(
            refused.contains(r#"aria-describedby="work-password-error""#),
            "the field does not point at its error"
        );
        assert!(refused.contains(r#"aria-invalid="true""#), "the field is not marked invalid");
    }

    /// A job's `title.txt` names it, on the index card and on the delivery page.
    ///
    /// The folder name has to be a valid path segment and has to stay put once
    /// links have gone out, so it cannot also be the thing a client reads.
    /// `humanize` closes most of the gap but cannot add a capital, a comma, or a
    /// word the folder does not contain — so the job carries its own title, and
    /// the fallback when it does not is still the folder name and never an
    /// invented sentence.
    #[tokio::test]
    async fn a_job_is_named_by_its_title_file_and_falls_back_to_its_folder() {
        let f = work_sets_fixture();

        // No title file yet: the folder name, spelled out.
        let index = body_string(get(&f.router, "/work").await).await;
        assert!(index.contains(">big job<"), "{index}");
        let page = body_string(get(&f.router, "/work/big-job").await).await;
        assert!(page.contains(r#"<h1 class="site-title">big job</h1>"#), "{page}");

        // Written verbatim — the hyphen is punctuation the owner typed, not a
        // separator standing in for a space, so `humanize` must not touch it.
        std::fs::write(
            f.photos.join("work/big-job").join(work::TITLE_FILE),
            "Marisol & Sam \u{2014} June 2026\n",
        )
        .unwrap();

        let index = body_string(get(&f.router, "/work").await).await;
        assert!(
            index.contains(">Marisol &amp; Sam — June 2026<"),
            "the index card ignored title.txt: {index}"
        );
        assert!(!index.contains(">big job<"), "the folder name is still showing");
        // The URL is the folder, always: a title is not an address.
        assert!(index.contains(r#"href="/work/big-job""#), "the URL moved with the title");

        let page = body_string(get(&f.router, "/work/big-job").await).await;
        assert!(
            page.contains(r#"<h1 class="site-title">Marisol &amp; Sam — June 2026</h1>"#),
            "the page heading ignored title.txt: {page}"
        );
        assert!(
            page.contains("<title>Marisol &amp; Sam — June 2026 — Photo Delivery"),
            "the browser tab ignored title.txt: {page}"
        );
        assert!(
            page.contains(r#"href="https://paulborrego.com/work/big-job""#),
            "the canonical moved with the title"
        );

        // An empty file is a file nobody has filled in, not a job with no name.
        std::fs::write(f.photos.join("work/big-job").join(work::TITLE_FILE), "\n  \n").unwrap();
        let page = body_string(get(&f.router, "/work/big-job").await).await;
        assert!(
            page.contains(r#"<h1 class="site-title">big job</h1>"#),
            "an empty title.txt did not fall back: {page}"
        );

        // One line only: an editor's trailing newline is not a second line, and
        // a stray one cannot break the <title> in half.
        std::fs::write(
            f.photos.join("work/big-job").join(work::TITLE_FILE),
            "The Real Title\nnotes to self\n",
        )
        .unwrap();
        let page = body_string(get(&f.router, "/work/big-job").await).await;
        assert!(page.contains(r#"<h1 class="site-title">The Real Title</h1>"#), "{page}");
        assert!(!page.contains("notes to self"), "a second line reached the page");
    }

    /// `title.txt` is a name, not a photograph and not a deliverable.
    #[tokio::test]
    async fn the_title_file_is_not_served_as_part_of_the_job() {
        let f = work_sets_fixture();
        std::fs::write(
            f.photos.join("work/big-job").join(work::TITLE_FILE),
            "Marisol & Sam",
        )
        .unwrap();
        let page = body_string(get(&f.router, "/work/big-job").await).await;
        // Not a tile, not a tab, and not counted among the files on offer.
        assert!(!page.contains(work::TITLE_FILE), "title.txt reached the page: {page}");
        assert!(
            page.contains(r#"<li class="mtile""#),
            "the job root set lost its photograph"
        );
        let index = body_string(get(&f.router, "/work").await).await;
        // The fixture's seven JPEGs, still seven.
        assert!(index.contains("7 JPEG"), "title.txt was counted as a file: {index}");
    }

    /// Five wrong passwords and the job stops answering for a while.
    ///
    /// The limit reads the same append-only failure log the owner's report is
    /// built from, so it survives a restart — a counter held in memory would be
    /// cleared by one, which is the difference between a limit and a speed bump.
    #[tokio::test]
    async fn a_job_stops_checking_passwords_after_five_wrong_ones() {
        let f = work_sets_fixture();

        for i in 1..=4 {
            let resp = post(&f.router, "/work/big-job/auth", "password=wrong").await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "try {i}");
            let html = body_string(resp).await;
            assert!(html.contains("Incorrect password."), "try {i} said something else");
        }

        // The fifth is still checked, and still wrong.
        let fifth = post(&f.router, "/work/big-job/auth", "password=wrong").await;
        assert_eq!(fifth.status(), StatusCode::UNAUTHORIZED);

        // The sixth is not checked at all — including the right one, which is
        // the whole point: past the limit the route has no opinion about what
        // was submitted.
        let refused = post(&f.router, "/work/big-job/auth", "password=hunter2").await;
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        let html = body_string(refused).await;
        assert!(html.contains(views::WORK_ERR_RATE), "{html}");
        assert!(!html.contains("dl-row"), "a locked-out attempt was let through: {html}");

        // A refused attempt is not itself recorded: counting it would push the
        // window forward on every retry and the pause would never end.
        assert_eq!(auth_failure_log(&f).len(), 5, "a rate-limited try was counted");

        // And the limit is per job, so the one next door is unaffected.
        std::fs::create_dir_all(f.photos.join("work/other-job/edited")).unwrap();
        image::RgbImage::from_pixel(900, 600, image::Rgb([120, 140, 100]))
            .save(f.photos.join("work/other-job/edited/A.jpg"))
            .unwrap();
        std::fs::write(f.photos.join("work/other-job/.password"), b"hunter2").unwrap();
        let ok = post(&f.router, "/work/other-job/auth", "password=hunter2").await;
        assert_eq!(ok.status(), StatusCode::SEE_OTHER, "the limit leaked across jobs");
    }

    /// An accepted password lands on the files rather than on a button.
    ///
    /// The client typed a password to reach the downloads; making them press
    /// Download afterwards is a step that asks nothing and answers nothing. The
    /// redirect carries `?downloads=1`, which the view turns into the flag
    /// `work.js` opens the panel on.
    #[tokio::test]
    async fn an_accepted_password_lands_with_the_downloads_open() {
        let f = work_sets_fixture();
        let ok = post(&f.router, "/work/big-job/auth", "password=hunter2").await;
        assert_eq!(ok.status(), StatusCode::SEE_OTHER);
        let location = ok
            .headers()
            .get(header::LOCATION)
            .expect("no redirect")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(location, "/work/big-job?downloads=1");

        let cookie = ok
            .headers()
            .get(header::SET_COOKIE)
            .expect("no auth cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let landed = f
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&location)
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_string(landed).await;
        assert!(
            html.contains(r#"data-auto-open="true""#),
            "the panel stayed shut after the password was accepted: {html}"
        );
        assert_eq!(html.matches(r#"class="dl-row""#).count(), 3, "no download rows");

        // It lasts exactly one page view: the tab links carry no such parameter,
        // so moving between sets does not reopen the panel every time.
        assert!(
            !html.contains("downloads=1\""),
            "the flag leaked into a link on the page: {html}"
        );
        let plain = body_string(
            f.router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/work/big-job")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert!(!plain.contains("data-auto-open"), "the panel opens on every visit");
    }

    /// A wrong password is counted per job and nothing else is kept. The report
    /// asks "is someone guessing at this job", which a tally answers; an
    /// address would turn the same file into a visitor log.
    #[tokio::test]
    async fn wrong_work_passwords_are_counted_and_right_ones_are_not() {
        let f = fixture();
        let bad = post(&f.router, "/work/smith/auth", "password=wrong").await;
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

        let lines = auth_failure_log(&f);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains(r#""job":"smith""#), "{}", lines[0]);
        assert!(!lines[0].contains("wrong"), "the attempt was stored: {}", lines[0]);

        let good = post(&f.router, "/work/smith/auth", "password=hunter2").await;
        assert_eq!(good.status(), StatusCode::SEE_OTHER);
        assert_eq!(auth_failure_log(&f).len(), 1, "a success was counted as a failure");
    }

    /// The full client path: unlock the job, take the zip, take a single file.
    /// Both land in the log, and the zip carries the scope and kind — "they
    /// took the JPEGs but never the RAWs" is the answer the work report exists
    /// to give, and it is unrecoverable if only a count is stored.
    #[tokio::test]
    async fn served_work_downloads_record_their_scope_and_kind() {
        let f = fixture();
        let unlocked = post(&f.router, "/work/smith/auth", "password=hunter2").await;
        assert_eq!(unlocked.status(), StatusCode::SEE_OTHER);
        let cookie = unlocked
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .expect("auth issued a cookie")
            .to_string();

        let with_cookie = |uri: &str, form: &str| {
            let router = f.router.clone();
            let cookie = cookie.clone();
            let uri = uri.to_string();
            let form = form.to_string();
            async move {
                router
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(uri)
                            .header(header::COOKIE, cookie)
                            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                            .body(Body::from(form))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            }
        };

        let zip = with_cookie("/work/smith/download", "kind=jpeg&scope=edited").await;
        assert_eq!(zip.status(), StatusCode::OK);
        let one = with_cookie("/work/smith/file/edited/A.jpg", "").await;
        assert_eq!(one.status(), StatusCode::OK);

        let lines = download_log(&f);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains(r#""route":"work_zip""#), "{}", lines[0]);
        assert!(lines[0].contains(r#""job":"smith""#), "{}", lines[0]);
        assert!(lines[0].contains(r#""scope":"edited""#), "{}", lines[0]);
        assert!(lines[0].contains(r#""kind":"jpeg""#), "{}", lines[0]);
        assert!(lines[1].contains(r#""route":"work_file""#), "{}", lines[1]);
        assert!(lines[1].contains(r#""path":"edited/A.jpg""#), "{}", lines[1]);
    }

    /// An unauthenticated work download is refused, and a refusal is not a
    /// delivery — logging it would tell the owner a client collected photos
    /// they never received.
    #[tokio::test]
    async fn refused_work_downloads_are_not_logged() {
        let f = fixture();
        let resp = post(&f.router, "/work/smith/download", "kind=jpeg&scope=all").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = post(&f.router, "/work/smith/file/edited/A.jpg", "").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(download_log(&f).is_empty(), "{:?}", download_log(&f));
    }

    /// The log records deliberate saves, and only those. `/image`, `/thumb`,
    /// `/medium` and `/preview` are how a gallery page draws itself — one visit
    /// to `/all` is a thousand of them — so a line from any of those would bury
    /// the signal the log exists to carry.
    #[tokio::test]
    async fn only_the_download_route_is_logged() {
        let f = fixture();
        let resp = get(&f.router, "/download/2024/roll.jpg").await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Drain the body: the handler streams it, and the assertion below is
        // about a side effect that happens before the first byte either way.
        let _ = body_string(resp).await;

        for uri in [
            "/image/2024/roll.jpg",
            "/thumb/2024/roll.jpg",
            "/medium/2024/roll.jpg",
            "/preview/2024/roll.jpg",
        ] {
            assert_eq!(get(&f.router, uri).await.status(), StatusCode::OK, "{uri}");
        }

        let lines = download_log(&f);
        assert_eq!(lines.len(), 1, "renditions leaked into the log: {lines:?}");
        assert!(lines[0].contains(r#""route":"public""#), "{}", lines[0]);
        assert!(lines[0].contains(r#""path":"2024/roll.jpg""#), "{}", lines[0]);
    }

    /// Logging before the response is built would file every miss as a
    /// download, and the misses are exactly what a scraper walking filenames
    /// generates — so the log would read busiest when nothing was served.
    #[tokio::test]
    async fn a_download_that_404s_is_not_logged() {
        let f = fixture();
        for uri in [
            "/download/2024/does-not-exist.jpg",
            "/download/2024/roll.txt",
            "/download/../escape.jpg",
        ] {
            let status = get(&f.router, uri).await.status();
            assert_ne!(status, StatusCode::OK, "{uri} unexpectedly served");
        }
        assert!(download_log(&f).is_empty(), "{:?}", download_log(&f));
    }

}
