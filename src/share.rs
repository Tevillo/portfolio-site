//! `/share` — a browser front end for filedrop (`encryptor-api`).
//!
//! filedrop only accepts uploads carrying an API key, and a key put into a web
//! page is a key handed to everyone who opens it. So the page never talks to
//! filedrop directly: it `PUT`s the file here, and this handler streams the body
//! on to filedrop with the site's own key attached, then hands back the share
//! link filedrop answers with. Downloads do not come through here at all — the
//! link points at filedrop's public URL, which needs no key.
//!
//! Open to anyone, by the owner's choice. What bounds the damage a stranger can
//! do is filedrop's own limits: 2 GiB per file, `FILEDROP_MAX_TOTAL_BYTES` in
//! total (507 once full), and at most 48 hours before the cleaner removes it.
//!
//! Configuration, one secret per file like the notify credentials:
//!
//! - `data/token/filedrop.key` — the API key from `filedrop keys create`.
//!   Missing means uploads answer 503 and the page says uploads are off.
//! - `data/token/filedrop.endpoint` — optional; filedrop's base URL as seen
//!   from this server. Defaults to [`DEFAULT_ENDPOINT`].
//!
//! Both are read per request, so neither needs a restart.

use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::warn;

use crate::notify::{read_secret, token_path};
use crate::state::AppState;
use crate::views;

/// filedrop's default `FILEDROP_MAX_FILE_BYTES`. Checked here as well so an
/// oversized upload is refused before a byte of it is forwarded.
pub const MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// filedrop's default listen address plus its route prefix.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8090/filedrop";

/// filedrop's own limit on a file name (`maxNameBytes`).
const MAX_NAME_BYTES: usize = 255;

pub const KEY_FILE: &str = "filedrop.key";
pub const ENDPOINT_FILE: &str = "filedrop.endpoint";

pub async fn page() -> Response {
    views::share_page(MAX_UPLOAD_BYTES).into_response()
}

#[derive(Deserialize)]
pub struct UploadQuery {
    name: String,
    expires: Option<String>,
    downloads: Option<String>,
}

/// No overall timeout: a 2 GiB upload on a slow connection legitimately takes
/// a long time. filedrop cuts off a transfer that stalls for 60 seconds, which
/// is the bound that matters.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("building the filedrop http client")
    })
}

fn fail(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        msg.to_string(),
    )
        .into_response()
}

