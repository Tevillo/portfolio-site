//! Serves a read-only view of an Obsidian vault under `/nether`.
//!
//! Notes are rendered from markdown to HTML with `[[wikilinks]]` resolved by
//! note name against the whole vault, mirroring Obsidian's own link behaviour.
//! A folder-tree sidebar listing every note is rendered alongside each page.
//! No links into `/nether` are exposed elsewhere on the site.
//!
//! Embedded images are the one kind of vault *file* served as bytes, and only
//! from [`MEDIA_DIR`] — see [`resolve_media`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::PreEscaped;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html as md_html};

use crate::handlers::{build_etag, encode_path, matches_etag, not_modified};
use crate::paths::safe_resolve;
use crate::state::AppState;
use crate::views::{self, Crumb, NavNode};

/// The note rendered at `/nether` (the vault's home note).
const HOME_NOTE: &str = "Vault";

/// The only vault folder whose files are servable as bytes: recipe cards. The
/// rest of the vault is private notes, so an image pointing anywhere else is
/// dropped from the rendered page rather than linked.
const MEDIA_DIR: &str = "Home/Cooking/Recipes/Engineer";

/// URL prefix the rewritten `<img src>` values point at, handled by [`media`].
const MEDIA_URL: &str = "/nether-media";

/// Vault folders that are never exposed: personal records. The walk skips them,
/// so they stay out of the sidebar tree, the graph, the wikilink index and the
/// sitemap, and [`render`] refuses a hand-typed URL into one.
const PRIVATE_DIRS: &[&str] = &["Home/Medical"];

/// Whether a `/`-separated vault-relative path names a [`PRIVATE_DIRS`] folder
/// or anything inside it.
fn is_private(rel: &str) -> bool {
    PRIVATE_DIRS
        .iter()
        .any(|dir| rel == *dir || rel.strip_prefix(*dir).is_some_and(|r| r.starts_with('/')))
}

pub async fn root(State(state): State<AppState>) -> Response {
    render(&state, HOME_NOTE, true).await
}

pub async fn note(State(state): State<AppState>, AxumPath(path): AxumPath<String>) -> Response {
    // Trim a trailing slash so `/nether/Home/` and `/nether/Home` behave alike.
    render(&state, path.trim_end_matches('/'), false).await
}

/// Obsidian-style graph view: every note is a node, every resolved `[[wikilink]]`
/// an edge. The layout is computed client-side; we only ship the topology.
pub async fn graph(State(state): State<AppState>) -> Response {
    let root = state.nether_root();
    let notes = collect_notes(root).await;
    let index = build_index(&notes);
    let data = build_graph(root, &notes, &index).await;

    let crumbs = vec![
        Crumb {
            label: "Nether".into(),
            url: Some("/nether".into()),
        },
        Crumb {
            label: "Graph".into(),
            url: None,
        },
    ];
    let nav = build_nav(&notes, "");
    views::nether_graph_page(&crumbs, &nav, &data.to_json()).into_response()
}

/// GET /nether-media/*path — an image embedded in a note, resolved against
/// [`MEDIA_DIR`] and nothing above it. Paths are minted by [`resolve_media`],
/// but this handler re-checks the folder, the extension and hidden names so a
/// hand-typed URL can't reach the rest of the vault either.
pub async fn media(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(mime) = image_mime(&rel) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if rel.split('/').any(|c| c.starts_with('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Canonicalize the media root itself so the containment check in
    // `safe_resolve` holds even if a component of it is a symlink.
    let Ok(root) = tokio::fs::canonicalize(state.nether_root().join(MEDIA_DIR)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(path) = safe_resolve(&root, &rel).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !meta.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let etag = build_etag(meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len());
    if matches_etag(&headers, &etag) {
        return not_modified(&etag);
    }
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (header::ETAG, etag.as_str()),
        ],
        bytes,
    )
        .into_response()
}

/// Content type for a servable image name, or `None` if the extension isn't one
/// we serve. SVG is excluded deliberately: it can carry script, and these are
/// photos of recipe cards.
fn image_mime(name: &str) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "avif" => Some("image/avif"),
        _ => None,
    }
}

