use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Rendered pixel dimensions of `thumb_url`, when known. Only the
    /// natural-ratio masonry grids need this: their tiles have no CSS
    /// `aspect-ratio`, so without `<img width height>` every image lays out
    /// at zero height, the whole grid collapses into the viewport, and
    /// `loading="lazy"` defers nothing. Square grids reserve their space via
    /// `aspect-ratio: 1` in CSS and leave this `None`.
    pub dims: Option<(u32, u32)>,
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
    Folder {
        name: String,
        children: Vec<NavNode>,
    },
    Note {
        name: String,
        url: String,
        active: bool,
    },
}

/// Which top-nav entry should render as the current section, so the header can
/// mark it with `aria-current` and an underline. `None` for pages that hang off
/// no nav entry (e.g. a job detail reached from Work still marks Work).
#[derive(Clone, Copy, PartialEq)]
pub enum Nav {
    Home,
    Browse,
    All,
    People,
    Work,
    About,
    /// Pages that belong to no nav entry — the Nether vault, which is reachable
    /// by URL and from the About page but has no top-nav tab of its own.
    None,
}

// ---------------------------------------------------------------------------
// Site copy. Everything a human would want to reword lives here — the About
// page prose, the contact links, and the name in the footer. Edit these
// constants rather than hunting through markup.
// ---------------------------------------------------------------------------

pub const OWNER_NAME: &str = "Paul Borrego";
pub const OWNER_EMAIL: &str = "borregopaulj@gmail.com";

/// One-liner under the name on the About page and in the home page hero, and
/// the `<meta name="description">` for both. Left empty deliberately — write
/// your own. While it is empty both spots render nothing and the description
/// falls back to [`DESCRIPTION_FALLBACK`].
pub const OWNER_TAGLINE: &str = "Film photography enthusiast";

/// Plain factual `<meta name="description">` used only while `OWNER_TAGLINE`
/// is empty. Search results and link previews need *some* description; this is
/// a placeholder, not prose.
const DESCRIPTION_FALLBACK: &str = "Photographs by Paul Borrego.";

/// About page body. Each entry is one paragraph, rendered in order. Empty
/// means the About page shows just the name, portrait and links — add your own
/// paragraphs here, e.g.
///
/// ```ignore
/// pub const ABOUT_PARAGRAPHS: &[&str] = &[
///     "First paragraph.",
///     "Second paragraph.",
/// ];
/// ```
pub const ABOUT_PARAGRAPHS: &[&str] = &[
    "Self hosting enjoyer and lover of film photography. 
    This website contains all of my photos that I have taken and scanned.",
    "If you want to find yorself or a frind check out the \"People\" tab. 
    Any professional work I have done is under the \"Work\" Tab.
    And if you just want to look around \"Browse\" is a folder like system
    sorted by year and then content and \"All\" is all of my folders that can scroll",
];

/// Links rendered as a list at the bottom of the About page. Add or remove
/// rows freely; an empty list simply hides the section.
pub const ABOUT_LINKS: &[(&str, &str)] = &[
    ("Email", "mailto:borregopaulj@gmail.com"),
    ("Notes", "/nether"),
];

/// `<meta name="description">` for the home and About pages: the owner's own
/// tagline when they have written one, the neutral placeholder until then.
fn site_description() -> &'static str {
    if OWNER_TAGLINE.is_empty() {
        DESCRIPTION_FALLBACK
    } else {
        OWNER_TAGLINE
    }
}

/// Optional portrait for the About page: drop a JPEG at `<photos>/about.jpg`
/// and it appears beside the prose. Absent, the page renders text-only.
pub const ABOUT_PORTRAIT_REL: &str = "about.jpg";

fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Newest mtime among the files in `static/`, so a CSS/JS-only deploy still
/// mints a fresh build id even though the binary did not relink.
fn newest_static_mtime() -> Option<u64> {
    let dir = std::env::current_dir().ok()?.join("static");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| mtime_secs(&e.path()))
        .max()
}

