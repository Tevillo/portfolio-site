use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use tokio_util::io::ReaderStream;
use tracing::warn;

use crate::audit;
use crate::work::{self, DownloadKind, Scope};
use crate::paths::{PathError, leading_year, safe_resolve};
use crate::people;
use crate::portfolio;
use crate::notify;
use crate::recent;
use crate::state::AppState;
use crate::thumbs::{self, ThumbKind};
use crate::views::{
    self, Crumb, DirEntry, FolderGroup, ImageEntry, SectionTab, WorkIndexEntry,
    PersonEntry,
};

#[derive(Clone, Copy)]
enum PageKind {
    BrowseRoot,
    BrowseSub,
}

/// The router's `.fallback()` — every URL that matches no route.
///
/// Page handlers that come up empty call [`not_found_response`] directly rather
/// than routing through here, since by then the request has already matched.
pub async fn not_found() -> Response {
    not_found_response()
}

/// A 404 that renders the site instead of a blank page: status code plus
/// [`views::not_found_page`]. Every HTML page route uses this; the image,
/// rendition and download routes keep returning a bare status, because a client
/// asking for a JPEG has no use for a page of markup.
pub fn not_found_response() -> Response {
    (StatusCode::NOT_FOUND, views::not_found_page()).into_response()
}

/// A page route's status turned into a response: a miss renders the 404 page,
/// anything else stays a bare status. Used where a page helper reports failure
/// as a `StatusCode` rather than returning a rendered body.
fn page_status_response(status: StatusCode) -> Response {
    if status == StatusCode::NOT_FOUND {
        not_found_response()
    } else {
        status.into_response()
    }
}

/// The portfolio front door: whichever section leads [`views::SECTION_ORDER`],
/// rendered at `/`.
///
/// Every other section lives at its own `/portfolio/<slug>` and is reached from
/// the sub-tab strip; the leading one is served here and *only* here, so the same
/// photographs are never published at two addresses.
pub async fn index(State(state): State<AppState>) -> Response {
    portfolio_response(&state, None).await
}

/// `/portfolio/:slug` — one named portfolio section.
///
/// The section that `/` already renders redirects there permanently rather than
/// serving a second copy of itself: two URLs for one set of photographs is the
/// duplicate this layout was arranged to avoid.
pub async fn portfolio_section(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    portfolio_response(&state, Some(&slug)).await
}

/// `/portfolio` with no section named. Not a page of its own — the front door
/// already is the portfolio.
pub async fn portfolio_root() -> Response {
    Redirect::permanent("/").into_response()
}

/// Shared body of [`index`] and [`portfolio_section`].
///
/// `want` is `None` for `/` (meaning "the leading section") and `Some(slug)` for
/// a named one. Keeping both in one function is what guarantees the two pages
/// cannot disagree about the section order, the tab strip, or which section is
/// the front door.
async fn portfolio_response(state: &AppState, want: Option<&str>) -> Response {
    // Every failure mode here — no database, unreadable database, no `portfolio`
    // tag — collapses to an empty section list. For `/` that renders "Nothing
    // here yet."; for a named section it is a 404, because the URL claimed a
    // section that is not there. The front page of the site is the wrong place to
    // surface an infrastructure problem, and the log line is the actionable half
    // anyway.
    let sections = portfolio_sections(state).await;
    let Some(front) = sections.first() else {
        return match want {
            Some(_) => not_found_response(),
            None => views::portfolio_page(None, "", "/", &[]).into_response(),
        };
    };

    let active_slug = match want {
        None => front.slug.clone(),
        // The leading section is published at `/`, so its own slug is a second
        // address for the same page. Redirect rather than render.
        Some(s) if s == front.slug => return Redirect::permanent("/").into_response(),
        Some(s) => match sections.iter().find(|sec| sec.slug == s) {
            Some(sec) => sec.slug.clone(),
            None => return not_found_response(),
        },
    };

    let tabs: Vec<views::SectionTab> = sections
        .iter()
        .map(|sec| views::SectionTab {
            label: sec.label.clone(),
            url: if sec.slug == front.slug {
                "/".to_string()
            } else {
                format!("/portfolio/{}", encode_path(&sec.slug))
            },
            active: sec.slug == active_slug,
        })
        .collect();

    let canonical = if active_slug == front.slug {
        "/".to_string()
    } else {
        format!("/portfolio/{}", encode_path(&active_slug))
    };

    // Only the section being rendered is resolved against the filesystem. The
    // tab strip needs labels and slugs, which the tag query already gave us; a
    // `safe_resolve` per photograph across every section would be a few hundred
    // syscalls to draw three links.
    let Some(active) = sections.iter().find(|sec| sec.slug == active_slug) else {
        return not_found_response();
    };
    let group = portfolio_group(state, active).await;
    views::portfolio_page(group.as_ref(), &active_slug, &canonical, &tabs).into_response()
}

/// The `portfolio/*` tags, in display order, with no filesystem access — labels
/// and slugs only.
///
/// Every failure collapses to an empty list; see [`portfolio_response`] for what
/// each caller does with that.
async fn portfolio_sections(state: &AppState) -> Vec<portfolio::Section> {
    let Some(db) = state.db_path().cloned() else {
        warn!("portfolio tag database not available; rendering empty portfolio");
        return Vec::new();
    };
    match portfolio::list_sections(db).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "listing portfolio sections failed");
            Vec::new()
        }
    }
}

/// Resolve one section's tagged photos into a renderable [`FolderGroup`],
/// dropping anything that cannot be turned into a live URL. `None` when nothing
/// in the section survived.
async fn portfolio_group(state: &AppState, section: &portfolio::Section) -> Option<FolderGroup> {
    // Tagged photos arrive as flat rel paths scattered across the tree, so —
    // exactly as in `person_photos` — there is no directory walk to piggyback
    // the raw-sibling lookup on. Cache each album folder's stem->raw map, which
    // pays off here because whole sections tend to share one folder.
    let mut dir_raws: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut images: Vec<ImageEntry> = Vec::with_capacity(section.photos.len());
    // Absolute source paths in `images` order, the shape `fill_image_dims`
    // expects.
    let mut sources: Vec<PathBuf> = Vec::new();

    for p in &section.photos {
        // `safe_resolve` doubles as the existence check: a tag can outlive the
        // file it points at (renamed outside digiKam, moved to a folder the site
        // skips), and a tile whose source is gone would render as a broken image
        // with no dimensions to reserve space with.
        let Ok(abs) = safe_resolve(state.photos_root(), &p.rel).await else {
            warn!(rel = %p.rel, "portfolio-tagged photo missing on disk; skipping");
            continue;
        };
        let parent = parent_rel(&p.rel).to_string();
        if !dir_raws.contains_key(&parent) {
            let map = scan_raw_siblings(state.photos_root(), &parent).await;
            dir_raws.insert(parent.clone(), map);
        }
        let raw_download_url = dir_raws
            .get(&parent)
            .and_then(|m| m.get(&file_stem_lower(&p.name)))
            .map(|raw| format!("/download/{}", encode_path(&join_rel(&parent, raw))));
        sources.push(abs);
        images.push(ImageEntry {
            // The portfolio shows large tiles, so it loads the 1600px preview
            // rendition rather than the 400px grid thumb that dense listings use.
            thumb_url: format!("/preview/{}", encode_path(&p.rel)),
            image_url: format!("/image/{}", encode_path(&p.rel)),
            jpg_download_url: format!("/download/{}", encode_path(&p.rel)),
            raw_download_url,
            // The archive galleries download over GET; the POST route
            // belongs to the work delivery pages alone.
            download_action: None,
            dims: None,
            srcset: None,
            name: p.name.clone(),
        });
    }
    if images.is_empty() {
        return None;
    }

    let mut groups = vec![FolderGroup {
        label: section.label.clone(),
        // No longer read by `collapse.js` — the portfolio has no collapsing
        // sections — but still the section's identity in the group shape the
        // other pages share.
        path: format!("portfolio/{}", section.slug),
        // A tag's photos span many folders, so there is no single folder to
        // browse to.
        browse_url: String::new(),
        images,
        // Tag-driven sections have no folder, so no favs/ folder to fold in.
        favs_count: 0,
        // Only `/all` reads this, and `/all` never sees a tag-driven section:
        // these have no folder to be recently changed.
        newest_mtime: None,
    }];

    // The column grid is built entirely from these: `--ar` on each tile is the
    // photo's aspect ratio, and without it a tile falls back to a guessed 3:2
    // rather than laying out at its true shape.
    fill_portfolio_dims(&sources, &mut groups).await;
    groups.pop()
}

/// Dimensions for a portfolio section, plus a higher-resolution candidate for
/// every panorama in it.
///
/// Not [`fill_image_dims`], because the second candidate here is wanted for some
/// tiles and not others. A column tile is ~500-600 CSS px wide, so the 1600px
/// Preview it links already covers it twice over. A panorama is promoted to the
/// full width of the page — ~1840 CSS px at a 1920 viewport — where the same
/// file is *upscaled* on a 1x screen and half of what a 2x screen wants, so
/// those tiles, and only those, also offer [`ThumbKind::Wide`].
///
/// The `src` stays the Preview either way, so the `width`/`height` attributes
/// keep describing the bytes a browser with no `srcset` support fetches, and a
/// phone on the wide tile still picks the small file.
async fn fill_portfolio_dims(sources: &[PathBuf], groups: &mut [FolderGroup]) {
    if sources.is_empty() {
        return;
    }
    let sources = sources.to_vec();
    let dims = match tokio::task::spawn_blocking(move || {
        sources
            .into_iter()
            .map(|p| {
                let oriented = thumbs::oriented_dimensions(&p).ok()?;
                let scaled = thumbs::scale_to(oriented, ThumbKind::Preview.max_dim());
                // `scale_to` never enlarges, so a source narrower than the
                // Preview cap yields the same bytes under both routes, and a
                // `srcset` offering one file at two widths tells the browser
                // nothing.
                let wide = crate::views::is_wide_ratio(oriented)
                    .then(|| thumbs::scale_to(oriented, ThumbKind::Wide.max_dim()).0)
                    .filter(|w| *w > scaled.0);
                Some((scaled, wide))
            })
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "portfolio dimension batch panicked");
            return;
        }
    };
    for (img, dim) in groups
        .iter_mut()
        .flat_map(|g| g.images.iter_mut())
        .zip(dims)
    {
        let Some((scaled, wide_w)) = dim else { continue };
        img.dims = Some(scaled);
        img.srcset = wide_w.and_then(|w| {
            swap_rendition_route(&img.thumb_url, ThumbKind::Preview, ThumbKind::Wide)
                .map(|u| (u, w))
        });
    }
}

