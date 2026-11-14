//! The private archive: photographs tagged under `Archive` in digiKam, shown
//! only to the few people the owner has handed a key.
//!
//! There is no login page. The owner runs `archive_key.sh add <name>`, which
//! appends a random key to `data/archive_keys` and prints a link,
//! `/archive?k=<key>`. Opening that link checks the key, sets a cookie that
//! carries it, and redirects to a clean `/archive`, so the key does not stay in
//! the address bar or the history. After that the cookie is the key. Every
//! request checks it against the file, so `archive_key.sh revoke <name>` locks
//! that person out on their next request. Nothing is cached in memory to
//! expire.
//!
//! To everyone else, every archive URL is the site's ordinary 404: the page,
//! the images and the downloads alike. That covers no cookie, a wrong one and
//! a revoked one. The headers that mark the archive private (`X-Robots-Tag`,
//! `Referrer-Policy`) go on authorized responses only, so a 404 here is
//! byte-for-byte the 404 any made-up URL gets.
//!
//! What is in the archive is decided entirely by tags; see `tags.rs`.

use std::collections::HashMap;
use std::path::Path;

use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use tracing::warn;

use crate::audit::{self, ArchiveEvent};
use crate::handlers::{self, Audience, encode_path, not_found_response};
use crate::state::AppState;
use crate::thumbs::ThumbKind;
use crate::views;

/// The archive's route prefix. Every archive URL starts with it, and its file
/// routes mirror the public ones underneath it: `/archive/image/…`,
/// `/archive/download/…`, `/archive/<rendition>/…`.
pub const PREFIX: &str = "/archive";

/// `data/archive_keys`: one key per line, `<name> <key>`. Written by
/// `archive_key.sh`, mode 600. Blank lines and `#` comments are ignored.
pub const KEYS_FILE: &str = "archive_keys";

const COOKIE: &str = "archive_key";

/// A year, and renewed on every archive page view, so someone who keeps
/// visiting never has to open the link again. Revocation does not wait for
/// this: it takes effect on the next request.
const COOKIE_TTL_SECS: u64 = 365 * 24 * 60 * 60;

/// `?k=` attempts allowed per client address per hour, right or wrong. A
/// friend opens their link once. A guesser, at this rate, never gets near a
/// 128-bit key.
const UNLOCK_MAX: u32 = 20;
const UNLOCK_WINDOW_SECS: u64 = 60 * 60;

pub struct Key {
    pub name: String,
    key: String,
}

fn parse_keys(text: &str) -> Vec<Key> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (name, key) = l.rsplit_once(char::is_whitespace)?;
            let (name, key) = (name.trim(), key.trim());
            (!name.is_empty() && !key.is_empty()).then(|| Key {
                name: name.to_string(),
                key: key.to_string(),
            })
        })
        .collect()
}

/// Every issued key. A missing file means none have been issued.
pub async fn read_keys(data_root: &Path) -> Vec<Key> {
    match tokio::fs::read_to_string(data_root.join(KEYS_FILE)).await {
        Ok(text) => parse_keys(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!(error = %e, "reading archive keys failed");
            Vec::new()
        }
    }
}

/// The name on the key that matches `submitted`, if one does. Every key is
/// compared, in constant time, so the response time does not say how many
/// keys were checked before the match.
fn match_key(keys: &[Key], submitted: &str) -> Option<String> {
    let mut found = None;
    for k in keys {
        if crate::work::verify(&k.key, submitted) && found.is_none() {
            found = Some(k.name.clone());
        }
    }
    found
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

/// The key holder making this request, or `None`. Reads the keys file only
/// when the request carries an archive cookie, so ordinary visitors cost
/// nothing.
pub async fn viewer(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let submitted = cookie_value(headers)?;
    match_key(&read_keys(state.data_root()).await, &submitted)
}

fn build_cookie(key: &str) -> String {
    format!("{COOKIE}={key}; Path=/; Max-Age={COOKIE_TTL_SECS}; HttpOnly; Secure; SameSite=Lax")
}

/// The headers every authorized archive response carries. Not applied to the
/// 404s; see the module docs.
fn mark_private(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    // Images and downloads come back with the public routes' `public` cache
    // policy. A shared cache must never keep a copy of an archive file, so it
    // is overridden here, on 200s and 304s alike.
    if resp.status().is_success() || resp.status() == StatusCode::NOT_MODIFIED {
        let cc = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.replace("public", "private"))
            .unwrap_or_else(|| "private, no-cache".to_string());
        if let Ok(v) = HeaderValue::from_str(&cc) {
            resp.headers_mut().insert(header::CACHE_CONTROL, v);
        }
    }
    resp
}

/// `GET /archive`: the leading section, or the unlock when `?k=` is present.
pub async fn front(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(submitted) = query.get("k") {
        return unlock(&state, &headers, submitted).await;
    }
    page(&state, &headers, None).await
}

/// `GET /archive/:slug`: a named section.
pub async fn section(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    page(&state, &headers, Some(&slug)).await
}