/// Resolve an image `src` written in a note to its path *relative to*
/// [`MEDIA_DIR`], or `None` when it points anywhere else.
///
/// A src containing `/` is taken as relative to the note's own folder, the way
/// Obsidian and any markdown renderer read it. A bare filename is looked up
/// directly in [`MEDIA_DIR`], which is how Obsidian's `![[image.png]]` embeds
/// name their target. Absolute paths, remote URLs and `..` escapes all fail
/// the containment check and so return `None`.
fn resolve_media(note_dir: &str, dest: &str) -> Option<String> {
    if dest.is_empty() || dest.starts_with('/') || dest.contains("://") {
        return None;
    }
    let dest = percent_decode(dest);
    let base = if dest.contains('/') {
        note_dir
    } else {
        MEDIA_DIR
    };

    // Normalize lexically; `..` popping past the start means the src reaches
    // outside the vault, which can never land in MEDIA_DIR.
    let mut parts: Vec<&str> = Vec::new();
    for comp in base.split('/').chain(dest.split('/')) {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }

    let rel = parts
        .join("/")
        .strip_prefix(MEDIA_DIR)?
        .strip_prefix('/')?
        .to_string();
    if rel.split('/').any(|c| c.starts_with('.')) || image_mime(&rel).is_none() {
        return None;
    }
    Some(rel)
}

/// Decode `%XX` escapes; malformed escapes are left as written. Note authors
/// write spaces literally, but Obsidian percent-encodes them when it rewrites
/// a link after a rename.
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn render(state: &AppState, rel_no_ext: &str, is_home: bool) -> Response {
    if is_private(rel_no_ext) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let root = state.nether_root();

    // The vault is small; rescanning per request keeps the sidebar and link
    // index live against edits without any cache-invalidation machinery.
    let notes = collect_notes(root).await;
    let index = build_index(&notes);

    let file_rel = format!("{rel_no_ext}.md");
    let abs = match safe_resolve(root, &file_rel).await {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let source = match tokio::fs::read_to_string(&abs).await {
        Ok(s) => s,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Image srcs are resolved relative to the note's own folder.
    let note_dir = rel_no_ext.rsplit_once('/').map_or("", |(dir, _)| dir);
    let expanded = expand_wikilinks(&expand_embeds(&source), &index);
    let body = render_markdown(&expanded, note_dir);
    let nav = build_nav(&notes, rel_no_ext);
    let crumbs = build_crumbs(rel_no_ext, is_home);
    let title = rel_no_ext.rsplit('/').next().unwrap_or(rel_no_ext);

    // The vault home is reachable as both /nether and /nether/<home note>;
    // both spellings name /nether as the original so only one gets indexed.
    let canonical = if is_home {
        "/nether".to_string()
    } else {
        format!("/nether/{}", encode_path(rel_no_ext))
    };
    views::nether_page(title, &canonical, &crumbs, &nav, PreEscaped(body)).into_response()
}

/// Walk the vault and return every note's path relative to the root, including
/// the `.md` suffix, using `/` separators. Dotfiles/dirs (`.obsidian`,
/// `.trash`, `.git`) and [`PRIVATE_DIRS`] are skipped.
/// Canonical `/nether/...` paths for every note in the vault, for the sitemap.
/// Lives here rather than in `handlers` so the vault's layout rules — which
/// files count as notes, how a path becomes a URL — stay in one module.
pub(crate) async fn sitemap_paths(root: &Path) -> Vec<String> {
    let mut out = vec!["/nether".to_string(), "/nether/graph".to_string()];
    for rel in collect_notes(root).await {
        let no_ext = rel.strip_suffix(".md").unwrap_or(&rel);
        out.push(format!("/nether/{}", encode_path(no_ext)));
    }
    out
}

async fn collect_notes(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((abs, rel)) = stack.pop() {
        let mut read = match tokio::fs::read_dir(&abs).await {
            Ok(r) => r,
            Err(_) => continue,
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
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if is_private(&child_rel) {
                continue;
            }
            if ftype.is_dir() {
                stack.push((entry.path(), child_rel));
            } else if ftype.is_file() && has_md_ext(&name) {
                out.push(child_rel);
            }
        }
    }
    out.sort();
    out
}

fn has_md_ext(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn strip_md(rel: &str) -> &str {
    rel.strip_suffix(".md")
        .or_else(|| rel.strip_suffix(".MD"))
        .unwrap_or(rel)
}

/// Map a lowercased note name to its extension-less relative path, so a bare
/// `[[Cooking]]` resolves to `Home/Cooking/Cooking`. First match wins on the
/// rare name collision.
fn build_index(notes: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for rel in notes {
        let no_ext = strip_md(rel);
        let stem = no_ext.rsplit('/').next().unwrap_or(no_ext).to_lowercase();
        map.entry(stem).or_insert_with(|| no_ext.to_string());
    }
    map
}

/// Rewrite Obsidian image embeds — `![[image.png]]`, optionally with a
/// `|caption` or `|300` size hint — into standard markdown images, so both
/// spellings reach the image handling in [`render_markdown`]. Embeds of
/// anything else (note transclusions) are left for `expand_wikilinks`.
fn expand_embeds(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(start) = rest.find("![[") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[..start + 3]);
            rest = after;
            continue;
        };
        let inner = &after[..end];
        let (target, alias) = match inner.split_once('|') {
            Some((t, a)) => (t.trim(), a.trim()),
            None => (inner.trim(), ""),
        };
        if image_mime(target).is_none() {
            // Not an image; emit as written and let the wikilink pass see it.
            out.push_str(&rest[..start + 3 + end + 2]);
            rest = &after[end + 2..];
            continue;
        }
        out.push_str(&rest[..start]);
        let _ = write!(
            out,
            "![{}]({})",
            escape_link_text(alias),
            encode_path(target)
        );
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Replace Obsidian `[[target]]` / `[[target|alias]]` links with standard
/// markdown links into `/nether/...`. Unresolved links render as muted text.
fn expand_wikilinks(src: &str, index: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            // No closing delimiter; emit the marker literally and move on.
            out.push_str("[[");
            rest = after;
            continue;
        };
        let inner = &after[..end];
        rest = &after[end + 2..];

        let (target, alias) = match inner.split_once('|') {
            Some((t, a)) => (t.trim(), Some(a.trim())),
            None => (inner.trim(), None),
        };
        // Drop any `#heading` / `^block` anchor, then key on the final path
        // component so both `[[Note]]` and `[[folder/Note]]` resolve.
        let name = target.split(['#', '^']).next().unwrap_or(target).trim();
        let key = name.rsplit('/').next().unwrap_or(name).to_lowercase();
        let display = alias.unwrap_or(if name.is_empty() { target } else { name });

        match index.get(&key) {
            Some(rel) => {
                let url = format!("/nether/{}", encode_path(rel));
                out.push('[');
                out.push_str(&escape_link_text(display));
                out.push_str("](");
                out.push_str(&url);
                out.push(')');
            }
            None => {
                out.push_str("<span class=\"wikilink-missing\">");
                out.push_str(&html_escape(display));
                out.push_str("</span>");
            }
        }
    }
    out.push_str(rest);
    out
}

