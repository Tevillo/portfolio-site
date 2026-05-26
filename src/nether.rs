//! Serves a read-only view of an Obsidian vault under `/nether`.
//!
//! Notes are rendered from markdown to HTML with `[[wikilinks]]` resolved by
//! note name against the whole vault, mirroring Obsidian's own link behaviour.
//! A folder-tree sidebar listing every note is rendered alongside each page.
//! No links into `/nether` are exposed elsewhere on the site.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::PreEscaped;
use pulldown_cmark::{Event, Options, Parser, html as md_html};

use crate::handlers::encode_path;
use crate::paths::safe_resolve;
use crate::state::AppState;
use crate::views::{self, Crumb, NavNode};

/// The note rendered at `/nether` (the vault's home note).
const HOME_NOTE: &str = "Vault";

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

async fn render(state: &AppState, rel_no_ext: &str, is_home: bool) -> Response {
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

    let body = render_markdown(&expand_wikilinks(&source, &index));
    let nav = build_nav(&notes, rel_no_ext);
    let crumbs = build_crumbs(rel_no_ext, is_home);
    let title = rel_no_ext.rsplit('/').next().unwrap_or(rel_no_ext);

    views::nether_page(title, &crumbs, &nav, PreEscaped(body)).into_response()
}

/// Walk the vault and return every note's path relative to the root, including
/// the `.md` suffix, using `/` separators. Dotfiles/dirs (`.obsidian`,
/// `.trash`, `.git`) are skipped.
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
async fn build_graph(
    root: &Path,
    notes: &[String],
    index: &HashMap<String, String>,
) -> GraphData {
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
fn render_markdown(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::SoftBreak => Event::HardBreak,
        other => other,
    });
    let mut html = String::new();
    md_html::push_html(&mut html, parser);
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
        url: if is_home { None } else { Some("/nether".into()) },
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
