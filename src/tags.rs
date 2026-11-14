//! What the public site withholds, read from digiKam's `Archive` tag tree.
//!
//! This replaced the old `hidden`-in-the-filename convention. Hiding a
//! photograph is now a tag, never a rename:
//!
//! - **`Archive/hidden`** (or anything under it): served nowhere, the private
//!   archive included.
//! - **Anything else under `Archive`**, or bare `Archive`: off every public page
//!   and route, and shown in the key-gated archive (see `archive.rs`), filed
//!   under the direct child tag it carries (`Archive/Eliana` → section
//!   "Eliana"). A photo tagged with bare `Archive` and no child tag goes in an
//!   unsorted section, labelled with the root tag's own name.
//!
//! Tag names are matched case-insensitively: the owner's tag is `Archive`, and
//! a site that stopped hiding photographs because someone typed `archive` would
//! fail in the worst direction.
//!
//! **Matching is by stem, not by file.** digiKam tags each file separately, so
//! a JPEG and the TIFF or DNG it was exported from can carry different tags,
//! or only one of them can be tagged. A tag on either one applies to every file
//! in that folder with the same stem (case-insensitive), so tagging the scan
//! hides the JPEG made from it, and the RAW beside a hidden JPEG cannot be
//! downloaded from the public route. If the files in a stem group carry
//! different tags, `hidden` wins.
//!
//! The same rule exists twice, once here for the filesystem walks and the
//! file routes, and once as [`withheld_filter!`] for the SQL queries (people,
//! portfolio, faces). Those two must agree, and the tests check that they do.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use tracing::{error, warn};

use crate::paths::{is_jpeg, join_rel, parent_rel};
use crate::people::open_readonly;
use crate::portfolio::{STAR_RANK, Section, SectionPhoto, unique_slug};

/// The root tag. Matched with `COLLATE NOCASE` at `pid = 0`.
pub const ARCHIVE_TAG: &str = "archive";

/// The child of [`ARCHIVE_TAG`] that means "never show this anywhere". It is
/// reserved: it never becomes an archive section.
pub const HIDDEN_TAG: &str = "hidden";

/// The SQL half of the rule: true for an `Images` row (aliased `i`) that is
/// *not* withheld, meaning no file in its album with the same stem carries
/// `Archive` or any tag under it.
///
/// `rtrim(name, replace(name, '.', ''))` is SQLite's way of cutting a filename
/// back to its last dot: it strips trailing characters drawn from the set of
/// every non-dot character in the name. `a.b.jpg` and `a.b.tif` both become
/// `a.b.`, which is the same split `Path::file_stem` makes in [`stem_key`].
///
/// A recursive CTE rather than digiKam's `TagsTree` table, so the test fixtures
/// do not have to maintain a closure table, and so a tag added under
/// `Archive/Eliana/…` is covered without trusting digiKam to have refreshed it.
macro_rules! withheld_filter {
    () => {
        "NOT EXISTS (
            WITH RECURSIVE arch(id) AS (
                SELECT id FROM Tags WHERE pid = 0 AND name = 'archive' COLLATE NOCASE
                UNION ALL
                SELECT t2.id FROM Tags t2 JOIN arch ON t2.pid = arch.id
            )
            SELECT 1 FROM Images s
            JOIN ImageTags wt ON wt.imageid = s.id
            WHERE s.album = i.album
              AND rtrim(s.name, replace(s.name, '.', ''))
                  = rtrim(i.name, replace(i.name, '.', '')) COLLATE NOCASE
              AND wt.tagid IN (SELECT id FROM arch)
        )"
    };
}
pub(crate) use withheld_filter;

/// How a stem group is withheld. `Hidden` outranks `Archived`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Archived,
    Hidden,
}

/// Everything the site needs to know about the `Archive` tree, loaded once per
/// change to the database file. See [`Cache`].
#[derive(Default)]
pub struct TagIndex {
    /// The root tag's name as digiKam spells it (`Archive`), or `None` if the
    /// database has no such tag. It labels the nav tab and the unsorted
    /// section, so those words are the owner's own, taken from the tag
    /// they named.
    root_label: Option<String>,
    /// [`stem_key`] → the strongest tag any file in that stem group carries.
    by_stem: HashMap<String, Kind>,
    /// The archive's sections in display order, with slugs assigned. Holds
    /// JPEGs only, and no hidden ones.
    sections: Vec<Section>,
}

impl TagIndex {
    /// True for a photo the public site must not show or serve, whether hidden
    /// or archived.
    pub fn withheld(&self, rel: &str) -> bool {
        self.by_stem.contains_key(&stem_key(rel))
    }