/// A node (one note) plus an edge (one resolved link) in the vault graph.
struct GraphData {
    nodes: Vec<GraphNode>,
    edges: Vec<(usize, usize)>,
}

struct GraphNode {
    /// Extension-less relative path, e.g. `Home/Cooking/Cooking`. Used as the
    /// stable id the client maps edges onto.
    id: String,
    label: String,
    url: String,
}

impl GraphData {
    /// Serialize to a compact JSON object: `{nodes:[{id,label,url,deg}], edges:[[i,j]]}`.
    /// Edges reference nodes by array index to keep the payload small.
    fn to_json(&self) -> String {
        let mut degree = vec![0u32; self.nodes.len()];
        for &(a, b) in &self.edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let mut out = String::from("{\"nodes\":[");
        for (i, n) in self.nodes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"id\":{},\"label\":{},\"url\":{},\"deg\":{}}}",
                json_str(&n.id),
                json_str(&n.label),
                json_str(&n.url),
                degree[i],
            );
        }
        out.push_str("],\"edges\":[");
        for (i, &(a, b)) in self.edges.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "[{a},{b}]");
        }
        out.push_str("]}");
        out
    }
}

/// Build the link graph by reading every note and resolving its wikilinks
/// against the vault index. Edges are undirected and de-duplicated.
async fn build_graph(root: &Path, notes: &[String], index: &HashMap<String, String>) -> GraphData {
    let mut nodes = Vec::with_capacity(notes.len());
    let mut idx_of = HashMap::new();
    for rel in notes {
        let no_ext = strip_md(rel);
        let label = no_ext.rsplit('/').next().unwrap_or(no_ext).to_string();
        idx_of.insert(no_ext.to_string(), nodes.len());
        nodes.push(GraphNode {
            id: no_ext.to_string(),
            label,
            url: format!("/nether/{}", encode_path(no_ext)),
        });
    }

    let mut seen = HashSet::new();
    let mut edges = Vec::new();
    for rel in notes {
        let from = idx_of[strip_md(rel)];
        let Ok(src) = tokio::fs::read_to_string(root.join(rel)).await else {
            continue;
        };
        for target in resolve_links(&src, index) {
            let Some(&to) = idx_of.get(&target) else {
                continue;
            };
            if to == from {
                continue;
            }
            // Normalize endpoint order so A->B and B->A collapse to one edge.
            let key = if from < to { (from, to) } else { (to, from) };
            if seen.insert(key) {
                edges.push(key);
            }
        }
    }

    GraphData { nodes, edges }
}

