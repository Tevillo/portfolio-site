use maud::{DOCTYPE, Markup, html};

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

fn site_header() -> Markup {
    html! {
        header.site {
            a.brand href="/" { "Portfolio" }
            nav.topnav {
                a href="/" { "Home" }
                a href="/browse" { "Browse" }
                a href="/all" { "All" }
                a href="/people" { "People" }
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