/// Identifier for the running build, in hex. Serves double duty: it is the
/// `?v=` cache-busting stamp on every `/static/*` URL, and the value
/// `static/version.js` compares against `GET /version` to decide whether an
/// already-open page is running an outdated build.
///
/// Executable mtime rather than process start time: `reset.sh` has no
/// `set -e`, so a failed `cargo build` still reaches `systemctl restart` and
/// relaunches the *old* binary. A start-time version would force-reload every
/// visitor for a deploy that never happened, and would do the same on every
/// crash-loop restart and host reboot. Cargo only relinks when something
/// actually changed, so exe mtime tracks real deploys.
pub fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(mtime_secs)
            .unwrap_or(0);
        let newest = exe.max(newest_static_mtime().unwrap_or(0));
        // Never fall back to a constant. Assets are served `immutable`, so a
        // stamp that can never change would freeze the CSS/JS in every browser
        // with no URL left to bust — and the reload feature could not rescue
        // it, because the asset URL would be identical. Start time at least
        // moves on the next restart.
        let id = if newest == 0 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(1)
        } else {
            newest
        };
        format!("{id:x}")
    })
}

/// A `/static/...` URL with the cache-busting stamp attached.
fn asset(path: &str) -> String {
    format!("{path}?v={}", build_id())
}

/// Camera-lens mark, inlined as a data URI so the favicon costs no extra
/// request and never 404s (there is no `static/favicon.ico`).
const FAVICON: &str = "data:image/svg+xml,\
     %3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E\
     %3Crect width='32' height='32' rx='7' fill='%236b8e4e'/%3E\
     %3Ccircle cx='16' cy='16' r='7.5' fill='none' stroke='%23f6f7f1' stroke-width='2.5'/%3E\
     %3Ccircle cx='16' cy='16' r='2.25' fill='%23f6f7f1'/%3E%3C/svg%3E";

/// The one `<head>` every page shares. `scripts` lists extra `/static/*.js`
/// files to defer-load beyond the theme handler. Previously each of the seven
/// page functions carried its own copy of this block, so every meta/script
/// change had to be made seven times.
fn head_block(title: &str, description: &str, scripts: &[&str]) -> Markup {
    head_block_with_preload(title, description, scripts, None)
}

/// `preload_image` starts the fetch of the page's Largest Contentful Paint
/// image during HTML parse, rather than waiting for the parser to reach the
/// `<img>` in the body.
fn head_block_with_preload(
    title: &str,
    description: &str,
    scripts: &[&str],
    preload_image: Option<&str>,
) -> Markup {
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (title) }
            meta name="description" content=(description);
            @if let Some(href) = preload_image {
                link rel="preload" as="image" href=(href) fetchpriority="high";
            }
            // Matches --surface in each theme so mobile browser chrome blends
            // with the site header instead of flashing white/black.
            meta name="theme-color" content="#f6f7f1" media="(prefers-color-scheme: light)";
            meta name="theme-color" content="#181a17" media="(prefers-color-scheme: dark)";
            meta property="og:title" content=(title);
            meta property="og:description" content=(description);
            meta property="og:type" content="website";
            link rel="icon" href=(FAVICON);
            // The build this page was rendered by. `version.js` compares it
            // against `GET /version` on tab refocus and reloads the page when
            // a newer build is live, so a tab left open across a deploy does
            // not keep showing stale markup.
            meta name="build-version" content=(build_id());
            (theme_head())
            link rel="stylesheet" href=(asset("/static/style.css"));
            script src=(asset("/static/version.js")) defer {}
            @for src in scripts {
                script src=(asset(src)) defer {}
            }
        }
    }
}

fn site_header(active: Nav) -> Markup {
    html! {
        header.site {
            div.site-left {
                a.brand href="/" aria-current=[(active == Nav::Home).then_some("page")] { "Portfolio" }
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
                a href="/browse" aria-current=[(active == Nav::Browse).then_some("page")] { "Browse" }
                a href="/all" aria-current=[(active == Nav::All).then_some("page")] { "All" }
                a href="/people" aria-current=[(active == Nav::People).then_some("page")] { "People" }
                a href="/work" aria-current=[(active == Nav::Work).then_some("page")] { "Work" }
                a href="/about" aria-current=[(active == Nav::About).then_some("page")] { "About" }
            }
        }
    }
}