/// Collect the extension-less targets of every `[[wikilink]]` in `src` that
/// resolves to a real note. Mirrors the resolution rules of `expand_wikilinks`.
fn resolve_links(src: &str, index: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = &after[..end];
        rest = &after[end + 2..];

        let target = inner.split_once('|').map_or(inner, |(t, _)| t).trim();
        let name = target.split(['#', '^']).next().unwrap_or(target).trim();
        let key = name.rsplit('/').next().unwrap_or(name).to_lowercase();
        if let Some(rel) = index.get(&key) {
            out.push(rel.clone());
        }
    }
    out
}

/// Escape a string as a JSON string literal, including the surrounding quotes.
/// `<` is escaped so the payload is safe to embed inside a `<script>` tag.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Escape characters that would break markdown link text.
fn escape_link_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render markdown to HTML. Single newlines become `<br>` to match Obsidian's
/// reading view, where source line breaks are preserved.
///
/// Images are pointed at `/nether-media`; one whose src doesn't resolve into
/// [`MEDIA_DIR`] is dropped entirely (alt text included) rather than left as a
/// broken `<img>`, since a note's other embeds are vault-private files that
/// were never servable in the first place.
fn render_markdown(md: &str, note_dir: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let mut events = Vec::new();
    let mut dropping = false;
    for ev in Parser::new_ext(md, opts) {
        if dropping {
            dropping = !matches!(ev, Event::End(TagEnd::Image));
            continue;
        }
        match ev {
            Event::SoftBreak => events.push(Event::HardBreak),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => match resolve_media(note_dir, &dest_url) {
                Some(rel) => events.push(Event::Start(Tag::Image {
                    link_type,
                    dest_url: format!("{MEDIA_URL}/{}", encode_path(&rel)).into(),
                    title,
                    id,
                })),
                None => dropping = true,
            },
            other => events.push(other),
        }
    }

    let mut html = String::new();
    md_html::push_html(&mut html, events.into_iter());
    html
}

/// Build the sidebar tree from the flat note list, marking `current` active.
fn build_nav(notes: &[String], current: &str) -> Vec<NavNode> {
    let mut root = Dir::default();
    for rel in notes {
        let no_ext = strip_md(rel);
        let comps: Vec<&str> = no_ext.split('/').collect();
        root.insert(&comps, no_ext);
    }
    root.into_nodes(current)
}

#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    notes: Vec<(String, String)>, // (display name, rel path without extension)
}

impl Dir {
    fn insert(&mut self, comps: &[&str], rel_no_ext: &str) {
        match comps {
            [] => {}
            [name] => self.notes.push((name.to_string(), rel_no_ext.to_string())),
            [head, tail @ ..] => self
                .dirs
                .entry(head.to_string())
                .or_default()
                .insert(tail, rel_no_ext),
        }
    }

    /// Folders first (alphabetical via the BTreeMap), then notes alphabetically.
    fn into_nodes(self, current: &str) -> Vec<NavNode> {
        let mut nodes = Vec::new();
        for (name, sub) in self.dirs {
            nodes.push(NavNode::Folder {
                name,
                children: sub.into_nodes(current),
            });
        }
        let mut notes = self.notes;
        notes.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        for (name, rel) in notes {
            nodes.push(NavNode::Note {
                url: format!("/nether/{}", encode_path(&rel)),
                active: rel == current,
                name,
            });
        }
        nodes
    }
}

