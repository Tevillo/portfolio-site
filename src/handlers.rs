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
use crate::state::AppState;
use crate::thumbs::{self, ThumbKind};
use crate::views::{
    self, Crumb, DirEntry, FolderGroup, ImageEntry, WorkFeedPhoto, WorkFeedSection, WorkIndexEntry,
    PersonEntry,
};

const FRONT_PAGE_DIR: &str = "portfolio";

#[derive(Clone, Copy)]
enum PageKind {
    Index,
    BrowseRoot,
    BrowseSub,
}

pub async fn index(State(state): State<AppState>) -> Response {
    match render_dir(&state, FRONT_PAGE_DIR, PageKind::Index).await {
        Ok(resp) => resp,
        Err(status) => status.into_response(),
    }
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
    let mut images = Vec::new();
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
            candidate_subdirs.push((name, rel_child, entry.path()));
        } else if ftype.is_file() && is_jpeg(&name) && !is_hidden(&name) {
            images.push(ImageEntry {
                thumb_url: format!("/thumb/{}", encode_path(&rel_child)),
                image_url: format!("/image/{}", encode_path(&rel_child)),
                name,
            });
        }
    }

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

    if images.is_empty() && subdirs.len() == 1 {
        let target = subdirs.into_iter().next().unwrap().url;
        return Ok(Redirect::to(&target).into_response());
    }

    let crumbs = breadcrumbs(rel, kind);
    let title = match kind {
        PageKind::Index => "Portfolio".to_string(),
        PageKind::BrowseRoot => "Browse".to_string(),
        PageKind::BrowseSub => rel
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string(),
    };
    Ok(views::page(&title, &crumbs, &subdirs, &images).into_response())
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
    let images: Vec<ImageEntry> = photos
        .into_iter()
        .map(|p| ImageEntry {
            thumb_url: format!("/thumb/{}", encode_path(&p.rel)),
            image_url: format!("/image/{}", encode_path(&p.rel)),
            name: p.name,
        })
        .collect();
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
    views::page(&name, &crumbs, &[], &images).into_response()
}

fn people_unavailable_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        "people tag database not available",
    )
        .into_response()
}

pub async fn all_photos(State(state): State<AppState>) -> Response {
    match walk_groups(state.photos_root()).await {
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
            views::all_page("All", &crumbs, &groups).into_response()
        }
        Err(status) => status.into_response(),
    }
}

async fn walk_groups(root: &Path) -> Result<Vec<FolderGroup>, StatusCode> {
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    let mut groups: Vec<FolderGroup> = Vec::new();

    while let Some((abs, rel)) = stack.pop() {
        let mut read = match tokio::fs::read_dir(&abs).await {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %abs.display(), error = %e, "read_dir failed during walk");
                continue;
            }
        };

        let mut images: Vec<ImageEntry> = Vec::new();
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
                images.push(ImageEntry {
                    thumb_url: format!("/thumb/{}", encode_path(&rel_child)),
                    image_url: format!("/image/{}", encode_path(&rel_child)),
                    name,
                });
            }
        }

        if !images.is_empty() {
            images.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            let (label, browse_url) = if rel.is_empty() {
                ("Photos (root)".to_string(), "/browse".to_string())
            } else {
                (rel.clone(), format!("/browse/{}", encode_path(&rel)))
            };
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

    Ok(groups)
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
        return StatusCode::NOT_MODIFIED.into_response();
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
        return StatusCode::NOT_MODIFIED.into_response();
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

fn is_jpeg(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false)
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

fn build_etag(mtime: SystemTime, size: u64) -> String {
    let secs = mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", secs, size)
}

fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
}

fn breadcrumbs(rel: &str, kind: PageKind) -> Vec<Crumb> {
    let mut out = Vec::new();
    match kind {
        PageKind::Index => {
            out.push(Crumb {
                label: "Portfolio".into(),
                url: None,
            });
            return out;
        }
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
    (status, body).into_response()
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

