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
/// the photos carrying it. Renders as one page of the portfolio.
pub struct Section {
    /// Sub-tag name exactly as digiKam stores it ("pastel"), used verbatim as
    /// the section heading.
    pub label: String,
    /// URL-safe form of `label`, unique across the returned set. This is the
    /// `:slug` in `/portfolio/:slug`. See [`slug`] and [`assign_slugs`].
    pub slug: String,
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
    // The ordering has three jobs, in this order:
    //
    // 1. `t.name` first, always. The loop below cuts a new section every time
    //    the label changes, so a section's rows have to be contiguous. Sorting
    //    by anything ahead of the label would interleave sections and shatter
    //    each one into fragments.
    // 2. Stars, highest first — the ranking the portfolio is curated by. See
    //    `STAR_RANK` for what digiKam actually stores.
    // 3. Album then filename, which is the tie-break within one star level. Most
    //    photographs share a rating, so in practice this is still what decides
    //    the order on the page, and it keeps a roll's frames together instead of
    //    interleaved.
    let sql = format!(
        "
        SELECT t.name, a.relativePath, i.name
        FROM Tags t
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        LEFT JOIN ImageInformation ii ON ii.imageid = i.id
        WHERE t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'portfolio' LIMIT 1)
          AND i.status = 1
          AND {VISIBLE_IMAGE_FILTER}
        ORDER BY t.name COLLATE NOCASE ASC,
                 {STAR_RANK} DESC,
                 a.relativePath,
                 i.name COLLATE NOCASE ASC
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
                // Filled in one pass over the whole set below, because
                // uniqueness cannot be decided one section at a time.
                slug: String::new(),
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
    // Both derived from the whole set rather than per row: uniqueness cannot be
    // decided one section at a time, and the display order is a property of the
    // list. Slugs first — see `assign_slugs` for why that order matters.
    assign_slugs(&mut out);
    apply_display_order(&mut out, crate::views::SECTION_ORDER);
    Ok(out)
}

/// A photo's star rating as a number that sorts correctly, for use in an
/// `ORDER BY` against a query that has `ImageInformation` joined as `ii`.
///
/// digiKam keeps the stars in `ImageInformation.rating`, which needs two
/// corrections before it can be sorted on:
///
/// - **`-1` means "never rated", not "worse than zero stars".** Sorting the raw
///   column would push every unrated photograph below the explicitly-zero-star
///   ones, and to a visitor those are the same thing: no stars. Worse, which of
///   the two a photo gets is an artefact of whether digiKam ever happened to
///   write a row for it — in this archive one section holds a mix of `-1` and
///   `0` with no difference in intent. Clamping to 0 collapses them.
/// - **The row can be missing entirely.** `ImageInformation.imageid` is a
///   primary key, so the `LEFT JOIN` cannot duplicate a photo, but it can yield
///   `NULL`. `COALESCE` runs first because SQLite's `MAX` returns `NULL` if any
///   argument is `NULL`.
///
/// So: unrated, zero-star and missing-row all rank 0, and 1–5 stars rank above
/// them in order.
const STAR_RANK: &str = "MAX(COALESCE(ii.rating, 0), 0)";

/// URL-safe identifier for a section, derived from its digiKam tag name.
///
/// Runs of anything outside `[A-Za-z0-9]` collapse to a single `-`, and the
/// result is lowercased: `Black & White` becomes `black-white`. Non-ASCII is
/// dropped rather than percent-encoded, which keeps every portfolio URL
/// readable in a share sheet at the cost of needing the collision handling in
/// [`assign_slugs`].
///
/// Derived rather than stored, so there is no second table to fall out of sync
/// with the tag database. The cost is that renaming a tag changes its URL;
/// that is the same trade `/people/:name` already makes.
pub fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut gap = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    out
}

