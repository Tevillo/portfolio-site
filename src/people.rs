use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

use crate::handlers::{is_hidden, is_skipped_dir};

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

fn open_readonly(db_path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening digikam db at {}", db_path.display()))
}

fn list_people_blocking(db_path: &Path) -> Result<Vec<Person>> {
    let conn = open_readonly(db_path)?;
    // Named person tags: parented under the "People" tag (id=4), marked with
    // a 'person' TagProperty, excluding the built-in stubs (unknown/ignored/
    // unconfirmed person).
    let mut stmt = conn.prepare(
        "
        SELECT t.name, COUNT(it.imageid) AS cnt
        FROM Tags t
        JOIN TagProperties tp ON tp.tagid = t.id AND tp.property = 'person'
        LEFT JOIN ImageTags it ON it.tagid = t.id
        WHERE t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'People' LIMIT 1)
          AND t.id NOT IN (
              SELECT tagid FROM TagProperties
              WHERE property IN ('unknownPerson', 'ignoredPerson', 'unconfirmedPerson')
          )
        GROUP BY t.id
        HAVING cnt > 0
        ORDER BY cnt DESC, t.name COLLATE NOCASE ASC
        ",
    )?;
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

fn list_person_photos_blocking(db_path: &Path, person_name: &str) -> Result<Vec<PersonPhoto>> {
    let conn = open_readonly(db_path)?;
    let mut stmt = conn.prepare(
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
        ORDER BY a.relativePath, i.name COLLATE NOCASE ASC
        ",
    )?;
    let rows = stmt.query_map(params![person_name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in rows {
        let (album_rel, name) = r?;
        if !is_jpeg(&name) || is_hidden(&name) {
            continue;
        }
        if album_has_skipped_segment(&album_rel) {
            continue;
        }
        let rel = combine_rel(&album_rel, &name);
        if seen.insert(rel.clone()) {
            out.push(PersonPhoto { rel, name });
        }
    }
    Ok(out)
}

fn combine_rel(album_rel: &str, name: &str) -> String {
    let trimmed = album_rel.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        name.to_string()
    } else {
        format!("{trimmed}/{name}")
    }
}

fn album_has_skipped_segment(album_rel: &str) -> bool {
    album_rel
        .split('/')
        .any(|seg| !seg.is_empty() && is_skipped_dir(seg))
}

fn is_jpeg(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jpg") || lower.ends_with(".jpeg")
}
