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
                for kind in ThumbKind::ALL {
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
    let attempted = total * ThumbKind::ALL.len();
    let built = built.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    println!(
        "done in {:.1}s — {built} renditions generated, {} already fresh, {failed} failed",
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

        let state = AppState::new(photos, cache, data.clone(), None, root.join("nether"));
        Fixture {
            router: build_router(state, static_dir),
            _dir: dir,
            data,
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
        for route in ["/image/", "/thumb/", "/medium/", "/preview/"] {
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