fn site_footer() -> Markup {
    html! {
        footer.site-footer {
            span.footer-name { (OWNER_NAME) }
            nav.footer-links {
                a href="/about" { "About" }
                a href=(format!("mailto:{OWNER_EMAIL}")) { "Email" }
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
        script src=(asset("/static/theme.js")) defer {}
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

/// How many leading tiles load eagerly at high priority. These are the ones
/// above the fold on essentially every viewport, so deferring them only delays
/// the Largest Contentful Paint; everything after is lazy.
///
/// This budget is for the *page*, not for each grid — a page with several
/// folder sections must only spend it on the first, since every later section
/// starts well below the fold.
const EAGER_TILES: usize = 2;

/// `masonry` adds the `work-grid` class so the grid renders as a natural-ratio
/// CSS-columns masonry (used by the work and portfolio pages) instead of the
/// default square-cropped grid. `eager` is how many leading tiles skip lazy
/// loading; pass 0 for any grid that is not the first on the page.
fn image_grid(images: &[ImageEntry], masonry: bool, eager: usize) -> Markup {
    html! {
        ul.grid.work-grid[masonry] {
            @for (i, img) in images.iter().enumerate() {
                li.tile {
                    // Per-photo download URLs ride on the anchor as data-* so
                    // the lightbox can surface JPG / RAW choices to the side of
                    // the selected photo (see lightbox.js). `data-raw` is
                    // omitted when no sibling raw exists, hiding the RAW choice.
                    a href=(img.image_url)
                      data-name=(img.name)
                      data-jpg=(img.jpg_download_url)
                      data-raw=[img.raw_download_url.as_deref()] {
                        (grid_img(img, i < eager))
                    }
                }
            }
        }
    }
}

/// One tile's `<img>`. Intrinsic `width`/`height` are emitted whenever known so
/// the browser reserves the tile's real height before the bytes arrive — that
/// is what makes `loading="lazy"` able to skip off-screen photos at all, and it
/// removes the layout shift as each image lands.
fn grid_img(img: &ImageEntry, eager: bool) -> Markup {
    html! {
        @match img.dims {
            Some((w, h)) => img
                src=(img.thumb_url) alt=(img.name)
                width=(w) height=(h)
                decoding="async"
                loading=(if eager { "eager" } else { "lazy" })
                fetchpriority=[eager.then_some("high")];,
            None => img
                src=(img.thumb_url) alt=(img.name)
                decoding="async"
                loading=(if eager { "eager" } else { "lazy" })
                fetchpriority=[eager.then_some("high")];,
        }
    }
}

/// Shared flat gallery page used by the per-folder browse views and per-person
/// photo lists: a folder list plus a single square-cropped photo grid. When
/// `show_favs` is set, a "Favorites only" toggle is shown that filters the grid
/// down to photos living inside a `favs` folder (used by the per-person view).
pub fn page(
    title: &str,
    crumbs: &[Crumb],
    subdirs: &[DirEntry],
    images: &[ImageEntry],
    show_favs: bool,
    active: Nav,
) -> Markup {
    let scripts: &[&str] = if show_favs {
        &["/static/lightbox.js", "/static/favs.js"]
    } else {
        &["/static/lightbox.js"]
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                &format!("{title} - Portfolio"),
                &format!("Photographs in {title}, by {OWNER_NAME}."),
                scripts,
            ))
            body {
                (site_header(active))
                main {
                    (crumbs_nav(crumbs))
                    @if show_favs && !images.is_empty() {
                        section.all-controls {
                            button.favs-toggle type="button" aria-pressed="false" {
                                span.favs-toggle-track { span.favs-toggle-thumb {} }
                                span.favs-toggle-label { "Favorites only" }
                            }
                        }
                    }
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
                            (image_grid(images, false, EAGER_TILES))
                        }
                    }
                    @if subdirs.is_empty() && images.is_empty() {
                        p.empty { "Nothing here yet." }
                    }
                }
                (site_footer())
            }
        }
    }
}

pub fn people_index_page(title: &str, crumbs: &[Crumb], people: &[PersonEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                &format!("{title} - Portfolio"),
                "Photographs indexed by the people in them.",
                &[],
            ))
            body {
                (site_header(Nav::People))
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
                (site_footer())
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
            (head_block(
                &format!("{title} - Nether"),
                &format!("{title} — a note from {OWNER_NAME}'s vault."),
                &[],
            ))
            body {
                (site_header(Nav::None))
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
            (head_block("Graph - Nether", "Link graph of the note vault.", &[]))
            body {
                (site_header(Nav::None))
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
                script src=(asset("/static/graph.js")) defer {}
            }
        }
    }
}

