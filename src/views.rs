use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::work::WorkCounts;

pub struct DirEntry {
    pub name: String,
    pub url: String,
}

pub struct ImageEntry {
    pub name: String,
    pub thumb_url: String,
    pub image_url: String,
    /// Force-download URL for the JPEG itself (served with an attachment
    /// Content-Disposition, unlike `image_url` which displays inline).
    pub jpg_download_url: String,
    /// Download URL for a sibling raw/edit-master file (same basename, raw
    /// extension, same folder), when one exists alongside the JPEG. `None`
    /// hides the RAW choice in the per-photo download menu.
    pub raw_download_url: Option<String>,
}

pub struct Crumb {
    pub label: String,
    pub url: Option<String>,
}

pub struct FolderGroup {
    pub label: String,
    pub path: String,
    pub browse_url: String,
    pub images: Vec<ImageEntry>,
}

pub struct PersonEntry {
    pub name: String,
    pub url: String,
    pub photo_count: u32,
}

pub struct WorkIndexEntry {
    pub name: String,
    pub url: String,
    pub jpeg_count: u32,
    pub raw_count: u32,
}

pub struct WorkFeedPhoto {
    pub name: String,
    pub preview_url: String,
    pub image_url: String,
    pub download_action: String,
    /// Final preview pixel dimensions (post-orientation, post-downscale).
    /// Rendered as `<img width=… height=…>` so the browser reserves the
    /// right space pre-load and `loading="lazy"` can actually defer tiles
    /// that aren't near the viewport.
    pub preview_dims: Option<(u32, u32)>,
}

pub struct WorkFeedSection {
    /// Empty string for files at the job root; otherwise the full subfolder
    /// path (e.g. "digital", "digital/edited", "film/medium-format/positive").
    pub label: String,
    pub photos: Vec<WorkFeedPhoto>,
    /// Unique-per-page identifier used by `collapse.js` to keep the user's
    /// toggle choice in localStorage, e.g. `job:marisol-sam-wedding:digital/edited`.
    pub data_path: String,
    /// True when this section should be open on first load. Sections whose
    /// path contains an `edited` segment (and the job-root section) are
    /// promoted; everything else collapses by default until the client
    /// expands it.
    pub default_open: bool,
}

/// One node in the Obsidian vault sidebar tree.
pub enum NavNode {
    Folder { name: String, children: Vec<NavNode> },
    Note { name: String, url: String, active: bool },
}

fn site_header() -> Markup {
    html! {
        header.site {
            div.site-left {
                a.brand href="/" { "Portfolio" }
                button.theme-toggle type="button" aria-label="Toggle dark mode" {
                    svg.theme-icon.theme-icon-sun viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                        circle cx="12" cy="12" r="4" fill="currentColor" {}
                        g stroke="currentColor" stroke-width="2" stroke-linecap="round" {
                            line x1="12" y1="2" x2="12" y2="5" {}
                            line x1="12" y1="19" x2="12" y2="22" {}
                            line x1="2" y1="12" x2="5" y2="12" {}
                            line x1="19" y1="12" x2="22" y2="12" {}
                            line x1="4.5" y1="4.5" x2="6.5" y2="6.5" {}
                            line x1="17.5" y1="17.5" x2="19.5" y2="19.5" {}
                            line x1="4.5" y1="19.5" x2="6.5" y2="17.5" {}
                            line x1="17.5" y1="6.5" x2="19.5" y2="4.5" {}
                        }
                    }
                    svg.theme-icon.theme-icon-moon viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                        path d="M20 14.5A8 8 0 0 1 9.5 4a7 7 0 1 0 10.5 10.5z" fill="currentColor" {}
                    }
                }
            }
            nav.topnav {
                a href="/" { "Home" }
                a href="/browse" { "Browse" }
                a href="/all" { "All" }
                a href="/people" { "People" }
                a href="/work" { "Work" }
            }
        }
    }
}