/// About page. The prose is a compile-time constant in `views`; the only
/// dynamic part is the optional portrait at `<photos>/about.jpg`, which is
/// served through the normal preview rendition pipeline so it gets the same
/// downscaling, EXIF orientation and caching as any other photo.
pub async fn about(State(state): State<AppState>) -> Response {
    let rel = views::ABOUT_PORTRAIT_REL;
    let portrait = match safe_resolve(state.photos_root(), rel).await {
        Ok(path) if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file()) => {
            let url = format!("/preview/{}", encode_path(rel));
            let dims = tokio::task::spawn_blocking(move || {
                thumbs::rendition_dimensions(&path, ThumbKind::Preview).ok()
            })
            .await
            .unwrap_or(None);
            dims.map(|d| (url, d))
        }
        _ => None,
    };
    views::about_page(portrait.as_ref().map(|(url, d)| (url.as_str(), *d))).into_response()
}

/// Current build identifier, polled by `static/version.js` when a tab regains
/// focus. `no-store` so neither the browser nor any intermediary can answer
/// from cache — a stale answer here would either mask a deploy or, worse,
/// disagree with the page's own meta tag forever and drive a reload loop.
/// `GET /robots.txt`. Two jobs: point crawlers at the sitemap, and keep them
/// out of the routes that transfer bytes rather than serve pages.
///
/// Deliberately leaves `/image/` and the rendition routes crawlable. Blocking
/// them is the reflex, but Google Images is a real discovery path for a
/// photographer and those routes are what it would index; only `/download/`
/// (full-resolution originals and RAW siblings, served as attachments) is worth
/// refusing.
///
/// The password-gated `/work/<job>` pages are *not* disallowed here on purpose.
/// A `Disallow` stops the crawl but not the indexing — a disallowed URL that is
/// linked can still be listed, just with no description, and the crawler can no
/// longer read the `noindex` that would have excluded it properly. Letting it
/// fetch the page and honour the tag is what actually keeps those out.
pub async fn robots_txt() -> Response {
    let body = format!(
        "User-agent: *\n\
         Allow: /\n\
         \n\
         # Byte transfers, not pages: originals, RAW siblings and job zips.\n\
         Disallow: /download/\n\
         Disallow: /work/*/download\n\
         Disallow: /work/*/file/\n\
         # Build-id probe behind the client-side reload check.\n\
         Disallow: /version\n\
         \n\
         Sitemap: {}\n",
        views::abs_url("/sitemap.xml"),
    );
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

/// `GET /sitemap.xml`. Every indexable page, so a crawler does not have to
/// discover the archive by following links through the folder tree — which is
/// how deep galleries end up uncrawled.
///
/// Lists only canonical URLs, and only pages that render for an anonymous
/// visitor: the `/work/<job>` deliveries are `noindex` (see
/// [`views::work_page`]) and are left out, as are the asset routes.
///
/// `walk_groups` is the same tree walk `/all` runs per request, and it only
/// reads directory entries — no image decoding — so reusing it here costs a
/// crawler-frequency directory scan rather than justifying a second walker.
pub async fn sitemap_xml(State(state): State<AppState>) -> Response {
    let mut paths: Vec<String> = vec![
        "/".to_string(),
        "/about".to_string(),
        "/all".to_string(),
        "/browse".to_string(),
        "/recent".to_string(),
        "/people".to_string(),
        "/work".to_string(),
    ];

    // Every portfolio section except the leading one, which is already listed as
    // "/" above. Adding its slug too would put a URL that 301s into the sitemap.
    if let Some((_front, rest)) = portfolio_sections(&state).await.split_first() {
        paths.extend(
            rest.iter()
                .map(|s| format!("/portfolio/{}", encode_path(&s.slug))),
        );
    }

    // Every folder that directly holds photos, i.e. every /browse page with
    // something on it. Intermediate folders come along as each group's parents
    // are themselves groups only when they hold photos too; listing the leaves
    // is what matters, since the crawler reaches the rest from /browse.
    if let Ok(groups) = walk_groups(state.photos_root(), "", ThumbKind::Grid, false, None).await {
        paths.extend(
            groups
                .iter()
                .map(|g| g.browse_url.clone())
                .filter(|u| u != "/browse"),
        );
    }

    if let Some(db) = state.db_path() {
        match people::list_people(db.clone()).await {
            Ok(people) => paths.extend(
                people
                    .iter()
                    .map(|p| format!("/people/{}", encode_path(&p.name))),
            ),
            Err(e) => warn!(error = ?e, "listing people for sitemap failed"),
        }
    }

    paths.extend(crate::nether::sitemap_paths(state.nether_root()).await);

    let mut xml = String::with_capacity(paths.len() * 80 + 200);
    xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push('\n');
    xml.push_str(r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#);
    xml.push('\n');
    for path in &paths {
        // Every path here was built by `encode_path`, which percent-encodes
        // everything outside `[A-Za-z0-9-_.~/]` — so none of the five XML
        // predefined entities can appear and the escape below is belt-and-braces
        // against a future caller that forgets.
        let _ = writeln!(xml, "  <url><loc>{}</loc></url>", xml_escape(&views::abs_url(path)));
    }
    xml.push_str("</urlset>\n");

    (
        [
            (header::CONTENT_TYPE, "application/xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        xml,
    )
        .into_response()
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

pub async fn version() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(views::build_id()))
        .unwrap()
}

pub async fn browse_root(State(state): State<AppState>) -> Response {
    match render_dir(&state, "", PageKind::BrowseRoot).await {
        Ok(resp) => resp,
        Err(status) => page_status_response(status),
    }
}

pub async fn browse(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    match render_dir(&state, &rel, PageKind::BrowseSub).await {
        Ok(resp) => resp,
        Err(status) => page_status_response(status),
    }
}

async fn render_dir(
    state: &AppState,
    rel: &str,
    kind: PageKind,
) -> Result<Response, StatusCode> {
    let dir = safe_resolve(state.photos_root(), rel).await.map_err(map_path_err)?;

    let mut read = tokio::fs::read_dir(&dir)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut candidate_subdirs: Vec<(String, String, PathBuf)> = Vec::new();
    let mut jpegs: Vec<(String, String)> = Vec::new();
    let mut raw_by_stem: HashMap<String, String> = HashMap::new();
    // A "favs" subfolder is not listed as a browsable folder; its photos are
    // inlined ahead of this folder's own so favorites lead the grid.
    let mut favs_rel: Option<String> = None;
    while let Some(entry) = read.next_entry().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)? {
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
        let rel_child = join_rel(rel, &name);
        if ftype.is_dir() {
            if is_skipped_dir(&name) || is_work_root(rel, &name) {
                continue;
            }
            if name.eq_ignore_ascii_case("favs") {
                favs_rel = Some(rel_child);
                continue;
            }
            candidate_subdirs.push((name, rel_child, entry.path()));
        } else if ftype.is_file() && is_jpeg(&name) && !is_hidden(&name) {
            jpegs.push((name, rel_child));
        } else if ftype.is_file() && is_raw_download_name(&name) {
            raw_by_stem.insert(file_stem_lower(&name), name);
        }
    }
    let mut images: Vec<ImageEntry> = jpegs
        .into_iter()
        .map(|(name, rel_child)| {
            let raw_download_url = raw_by_stem
                .get(&file_stem_lower(&name))
                .map(|raw| format!("/download/{}", encode_path(&join_rel(rel, raw))));
            ImageEntry {
                thumb_url: format!("/thumb/{}", encode_path(&rel_child)),
                image_url: format!("/image/{}", encode_path(&rel_child)),
                jpg_download_url: format!("/download/{}", encode_path(&rel_child)),
                raw_download_url,
                // The archive galleries download over GET; the POST route
                // belongs to the work delivery pages alone.
                download_action: None,
                dims: None,
                srcset: None,
                name,
            }
        })
        .collect();

    let mut subdirs = Vec::new();
    for (name, rel_child, path) in candidate_subdirs {
        if subtree_has_jpeg(&path).await {
            subdirs.push(DirEntry {
                name,
                url: format!("/browse/{}", encode_path(&rel_child)),
            });
        }
    }

    subdirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    images.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    // Prepend any "favs" subfolder's photos so favorites lead this folder's grid.
    if let Some(favs_rel) = favs_rel {
        let mut fav_images = read_dir_images(state.photos_root(), &favs_rel).await;
        fav_images.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        if !fav_images.is_empty() {
            fav_images.append(&mut images);
            images = fav_images;
        }
    }

    if images.is_empty() && subdirs.len() == 1 {
        let target = subdirs.into_iter().next().unwrap().url;
        return Ok(Redirect::to(&target).into_response());
    }

    let crumbs = breadcrumbs(rel, kind);
    let title = match kind {
        PageKind::BrowseRoot => "Browse".to_string(),
        PageKind::BrowseSub => rel
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string(),
    };
    // Matches the URL the crawler followed to get here: same encoding as the
    // subdirectory links, and no trailing slash, which is the spelling the
    // trailing-slash redirects in main.rs funnel everything toward.
    let canonical = match kind {
        PageKind::BrowseRoot => "/browse".to_string(),
        PageKind::BrowseSub => format!("/browse/{}", encode_path(rel.trim_end_matches('/'))),
    };
    Ok(
        views::page(&title, &canonical, &crumbs, &subdirs, &images, false, views::Nav::Browse)
            .into_response(),
    )
}

/// Read the JPEGs directly inside `root`/`rel` (non-recursive) as gallery
/// entries, pairing each with its sibling raw download when present. Used to
/// inline a `favs` subfolder's photos into its parent's browse view.
async fn read_dir_images(root: &Path, rel: &str) -> Vec<ImageEntry> {
    let dir = match safe_resolve(root, rel).await {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut read = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut jpegs: Vec<(String, String)> = Vec::new();
    let mut raw_by_stem: HashMap<String, String> = HashMap::new();
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
        if ftype.is_file() && is_jpeg(&name) && !is_hidden(&name) {
            jpegs.push((name.clone(), join_rel(rel, &name)));
        } else if ftype.is_file() && is_raw_download_name(&name) {
            raw_by_stem.insert(file_stem_lower(&name), name);
        }
    }
    jpegs
        .into_iter()
        .map(|(name, rel_child)| {
            let raw_download_url = raw_by_stem
                .get(&file_stem_lower(&name))
                .map(|raw| format!("/download/{}", encode_path(&join_rel(rel, raw))));
            ImageEntry {
                thumb_url: format!("/thumb/{}", encode_path(&rel_child)),
                image_url: format!("/image/{}", encode_path(&rel_child)),
                jpg_download_url: format!("/download/{}", encode_path(&rel_child)),
                raw_download_url,
                // The archive galleries download over GET; the POST route
                // belongs to the work delivery pages alone.
                download_action: None,
                dims: None,
                srcset: None,
                name,
            }
        })
        .collect()
}

pub(crate) async fn subtree_has_jpeg(root: &Path) -> bool {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut read = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %dir.display(), error = %e, "read_dir failed in subtree scan");
                continue;
            }
        };
        loop {
            let entry = match read.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    warn!(path = %dir.display(), error = %e, "next_entry failed in subtree scan");
                    break;
                }
            };
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
            if ftype.is_file() && is_jpeg(&name) && !is_hidden(&name) {
                return true;
            }
            if ftype.is_dir() && !is_skipped_dir(&name) {
                stack.push(entry.path());
            }
        }
    }
    false
}

