use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::Markup;
use tracing::warn;

use crate::paths::{PathError, safe_resolve};
use crate::state::AppState;
use crate::thumbs;
use crate::views::{self, Crumb, DirEntry, ImageEntry};

const FRONT_PAGE_DIR: &str = "portfolio";

#[derive(Clone, Copy)]
enum PageKind {
    Index,
    BrowseRoot,
    BrowseSub,
}

pub async fn index(State(state): State<AppState>) -> Response {
    match render_dir(&state, FRONT_PAGE_DIR, PageKind::Index).await {
        Ok(html) => html.into_response(),
        Err(status) => status.into_response(),
    }
}

pub async fn browse_root(State(state): State<AppState>) -> Response {
    match render_dir(&state, "", PageKind::BrowseRoot).await {
        Ok(html) => html.into_response(),
        Err(status) => status.into_response(),
    }
}

pub async fn browse(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    match render_dir(&state, &rel, PageKind::BrowseSub).await {
        Ok(html) => html.into_response(),
        Err(status) => status.into_response(),
    }
}

async fn render_dir(
    state: &AppState,
    rel: &str,
    kind: PageKind,
) -> Result<Markup, StatusCode> {
    let dir = safe_resolve(state.photos_root(), rel).await.map_err(map_path_err)?;

    let mut read = tokio::fs::read_dir(&dir)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut subdirs = Vec::new();
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
            subdirs.push(DirEntry {
                name: name.clone(),
                url: format!("/browse/{}", encode_path(&rel_child)),
            });
        } else if ftype.is_file() && is_png(&name) && !is_hidden(&name) {
            images.push(ImageEntry {
                thumb_url: format!("/thumb/{}", encode_path(&rel_child)),
                image_url: format!("/image/{}", encode_path(&rel_child)),
                name,
            });
        }
    }

    subdirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    images.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

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
    Ok(views::page(&title, &crumbs, &subdirs, &images))
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
    if !is_png(&rel) || rel_filename_is_hidden(&rel) {
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
    if !is_png(&rel) || rel_filename_is_hidden(&rel) {
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
        .header(header::CONTENT_TYPE, "image/png")
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

fn is_png(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("png"))
        .unwrap_or(false)
}

/// A file is "hidden" if its basename contains the substring "hidden"
/// (case-insensitive). Applied on top of the .png filter.
fn is_hidden(name: &str) -> bool {
    name.to_ascii_lowercase().contains("hidden")
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