    /// True for a photo that is shown nowhere, the archive included.
    pub fn hidden(&self, rel: &str) -> bool {
        self.by_stem.get(&stem_key(rel)) == Some(&Kind::Hidden)
    }

    /// True for a photo key holders may see in the archive.
    pub fn archived(&self, rel: &str) -> bool {
        self.by_stem.get(&stem_key(rel)) == Some(&Kind::Archived)
    }

    pub fn root_label(&self) -> Option<&str> {
        self.root_label.as_deref()
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }
}

/// `2026/misc/Misc-4.JPG` → `2026/misc/misc-4`. The folder keeps its case,
/// because the filesystem is case-sensitive about folders and digiKam stores
/// them verbatim. The stem is lowercased, because the RAW and the JPEG beside
/// it are routinely `.DNG` and `.jpg` with the same name.
fn stem_key(rel: &str) -> String {
    let rel = rel.trim_matches('/');
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    join_rel(parent_rel(rel), &stem.to_lowercase())
}

pub async fn load(db_path: PathBuf) -> Result<TagIndex> {
    tokio::task::spawn_blocking(move || load_blocking(&db_path))
        .await
        .context("tag index task panicked")?
}

pub fn load_blocking(db_path: &Path) -> Result<TagIndex> {
    let conn = open_readonly(db_path)?;

    let root_label: Option<String> = conn
        .query_row(
            "SELECT name FROM Tags WHERE pid = 0 AND name = ?1 COLLATE NOCASE
             ORDER BY id LIMIT 1",
            [ARCHIVE_TAG],
            |row| row.get(0),
        )
        .ok();
    let Some(root_label) = root_label else {
        return Ok(TagIndex::default());
    };

    // One row per (file, tag) pair, where the file is anything in the stem
    // group of a tagged file. `section` is the name of the root's direct child
    // that the tag sits under, or NULL for the root tag itself, so
    // `Archive/Eliana/2019` still files under "Eliana".
    //
    // Joining through the stem group (`s` is the tagged file, `i` each of its
    // siblings) is what lets a tag on the TIFF scan put the JPEG made from it
    // into the archive, not merely hide it. The stem expression is the one
    // `withheld_filter!` uses.
    //
    // Every status is read here, unlike the portfolio query's `status = 1`.
    // Withholding errs toward hiding: a row digiKam has lost track of costs
    // nothing to include, and a file that comes back at that path stays
    // hidden. The sections below keep only live rows.
    let sql = format!(
        "
        WITH RECURSIVE sub(id, section) AS (
            SELECT id, NULL FROM Tags WHERE pid = 0 AND name = ?1 COLLATE NOCASE
            UNION ALL
            SELECT t.id, COALESCE(sub.section, t.name) FROM Tags t JOIN sub ON t.pid = sub.id
        )
        SELECT a.relativePath, i.name, sub.section, i.status, {STAR_RANK}
        FROM sub
        JOIN ImageTags it ON it.tagid = sub.id
        JOIN Images s ON s.id = it.imageid
        JOIN Images i ON i.album = s.album
             AND rtrim(i.name, replace(i.name, '.', ''))
                 = rtrim(s.name, replace(s.name, '.', '')) COLLATE NOCASE
        JOIN Albums a ON a.id = i.album
        LEFT JOIN ImageInformation ii ON ii.imageid = i.id
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([ARCHIVE_TAG], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;

    let mut by_stem: HashMap<String, Kind> = HashMap::new();
    // Live JPEGs and what they are filed under. `None` in the set stands for
    // the bare root tag.
    struct Candidate {
        name: String,
        stars: i64,
        sections: HashSet<Option<String>>,
    }
    let mut candidates: HashMap<String, Candidate> = HashMap::new();

    for row in rows {
        let (album, name, section, status, stars) = row?;
        let rel = join_rel(&album, &name);
        let kind = match &section {
            Some(s) if s.eq_ignore_ascii_case(HIDDEN_TAG) => Kind::Hidden,
            _ => Kind::Archived,
        };
        let slot = by_stem.entry(stem_key(&rel)).or_insert(kind);
        *slot = (*slot).max(kind);

        if status == Some(1) && is_jpeg(&name) {
            let c = candidates.entry(rel).or_insert_with(|| Candidate {
                name: name.clone(),
                stars,
                sections: HashSet::new(),
            });
            c.sections.insert(section);
        }
    }

    // (stars, rel, filename), ranked below.
    type Ranked = (i64, String, String);
    // Grouped by section label, compared case-insensitively so `Eliana` and
    // `eliana` on two tags are one section.
    let mut grouped: BTreeMap<String, (String, Vec<Ranked>)> = BTreeMap::new();
    let mut unsorted: Vec<Ranked> = Vec::new();
    for (rel, c) in candidates {
        if by_stem.get(&stem_key(&rel)) == Some(&Kind::Hidden) {
            continue;
        }
        let named: Vec<&String> = c.sections.iter().flatten().collect();
        if named.is_empty() {
            unsorted.push((c.stars, rel, c.name));
            continue;
        }
        for label in named {
            grouped
                .entry(label.to_lowercase())
                .or_insert_with(|| (label.clone(), Vec::new()))
                .1
                .push((c.stars, rel.clone(), c.name.clone()));
        }
    }

    // Stars first, then album and filename, the same order the portfolio uses.
    let into_photos = |mut v: Vec<Ranked>| -> Vec<SectionPhoto> {
        v.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| parent_rel(&a.1).cmp(parent_rel(&b.1)))
                .then_with(|| a.2.to_lowercase().cmp(&b.2.to_lowercase()))
        });
        v.into_iter()
            .map(|(_, rel, name)| SectionPhoto { rel, name })
            .collect()
    };

    // Named sections alphabetically, unsorted last. The slugs are assigned in
    // that same order, so they stay put while the tag set does.
    let mut sections: Vec<Section> = grouped
        .into_values()
        .map(|(label, photos)| Section {
            label,
            slug: String::new(),
            photos: into_photos(photos),
        })
        .collect();
    if !unsorted.is_empty() {
        sections.push(Section {
            label: root_label.clone(),
            slug: String::new(),
            photos: into_photos(unsorted),
        });
    }
    let mut taken = HashSet::new();
    for s in &mut sections {
        s.slug = unique_slug(&s.label, "section", &mut taken);
    }

    Ok(TagIndex {
        root_label: Some(root_label),
        by_stem,
        sections,
    })
}