/// Inline `<head>` snippet that sets `<html data-theme="...">` before paint
/// to avoid a flash of the wrong theme, plus the deferred toggle handler.
/// Read order: localStorage override -> OS `prefers-color-scheme` -> light.
fn theme_head() -> Markup {
    html! {
        script {
            (PreEscaped(
                "(function(){var k='portfolio-theme',t;\
                 try{t=localStorage.getItem(k);}catch(e){}\
                 if(t!=='dark'&&t!=='light'){t=matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light';}\
                 document.documentElement.dataset.theme=t;})();"
            ))
        }
        script src="/static/theme.js" defer {}
    }
}

fn crumbs_nav(crumbs: &[Crumb]) -> Markup {
    html! {
        nav.crumbs aria-label="breadcrumb" {
            @for (i, c) in crumbs.iter().enumerate() {
                @if i > 0 { span.sep { "/" } }
                @match &c.url {
                    Some(u) => a href=(u) { (c.label) },
                    None => span.current { (c.label) },
                }
            }
        }
    }
}

fn image_grid(images: &[ImageEntry]) -> Markup {
    html! {
        ul.grid {
            @for img in images {
                li.tile {
                    // Per-photo download URLs ride on the anchor as data-* so
                    // the lightbox can surface JPG / RAW choices to the side of
                    // the selected photo (see lightbox.js). `data-raw` is
                    // omitted when no sibling raw exists, hiding the RAW choice.
                    a href=(img.image_url)
                      data-name=(img.name)
                      data-jpg=(img.jpg_download_url)
                      data-raw=[img.raw_download_url.as_deref()] {
                        img src=(img.thumb_url) alt=(img.name) loading="lazy";
                    }
                }
            }
        }
    }
}

pub fn page(title: &str, crumbs: &[Crumb], subdirs: &[DirEntry], images: &[ImageEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Portfolio" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
                script src="/static/lightbox.js" defer {}
            }
            body {
                (site_header())
                main {
                    (crumbs_nav(crumbs))
                    @if !subdirs.is_empty() {
                        section.dirs {
                            h2 { "Folders" }
                            ul.dirlist {
                                @for d in subdirs {
                                    li { a href=(d.url) { (d.name) "/" } }
                                }
                            }
                        }
                    }
                    @if !images.is_empty() {
                        section.gallery {
                            @if !subdirs.is_empty() { h2 { "Photos" } }
                            (image_grid(images))
                        }
                    }
                    @if subdirs.is_empty() && images.is_empty() {
                        p.empty { "Nothing here yet." }
                    }
                }
            }
        }
    }
}

