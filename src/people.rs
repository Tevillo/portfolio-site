use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

pub struct Person {
    pub name: String,
    pub photo_count: u32,
    /// The face to put on this person's tile: their digiKam tag thumbnail if
    /// they have one, otherwise their biggest confirmed face. `None` for
    /// someone with neither — see [`list_tag_thumbnail_faces_blocking`] and
    /// [`biggest_face`].
    pub face: Option<Face>,
}

/// One confirmed face: which photograph it is in, and where in it.
#[derive(Clone, Debug)]
pub struct Face {
    /// Path relative to the photos root, in the form "2025/foo/bar/baz.jpg".
    pub rel: String,
    pub rect: FaceRect,
}

/// A face rectangle in the photograph's **oriented** pixel space — that is,
/// after the EXIF orientation has been applied, which is the space the site
/// renders in and the space digiKam draws its boxes in.
///
/// Verified rather than assumed, because the two spaces are easy to confuse and
/// both fit: `ImageInformation.width/height` hold the *raw* file dimensions, so
/// a rectangle inside a portrait scan's raw box is usually also inside its
/// rotated one. Cropping `_DSC0716.jpg` (4180x6378, EXIF orientation 8) at
/// `2751,1301 681x888` gives a door in raw space and a face in oriented space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaceRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl FaceRect {
    /// Pixel count, which is what the fallback pick ranks by — see
    /// [`biggest_face`].
    pub fn area(self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    /// Parse digiKam's `tagRegion` value: `<rect x="2751" y="1301"
    /// width="681" height="888"/>`.
    ///
    /// Hand-scanned rather than parsed as XML, which would be a dependency for
    /// one element with four integer attributes. Anything that does not match
    /// that shape — a negative coordinate, a zero dimension, a value that is
    /// not a number — is `None` and the face is simply skipped, because a
    /// rectangle we cannot read is not a rectangle we can crop to.
    pub fn parse(value: &str) -> Option<Self> {
        fn attr(value: &str, name: &str) -> Option<u32> {
            let at = value.find(&format!("{name}=\""))? + name.len() + 2;
            let rest = &value[at..];
            let end = rest.find('"')?;
            rest[..end].parse().ok()
        }
        let rect = FaceRect {
            x: attr(value, "x")?,
            y: attr(value, "y")?,
            w: attr(value, "width")?,
            h: attr(value, "height")?,
        };
        (rect.w > 0 && rect.h > 0).then_some(rect)
    }
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
//
// Split in two because one query needs half of it. A person's *tag thumbnail*
// can be a TIFF or a DNG — digiKam does not care what a face is drawn on — and
// that row has to be read before it can be resolved to the JPEG beside it, so
// the extension half of the rule cannot be applied in SQL there. Which half is
// which is worth keeping honest: where a photograph lives is a statement about
// whether it is published at all, while its extension is only a statement about
// whether this program can decode it.
//
// `macro_rules!` rather than two consts, because `concat!` takes literals and
// not const names, and the alternative is writing the negative-folder clauses
// out twice.
macro_rules! jpeg_name_filter {
    () => {
        "(i.name LIKE '%.jpg' OR i.name LIKE '%.jpeg')"
    };
}
macro_rules! published_place_filter {
    () => {
        "
        i.name NOT LIKE '%hidden%'
        AND a.relativePath NOT LIKE '%/negative/%'
        AND a.relativePath NOT LIKE '%/negative'
        AND a.relativePath NOT LIKE '/negative/%'
        AND a.relativePath NOT LIKE '/negative'
        "
    };
}
pub(crate) const PUBLISHED_PLACE_FILTER: &str = published_place_filter!();

/// What counts as a person: a tag parented directly under digiKam's "People"
/// tag, carrying a `person` property, and not one of the three built-in stubs
/// (unknown / ignored / unconfirmed).
///
/// One definition, used by every query in this module that means "a person",
/// because the failure mode of having several is silent: a person listed by one
/// query and filtered out by another loses their face rather than their row, and
/// the page still renders.
///
/// `EXISTS` rather than a join on `TagProperties`, so a tag carrying the
/// property twice cannot multiply the rows a `COUNT` is taken over.
const PERSON_TAG_FILTER: &str = "
    t.pid = (SELECT id FROM Tags WHERE pid = 0 AND name = 'People' LIMIT 1)
    AND EXISTS (
        SELECT 1 FROM TagProperties tp
        WHERE tp.tagid = t.id AND tp.property = 'person'
    )
    AND t.id NOT IN (
        SELECT tagid FROM TagProperties
        WHERE property IN ('unknownPerson', 'ignoredPerson', 'unconfirmedPerson')
    )
";
pub(crate) const VISIBLE_IMAGE_FILTER: &str =
    concat!(jpeg_name_filter!(), " AND ", published_place_filter!());

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
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        WHERE {PERSON_TAG_FILTER}
          AND {VISIBLE_IMAGE_FILTER}
        GROUP BY t.id
        HAVING cnt > 0
        ORDER BY cnt DESC, t.name COLLATE NOCASE ASC
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
    })?;
    let mut counted = Vec::new();
    for r in rows {
        counted.push(r?);
    }

    let mut chosen = list_tag_thumbnail_faces_blocking(&conn)?;
    let mut fallbacks = list_faces_blocking(&conn)?;
    Ok(counted
        .into_iter()
        .map(|(name, photo_count)| {
            // The tag thumbnail is the answer whenever there is one; the
            // biggest face is what a person who has never had one set gets.
            let face = chosen
                .remove(&name)
                .or_else(|| fallbacks.remove(&name).and_then(biggest_face));
            Person {
                name,
                photo_count,
                face,
            }
        })
        .collect())
}