/// The index for a CLI command (`warm`, `prebuild`, `recent set`, `audit`),
/// read from `photos_root/digikam4.db`.
///
/// Unlike the server's [`Cache`], a failed read is an error. A command that
/// guessed would build zips that include withheld files, or warm renditions for
/// hidden ones, and the owner is right there to see the message.
pub async fn load_for_cli(photos_root: &Path) -> Result<Arc<TagIndex>> {
    let db = photos_root.join("digikam4.db");
    if !db.is_file() {
        return Ok(Arc::new(TagIndex::default()));
    }
    Ok(Arc::new(load(db).await?))
}

/// The current [`TagIndex`], reloaded when the database file's mtime moves.
///
/// One stat per request is the whole cost of keeping it fresh. digiKam writes
/// the file in place, so a tag change shows up on the next request without a
/// restart, and the expensive part (the query) runs only when something
/// changed.
#[derive(Default)]
pub struct Cache {
    slot: Mutex<Option<(Option<SystemTime>, Arc<TagIndex>)>>,
}

impl Cache {
    pub async fn get(&self, db_path: Option<&PathBuf>) -> Arc<TagIndex> {
        let Some(db) = db_path else {
            return Arc::new(TagIndex::default());
        };
        let mtime = tokio::fs::metadata(db).await.and_then(|m| m.modified()).ok();
        if let Some((seen, index)) = self.slot.lock().unwrap().as_ref()
            && *seen == mtime
            && mtime.is_some()
        {
            return index.clone();
        }
        match load(db.clone()).await {
            Ok(index) => {
                let index = Arc::new(index);
                *self.slot.lock().unwrap() = Some((mtime, index.clone()));
                index
            }
            Err(e) => {
                // The database is assumed readable, so this is a fault to fix,
                // not a state to run in. Keep the last good index if there is
                // one, so a read that races digiKam's write does not briefly
                // publish everything.
                error!(error = ?e, "loading the Archive tag index failed");
                match self.slot.lock().unwrap().as_ref() {
                    Some((_, index)) => {
                        warn!("serving the previous Archive tag index");
                        index.clone()
                    }
                    None => Arc::new(TagIndex::default()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stem_ignores_extension_and_case_but_not_folder() {
        assert_eq!(stem_key("2026/misc/Misc-4.JPG"), "2026/misc/misc-4");
        assert_eq!(stem_key("/2026/misc/misc-4.dng"), "2026/misc/misc-4");
        assert_eq!(stem_key("a.b.jpg"), "a.b");
        assert_ne!(stem_key("2026/Misc/a.jpg"), stem_key("2026/misc/a.jpg"));
    }
}