/// Portfolio home page: one collapsible section per `portfolio/*` tag, photos at
/// their natural aspect ratio in the work page's masonry chrome. `/all` renders
/// the same [`FolderGroup`] shape through [`all_photos_page`] instead, which adds
/// the folder tree.
pub fn grouped_gallery_page(groups: &[FolderGroup]) -> Markup {
    // The home page's first tile is the LCP element on every viewport. Telling
    // the browser about it in <head> starts the fetch during HTML parse instead
    // of waiting for the image to be discovered in the body.
    let lcp = groups
        .first()
        .and_then(|g| g.images.first())
        .map(|img| img.thumb_url.as_str());
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block_with_preload(
                "Portfolio - Portfolio",
                site_description(),
                &["/static/lightbox.js", "/static/collapse.js"],
                lcp,
            ))
            body {
                (site_header(Nav::Home))
                main.portfolio {
                    // The home page's own breadcrumb only ever read "Portfolio",
                    // duplicating the brand; a name and a line of context earn
                    // that space better.
                    section.hero {
                        h1.hero-name { (OWNER_NAME) }
                        @if !OWNER_TAGLINE.is_empty() {
                            p.hero-tagline { (OWNER_TAGLINE) }
                        }
                    }
                    @if groups.is_empty() {
                        p.empty { "Nothing here yet." }
                    } @else {
                        @for (gi, g) in groups.iter().enumerate() {
                            section.gallery.work-gallery data-path=(g.path) {
                                h2 {
                                    button.collapse-toggle type="button" aria-label="Collapse folder" aria-expanded="true" {
                                        (chevron())
                                    }
                                    // A tag-driven section's photos are spread
                                    // across the tree, so it has no folder to
                                    // link to and renders as plain text — the
                                    // shared `.section-label` styling is
                                    // element-agnostic, so the heading looks the
                                    // same either way.
                                    @if g.browse_url.is_empty() {
                                        span.section-label { (g.label) }
                                    } @else {
                                        a.section-label href=(g.browse_url) { (g.label) }
                                    }
                                    span.section-count { "(" (g.images.len()) ")" }
                                }
                                // Only the first section is above the fold, so it
                                // is the only one that gets the eager budget.
                                (image_grid(&g.images, true, if gi == 0 { EAGER_TILES } else { 0 }))
                            }
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// /all — folder tree sidebar
// ---------------------------------------------------------------------------

/// One directory in the `/all` sidebar tree.
///
/// `walk_groups` only emits a [`FolderGroup`] for directories that *directly*
/// hold JPEGs, so a year folder whose photos all live in subfolders never got a
/// section — and therefore had nothing to collapse. The tree is rebuilt here
/// from the group paths, so every level becomes a node the reader can fold,
/// image-bearing or not.
struct TreeNode {
    /// Last path segment. The root carries a display name instead.
    name: String,
    /// Folder path relative to the photos root; `""` is the root.
    path: String,
    /// DOM id of this folder's section, so a tree row can link to it.
    id: String,
    browse_url: String,
    /// Images directly in this folder.
    direct: usize,
    /// Images in this folder and every descendant.
    total: usize,
    /// Distance from the root, used to indent the inline section headings that
    /// stand in for the tree on narrow screens.
    depth: usize,
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(name: &str, path: &str) -> Self {
        let browse_url = if path.is_empty() {
            "/browse".to_string()
        } else {
            format!("/browse/{}", crate::handlers::encode_path(path))
        };
        Self {
            name: name.to_string(),
            path: path.to_string(),
            id: String::new(),
            browse_url,
            direct: 0,
            total: 0,
            depth: 0,
            children: Vec::new(),
        }
    }

    /// Pre-order pass that mints DOM ids, records depth, and rolls the image
    /// counts up the tree. Ids are positional rather than derived from the
    /// path: folder names may contain anything, and two distinct paths must
    /// never collide on one id.
    fn finish(&mut self, next_id: &mut usize, depth: usize) -> usize {
        self.id = format!("folder-{}", *next_id);
        *next_id += 1;
        self.depth = depth;
        self.total = self.direct;
        for child in &mut self.children {
            self.total += child.finish(next_id, depth + 1);
        }
        self.total
    }
}

/// Rebuild the directory hierarchy from the flat group list. Children keep the
/// order in which `walk_groups` first mentioned them, which is alphabetical
/// pre-order, so the tree and the page scroll in the same sequence.
fn build_tree(groups: &[FolderGroup]) -> TreeNode {
    let mut root = TreeNode::new("All photos", "");
    for g in groups {
        let node = if g.path.is_empty() {
            &mut root
        } else {
            let mut cur = &mut root;
            let mut acc = String::new();
            for seg in g.path.split('/') {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(seg);
                let idx = match cur.children.iter().position(|c| c.name == seg) {
                    Some(i) => i,
                    None => {
                        cur.children.push(TreeNode::new(seg, &acc));
                        cur.children.len() - 1
                    }
                };
                cur = &mut cur.children[idx];
            }
            cur
        };
        node.direct = g.images.len();
    }
    root.finish(&mut 0, 0);
    root
}

/// Chevron shared by the tree twisties and the inline section headers, so both
/// rotate identically when their folder closes.
fn chevron() -> Markup {
    html! {
        svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
            polyline points="6,9 12,15 18,9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" {}
        }
    }
}

fn folder_icon() -> Markup {
    html! {
        svg.tree-icon viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
            path d="M3.5 6.5a1.5 1.5 0 0 1 1.5-1.5h3.6l1.8 2h8.1a1.5 1.5 0 0 1 1.5 1.5v9a1.5 1.5 0 0 1-1.5 1.5H5a1.5 1.5 0 0 1-1.5-1.5z"
                fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round" {}
        }
    }
}

/// The sticky left-hand tree. Hidden below the desktop breakpoint, where the
/// inline section headers carry the same collapse state instead.
fn folder_tree_sidebar(root: &TreeNode) -> Markup {
    html! {
        aside.tree-sidebar aria-label="Folder tree" {
            div.tree-head {
                span.tree-title { "Albums" }
                div.tree-head-actions {
                    button.tree-action type="button" data-tree-all="open" title="Expand all" { "Expand" }
                    button.tree-action type="button" data-tree-all="close" title="Collapse all" { "Collapse" }
                }
            }
            div.tree-scroll {
                ul.tree-list.tree-root {
                    (tree_row(root))
                }
            }
            div.tree-search {
                input.tree-search-input type="search" placeholder="Search folders…" aria-label="Search folders" autocomplete="off";
            }
        }
    }
}

fn tree_row(node: &TreeNode) -> Markup {
    let has_children = !node.children.is_empty();
    html! {
        li.tree-node data-path=(node.path) data-name=(node.name.to_lowercase()) data-target=(node.id) {
            div.tree-row {
                @if has_children {
                    button.tree-twisty type="button" aria-expanded="true"
                        aria-label=(format!("Collapse {}", node.name)) {
                        (chevron())
                    }
                } @else {
                    span.tree-twisty.tree-twisty-leaf aria-hidden="true" {}
                }
                a.tree-link href=(format!("#{}", node.id)) {
                    (folder_icon())
                    span.tree-name { (node.name) }
                    span.tree-count { (node.total) }
                }
            }
            @if has_children {
                ul.tree-list {
                    @for child in &node.children {
                        (tree_row(child))
                    }
                }
            }
        }
    }
}

/// Heading for one `/all` section. Folders with no photos of their own still
/// get one: on narrow screens, where the sidebar is hidden, it is the only
/// handle for collapsing a whole year or trip.
fn tree_section_heading(node: &TreeNode, group: Option<&FolderGroup>) -> Markup {
    let label = if node.path.is_empty() {
        "Photos (root)"
    } else {
        node.path.as_str()
    };
    let count = match group {
        Some(g) => g.images.len(),
        None => node.total,
    };
    html! {
        h2 {
            button.collapse-toggle type="button" aria-label="Collapse folder" aria-expanded="true" {
                (chevron())
            }
            a.section-label href=(group.map_or(node.browse_url.as_str(), |g| g.browse_url.as_str())) { (label) }
            span.section-count { "(" (count) ")" }
        }
    }
}

/// Emit one section per tree node, in the same pre-order the sidebar lists, so
/// clicking a row always scrolls to a target that exists. `eager` is the
/// remaining above-the-fold image budget: the first section that actually has
/// photos consumes it, everything below stays lazy.
fn tree_sections(node: &TreeNode, groups: &[FolderGroup], eager: &mut usize) -> Markup {
    let group = groups.iter().find(|g| g.path == node.path);
    html! {
        @match group {
            Some(g) => {
                section.gallery data-path=(node.path) id=(node.id) style=(format!("--depth:{}", node.depth)) {
                    (tree_section_heading(node, Some(g)))
                    (image_grid(&g.images, false, std::mem::take(eager)))
                }
            }
            None => {
                section.gallery.folder-node data-path=(node.path) id=(node.id) style=(format!("--depth:{}", node.depth)) {
                    (tree_section_heading(node, None))
                }
            }
        }
        @for child in &node.children {
            (tree_sections(child, groups, eager))
        }
    }
}

/// `/all`: the whole library on one page, with a collapsible folder tree down
/// the left on desktop and inline collapsible headers on narrow screens. Both
/// drive the same per-path open/closed state in `collapse.js`.
pub fn all_photos_page(crumbs: &[Crumb], groups: &[FolderGroup]) -> Markup {
    let root = build_tree(groups);
    let mut eager = EAGER_TILES;
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                "All - Portfolio",
                "Every photograph in the library, grouped by folder.",
                &["/static/lightbox.js", "/static/collapse.js", "/static/favs.js"],
            ))
            body {
                (site_header(Nav::All))
                main.all-layout {
                    @if groups.is_empty() {
                        div.all-column {
                            (crumbs_nav(crumbs))
                            p.empty { "Nothing here yet." }
                        }
                    } @else {
                        (folder_tree_sidebar(&root))
                        div.all-column {
                            (crumbs_nav(crumbs))
                            section.all-controls {
                                button.favs-toggle type="button" aria-pressed="false" {
                                    span.favs-toggle-track { span.favs-toggle-thumb {} }
                                    span.favs-toggle-label { "Favorites only" }
                                }
                                // Duplicated from the sidebar head so the same
                                // reach exists on phones, where the tree is not
                                // rendered at all.
                                div.all-controls-actions {
                                    button.tree-action type="button" data-tree-all="open" { "Expand all" }
                                    button.tree-action type="button" data-tree-all="close" { "Collapse all" }
                                }
                            }
                            (tree_sections(&root, groups, &mut eager))
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}

pub fn work_index_page(title: &str, crumbs: &[Crumb], items: &[WorkIndexEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                &format!("{title} - Portfolio"),
                "Client galleries and photo deliveries.",
                &[],
            ))
            body {
                (site_header(Nav::Work))
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
                (site_footer())
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
            Some((w, h)) => img src=(p.preview_url) alt=(p.name) loading="lazy" decoding="async" width=(w) height=(h);,
            None => img src=(p.preview_url) alt=(p.name) loading="lazy" decoding="async";,
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
            (head_block(
                &format!("{name} - Work"),
                &format!("Photo delivery for {name}."),
                &["/static/lightbox.js", "/static/collapse.js"],
            ))
            body {
                (site_header(Nav::Work))
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
                (site_footer())
            }
        }
    }
}

/// About page: the prose from `ABOUT_PARAGRAPHS` in a single readable column,
/// with an optional portrait alongside. `portrait` is the preview URL for
/// `ABOUT_PORTRAIT_REL` plus its rendered dimensions, or `None` when that file
/// does not exist under the photos root.
pub fn about_page(portrait: Option<(&str, (u32, u32))>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                &format!("About - {OWNER_NAME}"),
                site_description(),
                &[],
            ))
            body {
                (site_header(Nav::About))
                main.about {
                    div.about-layout {
                        @if let Some((src, (w, h))) = portrait {
                            img.about-portrait
                                src=(src)
                                alt=(format!("Portrait of {OWNER_NAME}"))
                                width=(w) height=(h)
                                decoding="async"
                                fetchpriority="high";
                        }
                        div.about-body {
                            h1.about-name { (OWNER_NAME) }
                            @if !OWNER_TAGLINE.is_empty() {
                                p.about-tagline { (OWNER_TAGLINE) }
                            }
                            @for para in ABOUT_PARAGRAPHS {
                                p { (para) }
                            }
                            @if !ABOUT_LINKS.is_empty() {
                                h2.about-links-heading { "Elsewhere" }
                                ul.about-links {
                                    @for (label, href) in ABOUT_LINKS {
                                        li { a href=(href) { (label) } }
                                    }
                                }
                            }
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}