/// The fallback pick: the face with the most pixels in it.
///
/// A quality rule rather than an arbitrary one — the crop is enlarged from that
/// rectangle, so the biggest face is the one that survives being shrunk to a
/// tile. Ties break on the path, so a person's tile does not change photograph
/// between two requests that read the same database.
fn biggest_face(mut faces: Vec<Face>) -> Option<Face> {
    faces.sort_by(|a, b| {
        b.rect
            .area()
            .cmp(&a.rect.area())
            .then(a.rel.cmp(&b.rel))
    });
    faces.into_iter().next()
}

/// The face each person has been given as their **tag thumbnail** in digiKam —
/// right-click a face, "Set as Tag Thumbnail" — read out of `Tags.icon`.
///
/// This is the deliberate pick, and it is digiKam's own: one per person, chosen
/// in the same window where the faces are confirmed, with nothing to learn and
/// no second tag to maintain. `Tags.icon` holds an *image id*, and the
/// rectangle comes from that image's `tagRegion` for that same person, so a
/// frame holding two people gives each of them their own crop of it.
///
/// **Taken literally, whatever file it names.** A tag thumbnail set on a TIFF
/// or a RAW is not quietly redirected to the JPEG beside it — the pick stands,
/// and rendering it fails with a message naming the file (see
/// [`crate::thumbs::ensure_face`]). Substituting the sibling would be a guess
/// about which two files are the same photograph, and it would hide the thing
/// worth knowing: the tag thumbnail is set on a file this site cannot publish,
/// which is a five-second fix in digiKam and invisible if the site papers over
/// it.
///
/// So the only rows filtered out here are the ones that are not published at
/// all — a lost file, a `hidden` name, a `negative/` folder — because those are
/// statements about the archive rather than about a person's setup.
fn list_tag_thumbnail_faces_blocking(conn: &Connection) -> Result<HashMap<String, Face>> {
    let sql = format!(
        "
        SELECT t.name, a.relativePath, i.name, itp.value
        FROM Tags t
        JOIN Images i ON i.id = t.icon
        JOIN Albums a ON a.id = i.album
        JOIN ImageTagProperties itp
             ON itp.imageid = i.id AND itp.tagid = t.id AND itp.property = 'tagRegion'
        WHERE {PERSON_TAG_FILTER}
          AND i.status = 1
          AND {PUBLISHED_PLACE_FILTER}
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut out: HashMap<String, Face> = HashMap::new();
    for row in rows {
        let (person, album_rel, file, region) = row?;
        let Some(rect) = FaceRect::parse(&region) else {
            continue;
        };
        out.insert(
            person,
            Face {
                rel: combine_rel(&album_rel, &file),
                rect,
            },
        );
    }
    Ok(out)
}


/// Every confirmed face on a visible photograph, by person name — the pool
/// [`biggest_face`] picks from when a person has no tag thumbnail.
///
/// digiKam writes one `tagRegion` row per confirmed face — the rectangle it
/// drew and you named — so this is the archive's own record of where a person
/// is in a frame, and the site does not need a face detector of its own.
///
/// The same visibility rules as everything else, for the same reasons: a
/// negative scan or a `hidden` frame is not published, so it cannot be someone's
/// portrait either. `i.status = 1` drops the rows digiKam keeps for files it has
/// lost track of, which would otherwise put a tile on a path that is gone.
fn list_faces_blocking(conn: &Connection) -> Result<HashMap<String, Vec<Face>>> {
    let sql = format!(
        "
        SELECT t.name, a.relativePath, i.name, itp.value
        FROM ImageTagProperties itp
        JOIN Tags t ON t.id = itp.tagid
        JOIN Images i ON i.id = itp.imageid
        JOIN Albums a ON a.id = i.album
        WHERE itp.property = 'tagRegion'
          AND {PERSON_TAG_FILTER}
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
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut out: HashMap<String, Vec<Face>> = HashMap::new();
    for row in rows {
        let (person, album_rel, file, region) = row?;
        let Some(rect) = FaceRect::parse(&region) else {
            continue;
        };
        out.entry(person).or_default().push(Face {
            rel: combine_rel(&album_rel, &file),
            rect,
        });
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
        JOIN ImageTags it ON it.tagid = t.id
        JOIN Images i ON i.id = it.imageid
        JOIN Albums a ON a.id = i.album
        WHERE {PERSON_TAG_FILTER}
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
