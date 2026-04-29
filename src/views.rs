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
                header.site {
                    a.brand href="/" { "Portfolio" }
                }
                main {
                    nav.crumbs aria-label="breadcrumb" {
                        @for (i, c) in crumbs.iter().enumerate() {
                            @if i > 0 { span.sep { "/" } }
                            @match &c.url {
                                Some(u) => a href=(u) { (c.label) },
                                None => span.current { (c.label) },
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
                    @if subdirs.is_empty() && images.is_empty() {
                        p.empty { "Nothing here yet." }
                    }
                }
            }
        }
    }
}