async fn unlock(state: &AppState, headers: &HeaderMap, submitted: &str) -> Response {
    let client = handlers::client_key(headers);
    if !state
        .archive_limiter()
        .allow(&client, UNLOCK_MAX, UNLOCK_WINDOW_SECS)
    {
        warn!("archive unlock rate limit hit");
        return not_found_response();
    }
    let keys = read_keys(state.data_root()).await;
    let Some(name) = match_key(&keys, submitted) else {
        warn!("archive unlock with an unknown key");
        return not_found_response();
    };
    audit::record_archive(state.data_root(), &name, ArchiveEvent::Unlock, PREFIX).await;
    // 303, so the key-bearing URL is replaced in history by the clean one.
    let mut resp = Redirect::to(PREFIX).into_response();
    if let Ok(v) = HeaderValue::from_str(&build_cookie(submitted)) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    mark_private(resp)
}

/// Shared body of [`front`] and [`section`], shaped like the portfolio's: the
/// leading section is served at `/archive` and nowhere else, so its own slug
/// redirects there.
async fn page(state: &AppState, headers: &HeaderMap, want: Option<&str>) -> Response {
    let Some(name) = viewer(state, headers).await else {
        return not_found_response();
    };
    let index = state.tag_index().await;
    let sections = index.sections();

    let canonical_of = |slug: &str, front: &str| {
        if slug == front {
            PREFIX.to_string()
        } else {
            format!("{PREFIX}/{}", encode_path(slug))
        }
    };

    let resp = match sections.first() {
        // A key holder with nothing to see yet gets an empty archive at
        // `/archive`, rather than a 404 that would look like a revoked key.
        None => match want {
            Some(_) => return not_found_response(),
            None => views::archive_page(None, PREFIX, &[]).into_response(),
        },
        Some(front) => {
            let active = match want {
                None => front,
                Some(s) if s == front.slug => {
                    return mark_private(Redirect::permanent(PREFIX).into_response());
                }
                Some(s) => match sections.iter().find(|sec| sec.slug == s) {
                    Some(sec) => sec,
                    None => return not_found_response(),
                },
            };
            let tabs: Vec<views::SectionTab> = sections
                .iter()
                .map(|sec| views::SectionTab {
                    label: sec.label.clone(),
                    url: canonical_of(&sec.slug, &front.slug),
                    active: sec.slug == active.slug,
                })
                .collect();
            let canonical = canonical_of(&active.slug, &front.slug);
            let group = handlers::portfolio_group(state, active, PREFIX).await;
            audit::record_archive(state.data_root(), &name, ArchiveEvent::View, &canonical).await;
            views::archive_page(group.as_ref(), &canonical, &tabs).into_response()
        }
    };

    // Renew the year on every visit, re-sending the key the cookie already
    // holds.
    let mut resp = resp;
    if let Some(key) = cookie_value(headers)
        && let Ok(v) = HeaderValue::from_str(&build_cookie(&key))
    {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    mark_private(resp)
}

/// `GET /archive/image/*path`.
pub async fn image(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if viewer(&state, &headers).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    gated(handlers::serve_image(&state, &rel, &headers, Audience::Archive).await)
}

/// `GET /archive/download/*path`. Logged under the key holder's name.
pub async fn download(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(name) = viewer(&state, &headers).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    gated(handlers::serve_download(&state, &rel, Audience::Archive, Some(name)).await)
}

/// `GET /archive/<rendition>/*path`, one per [`ThumbKind`].
pub async fn rendition(
    kind: ThumbKind,
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if viewer(&state, &headers).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    gated(handlers::render_thumb_response(&state, &rel, &headers, kind, Audience::Archive).await)
}

/// Marks a file response private, unless it is a 404, which must stay
/// identical to the public routes' 404.
fn gated(resp: Response) -> Response {
    if resp.status() == StatusCode::NOT_FOUND {
        resp
    } else {
        mark_private(resp)
    }
}

/// Around every page handler: tells the header whether to show the archive
/// tab (see `views::ARCHIVE_TAB`) and adds `Vary: Cookie`.
///
/// `Vary: Cookie` because a key holder's copy of any page differs from
/// everyone else's by that tab. The pages are already `private`, so no shared
/// cache should store them. This makes sure one never hands a key holder's
/// copy to someone else.
pub async fn nav_layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let label = match viewer(&state, req.headers()).await {
        Some(_) => state.tag_index().await.root_label().map(str::to_string),
        None => None,
    };
    let mut resp = views::ARCHIVE_TAB.scope(label, next.run(req)).await;
    resp.headers_mut()
        .append(header::VARY, HeaderValue::from_static("Cookie"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_name_then_key_and_skip_comments() {
        let keys = parse_keys("# who\n\neliana  abc123\nPaul B\tdef456\nbroken\n");
        let pairs: Vec<(&str, &str)> = keys.iter().map(|k| (k.name.as_str(), k.key.as_str())).collect();
        assert_eq!(pairs, [("eliana", "abc123"), ("Paul B", "def456")]);
    }

    #[test]
    fn a_key_matches_only_itself() {
        let keys = parse_keys("eliana abc123\npaul def456\n");
        assert_eq!(match_key(&keys, "def456").as_deref(), Some("paul"));
        assert_eq!(match_key(&keys, "abc12"), None);
        assert_eq!(match_key(&keys, ""), None);
    }
}
