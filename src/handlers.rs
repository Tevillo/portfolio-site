use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use tokio_util::io::ReaderStream;
use tracing::warn;

use crate::work::{self, DownloadKind, Scope};
use crate::paths::{PathError, safe_resolve};
use crate::people;
use crate::portfolio;
use crate::state::AppState;
use crate::thumbs::{self, ThumbKind};
use crate::views::{
    self, Crumb, DirEntry, FolderGroup, ImageEntry, WorkFeedPhoto, WorkFeedSection, WorkIndexEntry,
    PersonEntry,
};

#[derive(Clone, Copy)]
enum PageKind {
    BrowseRoot,
    BrowseSub,
}

/// Home page: one section per `portfolio/<label>` tag in the digiKam database,
/// photos shown at their natural aspect ratio. The curation lives in the tags
/// (see [`crate::portfolio`]) rather than in a folder, so the photos themselves
/// come from all over the tree.
pub async fn index(State(state): State<AppState>) -> Response {
    // Every failure mode here — no database, unreadable database, no
    // `portfolio` tag — collapses to an empty group list, which the view renders
    // as "Nothing here yet.". The front page of the site is the wrong place to
    // surface an infrastructure problem, and the log line is the actionable half
    // anyway.
    let groups = portfolio_groups(&state).await;
    views::grouped_gallery_page(&groups).into_response()
}

/// Resolve the `portfolio/*` tags into renderable sections, dropping anything
/// that cannot be turned into a live URL.
async fn portfolio_groups(state: &AppState) -> Vec<FolderGroup> {
    let Some(db) = state.db_path().cloned() else {
        warn!("portfolio tag database not available; rendering empty home page");
        return Vec::new();
    };
    let sections = match portfolio::list_sections(db).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "listing portfolio sections failed");
            return Vec::new();
        }
    };

    // Tagged photos arrive as flat rel paths scattered across the tree, so —
    // exactly as in `person_photos` — there is no directory walk to piggyback
    // the raw-sibling lookup on. Cache each album folder's stem->raw map, which
    // pays off here because whole sections tend to share one folder.
    let mut dir_raws: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut groups: Vec<FolderGroup> = Vec::new();
    // Absolute source paths in flattened `groups` order, the shape
    // `fill_preview_dims` expects.
    let mut sources: Vec<PathBuf> = Vec::new();

    for section in sections {
        let mut images: Vec<ImageEntry> = Vec::with_capacity(section.photos.len());
        for p in section.photos {
            // `safe_resolve` doubles as the existence check: a tag can outlive
            // the file it points at (renamed outside digiKam, moved to a folder
            // the site skips), and a tile whose source is gone would render as a
            // broken image with no dimensions to reserve space with.
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
                // The portfolio shows large tiles, so it loads the 1600px
                // preview rendition rather than the 400px grid thumb that dense
                // listings use.
                thumb_url: format!("/preview/{}", encode_path(&p.rel)),
                image_url: format!("/image/{}", encode_path(&p.rel)),
                jpg_download_url: format!("/download/{}", encode_path(&p.rel)),
                raw_download_url,
                dims: None,
                name: p.name,
            });
        }
        if images.is_empty() {
            continue;
        }
        groups.push(FolderGroup {
            label: section.label.clone(),
            // Namespaced so collapse.js keeps per-section state that cannot
            // collide with a real folder path of the same name.
            path: format!("portfolio/{}", section.label),
            // A tag's photos span many folders, so there is no single folder to
            // browse to; the view renders a plain heading when this is empty.
            browse_url: String::new(),
            images,
        });
    }

    // Same reason as the folder walk: the natural-ratio masonry carries no CSS
    // aspect-ratio, so without intrinsic dimensions every tile lays out at zero
    // height and `loading="lazy"` defers nothing.
    fill_preview_dims(&sources, &mut groups).await;
    groups
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
            let dims = tokio::task::spawn_blocking(move || thumbs::preview_dimensions(&path).ok())
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
/// Deliberately leaves `/image/`, `/thumb/` and `/preview/` crawlable. Blocking
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
        "/people".to_string(),
        "/work".to_string(),
    ];

    // Every folder that directly holds photos, i.e. every /browse page with
    // something on it. Intermediate folders come along as each group's parents
    // are themselves groups only when they hold photos too; listing the leaves
    // is what matters, since the crawler reaches the rest from /browse.
    if let Ok(groups) = walk_groups(state.photos_root(), "", "thumb").await {
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
        Err(status) => status.into_response(),
    }
}