pub fn people_index_page(title: &str, crumbs: &[Crumb], people: &[PersonEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Portfolio" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                (site_header())
                main {
                    (crumbs_nav(crumbs))
                    @if people.is_empty() {
                        p.empty { "No people tagged yet." }
                    } @else {
                        section.dirs {
                            ul.dirlist {
                                @for p in people {
                                    li {
                                        a href=(p.url) {
                                            (p.name)
                                            span.count { " (" (p.photo_count) ")" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn nav_tree(nodes: &[NavNode]) -> Markup {
    html! {
        ul.nav-tree {
            @for node in nodes {
                @match node {
                    NavNode::Folder { name, children } => {
                        li.nav-folder {
                            span.folder-name { (name) }
                            (nav_tree(children))
                        }
                    }
                    NavNode::Note { name, url, active } => {
                        li.nav-note {
                            a href=(url) class=(if *active { "active" } else { "" }) { (name) }
                        }
                    }
                }
            }
        }
    }
}

/// Which vault view is active, so the sidebar toggle can mark itself.
#[derive(Clone, Copy, PartialEq)]
pub enum NetherView {
    Notes,
    Graph,
}

fn nether_sidebar(nav: &[NavNode], view: NetherView) -> Markup {
    html! {
        aside.nether-sidebar {
            a.nether-home href="/nether" { "Nether" }
            nav.nether-views {
                a href="/nether" class=(if view == NetherView::Notes { "active" } else { "" }) { "Notes" }
                a href="/nether/graph" class=(if view == NetherView::Graph { "active" } else { "" }) { "Graph" }
            }
            (nav_tree(nav))
        }
    }
}

/// Render a single vault note: portfolio chrome, a folder-tree sidebar, and the
/// already-rendered note HTML as the main column. `content` is trusted markup.
pub fn nether_page(title: &str, crumbs: &[Crumb], nav: &[NavNode], content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Nether" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                (site_header())
                main.nether {
                    div.nether-layout {
                        (nether_sidebar(nav, NetherView::Notes))
                        article.nether-content {
                            (crumbs_nav(crumbs))
                            div.note-body { (content) }
                        }
                    }
                }
            }
        }
    }
}

/// Render the Obsidian-style graph view. `graph_json` is a trusted JSON string
/// describing nodes and edges, consumed by `graph.js` to lay out the canvas.
pub fn nether_graph_page(crumbs: &[Crumb], nav: &[NavNode], graph_json: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Graph - Nether" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                (site_header())
                main.nether {
                    div.nether-layout {
                        (nether_sidebar(nav, NetherView::Graph))
                        article.nether-content.graph-content {
                            div.graph-stage {
                                (crumbs_nav(crumbs))
                                canvas #graph-canvas {}
                                div.graph-empty hidden { "This vault has no notes to graph yet." }
                            }
                        }
                    }
                }
                script #graph-data type="application/json" { (PreEscaped(graph_json)) }
                script src="/static/graph.js" defer {}
            }
        }
    }
}

pub fn all_page(title: &str, crumbs: &[Crumb], groups: &[FolderGroup]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Portfolio" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
                script src="/static/lightbox.js" defer {}
                script src="/static/collapse.js" defer {}
            }
            body {
                (site_header())
                main {
                    (crumbs_nav(crumbs))
                    @if groups.is_empty() {
                        p.empty { "Nothing here yet." }
                    } @else {
                        @for g in groups {
                            section.gallery data-path=(g.path) {
                                h2 {
                                    button.collapse-toggle type="button" aria-label="Collapse folder" aria-expanded="true" {
                                        svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                                            polyline points="6,9 12,15 18,9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" {}
                                        }
                                    }
                                    a href=(g.browse_url) { (g.label) }
                                }
                                (image_grid(&g.images))
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn work_index_page(title: &str, crumbs: &[Crumb], items: &[WorkIndexEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Portfolio" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                (site_header())
                main {
                    (crumbs_nav(crumbs))
                    @if items.is_empty() {
                        p.empty { "No work yet." }
                    } @else {
                        section.work-index {
                            ul.work-cards {
                                @for j in items {
                                    li.work-card {
                                        a href=(j.url) {
                                            span.work-name { (j.name) }
                                            span.work-counts {
                                                (j.jpeg_count) " JPEG"
                                                @if j.raw_count > 0 { " · " (j.raw_count) " RAW" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One preview tile's `<img>`, with width/height attributes when we have
/// them so the browser reserves the right space pre-load and `loading=lazy`
/// only fetches tiles near the viewport.
fn work_preview_img(p: &WorkFeedPhoto) -> Markup {
    html! {
        @match p.preview_dims {
            Some((w, h)) => img src=(p.preview_url) alt=(p.name) loading="lazy" width=(w) height=(h);,
            None => img src=(p.preview_url) alt=(p.name) loading="lazy";,
        }
    }
}

/// One row in the bulk-download bar — a label plus JPEG/RAW/Both submit
/// buttons. The row is its own `<form>` so the `<input name="scope">` hidden
/// field rides along regardless of which kind button is pressed; no
/// JavaScript needed.
fn download_scope_row(label: &str, scope: &str, counts: &WorkCounts, action: &str) -> Markup {
    use crate::work::{DownloadKind, Scope};
    let parsed = Scope::parse(scope).expect("download_scope_row called with unknown scope");
    let jpeg = counts.count(parsed, DownloadKind::Jpeg);
    let raw = counts.count(parsed, DownloadKind::Raw);
    let both = counts.count(parsed, DownloadKind::Both);
    html! {
        form.dl-row method="post" action=(action) {
            input type="hidden" name="scope" value=(scope);
            span.dl-row-label { (label) }
            div.dl-buttons {
                button type="submit" name="kind" value="jpeg" disabled[jpeg == 0] {
                    "JPEG (" (jpeg) ")"
                }
                button type="submit" name="kind" value="raw" disabled[raw == 0] {
                    "RAW (" (raw) ")"
                }
                button type="submit" name="kind" value="both" disabled[both == 0] {
                    "Both (" (both) ")"
                }
            }
        }
    }
}

/// Job detail view: grid sections like /all but with bigger tiles. The page
/// has two visual states gated by an HttpOnly cookie:
///   - **Pre-auth**: only the password form is interactive; tile links omit
///     `data-download` so the lightbox download button stays hidden.
///   - **Post-auth**: password form disappears, bulk JPEG/RAW buttons appear,
///     and tile links carry `data-download` so the lightbox button renders.
pub fn work_page(
    name: &str,
    crumbs: &[Crumb],
    sections: &[WorkFeedSection],
    total_jpeg_count: u32,
    counts: WorkCounts,
    has_password: bool,
    authorized: bool,
    bulk_action: &str,
    auth_action: &str,
    error: Option<&str>,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (name) " - Work" }
                (theme_head())
                link rel="stylesheet" href="/static/style.css";
                script src="/static/lightbox.js" defer {}
                script src="/static/collapse.js" defer {}
            }
            body {
                (site_header())
                main.work {
                    (crumbs_nav(crumbs))
                    h1.work-title { (name) }

                    @if let Some(msg) = error {
                        div.banner.banner-error role="alert" { (msg) }
                    }

                    @if !has_password {
                        div.work-downloads.dl-locked-banner {
                            p.dl-locked {
                                "Downloads are locked — no password has been set for this job yet."
                            }
                        }
                    } @else if authorized {
                        div.work-downloads {
                            (download_scope_row("Download all", "all", &counts, bulk_action))
                            (download_scope_row("Download edited", "edited", &counts, bulk_action))
                            (download_scope_row("Download original", "original", &counts, bulk_action))
                        }
                    } @else {
                        form.work-downloads.work-auth method="post" action=(auth_action) {
                            label.dl-label for="work-password" { "Password" }
                            input #work-password
                                type="password"
                                name="password"
                                autocomplete="off"
                                placeholder="Enter password to unlock downloads"
                                required;
                            div.dl-buttons {
                                button type="submit" { "Unlock downloads" }
                            }
                        }
                    }

                    @if total_jpeg_count == 0 {
                        p.empty { "No JPEG photos in this job yet." }
                    } @else {
                        @for section in sections {
                            section
                                class=(if section.default_open { "gallery work-gallery" } else { "gallery work-gallery collapsed" })
                                data-path=(section.data_path)
                                data-default-collapsed=(if section.default_open { "false" } else { "true" })
                            {
                                h2 {
                                    button.collapse-toggle type="button"
                                        aria-label=(if section.default_open { "Collapse folder" } else { "Expand folder" })
                                        aria-expanded=(if section.default_open { "true" } else { "false" }) {
                                        svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                                            polyline points="6,9 12,15 18,9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" {}
                                        }
                                    }
                                    @if section.label.is_empty() {
                                        span.section-label { "(root)" }
                                    } @else {
                                        span.section-label { (section.label) }
                                    }
                                    span.section-count { "(" (section.photos.len()) ")" }
                                }
                                ul.grid.work-grid {
                                    @for p in &section.photos {
                                        li.tile {
                                            @if p.download_action.is_empty() {
                                                a href=(p.image_url) data-name=(p.name) {
                                                    (work_preview_img(p))
                                                }
                                            } @else {
                                                a href=(p.image_url)
                                                  data-name=(p.name)
                                                  data-download=(p.download_action) {
                                                    (work_preview_img(p))
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
