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
    pub browse_url: String,
    pub images: Vec<ImageEntry>,
}

fn site_header() -> Markup {
    html! {
        header.site {
            a.brand href="/" { "Portfolio" }
            nav.topnav {
                a href="/" { "Home" }
                a href="/browse" { "Browse" }
                a href="/all" { "All" }
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
                    a href=(img.image_url) target="_blank" rel="noopener" {
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

pub fn all_page(title: &str, crumbs: &[Crumb], groups: &[FolderGroup]) -> Markup {
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
                    @if groups.is_empty() {
                        p.empty { "Nothing here yet." }
                    } @else {
                        @for g in groups {
                            section.gallery {
                                h2 { a href=(g.browse_url) { (g.label) } }
                                (image_grid(&g.images))
                            }
                        }
                    }
                }
            }
        }
    }
}