/// Fill in every section's `slug`, guaranteeing the set is unique and non-empty.
///
/// [`slug`] can return the same string for two different tags (`misc` and
/// `Misc.`) or the empty string for a tag with no ASCII in it at all, and a
/// route keyed on an ambiguous slug would serve the wrong section. Duplicates
/// take a `-2`, `-3` suffix in the order the query returned them — which is
/// alphabetical by label, so a slug stays put as long as the tag set does.
///
/// Runs before the display reorder below, deliberately: if it ran after,
/// editing `SECTION_ORDER` could renumber a suffixed slug and break a link that
/// was already shared.
fn assign_slugs(sections: &mut [Section]) {
    let mut taken: HashSet<String> = HashSet::new();
    for s in sections.iter_mut() {
        let base = match slug(&s.label) {
            b if b.is_empty() => "section".to_string(),
            b => b,
        };
        let mut candidate = base.clone();
        let mut n = 2;
        while !taken.insert(candidate.clone()) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        s.slug = candidate;
    }
}

/// Reorder sections to match `order`, a list of slugs — in practice
/// [`crate::views::SECTION_ORDER`].
///
/// Listed slugs lead, in the order they appear there; anything unlisted follows
/// in the alphabetical order the query produced. A stable sort, so the tail
/// keeps that order rather than being shuffled.
///
/// The point of the list is the front door: whichever section ends up first is
/// what `/` renders, and alphabetical order is an accident of tag naming rather
/// than a decision about what a visitor should see first.
///
/// `order` is a parameter rather than read straight from the const so the tests
/// below can exercise both cases without depending on what the const currently
/// holds.
fn apply_display_order(sections: &mut [Section], order: &[&str]) {
    sections.sort_by_key(|s| {
        order
            .iter()
            .position(|want| *want == s.slug)
            .unwrap_or(usize::MAX)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(label: &str) -> Section {
        Section {
            label: label.to_string(),
            slug: String::new(),
            photos: Vec::new(),
        }
    }

    #[test]
    fn slug_lowercases_and_collapses_separators() {
        assert_eq!(slug("pastel"), "pastel");
        assert_eq!(slug("Black & White"), "black-white");
        assert_eq!(slug("CALDWELL-35"), "caldwell-35");
        assert_eq!(slug("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn slug_drops_non_ascii() {
        assert_eq!(slug("café"), "caf");
        assert_eq!(slug("★"), "");
    }

    #[test]
    fn assign_slugs_disambiguates_collisions() {
        let mut s = vec![section("misc"), section("Misc."), section("MISC")];
        assign_slugs(&mut s);
        assert_eq!(s[0].slug, "misc");
        assert_eq!(s[1].slug, "misc-2");
        assert_eq!(s[2].slug, "misc-3");
    }

    #[test]
    fn assign_slugs_names_a_sectionless_slug() {
        let mut s = vec![section("★"), section("☆")];
        assign_slugs(&mut s);
        assert_eq!(s[0].slug, "section");
        assert_eq!(s[1].slug, "section-2");
    }

    #[test]
    fn display_order_is_identity_when_unspecified() {
        let mut s = vec![section("misc"), section("pastel"), section("portraits")];
        assign_slugs(&mut s);
        apply_display_order(&mut s, &[]);
        let slugs: Vec<&str> = s.iter().map(|x| x.slug.as_str()).collect();
        assert_eq!(slugs, ["misc", "pastel", "portraits"]);
    }

    #[test]
    fn display_order_promotes_listed_slugs_and_keeps_the_tail() {
        let mut s = vec![
            section("misc"),
            section("pastel"),
            section("portraits"),
            section("street"),
        ];
        assign_slugs(&mut s);
        apply_display_order(&mut s, &["portraits", "pastel"]);
        let slugs: Vec<&str> = s.iter().map(|x| x.slug.as_str()).collect();
        assert_eq!(slugs, ["portraits", "pastel", "misc", "street"]);
    }

    #[test]
    fn display_order_ignores_a_slug_that_names_no_section() {
        let mut s = vec![section("misc"), section("pastel")];
        assign_slugs(&mut s);
        apply_display_order(&mut s, &["gone", "pastel"]);
        let slugs: Vec<&str> = s.iter().map(|x| x.slug.as_str()).collect();
        assert_eq!(slugs, ["pastel", "misc"]);
    }
}
