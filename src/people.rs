use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

pub struct Person {
    pub name: String,
    pub photo_count: u32,
}

pub struct PersonPhoto {
    /// Path relative to the photos root, in the form "2025/foo/bar/baz.jpg".
    pub rel: String,
    pub name: String,
}

pub async fn list_people(db_path: PathBuf) -> Result<Vec<Person>> {
    tokio::task::spawn_blocking(move || list_people_blocking(&db_path))
        .await
        .context("people listing task panicked")?
}

pub async fn list_person_photos(db_path: PathBuf, person_name: String) -> Result<Vec<PersonPhoto>> {
    tokio::task::spawn_blocking(move || list_person_photos_blocking(&db_path, &person_name))
        .await
        .context("person photo listing task panicked")?
}

/// Every (person tag, photo path) pair in the archive, for callers that need to
/// go from a *folder* to the people in it rather than the other way round.
///
/// `notify` uses this to answer "who is in this drop": one query and a prefix
/// match beats calling [`list_person_photos`] once per person, which would
/// re-scan the whole tag table for each of them.
pub async fn list_all_tagged_photos(db_path: PathBuf) -> Result<Vec<(String, String)>> {
    tokio::task::spawn_blocking(move || list_all_tagged_photos_blocking(&db_path))
        .await
        .context("tagged photo listing task panicked")?
}

// The three items below are the generic half of this module — opening the
// digiKam database, deciding which rows are publishable, and turning an
// (album, filename) pair back into a path. `portfolio` queries the same
// database for a different tag tree and shares them so the two pages cannot
// drift apart on what counts as a visible photo.
pub(crate) fn open_readonly(db_path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening digikam db at {}", db_path.display()))
}

// Same rules as the filesystem walker: only .jpg/.jpeg, no "hidden" filenames,
// and skip any album path with a "negative" segment. SQLite LIKE is ASCII
// case-insensitive by default, which mirrors the Rust helpers in handlers.rs.
pub(crate) const VISIBLE_IMAGE_FILTER: &str = "
    (i.name LIKE '%.jpg' OR i.name LIKE '%.jpeg')
    AND i.name NOT LIKE '%hidden%'
    AND a.relativePath NOT LIKE '%/negative/%'
    AND a.relativePath NOT LIKE '%/negative'
    AND a.relativePath NOT LIKE '/negative/%'
    AND a.relativePath NOT LIKE '/negative'
";

fn list_people_blocking(db_path: &Path) -> Result<Vec<Person>> {
    let conn = open_readonly(db_path)?;
    // Named person tags: parented under the "People" tag (id=4), marked with
    // a 'person' TagProperty, excluding the built-in stubs (unknown/ignored/
    // unconfirmed person). The count reflects only photos that would actually
    // be displayed (filtered by VISIBLE_IMAGE_FILTER).
    let sql = format!(
        "
        SELECT t.name, COUNT(i.id) AS cnt
        FROM Tags t
        JOIN TagProperties tp ON tp.tagid = t.id AND tp.property = 'person'
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        WHERE t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'People' LIMIT 1)
          AND t.id NOT IN (
              SELECT tagid FROM TagProperties
              WHERE property IN ('unknownPerson', 'ignoredPerson', 'unconfirmedPerson')
          )
          AND {VISIBLE_IMAGE_FILTER}
        GROUP BY t.id
        HAVING cnt > 0
        ORDER BY cnt DESC, t.name COLLATE NOCASE ASC
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(Person {
            name: row.get::<_, String>(0)?,
            photo_count: row.get::<_, i64>(1)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Every visible photograph tagged with `person_name`, deduplicated by path.
///
/// The `ORDER BY` is here only to make the result deterministic and to give the
/// deduplication a stable notion of which row came first. It is *not* the order
/// the page renders in: ascending album path would open a person's page on their
/// oldest photographs, and `relativePath` carries no date below its year segment
/// anyway, so the display order is settled in Rust by
/// `handlers::sort_person_photos` — which can reuse the same year rule `/all`
/// uses instead of reinventing one in SQL.
fn list_person_photos_blocking(db_path: &Path, person_name: &str) -> Result<Vec<PersonPhoto>> {
    let conn = open_readonly(db_path)?;
    let sql = format!(
        "
        SELECT a.relativePath, i.name
        FROM Tags t
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        WHERE t.name = ?1
          AND t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'People' LIMIT 1)
          AND EXISTS (
              SELECT 1 FROM TagProperties tp
              WHERE tp.tagid = t.id AND tp.property = 'person'
          )
          AND {VISIBLE_IMAGE_FILTER}
        ORDER BY a.relativePath, i.name COLLATE NOCASE ASC
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![person_name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in rows {
        let (album_rel, name) = r?;
        let rel = combine_rel(&album_rel, &name);
        if seen.insert(rel.clone()) {
            out.push(PersonPhoto { rel, name });
        }
    }
    Ok(out)
}

fn list_all_tagged_photos_blocking(db_path: &Path) -> Result<Vec<(String, String)>> {
    let conn = open_readonly(db_path)?;
    // Same person-tag definition as `list_people` — parented under "People",
    // carrying a 'person' property, and not one of digiKam's built-in stubs —
    // so the two can never disagree about who counts as a person.
    let sql = format!(
        "
        SELECT t.name, a.relativePath, i.name
        FROM Tags t
        JOIN TagProperties tp ON tp.tagid = t.id AND tp.property = 'person'
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        WHERE t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'People' LIMIT 1)
          AND t.id NOT IN (
              SELECT tagid FROM TagProperties
              WHERE property IN ('unknownPerson', 'ignoredPerson', 'unconfirmedPerson')
          )
          AND i.status = 1
          AND {VISIBLE_IMAGE_FILTER}
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
    let mut out = Vec::new();
    for r in rows {
        let (person, album_rel, name) = r?;
        out.push((person, combine_rel(&album_rel, &name)));
    }
    Ok(out)
}

pub(crate) fn combine_rel(album_rel: &str, name: &str) -> String {
    let trimmed = album_rel.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        name.to_string()
    } else {
        format!("{trimmed}/{name}")
    }
}