pub async fn browse(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    match render_dir(&state, &rel, PageKind::BrowseSub).await {
        Ok(resp) => resp,
        Err(status) => status.into_response(),
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
                dims: None,
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
                dims: None,
                name,
            }
        })
        .collect()
}

async fn subtree_has_jpeg(root: &Path) -> bool {
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
    let entries: Vec<PersonEntry> = people_list
        .into_iter()
        .map(|p| PersonEntry {
            url: format!("/people/{}", encode_path(&p.name)),
            name: p.name,
            photo_count: p.photo_count,
        })
        .collect();
    let crumbs = vec![
        Crumb {
            label: "Home".into(),
            url: Some("/".into()),
        },
        Crumb {
            label: "People".into(),
            url: None,
        },
    ];
    views::people_index_page("People", &crumbs, &entries).into_response()
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
        return StatusCode::NOT_FOUND.into_response();
    }
    let photos = match people::list_person_photos(db, name.clone()).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, person = %name, "listing person photos failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if photos.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
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
            dims: None,
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

fn people_unavailable_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        "people tag database not available",
    )
        .into_response()
}

pub async fn all_photos(State(state): State<AppState>) -> Response {
    match walk_groups(state.photos_root(), "", "thumb").await {
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

/// Pre-order DFS from `root`/`start_rel`, emitting one `FolderGroup` per
/// directory that directly contains JPEGs. `start_rel` is relative to `root`
/// (empty = the whole photos tree, as `/all` uses); paths and URLs stay
/// rooted at `root` so thumbnail/image links resolve regardless of where the
/// walk starts. `thumb_route` selects which rendition the displayed tile loads
/// — `"thumb"` (400px grid) for dense listings, `"preview"` (1600px) where tiles
/// render larger. Only `/all` walks the tree now; the home page is driven by the
/// `portfolio/*` tags instead.
async fn walk_groups(
    root: &Path,
    start_rel: &str,
    thumb_route: &str,
) -> Result<Vec<FolderGroup>, StatusCode> {
    let start_abs = if start_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(start_rel)
    };
    let mut stack: Vec<(PathBuf, String)> = vec![(start_abs, start_rel.to_string())];
    let mut groups: Vec<FolderGroup> = Vec::new();
    // Absolute source path of every image pushed into `groups`, kept in the
    // same flattened order so `fill_preview_dims` can zip the two together
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
                    thumb_url: format!("/{}/{}", thumb_route, encode_path(&rel_child)),
                    image_url: format!("/image/{}", encode_path(&rel_child)),
                    jpg_download_url: format!("/download/{}", encode_path(&rel_child)),
                    raw_download_url,
                    dims: None,
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
            sources.extend(images.iter().map(|img| abs.join(&img.name)));
            groups.push(FolderGroup {
                label,
                path: rel.clone(),
                browse_url,
                images,
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

    // The preview rendition feeds the natural-ratio masonry, whose tiles carry
    // no CSS aspect-ratio. Without intrinsic dimensions every <img> lays out at
    // zero height, so the browser thinks the entire grid is in the viewport and
    // `loading="lazy"` fetches all of it up front. Fill them in one blocking
    // batch: `preview_dimensions` only reads JPEG/EXIF headers, no decode.
    if thumb_route == "preview" {
        fill_preview_dims(&sources, &mut groups).await;
    }

    Ok(groups)
}

/// Populate `ImageEntry::dims` for every image in `groups`, off the async
/// runtime since the underlying reads are synchronous file I/O. `sources` is
/// the absolute path of each image, in the same order the images appear when
/// `groups` is flattened. Photos whose headers can't be read keep `None` and
/// simply render without the attributes.
async fn fill_preview_dims(sources: &[PathBuf], groups: &mut [FolderGroup]) {
    if sources.is_empty() {
        return;
    }
    let sources = sources.to_vec();
    let dims = match tokio::task::spawn_blocking(move || {
        sources
            .into_iter()
            .map(|p| thumbs::preview_dimensions(&p).ok())
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "preview dimension batch panicked");
            return;
        }
    };
    for (img, dim) in groups
        .iter_mut()
        .flat_map(|g| g.images.iter_mut())
        .zip(dims)
    {
        img.dims = dim;
    }
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
    stream_file_response(&path, mime, basename).await
}

pub async fn thumb(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_thumb_response(&state, &rel, &headers, ThumbKind::Grid).await
}

pub async fn preview(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_thumb_response(&state, &rel, &headers, ThumbKind::Preview).await
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
    let entries: Vec<WorkIndexEntry> = work_list
        .into_iter()
        .map(|j| WorkIndexEntry {
            url: format!("/work/{}", encode_path(&j.name)),
            name: j.name,
            jpeg_count: j.jpeg_count,
            raw_count: j.raw_count,
        })
        .collect();
    let crumbs = vec![
        Crumb {
            label: "Home".into(),
            url: Some("/".into()),
        },
        Crumb {
            label: "Work".into(),
            url: None,
        },
    ];
    views::work_index_page("Work", &crumbs, &entries).into_response()
}

pub async fn work_detail(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    render_work_page(&state, &name, &headers, None, StatusCode::OK).await
}

async fn render_work_page(
    state: &AppState,
    name: &str,
    headers: &HeaderMap,
    error: Option<&str>,
    status: StatusCode,
) -> Response {
    if !is_valid_work_name(name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let detail = match work::read_work(state.photos_root().clone(), name.to_string()).await {
        Ok(Some(d)) => d,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
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
    let mut total_jpeg_count: u32 = 0;
    let sections: Vec<WorkFeedSection> = detail
        .sections
        .into_iter()
        .map(|s| {
            let photos: Vec<WorkFeedPhoto> = s
                .photos
                .into_iter()
                .map(|p| WorkFeedPhoto {
                    preview_url: format!("/preview/{}", encode_path(&p.rel)),
                    image_url: format!("/image/{}", encode_path(&p.rel)),
                    download_action: if authorized {
                        format!(
                            "/work/{}/file/{}",
                            encode_path(name),
                            encode_path(&p.subpath)
                        )
                    } else {
                        String::new()
                    },
                    name: p.name,
                    preview_dims: p.preview_dims,
                })
                .collect();
            total_jpeg_count += photos.len() as u32;
            // The job-root section gets the empty-label slot; everything else
            // hangs off its subfolder path so collapse.js can persist per
            // section choices without colliding across work items.
            let data_path = if s.label.is_empty() {
                format!("work:{name}:")
            } else {
                format!("work:{name}:{}", s.label)
            };
            let default_open = s.is_edited || s.label.is_empty();
            WorkFeedSection {
                label: s.label,
                photos,
                data_path,
                default_open,
            }
        })
        .collect();
    let crumbs = vec![
        Crumb {
            label: "Home".into(),
            url: Some("/".into()),
        },
        Crumb {
            label: "Work".into(),
            url: Some("/work".into()),
        },
        Crumb {
            label: name.to_string(),
            url: None,
        },
    ];
    let bulk_action = format!("/work/{}/download", encode_path(name));
    let auth_action = format!("/work/{}/auth", encode_path(name));
    let body = views::work_page(
        name,
        &crumbs,
        &sections,
        total_jpeg_count,
        detail.counts,
        detail.has_password,
        authorized,
        &bulk_action,
        &auth_action,
        error,
    );
    // This page renders materially different markup pre- vs post-auth, keyed
    // off the path-scoped auth cookie. Without `Vary: Cookie` a shared cache
    // could store the unlocked variant and hand it to a different visitor.
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(header::VARY, header::HeaderValue::from_static("Cookie"));
    resp
}

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
    let form = parse_urlencoded(&body);
    let submitted = form.get("password").cloned().unwrap_or_default();
    let stored = match work::read_password(state.photos_root().clone(), name.clone()).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return render_work_page(
                &state,
                &name,
                &headers,
                Some("Downloads are locked — no password is set for this work item."),
                StatusCode::FORBIDDEN,
            )
            .await;
        }
        Err(e) => {
            warn!(error = ?e, "reading work password failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !work::verify(&stored, &submitted) {
        return render_work_page(
            &state,
            &name,
            &headers,
            Some("Incorrect password."),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }
    let set_cookie = build_work_cookie(&name, &submitted);
    let redirect_to = format!("/work/{}", encode_path(&name));
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
                &headers,
                Some("No files matching that selection are available for this work item."),
                StatusCode::NOT_FOUND,
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
    stream_file_response(&zip_path, "application/zip", &attach).await
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
    stream_file_response(&path, mime, basename).await
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
                    headers,
                    Some("Downloads are locked — no password is set for this work item."),
                    StatusCode::FORBIDDEN,
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
                headers,
                Some("Authentication required — enter the job password to unlock downloads."),
                StatusCode::UNAUTHORIZED,
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
fn parse_urlencoded(body: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
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
        out.insert(url_decode(k), url_decode(v));
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

