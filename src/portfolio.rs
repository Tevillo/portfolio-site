use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// The portfolio is curated in digiKam, not on disk: a photo earns its place by
// carrying a `portfolio/<label>` tag and keeps living wherever it was shot
// (2025/homage/, work/…/edited/, …). Hence the same tag database the People
// pages read, rather than a walk of `photos/portfolio/` — that folder still
// exists but has been emptied, so the tags are now the only source of truth.
use crate::people::{VISIBLE_IMAGE_FILTER, combine_rel, open_readonly};

/// One direct child of the root `portfolio` tag (e.g. `portfolio/pastel`) and
/// the photos carrying it. Renders as one section of the home page.
pub struct Section {
    /// Sub-tag name exactly as digiKam stores it ("pastel"), used verbatim as
    /// the section heading.
    pub label: String,
    pub photos: Vec<SectionPhoto>,
}

pub struct SectionPhoto {
    /// Path relative to the photos root, in the form "2025/foo/bar/baz.jpg".
    pub rel: String,
    pub name: String,
}

pub async fn list_sections(db_path: PathBuf) -> Result<Vec<Section>> {
    tokio::task::spawn_blocking(move || list_sections_blocking(&db_path))
        .await
        .context("portfolio listing task panicked")?
}

fn list_sections_blocking(db_path: &Path) -> Result<Vec<Section>> {
    let conn = open_readonly(db_path)?;
    // Sections are the *direct* children of the root-level `portfolio` tag, so
    // a photo carrying only the bare parent contributes to no section, and any
    // deeper nesting stays off the home page rather than being flattened into an
    // ambiguous heading.
    //
    // `i.status = 1` is the one filter the People queries don't need. Portfolio
    // photos move between folders as the archive is reorganised, and digiKam
    // keeps a row for a file it can no longer find (status 3) so the tags
    // survive the move; reading those back would emit tiles for paths that no
    // longer exist.
    //
    // The ordering is what lets the loop below cut sections on a label change,
    // and keeps each section's photos grouped album-by-album instead of
    // interleaved.
    let sql = format!(
        "
        SELECT t.name, a.relativePath, i.name
        FROM Tags t
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        WHERE t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'portfolio' LIMIT 1)
          AND i.status = 1
          AND {VISIBLE_IMAGE_FILTER}
        ORDER BY t.name COLLATE NOCASE ASC, a.relativePath, i.name COLLATE NOCASE ASC
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut out: Vec<Section> = Vec::new();
    // Reset per section: one photo may legitimately carry two `portfolio/*`
    // tags and appear under both headings, but never twice under one.
    let mut seen: HashSet<String> = HashSet::new();
    for r in rows {
        let (label, album_rel, name) = r?;
        if out.last().map(|s| s.label.as_str()) != Some(label.as_str()) {
            out.push(Section {
                label,
                photos: Vec::new(),
            });
            seen.clear();
        }
        let rel = combine_rel(&album_rel, &name);
        if !seen.insert(rel.clone()) {
            continue;
        }
        if let Some(section) = out.last_mut() {
            section.photos.push(SectionPhoto { rel, name });
        }
    }
    Ok(out)
}