fn build_crumbs(rel_no_ext: &str, is_home: bool) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb {
        label: "Nether".into(),
        url: if is_home {
            None
        } else {
            Some("/nether".into())
        },
    }];
    if is_home {
        return crumbs;
    }
    // Intermediate folders are not notes, so they appear as plain labels.
    let parts: Vec<&str> = rel_no_ext.split('/').filter(|s| !s.is_empty()).collect();
    for part in parts {
        crumbs.push(Crumb {
            label: part.to_string(),
            url: None,
        });
    }
    crumbs
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE_DIR: &str = "Home/Cooking/Recipes";

    #[test]
    fn private_dirs_cover_the_folder_and_its_contents() {
        assert!(is_private("Home/Medical"));
        assert!(is_private("Home/Medical/Notes"));
        assert!(is_private("Home/Medical/Notes/Visit.md"));
        assert!(!is_private("Home"));
        assert!(!is_private("Home/Medicals"));
        assert!(!is_private("Medical"));
        assert!(!is_private("Home/Cooking"));
    }

    #[test]
    fn resolves_relative_to_the_note() {
        assert_eq!(
            resolve_media(RECIPE_DIR, "Engineer/card.png").as_deref(),
            Some("card.png")
        );
        assert_eq!(
            resolve_media("Home/Cooking", "Recipes/Engineer/sub/card.jpg").as_deref(),
            Some("sub/card.jpg")
        );
        assert_eq!(
            resolve_media(RECIPE_DIR, "Engineer/chili%20noodles.png").as_deref(),
            Some("chili noodles.png")
        );
    }

    /// Obsidian's `![[card.png]]` embeds name the file, not its path.
    #[test]
    fn resolves_a_bare_filename_in_the_media_dir() {
        assert_eq!(resolve_media("", "card.png").as_deref(), Some("card.png"));
        assert_eq!(
            resolve_media("Programming", "card.png").as_deref(),
            Some("card.png")
        );
    }

    #[test]
    fn rejects_anything_outside_the_media_dir() {
        assert_eq!(resolve_media(RECIPE_DIR, "Engineering/card.png"), None);
        assert_eq!(resolve_media(RECIPE_DIR, "../secret.png"), None);
        assert_eq!(resolve_media(RECIPE_DIR, "Engineer/../../card.png"), None);
        assert_eq!(resolve_media(RECIPE_DIR, "/home/pborrego/card.png"), None);
        assert_eq!(
            resolve_media(RECIPE_DIR, "https://example.com/card.png"),
            None
        );
        // Escaping the vault entirely, as a note written against a filesystem
        // path outside it would.
        assert_eq!(
            resolve_media(RECIPE_DIR, "../../../../nether/Home/card.png"),
            None
        );
    }

    #[test]
    fn rejects_non_image_and_hidden_files() {
        assert_eq!(resolve_media(RECIPE_DIR, "Engineer/card.pdf"), None);
        assert_eq!(resolve_media(RECIPE_DIR, "Engineer/card.svg"), None);
        assert_eq!(resolve_media(RECIPE_DIR, "Engineer/notes.md"), None);
        assert_eq!(resolve_media(RECIPE_DIR, "Engineer/.hidden.png"), None);
    }

    #[test]
    fn rewrites_allowed_images_and_drops_the_rest() {
        let html = render_markdown(
            "![card](Engineer/card.png)\n![nope](Engineering/card.png)\n![out](/etc/card.png)",
            RECIPE_DIR,
        );
        assert!(
            html.contains(r#"<img src="/nether-media/card.png" alt="card""#),
            "{html}"
        );
        assert_eq!(html.matches("<img").count(), 1, "{html}");
        assert!(!html.contains("nope") && !html.contains("out"), "{html}");
    }

    #[test]
    fn expands_obsidian_image_embeds_only() {
        assert_eq!(expand_embeds("![[card.png]]"), "![](card.png)");
        assert_eq!(expand_embeds("![[card.png|A card]]"), "![A card](card.png)");
        assert_eq!(expand_embeds("![[my card.png]]"), "![](my%20card.png)");
        // Note transclusions stay untouched for the wikilink pass.
        assert_eq!(expand_embeds("![[Some Note]]"), "![[Some Note]]");
    }

    #[test]
    fn embed_syntax_renders_through_to_an_img() {
        let html = render_markdown(&expand_embeds("![[card.png]]"), "Programming");
        assert!(html.contains(r#"src="/nether-media/card.png""#), "{html}");
    }
}