/// The same rules filedrop's `validName` applies, so a bad name is refused
/// before the body is forwarded rather than after.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= MAX_NAME_BYTES
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// `PUT /share/upload?name=…&expires=…&downloads=…`, body = the file.
pub async fn upload(
    State(state): State<AppState>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // filedrop requires a Content-Length (it reserves the storage up front),
    // and knowing it here is what lets the size check happen before the body
    // is read.
    let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return fail(StatusCode::LENGTH_REQUIRED, views::SHARE_ERR_FAILED);
    };
    if len > MAX_UPLOAD_BYTES {
        return fail(StatusCode::PAYLOAD_TOO_LARGE, views::SHARE_ERR_TOO_LARGE);
    }
    if !valid_name(&q.name) {
        return fail(StatusCode::BAD_REQUEST, views::SHARE_ERR_BAD_NAME);
    }

    let data = state.data_root();
    let Some(key) = read_secret(token_path(data, KEY_FILE)).await else {
        warn!("share upload refused: no {KEY_FILE} under data/token");
        return fail(
            StatusCode::SERVICE_UNAVAILABLE,
            views::SHARE_ERR_UNAVAILABLE,
        );
    };
    let endpoint = read_secret(token_path(data, ENDPOINT_FILE))
        .await
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let mut url = format!(
        "{}/{}",
        endpoint.trim_end_matches('/'),
        crate::handlers::encode_path(&q.name)
    );
    // Only the two settings filedrop knows are forwarded, and only when set:
    // filedrop rejects unknown keys, and an empty value would fail its parse.
    let mut sep = '?';
    for (k, v) in [("expires", &q.expires), ("downloads", &q.downloads)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            url.push(sep);
            url.push_str(k);
            url.push('=');
            url.push_str(&crate::handlers::encode_path(v));
            sep = '&';
        }
    }

    let mut req = client()
        .put(url)
        .bearer_auth(key)
        // Set explicitly: a streamed body has no length of its own, and
        // without this hyper would send it chunked and filedrop answer 411.
        .header(header::CONTENT_LENGTH, len)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Also what a visitor cancelling mid-upload looks like; filedrop
            // has already discarded the partial file by then.
            warn!(error = %e, "share upload to filedrop failed");
            return fail(StatusCode::BAD_GATEWAY, views::SHARE_ERR_FAILED);
        }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    match status {
        201 => (
            StatusCode::CREATED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            text.trim().to_string(),
        )
            .into_response(),
        413 => fail(StatusCode::PAYLOAD_TOO_LARGE, views::SHARE_ERR_TOO_LARGE),
        507 => fail(StatusCode::INSUFFICIENT_STORAGE, views::SHARE_ERR_FULL),
        401 => {
            warn!("filedrop rejected the site's API key (revoked or wrong?)");
            fail(
                StatusCode::SERVICE_UNAVAILABLE,
                views::SHARE_ERR_UNAVAILABLE,
            )
        }
        // 400 is a bad setting or an upload that did not complete. filedrop's
        // own one-line message is the most specific thing there is to show.
        _ => {
            let msg = text.trim().trim_start_matches("error:").trim();
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            fail(
                status,
                if msg.is_empty() {
                    views::SHARE_ERR_FAILED
                } else {
                    msg
                },
            )
        }
    }
}

/// filedrop's `GET /filedrop/api/status` answer, as far as the status page
/// reads it. Entries are anonymous by filedrop's design — no name, ID or link —
/// so nothing here can leak a share link.
#[derive(Deserialize, Debug, Clone)]
pub struct Status {
    pub storage: Storage,
    pub files: Files,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Storage {
    /// Total of the files uploaded with this site's key.
    pub used_bytes: u64,
    pub service_limit_bytes: u64,
    /// What a new upload can use, across every key.
    pub service_remaining_bytes: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Files {
    pub items: Vec<OpenFile>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OpenFile {
    pub size_bytes: u64,
    pub expires_in_seconds: u64,
    /// `None` is unlimited.
    pub downloads_left: Option<u64>,
}

/// How long the status page waits on filedrop before showing it as down. The
/// upload client has no overall timeout; a status read is small and has no
/// reason to take long.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Ask filedrop for this key's open files and the service's storage. `None`
/// for anything short of a clean answer — no key, filedrop down, a rejected
/// key, a body that does not parse — each logged, all shown the same way.
async fn fetch_status(state: &AppState) -> Option<Status> {
    let data = state.data_root();
    let Some(key) = read_secret(token_path(data, KEY_FILE)).await else {
        warn!("share status unavailable: no {KEY_FILE} under data/token");
        return None;
    };
    let endpoint = read_secret(token_path(data, ENDPOINT_FILE))
        .await
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let url = format!("{}/api/status", endpoint.trim_end_matches('/'));
    let resp = match client()
        .get(url)
        .bearer_auth(key)
        .timeout(STATUS_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "filedrop status request failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(status = resp.status().as_u16(), "filedrop status answered an error");
        return None;
    }
    match resp.json::<Status>().await {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(error = %e, "filedrop status body did not parse");
            None
        }
    }
}

/// `GET /share/status`: storage left and the files still open. Rendered on
/// the server so the key never reaches the browser.
pub async fn status_page(State(state): State<AppState>) -> Response {
    let status = fetch_status(&state).await;
    views::share_status_page(status.as_ref()).into_response()
}