pub async fn people_index(State(state): State<AppState>) -> Response {
    let db = match state.db_path() {
        Some(p) => p.clone(),
        None => return people_unavailable_response(),
    };
    let people_list = match people::list_people(db).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, "listing people failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let entries: Vec<PersonEntry> = people_list.into_iter().map(person_entry).collect();
    views::people_index_page("People", &entries).into_response()
}

pub async fn person_photos(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let db = match state.db_path() {
        Some(p) => p.clone(),
        None => return people_unavailable_response(),
    };
    if name.is_empty() {
        return not_found_response();
    }
    let mut photos = match people::list_person_photos(db, name.clone()).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, person = %name, "listing person photos failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if photos.is_empty() {
        return not_found_response();
    }
    sort_person_photos(state.photos_root(), &mut photos).await;
    // People photos come from the tag DB as flat rel paths, so there's no
    // directory walk to piggyback the raw-sibling lookup on. Scan each
    // distinct album folder once and cache its stem->raw map.
    let mut dir_raws: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut images: Vec<ImageEntry> = Vec::with_capacity(photos.len());
    for p in photos {
        let parent = parent_rel(&p.rel).to_string();
        if !dir_raws.contains_key(&parent) {
            let map = scan_raw_siblings(state.photos_root(), &parent).await;
            dir_raws.insert(parent.clone(), map);
        }
        let raw_download_url = dir_raws
            .get(&parent)
            .and_then(|m| m.get(&file_stem_lower(&p.name)))
            .map(|raw| format!("/download/{}", encode_path(&join_rel(&parent, raw))));
        images.push(ImageEntry {
            thumb_url: format!("/thumb/{}", encode_path(&p.rel)),
            image_url: format!("/image/{}", encode_path(&p.rel)),
            jpg_download_url: format!("/download/{}", encode_path(&p.rel)),
            raw_download_url,
            // The archive galleries download over GET; the POST route
            // belongs to the work delivery pages alone.
            download_action: None,
            dims: None,
            srcset: None,
            name: p.name,
        });
    }
    let crumbs = vec![
        Crumb {
            label: "Home".into(),
            url: Some("/".into()),
        },
        Crumb {
            label: "People".into(),
            url: Some("/people".into()),
        },
        Crumb {
            label: name.clone(),
            url: None,
        },
    ];
    let canonical = format!("/people/{}", encode_path(&name));
    views::page(&name, &canonical, &crumbs, &[], &images, true, views::Nav::People).into_response()
}

/// The roll a photograph belongs to — a year's own child (`2026/utopia`), or the
/// top-level folder when the path carries no year.
///
/// A roll's `favs/` is a separate album in the tag database but the same roll to
/// a reader, so keying on this rather than on the album path keeps the two
/// adjacent and gives them one shared recency, exactly as `/all` renders them.
fn roll_rel(rel: &str) -> &str {
    let album = parent_rel(rel);
    let depth = match album.split('/').next() {
        Some(first) if leading_year(first).is_some() => 2,
        _ => 1,
    };
    match album.match_indices('/').nth(depth - 1) {
        Some((i, _)) => &album[..i],
        None => album,
    }
}

/// Order one person's photographs the way `/all` orders the archive.
///
/// The tag database hands these back in ascending album path, which opens a
/// person's page on their *oldest* photographs — the reverse of every other page
/// on the site. Sorting here rather than in the `ORDER BY` is what lets the key
/// be the same one `/all` uses: `relativePath` carries no date below the year
/// segment, so no amount of SQL ordering on it produces this.
///
/// The key has the same two halves as the folder tree, for the same reason. The
/// year segment is when the photographs were *taken*, so years descend and
/// albums with no year in their path sink below all of them. Inside a year the
/// rolls are ordered by when they were *published* — newest photograph mtime in
/// the roll — because a roll's name says nothing about time. Album path and
/// filename settle the rest, keeping a roll's own `favs/` behind it and each
/// folder's photographs in the sequence they came off the scanner.
///
/// The page stays a flat gallery: ordering by year does not add year headings.
async fn sort_person_photos(photos_root: &Path, photos: &mut [people::PersonPhoto]) {
    // One subtree walk per distinct roll, not per photograph. A person appears
    // in a handful of rolls, so this is a few walks of a few dozen files each.
    let mut recency: HashMap<String, Option<u64>> = HashMap::new();
    for p in photos.iter() {
        let roll = roll_rel(&p.rel);
        if recency.contains_key(roll) {
            continue;
        }
        let newest = match safe_resolve(photos_root, roll).await {
            Ok(abs) => newest_subtree_mtime(&abs).await,
            // A roll the tag database knows about and the filesystem does not:
            // no recency, so it sorts to the back of its year rather than the
            // front, and the photographs still render.
            Err(_) => None,
        };
        recency.insert(roll.to_string(), newest);
    }
    photos.sort_by_cached_key(|p| {
        let album = parent_rel(&p.rel);
        let year = match leading_year(album.split('/').next().unwrap_or("")) {
            Some(y) => (0, std::cmp::Reverse(y)),
            None => (1, std::cmp::Reverse(0)),
        };
        let newest = recency
            .get(roll_rel(&p.rel))
            .copied()
            .flatten()
            .unwrap_or(0);
        (
            year,
            std::cmp::Reverse(newest),
            album.to_lowercase(),
            p.name.to_lowercase(),
        )
    });
}

/// One row of the People page, and one row of the /notify form — the same
/// values, since /notify is a list of the same people with checkboxes on it.
///
/// The face URL is `/face/<name>` and carries no photograph path: which frame a
/// person's tile is cut from is a database answer that can change under a
/// re-tag, and putting it in the URL would publish a path the page does not
/// otherwise link. See [`face`].
fn person_entry(p: people::Person) -> PersonEntry {
    PersonEntry {
        url: format!("/people/{}", encode_path(&p.name)),
        face_url: p
            .face
            .as_ref()
            .map(|_| format!("/face/{}", encode_path(&p.name))),
        initial: person_initial(&p.name),
        name: p.name,
        photo_count: p.photo_count,
    }
}

/// The letter shown in place of a face for someone digiKam has no confirmed
/// face rectangle for. Empty when the name opens with something that has no
/// uppercase form, which the tile then renders as a plain block.
fn person_initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

