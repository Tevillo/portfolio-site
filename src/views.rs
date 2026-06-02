use maud::{DOCTYPE, Markup, PreEscaped, html};

pub struct DirEntry {
    pub name: String,
    pub url: String,
}

pub struct ImageEntry {
    pub name: String,
    pub thumb_url: String,
    pub image_url: String,
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

pub struct JobIndexEntry {
    pub name: String,
    pub url: String,
    pub jpeg_count: u32,
    pub raw_count: u32,
}

pub struct JobFeedPhoto {
    pub name: String,
    pub preview_url: String,
    pub image_url: String,
    pub download_action: String,
    pub exif_datetime: Option<String>,
    pub exif_camera: Option<String>,
}

/// One node in the Obsidian vault sidebar tree.
pub enum NavNode {
    Folder { name: String, children: Vec<NavNode> },
    Note { name: String, url: String, active: bool },
}

fn site_header() -> Markup {
    html! {
        header.site {
            a.brand href="/" { "Portfolio" }
            nav.topnav {
                a href="/" { "Home" }
                a href="/browse" { "Browse" }
                a href="/all" { "All" }
                a href="/people" { "People" }
                a href="/jobs" { "Jobs" }
            }
        }
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
                    a href=(img.image_url) {
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

pub fn jobs_index_page(title: &str, crumbs: &[Crumb], jobs: &[JobIndexEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Portfolio" }
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                (site_header())
                main {
                    (crumbs_nav(crumbs))
                    @if jobs.is_empty() {
                        p.empty { "No jobs yet." }
                    } @else {
                        section.jobs-index {
                            ul.job-cards {
                                @for j in jobs {
                                    li.job-card {
                                        a href=(j.url) {
                                            span.job-name { (j.name) }
                                            span.job-counts {
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

/// Job detail view: vertical feed of large photos with EXIF caption and a
/// sticky download bar. A single form holds the password input; each
/// download button declares its own `formaction`, so per-photo and bulk
/// downloads all share the same input without any JavaScript.
pub fn job_page(
    name: &str,
    crumbs: &[Crumb],
    photos: &[JobFeedPhoto],
    raw_count: u32,
    has_password: bool,
    bulk_action: &str,
    error: Option<&str>,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (name) " - Jobs" }
                link rel="stylesheet" href="/static/style.css";
                script src="/static/lightbox.js" defer {}
            }
            body {
                (site_header())
                main.job {
                    (crumbs_nav(crumbs))
                    h1.job-title { (name) }

                    @if let Some(msg) = error {
                        div.banner.banner-error role="alert" { (msg) }
                    }

                    form.job-downloads method="post" {
                        @if has_password {
                            label.dl-label for="job-password" { "Password" }
                            input #job-password
                                type="password"
                                name="password"
                                autocomplete="off"
                                placeholder="Enter password to download"
                                required;
                        } @else {
                            p.dl-locked {
                                "Downloads are locked — no password has been set for this job yet."
                            }
                        }
                        div.dl-buttons {
                            button
                                type="submit"
                                formaction=(bulk_action)
                                name="kind"
                                value="jpeg"
                                disabled[!has_password || photos.is_empty()] {
                                    "Download all JPEG (" (photos.len()) ")"
                                }
                            button
                                type="submit"
                                formaction=(bulk_action)
                                name="kind"
                                value="raw"
                                disabled[!has_password || raw_count == 0] {
                                    "Download all RAW (" (raw_count) ")"
                                }
                        }

                        @if photos.is_empty() {
                            p.empty { "No JPEG photos in this job yet." }
                        } @else {
                            ul.feed {
                                @for p in photos {
                                    li.feed-item {
                                        a.feed-image href=(p.image_url) data-name=(p.name) {
                                            img src=(p.preview_url) alt=(p.name) loading="lazy";
                                        }
                                        div.feed-caption {
                                            span.feed-name { (p.name) }
                                            @if p.exif_datetime.is_some() || p.exif_camera.is_some() {
                                                span.feed-meta {
                                                    @if let Some(dt) = &p.exif_datetime { (dt) }
                                                    @if p.exif_datetime.is_some() && p.exif_camera.is_some() { " · " }
                                                    @if let Some(cam) = &p.exif_camera { (cam) }
                                                }
                                            }
                                            button
                                                type="submit"
                                                class="feed-download"
                                                formaction=(p.download_action)
                                                disabled[!has_password] {
                                                    "Download"
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
