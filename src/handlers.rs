use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use tracing::warn;

use crate::paths::{PathError, safe_resolve};
use crate::people;
use crate::state::AppState;
use crate::thumbs;
use crate::views::{self, Crumb, DirEntry, FolderGroup, ImageEntry, PersonEntry};

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
            if is_skipped_dir(&name) {
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
                if is_skipped_dir(&name) {
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
    let source = match safe_resolve(state.photos_root(), &rel).await {
        Ok(p) => p,
        Err(e) => return map_path_err(e).into_response(),
    };
    if !is_jpeg(&rel) || rel_filename_is_hidden(&rel) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let info = match thumbs::ensure_thumb(&source, state.photos_root(), state.cache_root()).await {
        Ok(i) => i,
        Err(e) => {
            warn!(source = %source.display(), error = ?e, "thumbnail failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let etag = build_etag(info.mtime, info.size);
    if matches_etag(&headers, &etag) {
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

fn encode_path(s: &str) -> String {
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