/// `GET /face/:name` — the square face crop for one person's tile.
///
/// Addressed by person rather than by photograph because that is what the tile
/// is: one face per person — their digiKam tag thumbnail, or their biggest
/// confirmed face — and the page has no
/// business knowing which frame it came out of. The pick is recomputed here per
/// request, the same way every other page reads the database per request, so a
/// re-tag in digiKam shows up as soon as `update_db.sh` has run.
///
/// A bare status rather than the 404 page, like the other asset routes: a
/// client asking for a JPEG has no use for a page of markup.
pub async fn face(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(db) = state.db_path().cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let people_list = match people::list_people(db).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, "listing people for a face failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // No face is a 404 and not an error: three of the people on the page have
    // no confirmed face anywhere in the archive, and their tiles link no image
    // at all. A request for one is a stale page or a guess.
    let Some(face) = people_list
        .into_iter()
        .find(|p| p.name == name)
        .and_then(|p| p.face)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let source = match safe_resolve(state.photos_root(), &face.rel).await {
        Ok(p) => p,
        Err(e) => return map_path_err(e).into_response(),
    };
    let rect = (face.rect.x, face.rect.y, face.rect.w, face.rect.h);
    let cache_name = thumbs::face_cache_name(&name, &face.rel, rect);
    let info = match thumbs::ensure_face(&source, state.cache_root(), rect, &cache_name).await {
        Ok(i) => i,
        Err(e) => {
            // Named by person as well as by path: the fix is in digiKam, on
            // that person's tag, and the path alone does not say whose tile is
            // broken.
            warn!(person = %name, source = %source.display(), error = ?e, "face crop failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let etag = build_etag(info.mtime, info.size);
    if matches_etag(&headers, &etag) {
        return not_modified(&etag);
    }
    match tokio::fs::read(&info.path).await {
        Ok(bytes) => image_response(bytes, &etag),
        Err(e) => {
            warn!(path = %info.path.display(), error = %e, "face read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn people_unavailable_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        "people tag database not available",
    )
        .into_response()
}

pub async fn all_photos(State(state): State<AppState>) -> Response {
    match walk_groups(state.photos_root(), "", ThumbKind::Grid, false, None).await {
        Ok(groups) => {
            let crumbs = vec![
                Crumb {
                    label: "Home".into(),
                    url: Some("/".into()),
                },
                Crumb {
                    label: "All".into(),
                    url: None,
                },
            ];
            views::all_photos_page(&crumbs, &groups).into_response()
        }
        Err(status) => status.into_response(),
    }
}

// ---------------------------------------------------------------------------
// /notify — subscribing to photos of a person
// ---------------------------------------------------------------------------

/// At most this many submissions per client address per hour.
const NOTIFY_RATE_MAX: u32 = 5;
const NOTIFY_RATE_WINDOW_SECS: u64 = 60 * 60;

/// Hard ceiling on confirmed subscriptions. A personal photography site does
/// not have thousands of subjects, so a number far above the plausible audience
/// still bounds the damage if the honeypot and the rate limit are both beaten.
const NOTIFY_MAX_SUBSCRIBERS: usize = 500;

/// Longest accepted contact handle, matching the field's `maxlength`. Checked
/// again here because `maxlength` is advice to a browser, not a constraint on a
/// request.
const NOTIFY_MAX_HANDLE_LEN: usize = 254;

async fn person_entries(state: &AppState) -> Result<Vec<PersonEntry>, Response> {
    let db = match state.db_path() {
        Some(p) => p.clone(),
        None => return Err(people_unavailable_response()),
    };
    match people::list_people(db).await {
        Ok(list) => Ok(list.into_iter().map(person_entry).collect()),
        Err(e) => {
            warn!(error = ?e, "listing people for /notify failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// `GET /notify`. `?person=Name` pre-ticks a row, so the link from a person page
/// lands on a form that is already half filled in.
pub async fn notify_form(
    State(state): State<AppState>,
    AxumQuery(query): AxumQuery<HashMap<String, String>>,
) -> Response {
    let people_list = match person_entries(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let selected: Vec<String> = query
        .get("person")
        .filter(|name| people_list.iter().any(|p| &p.name == *name))
        .cloned()
        .into_iter()
        .collect();
    views::notify_page(&people_list, &selected, false, None).into_response()
}

/// The address to rate-limit against.
///
/// The server binds `127.0.0.1`, so every real request arrives through the
/// reverse proxy and the peer address is always the proxy — `X-Forwarded-For`
/// is the only place the client is named. Trusting a client-settable header is
/// normally wrong; here the header cannot reach the process except through the
/// proxy that rewrites it. Its first entry is the original client.
///
/// With no header at all (a direct request in development) everything shares one
/// bucket, which throttles too much rather than too little.
fn client_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `POST /notify`. Stages the signup and sends exactly one confirmation message;
/// nothing is added to the subscriber log until that link is followed.
pub async fn notify_subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let people_list = match person_entries(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let form = parse_urlencoded_multi(&body);
    let one = |key: &str| -> String {
        form.get(key)
            .and_then(|v| v.last())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let selected: Vec<String> = form.get("person").cloned().unwrap_or_default();
    let all_rolls = !one("all_rolls").is_empty();
    let render = |msg: &str, is_error: bool, status: StatusCode| -> Response {
        (
            status,
            views::notify_page(&people_list, &selected, all_rolls, Some((msg, is_error))),
        )
            .into_response()
    };

    // The honeypot is invisible to people, so a filled one is a bot. Answering
    // with the same success page it would have got tells it nothing about why
    // nothing happened.
    if !one("website").is_empty() {
        return render(views::NOTIFY_SENT_MSG, false, StatusCode::OK);
    }

    if !state.notify_limiter().allow(
        &client_key(&headers),
        NOTIFY_RATE_MAX,
        NOTIFY_RATE_WINDOW_SECS,
    ) {
        return render(
            views::NOTIFY_ERR_RATE,
            true,
            StatusCode::TOO_MANY_REQUESTS,
        );
    }

    let Some(channel) = notify::Channel::parse(&one("channel")) else {
        return render(views::NOTIFY_ERR_CHANNEL, true, StatusCode::BAD_REQUEST);
    };

    // Both handle fields are submitted when JavaScript is off; take the one the
    // chosen channel needs and ignore the other.
    let handle = match channel {
        notify::Channel::Email => one("handle_email"),
        notify::Channel::Discord => one("handle_discord"),
    };
    if handle.len() > NOTIFY_MAX_HANDLE_LEN || !channel.handle_looks_valid(&handle) {
        return render(
            match channel {
                notify::Channel::Email => views::NOTIFY_ERR_EMAIL,
                notify::Channel::Discord => views::NOTIFY_ERR_DISCORD,
            },
            true,
            StatusCode::BAD_REQUEST,
        );
    }

    // Following every roll is a subscription in its own right, so it is the one
    // case where naming nobody is still a valid choice.
    if selected.is_empty() && !all_rolls {
        return render(views::NOTIFY_ERR_NO_PEOPLE, true, StatusCode::BAD_REQUEST);
    }
    // Every name has to be one this site actually publishes. The form only ever
    // offers real tags, so a name that is not in the list came from a
    // hand-built request.
    if selected
        .iter()
        .any(|name| !people_list.iter().any(|p| &p.name == name))
    {
        return render(views::NOTIFY_ERR_UNKNOWN_PERSON, true, StatusCode::BAD_REQUEST);
    }

    let data_root = state.data_root();
    let sender = match notify::Sender::load(data_root).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "building notify sender failed");
            return render(views::NOTIFY_UNAVAILABLE_MSG, true, StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Refused rather than stored: a subscription on a channel that cannot even
    // deliver its own confirmation would sit in `pending.log` forever, and the
    // person would be left believing they had signed up.
    if !sender.configured(channel) {
        return render(views::NOTIFY_UNAVAILABLE_MSG, true, StatusCode::SERVICE_UNAVAILABLE);
    }

    if notify::current_subscriptions(data_root).await.len() >= NOTIFY_MAX_SUBSCRIBERS {
        return render(views::NOTIFY_UNAVAILABLE_MSG, true, StatusCode::SERVICE_UNAVAILABLE);
    }

    let token = match notify::stage_pending(data_root, channel, &handle, &selected, all_rolls).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, "staging a notify signup failed");
            return render(views::NOTIFY_UNAVAILABLE_MSG, true, StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let confirm_url = views::abs_url(&format!("/notify/confirm?t={token}"));
    let (subject, message) = notify::compose_confirmation(&selected, all_rolls, &confirm_url);
    if let Err(e) = sender.send(channel, &handle, &subject, &message).await {
        // The pending row stays. It expires on its own, and re-submitting the
        // form issues a fresh token, so a transient failure here costs the
        // person one retry rather than locking them out.
        warn!(error = ?e, channel = channel.as_str(), "sending a confirmation failed");
        return render(views::NOTIFY_UNDELIVERABLE_MSG, true, StatusCode::BAD_GATEWAY);
    }
    render(views::NOTIFY_SENT_MSG, false, StatusCode::OK)
}

/// `GET /notify/confirm?t=…`. Following the link is what actually subscribes.
pub async fn notify_confirm(
    State(state): State<AppState>,
    AxumQuery(query): AxumQuery<HashMap<String, String>>,
) -> Response {
    let people_list = match person_entries(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let token = query.get("t").map(String::as_str).unwrap_or_default();
    match notify::confirm(state.data_root(), token).await {
        // Both of these say the same thing, because from the subscriber's side
        // they are the same thing: the link worked, and this is what they now
        // get. Re-rendered with their choices ticked, so the page doubles as
        // the place to change them.
        Ok(notify::Confirmation::Confirmed(sub))
        | Ok(notify::Confirmation::AlreadyConfirmed(sub)) => views::notify_page(
            &people_list,
            &sub.people,
            sub.all_rolls,
            Some((&notify::subscription_sentence(&sub.people, sub.all_rolls), false)),
        )
        .into_response(),
        Ok(notify::Confirmation::Unknown) => (
            StatusCode::NOT_FOUND,
            views::notify_page(&people_list, &[], false, Some((views::NOTIFY_BAD_LINK_MSG, true))),
        )
            .into_response(),
        Err(e) => {
            warn!(error = ?e, "confirming a subscription failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                views::notify_page(
                    &people_list,
                    &[],
                    false,
                    Some((views::NOTIFY_UNAVAILABLE_MSG, true)),
                ),
            )
                .into_response()
        }
    }
}


/// The parent folder of a `favs/` directory, or `None` for anything else.
///
/// Case-insensitive on the last segment, matching how `render_dir` and
/// `walk_groups` already recognise the folder.
fn favs_parent(path: &str) -> Option<&str> {
    let (parent, last) = path.rsplit_once('/')?;
    last.eq_ignore_ascii_case("favs").then_some(parent)
}

/// Fold each `favs/` group into the roll it belongs to, favorites first.
///
/// `walk_groups` emits one group per directory holding photos, so a roll with a
/// `favs/` subfolder arrives as two sections — which is right for `/all`, where
/// the point is to mirror the folder tree, and wrong for `/recent`, where the
/// point is one section per roll. Merging keeps the favorites at the front and
/// records how many there are, so `image_grid` can draw the line between them
/// and the rest instead of splitting the grid in two.
///
/// One grid rather than two matters beyond looks: `lightbox.js` builds its
/// next/previous ring per `ul.grid`, so a second list would strand a viewer at
/// the last favorite instead of carrying them on into the roll.
///
/// A `favs/` folder whose parent holds no photos of its own has no group to
/// merge into and is left standing alone — there is no roll for it to lead.
fn fold_favs_into_rolls(groups: Vec<FolderGroup>) -> Vec<FolderGroup> {
    let mut out: Vec<FolderGroup> = Vec::with_capacity(groups.len());
    for group in groups {
        // The walk is a pre-order DFS, so a folder always precedes its
        // children and the parent is already in `out` by the time its `favs/`
        // is reached.
        let parent = favs_parent(&group.path).map(str::to_string);
        match parent.and_then(|p| out.iter_mut().find(|g| g.path == p)) {
            Some(roll) => {
                let mut merged = group.images;
                roll.favs_count += merged.len();
                merged.append(&mut roll.images);
                roll.images = merged;
            }
            None => out.push(group),
        }
    }
    out
}

/// `/recent` — only the folders named in `photos/.recent`, in that file's order.
///
/// One `walk_groups` per declared folder rather than one walk of the whole tree
/// with a filter: the set is a handful of folders, and walking each directly
/// costs nothing for the folders that are not in it.
///
/// `"preview"` rather than `"thumb"`: these sections render as natural-ratio
/// masonry, whose tiles carry no CSS `aspect-ratio` and so need `ImageEntry::dims`
/// to reserve their space — and `walk_groups` only fills those in for the preview
/// rendition. Passing `"thumb"` here would collapse the whole grid into the
/// viewport and defeat `loading="lazy"`.
pub async fn recent_photos(State(state): State<AppState>) -> Response {
    let folders = recent::load(state.photos_root()).await;
    let mut groups: Vec<FolderGroup> = Vec::new();
    for folder in &folders {
        // Grid for the tile, Medium as the second candidate: /recent's tiles are
        // 252-327 CSS px, so 400px is right for a 1x screen and half what a 2x
        // one wants. See `views::GRID_SIZES`.
        match walk_groups(
            state.photos_root(),
            folder,
            ThumbKind::Grid,
            true,
            Some(ThumbKind::Medium),
        )
        .await
        {
            Ok(mut g) => groups.append(&mut g),
            // One unreadable folder drops out of the page instead of 500ing the
            // whole drop, matching how `recent::load` treats an entry that no
            // longer resolves.
            Err(status) => {
                warn!(folder = %folder, ?status, "walking a recent folder failed; skipping")
            }
        }
    }
    views::recent_page(&fold_favs_into_rolls(groups)).into_response()
}

/// Pre-order DFS from `root`/`start_rel`, emitting one `FolderGroup` per
/// directory that directly contains JPEGs. `start_rel` is relative to `root`
/// (empty = the whole photos tree, as `/all` uses); paths and URLs stay
/// rooted at `root` so thumbnail/image links resolve regardless of where the
/// walk starts.
///
/// `kind` selects which rendition the displayed tile loads — [`ThumbKind::Grid`]
/// (400px) for dense listings, [`ThumbKind::Preview`] (1600px) where tiles render
/// larger. `natural_ratio` says whether the page lays those tiles out at their
/// own aspect ratio, which decides whether intrinsic dimensions get read: the
/// square grids reserve their space in CSS and need none, so /all is spared 800+
/// header reads it would never use. `srcset_kind` adds one larger candidate so a
/// high-density screen can take a sharper file than `kind` alone would give it;
/// it is only consulted when `natural_ratio` is set, since that is when the
/// dimensions the descriptors need are read at all.
///
/// Only `/all` and `/recent` walk the tree; the home page is driven by the
/// `portfolio/*` tags instead.
async fn walk_groups(
    root: &Path,
    start_rel: &str,
    kind: ThumbKind,
    natural_ratio: bool,
    srcset_kind: Option<ThumbKind>,
) -> Result<Vec<FolderGroup>, StatusCode> {
    let start_abs = if start_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(start_rel)
    };
    let mut stack: Vec<(PathBuf, String)> = vec![(start_abs, start_rel.to_string())];
    let mut groups: Vec<FolderGroup> = Vec::new();
    // Absolute source path of every image pushed into `groups`, kept in the
    // same flattened order so `fill_image_dims` can zip the two together
    // without having to reverse the URL encoding.
    let mut sources: Vec<PathBuf> = Vec::new();

    while let Some((abs, rel)) = stack.pop() {
        let mut read = match tokio::fs::read_dir(&abs).await {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %abs.display(), error = %e, "read_dir failed during walk");
                continue;
            }
        };

        let mut jpegs: Vec<(String, String)> = Vec::new();
        let mut raw_by_stem: HashMap<String, String> = HashMap::new();
        let mut child_dirs: Vec<(PathBuf, String)> = Vec::new();

        loop {
            let entry = match read.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
            };
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
            let rel_child = join_rel(&rel, &name);
            if ftype.is_dir() {
                if is_skipped_dir(&name) || is_work_root(&rel, &name) {
                    continue;
                }
                child_dirs.push((entry.path(), rel_child));
            } else if ftype.is_file() && is_jpeg(&name) && !is_hidden(&name) {
                jpegs.push((name, rel_child));
            } else if ftype.is_file() && is_raw_download_name(&name) {
                raw_by_stem.insert(file_stem_lower(&name), name);
            }
        }

        let mut images: Vec<ImageEntry> = jpegs
            .into_iter()
            .map(|(name, rel_child)| {
                let raw_download_url = raw_by_stem
                    .get(&file_stem_lower(&name))
                    .map(|raw| format!("/download/{}", encode_path(&join_rel(&rel, raw))));
                ImageEntry {
                    thumb_url: format!("/{}/{}", kind.route(), encode_path(&rel_child)),
                    image_url: format!("/image/{}", encode_path(&rel_child)),
                    jpg_download_url: format!("/download/{}", encode_path(&rel_child)),
                    raw_download_url,
                    // The archive galleries download over GET; the POST route
                    // belongs to the work delivery pages alone.
                    download_action: None,
                    dims: None,
                    srcset: None,
                    name,
                }
            })
            .collect();

        if !images.is_empty() {
            images.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            let (label, browse_url) = if rel.is_empty() {
                ("Photos (root)".to_string(), "/browse".to_string())
            } else {
                (rel.clone(), format!("/browse/{}", encode_path(&rel)))
            };
            let files: Vec<PathBuf> = images.iter().map(|img| abs.join(&img.name)).collect();
            let newest_mtime = newest_mtime_secs(files.clone()).await;
            sources.extend(files);
            groups.push(FolderGroup {
                label,
                path: rel.clone(),
                browse_url,
                images,
                favs_count: 0,
                newest_mtime,
            });
        }

        // Push children in reverse alphabetical order so the stack pops them in
        // alphabetical order, producing a pre-order DFS where each folder is
        // immediately followed by its descendants.
        child_dirs.sort_by(|a, b| b.1.to_lowercase().cmp(&a.1.to_lowercase()));
        for child in child_dirs {
            stack.push(child);
        }
    }

    // A natural-ratio masonry's tiles carry no CSS aspect-ratio. Without
    // intrinsic dimensions every <img> lays out at zero height, so the browser
    // thinks the entire grid is in the viewport and `loading="lazy"` fetches all
    // of it up front — which is how dropping these would undo the whole point of
    // serving a smaller rendition. Fill them in one blocking batch:
    // `rendition_dimensions` only reads JPEG/EXIF headers, no decode.
    if natural_ratio {
        fill_image_dims(&sources, &mut groups, kind, srcset_kind).await;
    }

    Ok(groups)
}

/// Newest mtime among `files`, in seconds since the epoch, or `None` if not one
/// of them could be stat'd.
///
/// This is what "recently changed" means on this site. The alternative was the
/// containing directory's own mtime, which is one stat instead of many but
/// records the wrong event: a `rsync` without `-t`, a re-copy of the archive, or
/// a `chmod` sweep rewrites every directory mtime to the same instant and leaves
/// an ordering that looks deliberate and is not. Photo mtimes survive all three.
///
/// A file that cannot be stat'd is skipped rather than failing the batch: a
/// folder should lose a little ordering accuracy over one unreadable JPEG, not
/// the whole page. Taking the vector by value rather than by slice is what lets
/// the stats run on a blocking thread.
async fn newest_mtime_secs(files: Vec<PathBuf>) -> Option<u64> {
    if files.is_empty() {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .filter_map(|m| m.modified().ok())
            .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .max()
    })
    .await
    .unwrap_or(None)
}

/// Newest photograph mtime anywhere under `dir`, by the same rules the tree walk
/// uses — only visible JPEGs count, and skipped directories are not descended
/// into — so a roll gets the same recency here as it does on `/all`.
async fn newest_subtree_mtime(dir: &Path) -> Option<u64> {
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    let mut newest: Option<u64> = None;
    while let Some(dir) = stack.pop() {
        let mut read = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %dir.display(), error = %e, "read_dir failed in mtime scan");
                continue;
            }
        };
        let mut files: Vec<PathBuf> = Vec::new();
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
            if ftype.is_file() && is_jpeg(&name) && !is_hidden(&name) {
                files.push(entry.path());
            } else if ftype.is_dir() && !is_skipped_dir(&name) {
                stack.push(entry.path());
            }
        }
        newest = newest.max(newest_mtime_secs(files).await);
    }
    newest
}

/// Populate `ImageEntry::dims` — and, when `srcset_kind` is set, the second
/// `srcset` candidate — for every image in `groups`, off the async runtime since
/// the underlying reads are synchronous file I/O. `sources` is the absolute path
/// of each image, in the same order the images appear when `groups` is
/// flattened. `kind` must be the rendition those images are linked at, so the
/// dimensions describe the file the browser will fetch. Photos whose headers
/// can't be read keep `None` and simply render without the attributes.
///
/// The two renditions share one `oriented_dimensions` call per photo:
/// `scale_to` is arithmetic, so a second candidate costs no extra disk read.
async fn fill_image_dims(
    sources: &[PathBuf],
    groups: &mut [FolderGroup],
    kind: ThumbKind,
    srcset_kind: Option<ThumbKind>,
) {
    if sources.is_empty() {
        return;
    }
    let sources = sources.to_vec();
    let dims = match tokio::task::spawn_blocking(move || {
        sources
            .into_iter()
            .map(|p| {
                let oriented = thumbs::oriented_dimensions(&p).ok()?;
                Some((
                    thumbs::scale_to(oriented, kind.max_dim()),
                    srcset_kind.map(|k| thumbs::scale_to(oriented, k.max_dim()).0),
                ))
            })
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "image dimension batch panicked");
            return;
        }
    };
    for (img, dim) in groups
        .iter_mut()
        .flat_map(|g| g.images.iter_mut())
        .zip(dims)
    {
        let Some((scaled, big_w)) = dim else { continue };
        img.dims = Some(scaled);
        // The larger candidate is the same photo under a different route, so the
        // URL is the linked one with its prefix swapped — `thumb_url` is always
        // built as `/{route}/{encoded rel}`, so the tail already carries the
        // encoding the other route wants.
        //
        // Skipped when the larger candidate is not actually larger, which is the
        // case for any source already smaller than `kind`'s cap: `scale_to`
        // never enlarges, so both renditions are the same bytes and a `srcset`
        // offering two identical widths tells the browser nothing.
        img.srcset = match (srcset_kind, big_w) {
            (Some(k), Some(w)) if w > scaled.0 => {
                swap_rendition_route(&img.thumb_url, kind, k).map(|u| (u, w))
            }
            _ => None,
        };
    }
}

/// `/thumb/2026/a.jpg` -> `/medium/2026/a.jpg`. `None` if `url` is not under
/// `from`'s route, which yields a tile with no `srcset` rather than a 404.
fn swap_rendition_route(url: &str, from: ThumbKind, to: ThumbKind) -> Option<String> {
    let rel = url.strip_prefix(&format!("/{}/", from.route()))?;
    Some(format!("/{}/{}", to.route(), rel))
}

pub async fn image(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let path = match safe_resolve(state.photos_root(), &rel).await {
        Ok(p) => p,
        Err(e) => return map_path_err(e).into_response(),
    };
    if !is_jpeg(&rel) || rel_filename_is_hidden(&rel) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) if m.is_file() => m,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => SystemTime::now(),
    };
    let etag = build_etag(mtime, meta.len());
    if matches_etag(&headers, &etag) {
        return not_modified(&etag);
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "image read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    image_response(bytes, &etag)
}

/// Per-photo download: GET /download/*path. Streams the file as an attachment
/// (Content-Disposition), unlike `/image` which serves inline. Serves both the
/// JPEG and its sibling raw/edit-master files (the URLs are minted by the
/// gallery views), so the allowlist here is JPEG ∪ raw extensions. Hidden
/// files stay unreachable, matching `/image`.
pub async fn download(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    let path = match safe_resolve(state.photos_root(), &rel).await {
        Ok(p) => p,
        Err(e) => return map_path_err(e).into_response(),
    };
    let basename = match Path::new(&rel).file_name().and_then(|s| s.to_str()) {
        Some(b) => b,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let is_jpeg_file = is_jpeg(basename);
    if (!is_jpeg_file && !is_raw_download_name(basename)) || is_hidden(basename) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::metadata(&path).await {
        Ok(m) if m.is_file() => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    let mime = if is_jpeg_file {
        "image/jpeg"
    } else {
        "application/octet-stream"
    };
    stream_and_log(
        &state,
        &path,
        mime,
        basename,
        audit::Download::public(rel.clone()),
    )
    .await
}

pub async fn thumb(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_thumb_response(&state, &rel, &headers, ThumbKind::Grid).await
}

pub async fn medium(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_thumb_response(&state, &rel, &headers, ThumbKind::Medium).await
}

pub async fn preview(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_thumb_response(&state, &rel, &headers, ThumbKind::Preview).await
}

pub async fn wide(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_thumb_response(&state, &rel, &headers, ThumbKind::Wide).await
}

async fn render_thumb_response(
    state: &AppState,
    rel: &str,
    headers: &HeaderMap,
    kind: ThumbKind,
) -> Response {
    let source = match safe_resolve(state.photos_root(), rel).await {
        Ok(p) => p,
        Err(e) => return map_path_err(e).into_response(),
    };
    if !is_jpeg(rel) || rel_filename_is_hidden(rel) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let info =
        match thumbs::ensure_thumb(&source, state.photos_root(), state.cache_root(), kind).await {
            Ok(i) => i,
            Err(e) => {
                warn!(source = %source.display(), error = ?e, "thumbnail failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    let etag = build_etag(info.mtime, info.size);
    if matches_etag(headers, &etag) {
        return not_modified(&etag);
    }
    let bytes = match tokio::fs::read(&info.path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %info.path.display(), error = %e, "thumb read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    image_response(bytes, &etag)
}

/// A 304 that echoes the validator and freshness of the 200 it stands in for.
/// A bare `StatusCode::NOT_MODIFIED.into_response()` carries no headers at
/// all, and per RFC 9111 §4.3.4 a cache *updates its stored entry's headers*
/// from the 304 — so a middleware that stamps `Cache-Control` onto a bare 304
/// silently rewrites the cached image's `max-age`. Sending the real values
/// here makes that whole class of bug impossible.
pub(crate) fn not_modified(etag: &str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .header(header::ETAG, etag)
        .body(Body::empty())
        .unwrap()
}

fn image_response(bytes: Vec<u8>, etag: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .header(header::ETAG, etag)
        .body(Body::from(bytes))
        .unwrap()
}

fn map_path_err(e: PathError) -> StatusCode {
    match e {
        PathError::NotFound => StatusCode::NOT_FOUND,
        PathError::Invalid | PathError::Escape => StatusCode::NOT_FOUND,
    }
}

pub(crate) fn is_jpeg(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false)
}

/// Extensions offered as the "RAW" download beside a gallery photo. This is
/// work's raw/edit-master set (camera raws + PSD/PSB) plus TIFF, which film
/// scans and edit masters ship as alongside the JPEG here (the `positive`
/// folders pair `*.jpg` with `*.tif`). Kept distinct from `work::is_raw_name`
/// so the work-item zip logic is unaffected.
fn is_raw_download_name(name: &str) -> bool {
    work::is_raw_name(name)
        || Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
            .unwrap_or(false)
}

/// Lowercased file stem (basename without its final extension), used to pair a
/// JPEG with a same-named raw sibling regardless of case, e.g. `Homer.JPG`
/// matches `Homer.psd`.
fn file_stem_lower(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Forward-slash parent of a root-relative path, or "" for a file at the root.
fn parent_rel(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

/// Read `dir_rel` (relative to the photos root) and return a map from each raw
/// file's lowercased stem to its filename, so a JPEG in that folder can find
/// its raw sibling. Returns empty on any error (missing dir, etc).
async fn scan_raw_siblings(root: &Path, dir_rel: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let dir = match safe_resolve(root, dir_rel).await {
        Ok(p) => p,
        Err(_) => return map,
    };
    let mut read = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(_) => return map,
    };
    while let Ok(Some(entry)) = read.next_entry().await {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        match entry.file_type().await {
            Ok(ft) if ft.is_file() && is_raw_download_name(&name) => {
                map.insert(file_stem_lower(&name), name);
            }
            _ => {}
        }
    }
    map
}

/// A file is "hidden" if its basename contains the substring "hidden"
/// (case-insensitive). Applied on top of the .jpg/.jpeg filter.
pub(crate) fn is_hidden(name: &str) -> bool {
    name.to_ascii_lowercase().contains("hidden")
}

/// Directories the lister and subtree scanners should pretend don't exist.
pub(crate) fn is_skipped_dir(name: &str) -> bool {
    name.eq_ignore_ascii_case("negative")
}

/// The "work" directory at the photos root is reserved for the client-delivery
/// area (`/work/...`) and must not appear in the regular browse/all listings.
/// Nested folders named "work" lower in the tree are unaffected.
pub(crate) fn is_work_root(parent_rel: &str, name: &str) -> bool {
    parent_rel.is_empty() && name == "work"
}

fn rel_filename_is_hidden(rel: &str) -> bool {
    Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .map(is_hidden)
        .unwrap_or(false)
}

fn join_rel(parent: &str, child: &str) -> String {
    let p = parent.trim_end_matches('/');
    if p.is_empty() {
        child.to_string()
    } else {
        format!("{p}/{child}")
    }
}

pub(crate) fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/' => out.push(b as char),
            _ => write!(out, "%{:02X}", b).unwrap(),
        }
    }
    out
}

pub(crate) fn build_etag(mtime: SystemTime, size: u64) -> String {
    let secs = mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", secs, size)
}

pub(crate) fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
}

fn breadcrumbs(rel: &str, kind: PageKind) -> Vec<Crumb> {
    let mut out = Vec::new();
    match kind {
        PageKind::BrowseRoot => {
            out.push(Crumb {
                label: "Home".into(),
                url: Some("/".into()),
            });
            out.push(Crumb {
                label: "Browse".into(),
                url: None,
            });
            return out;
        }
        PageKind::BrowseSub => {
            out.push(Crumb {
                label: "Home".into(),
                url: Some("/".into()),
            });
            out.push(Crumb {
                label: "Browse".into(),
                url: Some("/browse".into()),
            });
        }
    }
    let mut acc = String::new();
    let parts: Vec<_> = rel.split('/').filter(|s| !s.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let is_last = i == parts.len() - 1;
        if is_last {
            out.push(Crumb {
                label: (*part).to_string(),
                url: None,
            });
        } else {
            out.push(Crumb {
                label: (*part).to_string(),
                url: Some(format!("/browse/{}", encode_path(&acc))),
            });
        }
    }
    out
}

pub async fn work_index(State(state): State<AppState>) -> Response {
    let work_list = match work::list_work(state.photos_root().clone()).await {
        Ok(j) => j,
        Err(e) => {
            warn!(error = ?e, "listing work failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // A cover is a garnish on a filesystem feature: no database, an unreadable
    // one, or nothing tagged all mean "no covers" and never a failed page.
    let thumbs = match state.db_path() {
        Some(db) => match work::list_thumbnails(db.clone()).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = ?e, "listing work thumbnails failed");
                std::collections::HashMap::new()
            }
        },
        None => std::collections::HashMap::new(),
    };
    let entries: Vec<WorkIndexEntry> = work_list
        .into_iter()
        .map(|j| WorkIndexEntry {
            url: format!("/work/{}", encode_path(&j.name)),
            // Raw on disk and in the URL above, readable on the card. Which of
            // the two a card shows is `views::work_display_name`, in the view
            // rather than here so the index and the delivery page cannot answer
            // it differently.
            title: j.title,
            // The preview rendition, not the 400px grid one. The cover runs
            // the full width of a card, which is around 500px on a desktop —
            // the grid rendition would be upscaled and visibly soft there. The
            // card still sets its own box and crops to it, so there is nothing
            // for a larger candidate or an intrinsic size to decide.
            thumb_url: thumbs
                .get(&j.name)
                .map(|rel| format!("/preview/{}", encode_path(rel))),
            name: j.name,
            jpeg_count: j.jpeg_count,
            raw_count: j.raw_count,
        })
        .collect();
    views::work_index_page("Work", &entries).into_response()
}

/// One browsable set inside a job: an edited folder, or the job root.
///
/// `slug` is empty until [`assign_set_slugs`] runs, which needs every label
/// first — a slug has to be unique across the job to address a tab, and
/// uniqueness is not a property any single label has.
struct WorkSet {
    label: String,
    slug: String,
    photos: Vec<work::WorkPhoto>,
}

/// Give every set a unique `[a-z0-9-]` slug derived from its label.
///
/// Two labels can slug the same — `medium format positive` and
/// `medium-format/positive` both reduce to `medium-format-positive` — and a tab
/// addressed by an ambiguous slug would serve the wrong set. Collisions take a
/// `-2`, `-3` suffix in section order, which is stable as long as the folders
/// are. A label with no ASCII alphanumerics at all slugs to nothing, hence the
/// `set` fallback; it then collides with any other such label and gets numbered
/// the same way.
fn assign_set_slugs(sets: &mut [WorkSet]) {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in sets.iter_mut() {
        let base = match portfolio::slug(&s.label) {
            b if b.is_empty() => "set".to_string(),
            b => b,
        };
        let mut candidate = base.clone();
        let mut n = 2;
        while !used.insert(candidate.clone()) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        s.slug = candidate;
    }
}

/// `?set=<slug>` picks which of the job's sets the page shows, defaulting to the
/// first. A query rather than a path segment because a set is a view of one job
/// rather than a resource of its own — and because `/work/:name/*` is already
/// spoken for by `auth`, `download` and `file`, which a new capture would have
/// to be kept clear of forever.
pub async fn work_detail(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    AxumQuery(query): AxumQuery<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let set = query.get("set").map(String::as_str);
    // Set by the redirect `work_auth` issues on success, and by nothing else.
    let open_downloads = query.contains_key("downloads");
    render_work_page(
        &state,
        &name,
        set,
        &headers,
        None,
        StatusCode::OK,
        open_downloads,
    )
    .await
}

/// `set` is the requested tab slug, or `None` for the first set. The error
/// re-render paths pass `None`: an auth failure puts the visitor back at the top
/// of the page with a banner, where which set was showing no longer applies.
async fn render_work_page(
    state: &AppState,
    name: &str,
    set: Option<&str>,
    headers: &HeaderMap,
    error: Option<&str>,
    status: StatusCode,
    // `open_downloads` is true on the redirect after a password was accepted,
    // which opens the download panel on arrival. See `work_auth`.
    open_downloads: bool,
) -> Response {
    if !is_valid_work_name(name) {
        return not_found_response();
    }
    let detail = match work::read_work(state.photos_root().clone(), name.to_string()).await {
        Ok(Some(d)) => d,
        Ok(None) => return not_found_response(),
        Err(e) => {
            warn!(error = ?e, "reading work failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // Authorize against the .password file via the request cookie. Without a
    // valid cookie we suppress the per-photo download URLs so the lightbox
    // button stays hidden, and the view will render the password prompt.
    let stored_pw = match work::read_password(state.photos_root().clone(), name.to_string()).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, "reading work password failed");
            None
        }
    };
    let authorized = match &stored_pw {
        Some(pw) => cookie_authorizes(headers, name, pw),
        None => false,
    };
    // Browsable sets, in the order `read_work` sorted the sections: the edited
    // folders, plus any photographs sitting at the job root. An `original`
    // folder is the same frames unedited and is download-only — see
    // `views::work_page`. `total_jpeg_count` still counts every JPEG in the job,
    // which is what separates "nothing here yet" from "nothing browsable here".
    let display_name = views::work_display_name(name, detail.title.as_deref());
    let mut total_jpeg_count: u32 = 0;
    let mut sets: Vec<WorkSet> = Vec::new();
    for s in detail.sections {
        total_jpeg_count += s.photos.len() as u32;
        if !(s.is_edited || s.label.is_empty()) {
            continue;
        }
        sets.push(WorkSet {
            // The job's display name is the fallback label, so a job whose
            // photographs sit directly in `edited/` gets a tab reading what the
            // client calls the job rather than what the folder is called.
            label: views::work_set_label(&s.label, &display_name),
            // Filled in below, once every label is known.
            slug: String::new(),
            photos: s.photos,
        });
    }
    assign_set_slugs(&mut sets);
    // The requested tab, or the first — which `read_work` has already sorted to
    // be the job-root section if there is one, then `edited` ahead of its
    // siblings. An unknown or stale slug falls back rather than 404ing: it is a
    // view of a page that exists, and a client following an old link should get
    // their photographs rather than an error.
    let active = set
        .and_then(|want| sets.iter().position(|s| s.slug == want))
        .unwrap_or(0);
    let tabs: Vec<SectionTab> = sets
        .iter()
        .enumerate()
        .map(|(i, s)| SectionTab {
            label: s.label.clone(),
            // Slugs are `[a-z0-9-]` by construction, so nothing here needs
            // encoding beyond the job name.
            url: format!("/work/{}?set={}", encode_path(name), s.slug),
            active: i == active,
        })
        .collect();
    // Only the active set is built. The job on disk has 302 photographs in one
    // of its sets; rendering all of them to show one is work nobody asked for.
    let images: Vec<ImageEntry> = sets
        .get(active)
        .map(|s| {
            s.photos
                .iter()
                .map(|p| ImageEntry {
                    // The tile grid wants the preview rendition, not the
                    // original: `dims` below are the preview's, and the
                    // full-size file is what the lightbox swaps in.
                    thumb_url: format!("/preview/{}", encode_path(&p.rel)),
                    image_url: format!("/image/{}", encode_path(&p.rel)),
                    // `download_action`, not `jpg_download_url`: this route
                    // is a POST (auth rides on the job cookie), so it drives
                    // the lightbox's form-submitting Download button rather
                    // than its GET JPG/RAW menu. `None` pre-auth, which hides
                    // the button. See `views::ImageEntry::download_action`.
                    download_action: if authorized {
                        Some(format!(
                            "/work/{}/file/{}",
                            encode_path(name),
                            encode_path(&p.subpath)
                        ))
                    } else {
                        None
                    },
                    // The GET download menu belongs to the archive galleries;
                    // a work photo has no GET route to offer, per-JPEG or
                    // per-RAW. The bulk bar's RAW column is the only way to a
                    // raw file here.
                    jpg_download_url: String::new(),
                    raw_download_url: None,
                    name: views::humanize(&p.name),
                    dims: p.preview_dims,
                    // One rendition per work photo, so no second candidate to
                    // offer and nothing for a `sizes` attribute to choose from.
                    srcset: None,
                })
                .collect()
        })
        .unwrap_or_default();
    let bulk_action = format!("/work/{}/download", encode_path(name));
    let auth_action = format!("/work/{}/auth", encode_path(name));
    let body = views::work_page(
        name,
        detail.title.as_deref(),
        &tabs,
        &images,
        total_jpeg_count,
        detail.counts,
        detail.has_password,
        authorized,
        &bulk_action,
        &auth_action,
        error,
        open_downloads,
    );
    // This page renders materially different markup pre- vs post-auth, keyed
    // off the path-scoped auth cookie. Without `Vary: Cookie` a shared cache
    // could store the unlocked variant and hand it to a different visitor.
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(header::VARY, header::HeaderValue::from_static("Cookie"));
    resp
}

/// Wrong passwords accepted for one job before it stops answering.
///
/// Five, and then nothing is checked until the oldest of them ages out of
/// [`WORK_AUTH_WINDOW_SECS`] — a rolling window rather than a latch, so a client
/// who mistypes five times gets their gallery back on their own rather than
/// having to ask for it.
///
/// **Per job, and the cost of that is worth stating.** A stranger who guesses
/// five times locks the real client out for the rest of the window. Per visitor
/// would be fairer, and the site cannot do it: it keeps no visitor log by
/// design, and a limit kept in a cookie is one a guesser clears. A quarter hour
/// of collateral is the price of not starting to log the people who visit.
const WORK_AUTH_MAX_TRIES: usize = 5;

/// How long a wrong password counts against [`WORK_AUTH_MAX_TRIES`].
const WORK_AUTH_WINDOW_SECS: u64 = 15 * 60;

/// Verify the submitted password and issue a path-scoped cookie. On success
/// we 303-redirect back to the job page so the GET handler can re-render
/// with the authorized state visible. On failure we re-render the page with
/// an error banner.
pub async fn work_auth(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_valid_work_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Checked before the password is even read: past the limit this route has no
    // opinion about what was submitted, which is the point of having one.
    if audit::recent_auth_failures(state.data_root(), &name, WORK_AUTH_WINDOW_SECS).await
        >= WORK_AUTH_MAX_TRIES
    {
        return render_work_page(
            &state,
            &name,
            None,
            &headers,
            // Deliberately not recorded. Counting a refused attempt would push
            // the window forward on every retry and turn a fifteen-minute pause
            // into a lockout with no end.
            Some(views::WORK_ERR_RATE),
            StatusCode::TOO_MANY_REQUESTS,
            false,
        )
        .await;
    }
    let form = parse_urlencoded(&body);
    let submitted = form.get("password").cloned().unwrap_or_default();
    let stored = match work::read_password(state.photos_root().clone(), name.clone()).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return render_work_page(
                &state,
                &name,
                None,
                &headers,
                Some("Downloads are locked — no password is set for this work item."),
                StatusCode::FORBIDDEN,
                false,
            )
            .await;
        }
        Err(e) => {
            warn!(error = ?e, "reading work password failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !work::verify(&stored, &submitted) {
        // Counted, not identified: the report wants "is someone guessing at
        // this job", and a per-job tally answers that without a visitor log.
        audit::record_auth_failure(state.data_root(), &name).await;
        return render_work_page(
            &state,
            &name,
            None,
            &headers,
            Some("Incorrect password."),
            StatusCode::UNAUTHORIZED,
            false,
        )
        .await;
    }
    let set_cookie = build_work_cookie(&name, &submitted);
    // `?downloads=1` so the panel is open when the page comes back: the client
    // typed a password to reach the files, and making them press Download after
    // it was accepted is a step that asks nothing and answers nothing. The tab
    // links carry no such parameter, so it lasts exactly one page view.
    let redirect_to = format!("/work/{}?downloads=1", encode_path(&name));
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, redirect_to)
        .header(header::SET_COOKIE, set_cookie)
        .body(Body::empty())
        .unwrap()
}

/// Bulk download: POST /work/:name/download with form field `kind=jpeg|raw`.
/// Auth comes from the path-scoped cookie set by `work_auth`; this handler
/// never accepts a password directly. Streams the cached zip on success.
pub async fn work_download(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_valid_work_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let form = parse_urlencoded(&body);
    let kind = match form.get("kind").and_then(|s| DownloadKind::parse(s)) {
        Some(k) => k,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let scope = match form.get("scope").and_then(|s| Scope::parse(s)) {
        Some(s) => s,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    if let Some(err) = require_cookie_auth(&state, &name, &headers).await {
        return err;
    }

    let zip_path = match work::build_or_get_zip(
        state.photos_root().clone(),
        state.cache_root().clone(),
        name.clone(),
        scope,
        kind,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, "building work zip failed");
            return render_work_page(
                &state,
                &name,
                None,
                &headers,
                Some("No files matching that selection are available for this work item."),
                StatusCode::NOT_FOUND,
                false,
            )
            .await;
        }
    };
    let attach = format!(
        "{}-{}-{}.zip",
        sanitize_attachment(&name),
        scope.slug(),
        kind.slug()
    );
    let rec = audit::Download::work_zip(name.clone(), scope.slug(), kind.slug(), attach.clone());
    stream_and_log(&state, &zip_path, "application/zip", &attach, rec).await
}

/// Per-photo download: POST /work/:name/file/*subpath. `subpath` may include
/// forward slashes for nested folders (e.g. `digital/edited/foo.jpg`). Path
/// safety is enforced both by per-component validation here and by
/// `safe_resolve` downstream; auth comes from the cookie.
pub async fn work_file_download(
    State(state): State<AppState>,
    AxumPath((name, subpath)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_valid_work_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !is_valid_work_subpath(&subpath) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Only allow the extensions a job page actually links to.
    let basename = subpath.rsplit('/').next().unwrap_or("");
    if !(work::is_jpeg_name(basename) || work::is_raw_name(basename)) {
        return StatusCode::NOT_FOUND.into_response();
    }

    if let Some(err) = require_cookie_auth(&state, &name, &headers).await {
        return err;
    }

    let rel = format!("work/{}/{}", name, subpath);
    let path = match safe_resolve(state.photos_root(), &rel).await {
        Ok(p) => p,
        Err(e) => return map_path_err(e).into_response(),
    };
    let mime = if work::is_jpeg_name(basename) {
        "image/jpeg"
    } else {
        "application/octet-stream"
    };
    let rec = audit::Download::work_file(name.clone(), subpath.clone());
    stream_and_log(&state, &path, mime, basename, rec).await
}

fn is_valid_work_subpath(s: &str) -> bool {
    if s.is_empty() || s.contains('\\') {
        return false;
    }
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('.') {
            return false;
        }
    }
    true
}

/// Returns `Some(error_response)` when the request lacks a valid cookie for
/// this job; returns `None` when the caller may proceed.
async fn require_cookie_auth(
    state: &AppState,
    job_name: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    let stored = match work::read_password(state.photos_root().clone(), job_name.to_string()).await
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Some(
                render_work_page(
                    state,
                    job_name,
                    None,
                    headers,
                    Some("Downloads are locked — no password is set for this work item."),
                    StatusCode::FORBIDDEN,
                    false,
                )
                .await,
            );
        }
        Err(e) => {
            warn!(error = ?e, "reading work password failed");
            return Some(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    if cookie_authorizes(headers, job_name, &stored) {
        None
    } else {
        Some(
            render_work_page(
                state,
                job_name,
                None,
                headers,
                Some("Authentication required — enter the job password to unlock downloads."),
                StatusCode::UNAUTHORIZED,
                false,
            )
            .await,
        )
    }
}

/// Cookie carrying the auth token for one job. Hex-encoded so values pass
/// through cookie parsing untouched regardless of password characters, and
/// path-scoped so a token for one job can't be sent to another's endpoints.
const COOKIE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

fn cookie_name(job: &str) -> String {
    format!("work_token_{}", sanitize_attachment(job))
}

fn build_work_cookie(name: &str, password: &str) -> String {
    let value = encode_hex(password.as_bytes());
    let path = format!("/work/{}", encode_path(name));
    format!(
        "{}={}; Path={}; Max-Age={}; HttpOnly; SameSite=Lax",
        cookie_name(name),
        value,
        path,
        COOKIE_TTL_SECS,
    )
}

fn read_work_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header_val = headers.get(header::COOKIE)?.to_str().ok()?;
    let want = cookie_name(name);
    for pair in header_val.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k == want {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn cookie_authorizes(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    let val = match read_work_cookie(headers, name) {
        Some(v) => v,
        None => return false,
    };
    let bytes = match decode_hex(&val) {
        Some(b) => b,
        None => return false,
    };
    let submitted = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    work::verify(expected, submitted)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(out, "{:02x}", b).unwrap();
    }
    out
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

async fn stream_file_response(path: &Path, mime: &str, attach_name: &str) -> Response {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "open for stream failed");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let meta = match file.metadata().await {
        Ok(m) => m,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let len = meta.len();
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    let disposition = format!(
        "attachment; filename=\"{}\"",
        attach_name.replace('"', "")
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body)
        .unwrap()
}

/// `stream_file_response`, plus a line in the download log when it actually
/// served the file.
///
/// The status check is the whole point: `stream_file_response` answers 404 for
/// a path that resolved but does not exist, and logging before it would file
/// every miss as a download. Logging is best-effort and never changes the
/// response — see `audit::record_download`.
async fn stream_and_log(
    state: &AppState,
    path: &Path,
    mime: &str,
    attach_name: &str,
    rec: audit::Download,
) -> Response {
    let resp = stream_file_response(path, mime, attach_name).await;
    if resp.status().is_success() {
        audit::record_download(state.data_root(), rec).await;
    }
    resp
}

fn is_valid_work_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && name != "."
        && name != ".."
}

fn sanitize_attachment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` parser — keeps the dep tree
/// lean (no serde just for two fields). Last value wins on duplicates.
/// Single-valued view of a form body: the last occurrence of a repeated key
/// wins. Fine for every form that has one field per name.
fn parse_urlencoded(body: &[u8]) -> HashMap<String, String> {
    parse_urlencoded_multi(body)
        .into_iter()
        .filter_map(|(k, mut v)| v.pop().map(|last| (k, last)))
        .collect()
}

/// Every value for every key, in submission order.
///
/// A group of checkboxes sharing one name is the one shape `parse_urlencoded`
/// cannot represent: `person=Alice&person=Bob` collapses to `Bob` in a
/// `HashMap<String, String>`, which on `/notify` would silently discard all but
/// the last person a subscriber ticked. Both functions decode the same way —
/// the single-valued one is a fold over this one — so there is one decoder to
/// get right.
fn parse_urlencoded_multi(body: &[u8]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let s = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => return out,
    };
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(t) => t,
            None => (pair, ""),
        };
        out.entry(url_decode(k)).or_default().push(url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    (Some(a), Some(b)) => {
                        out.push((a << 4) | b);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(path: &str, names: &[&str]) -> FolderGroup {
        FolderGroup {
            label: path.to_string(),
            path: path.to_string(),
            browse_url: format!("/browse/{path}"),
            images: names
                .iter()
                .map(|n| ImageEntry {
                    name: (*n).to_string(),
                    thumb_url: String::new(),
                    image_url: String::new(),
                    jpg_download_url: String::new(),
                    raw_download_url: None,
                    download_action: None,
                    dims: None,
                    srcset: None,
                })
                .collect(),
            favs_count: 0,
            newest_mtime: None,
        }
    }

    #[test]
    fn favs_parent_matches_only_a_trailing_favs_segment() {
        assert_eq!(favs_parent("2026/roll/favs"), Some("2026/roll"));
        assert_eq!(favs_parent("2026/roll/FAVS"), Some("2026/roll"));
        assert_eq!(favs_parent("2026/roll"), None);
        assert_eq!(favs_parent("2026/favs-of-mine"), None);
        assert_eq!(favs_parent("favs"), None);
    }

    /// On `/recent` a roll is one section: the favorites lead it, the rest
    /// follow, and `favs_count` marks the boundary the grid draws its line at.
    #[test]
    fn favs_fold_into_the_front_of_their_roll() {
        let folded = fold_favs_into_rolls(vec![
            group("2026/roll", &["b.jpg", "c.jpg"]),
            group("2026/roll/favs", &["a.jpg"]),
            group("2026/other", &["d.jpg"]),
        ]);
        assert_eq!(folded.len(), 3 - 1, "the favs group should not survive alone");
        let roll = &folded[0];
        assert_eq!(roll.path, "2026/roll");
        assert_eq!(roll.favs_count, 1);
        let names: Vec<&str> = roll.images.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["a.jpg", "b.jpg", "c.jpg"]);
        assert_eq!(folded[1].path, "2026/other");
        assert_eq!(folded[1].favs_count, 0);
    }

    /// A favs folder whose parent holds no photos of its own has no roll to
    /// lead, so it stays a section rather than vanishing.
    #[test]
    fn an_orphan_favs_group_is_kept() {
        let folded = fold_favs_into_rolls(vec![group("2026/roll/favs", &["a.jpg"])]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].path, "2026/roll/favs");
        assert_eq!(folded[0].favs_count, 0, "nothing was folded, so no line");
    }

    /// The regression this function exists to prevent: a group of checkboxes
    /// sharing one name is the whole people-picker on `/notify`, and folding it
    /// into a `HashMap<String, String>` would keep only the last person ticked.
    #[test]
    fn repeated_form_keys_keep_every_value() {
        let multi = parse_urlencoded_multi(b"person=Alice&person=Bob&person=Carol&channel=email");
        assert_eq!(
            multi.get("person").unwrap(),
            &vec![
                "Alice".to_string(),
                "Bob".to_string(),
                "Carol".to_string()
            ]
        );
        assert_eq!(multi.get("channel").unwrap(), &vec!["email".to_string()]);
    }

    /// The single-valued view is a fold over the multi one, so the existing
    /// callers keep the last-wins behaviour they were written against.
    #[test]
    fn the_single_valued_view_still_takes_the_last() {
        let single = parse_urlencoded(b"password=first&password=second");
        assert_eq!(single.get("password").unwrap(), "second");
    }

    /// Names arrive percent-encoded; a person tag with a space in it has to
    /// survive the round trip or the server rejects a name the form offered.
    #[test]
    fn form_values_are_percent_decoded() {
        let multi = parse_urlencoded_multi(b"person=Parker%20Brown&person=Al+Green");
        assert_eq!(
            multi.get("person").unwrap(),
            &vec!["Parker Brown".to_string(), "Al Green".to_string()]
        );
    }

    /// Behind the proxy the client is only named in the header; its first entry
    /// is the original client, and the rest are hops.
    #[test]
    fn the_rate_limit_key_is_the_original_client() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        assert_eq!(client_key(&headers), "203.0.113.7");
        assert_eq!(client_key(&HeaderMap::new()), "unknown");
    }

    /// A roll is one level below a year, and everything deeper belongs to it —
    /// which is what keeps a `favs/` sorted with its roll instead of drifting
    /// off on a recency of its own.
    #[test]
    fn a_roll_is_the_year_folders_own_child() {
        assert_eq!(roll_rel("2026/utopia/favs/a.jpg"), "2026/utopia");
        assert_eq!(roll_rel("2026/utopia/a.jpg"), "2026/utopia");
        assert_eq!(roll_rel("2026/utopia/positive/favs/a.jpg"), "2026/utopia");
        // Directly in the year folder: the year is the roll.
        assert_eq!(roll_rel("2026/a.jpg"), "2026");
        // No year in the path, so the top-level folder is as far as it goes.
        assert_eq!(roll_rel("misc/deep/a.jpg"), "misc");
        assert_eq!(roll_rel("a.jpg"), "");
    }

    /// Write a JPEG at `rel` and stamp it with a known mtime, so a test can say
    /// exactly which roll was published last.
    fn photo_at(root: &Path, rel: &str, mtime: u64) {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, b"not really a jpeg").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&abs)
            .unwrap()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(mtime))
            .unwrap();
    }

    fn photo(rel: &str) -> people::PersonPhoto {
        people::PersonPhoto {
            rel: rel.to_string(),
            name: rel.rsplit('/').next().unwrap().to_string(),
        }
    }

    /// A person's page opens on their newest year, and inside a year on the
    /// roll published most recently — the same two rules `/all` sorts by, which
    /// is the whole point of doing this in Rust rather than in the `ORDER BY`.
    ///
    /// The mtimes are chosen so no assertion here can pass by accident: the
    /// alphabetically first roll is not the newest, and the single newest photo
    /// on disk sits in the *older* year, so a sort that let recency reach the
    /// year level would fail.
    #[tokio::test]
    async fn a_persons_photos_read_newest_year_then_newest_roll() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        photo_at(root, "2026/alpha/a.jpg", 1_000);
        // In `alpha`, but newer than anything in `beta` — so `alpha` outranks
        // `beta` only if the subtree is what counts, not the album.
        photo_at(root, "2026/alpha/favs/f.jpg", 4_000);
        photo_at(root, "2026/beta/b.jpg", 2_000);
        photo_at(root, "2026/gamma/g.jpg", 9_000);
        photo_at(root, "2025/older/o.jpg", 99_000);
        photo_at(root, "misc/loose/m.jpg", 50_000);

        let mut photos = vec![
            photo("misc/loose/m.jpg"),
            photo("2026/alpha/a.jpg"),
            photo("2025/older/o.jpg"),
            photo("2026/beta/b.jpg"),
            photo("2026/alpha/favs/f.jpg"),
            photo("2026/gamma/g.jpg"),
        ];
        sort_person_photos(root, &mut photos).await;

        let order: Vec<&str> = photos.iter().map(|p| p.rel.as_str()).collect();
        assert_eq!(
            order,
            vec![
                // 2026 leads despite holding the oldest photo on disk.
                "2026/gamma/g.jpg",
                // `alpha` before `beta` on its favs' mtime, and its own photo
                // stays ahead of that favs — a roll reads like it does on /all.
                "2026/alpha/a.jpg",
                "2026/alpha/favs/f.jpg",
                "2026/beta/b.jpg",
                "2025/older/o.jpg",
                // No year in the path, so it sinks below every year however
                // recently it was published.
                "misc/loose/m.jpg",
            ]
        );
    }
}
