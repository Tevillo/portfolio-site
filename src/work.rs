use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Extensions we treat as "raw" deliverables. Camera raws plus layered
/// edit masters (PSD/PSB) — for photographers, the .psd next to a
/// retouched .jpg is the source-of-truth file the client wants alongside
/// the export. Anything else (sidecars like .xmp, .DS_Store, etc) is
/// ignored when building the raw archive.
const RAW_EXTS: &[&str] = &[
    // Camera raws
    "cr2", "cr3", "nef", "nrw", "arw", "raf", "dng", "orf", "rw2", "pef", "srw", "x3f", "raw",
    "rwl", "iiq", "3fr",
    // Layered edit masters
    "psd", "psb",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DownloadKind {
    Jpeg,
    Raw,
    Both,
}

impl DownloadKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jpeg" | "jpg" => Some(DownloadKind::Jpeg),
            "raw" => Some(DownloadKind::Raw),
            // "both" builds a single merged archive of every JPEG and RAW in
            // the scope. The Both button submits this like any other kind so
            // the client gets one download — a hidden-iframe trick firing two
            // parallel downloads is unreliable on mobile browsers, which block
            // multi-file downloads.
            "both" => Some(DownloadKind::Both),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            DownloadKind::Jpeg => "jpeg",
            DownloadKind::Raw => "raw",
            DownloadKind::Both => "both",
        }
    }

    fn matches(self, name: &str) -> bool {
        match self {
            DownloadKind::Jpeg => is_jpeg_name(name),
            DownloadKind::Raw => is_raw_name(name),
            DownloadKind::Both => is_jpeg_name(name) || is_raw_name(name),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Original,
    Edited,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "all" => Some(Scope::All),
            "original" => Some(Scope::Original),
            "edited" => Some(Scope::Edited),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Original => "original",
            Scope::Edited => "edited",
        }
    }

    /// True when a file at `subpath` belongs to this scope. Scope membership
    /// is segmentwise: a file in `digital/edited/foo.jpg` matches `Edited`
    /// (and `All`), but not `Original`.
    fn includes(self, subpath: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Original => subpath
                .split('/')
                .any(|seg| seg.eq_ignore_ascii_case("original")),
            Scope::Edited => subpath
                .split('/')
                .any(|seg| seg.eq_ignore_ascii_case("edited")),
        }
    }
}

/// Number of JPEG and RAW files visible under each scope; used so the
/// download UI can show counts and disable empty buttons.
#[derive(Default, Clone, Copy)]
pub struct WorkCounts {
    pub all_jpeg: u32,
    pub all_raw: u32,
    pub original_jpeg: u32,
    pub original_raw: u32,
    pub edited_jpeg: u32,
    pub edited_raw: u32,
}

impl WorkCounts {
    pub fn count(&self, scope: Scope, kind: DownloadKind) -> u32 {
        let (j, r) = match scope {
            Scope::All => (self.all_jpeg, self.all_raw),
            Scope::Original => (self.original_jpeg, self.original_raw),
            Scope::Edited => (self.edited_jpeg, self.edited_raw),
        };
        match kind {
            DownloadKind::Jpeg => j,
            DownloadKind::Raw => r,
            DownloadKind::Both => j + r,
        }
    }
}

pub struct WorkSummary {
    pub name: String,
    /// Contents of the job's `title.txt`, when it has one. See [`TITLE_FILE`].
    pub title: Option<String>,
    pub jpeg_count: u32,
    pub raw_count: u32,
}

pub struct WorkPhoto {
    /// Basename shown to the user, e.g. "IMG_001.jpg".
    pub name: String,
    /// Path inside the job folder using forward slashes, e.g.
    /// "digital/edited/IMG_001.jpg". Used as the zip entry name and as the
    /// per-file download URL suffix.
    pub subpath: String,
    /// Path relative to the photos root, e.g. "work/wedding/digital/IMG_001.jpg".
    pub rel: String,
    /// Pixel dimensions of the rendered preview thumbnail (after EXIF
    /// orientation + downscale to the Preview max-dim). Optional because
    /// dimension probing can fail on corrupt files; when missing the view
    /// omits `<img width height>` and the browser falls back to its zero-
    /// size guess (which causes layout shift on load — the whole point of
    /// emitting these is to avoid that).
    pub preview_dims: Option<(u32, u32)>,
}

/// One visual grouping inside a job. Files are grouped by their immediate
/// parent subfolder path (e.g. `digital`, `digital/edited`,
/// `film/medium-format/positive`); files at the job root land in an empty-
/// label section. `is_edited` is set when the path contains an "edited"
/// segment, which the page treats as the prioritized client deliverable.
pub struct WorkSection {
    pub label: String,
    pub photos: Vec<WorkPhoto>,
    pub is_edited: bool,
}

pub struct WorkDetail {
    /// Contents of the job's `title.txt`, when it has one. See [`TITLE_FILE`].
    pub title: Option<String>,
    pub sections: Vec<WorkSection>,
    /// Per-scope file counts; powers the download UI's button labels and
    /// disabled state. Always populated by `read_work`.
    pub counts: WorkCounts,
    /// True when a `.password` file exists in the job folder. False means
    /// downloads are effectively locked (no password set yet).
    pub has_password: bool,
}

/// `photos_root/work/`. Returns Ok(None) if the directory doesn't exist.
pub fn work_root(photos_root: &Path) -> PathBuf {
    photos_root.join("work")
}

pub async fn list_work(photos_root: PathBuf) -> Result<Vec<WorkSummary>> {
    tokio::task::spawn_blocking(move || list_work_blocking(&photos_root))
        .await
        .context("list_work task panicked")?
}

fn list_work_blocking(photos_root: &Path) -> Result<Vec<WorkSummary>> {
    let root = work_root(photos_root);
    let mut out = Vec::new();
    let read = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return Ok(out),
    };
    for entry in read {
        let entry = entry?;
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type()?;
        if !ft.is_dir() {
            continue;
        }
        let (jpeg_count, raw_count) = count_files_recursive(&entry.path());
        let title = read_title_blocking(&entry.path());
        out.push(WorkSummary {
            name,
            title,
            jpeg_count,
            raw_count,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Recursively count JPEG and RAW files anywhere under `dir`. Hidden files
/// (dotfiles) are skipped; other dir names are walked unconditionally — the
/// `hidden`/`negative` conventions used elsewhere do not apply to work.
fn count_files_recursive(dir: &Path) -> (u32, u32) {
    let mut jpeg = 0u32;
    let mut raw = 0u32;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let read = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if is_jpeg_name(&name) {
                    jpeg += 1;
                } else if is_raw_name(&name) {
                    raw += 1;
                }
            }
        }
    }
    (jpeg, raw)
}

pub async fn read_work(photos_root: PathBuf, name: String) -> Result<Option<WorkDetail>> {
    tokio::task::spawn_blocking(move || read_work_blocking(&photos_root, &name))
        .await
        .context("read_work task panicked")?
}

fn read_work_blocking(photos_root: &Path, name: &str) -> Result<Option<WorkDetail>> {
    let dir = work_root(photos_root).join(name);
    let meta = match std::fs::metadata(&dir) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if !meta.is_dir() {
        return Ok(None);
    }

    // Group by top-level subfolder. Empty key = files directly under the job
    // root. Insertion order is preserved by collecting into a Vec at the end
    // after sorting keys alphabetically.
    let mut grouped: std::collections::BTreeMap<String, Vec<WorkPhoto>> =
        std::collections::BTreeMap::new();
    let mut counts = WorkCounts::default();

    let mut stack: Vec<PathBuf> = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        let read = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let fname = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if fname.starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let path = entry.path();
            let subpath = match relative_subpath(&dir, &path) {
                Some(s) => s,
                None => continue,
            };
            let is_jpeg = is_jpeg_name(&fname);
            let is_raw = is_raw_name(&fname);
            if !is_jpeg && !is_raw {
                continue;
            }
            let in_original = Scope::Original.includes(&subpath);
            let in_edited = Scope::Edited.includes(&subpath);
            if is_jpeg {
                counts.all_jpeg += 1;
                if in_original {
                    counts.original_jpeg += 1;
                }
                if in_edited {
                    counts.edited_jpeg += 1;
                }
                let bucket = parent_path(&subpath).to_string();
                let preview_dims =
                    crate::thumbs::rendition_dimensions(&path, crate::thumbs::ThumbKind::Preview)
                        .ok();
                grouped.entry(bucket).or_default().push(WorkPhoto {
                    rel: format!("work/{}/{}", name, subpath),
                    subpath,
                    name: fname,
                    preview_dims,
                });
            } else {
                counts.all_raw += 1;
                if in_original {
                    counts.original_raw += 1;
                }
                if in_edited {
                    counts.edited_raw += 1;
                }
            }
        }
    }

    let mut sections: Vec<WorkSection> = grouped
        .into_iter()
        .map(|(label, mut photos)| {
            photos.sort_by(|a, b| a.subpath.to_lowercase().cmp(&b.subpath.to_lowercase()));
            let is_edited = label_is_edited(&label);
            WorkSection {
                label,
                photos,
                is_edited,
            }
        })
        .collect();
    // Show order: job-root section first, then group by parent subfolder so
    // siblings stay adjacent (digital/edited next to digital/original, etc).
    // Within a sibling group, the leaf folder "edited" sorts ahead of any
    // other leaf so the prioritized deliverable appears above its source.
    sections.sort_by(|a, b| section_sort_key(&a.label).cmp(&section_sort_key(&b.label)));

    let has_password = read_password_blocking(&dir).is_some();
    let title = read_title_blocking(&dir);

    Ok(Some(WorkDetail {
        title,
        sections,
        counts,
        has_password,
    }))
}

/// Forward-slash relative path from `base` to `path`, or None if the path is
/// not actually under `base` or contains non-utf8 components.
fn relative_subpath(base: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let mut parts: Vec<&str> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?),
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Forward-slash parent of `subpath`, or "" if the file sits at the job root.
/// "digital/file.jpg" -> "digital";
/// "film/medium-format/positive/x.jpg" -> "film/medium-format/positive";
/// "flat.jpg" -> "".
fn parent_path(subpath: &str) -> &str {
    match subpath.rfind('/') {
        Some(i) => &subpath[..i],
        None => "",
    }
}

/// True when any forward-slash segment of `label` equals "edited"
/// (case-insensitive). Empty labels (job root) are not "edited" but get
/// promoted ahead of edited sections by tier in `read_work_blocking`.
fn label_is_edited(label: &str) -> bool {
    label
        .split('/')
        .any(|seg| seg.eq_ignore_ascii_case("edited"))
}

/// Sort key that keeps each `<parent>/edited` adjacent to its `<parent>/original`
/// (and other siblings), with `edited` always landing first inside the group.
/// Tuple: (tier, parent-path, leaf-priority, leaf-name) — all case-folded.
fn section_sort_key(label: &str) -> (u8, String, u8, String) {
    if label.is_empty() {
        return (0, String::new(), 0, String::new());
    }
    let (parent, leaf) = match label.rsplit_once('/') {
        Some((p, l)) => (p.to_string(), l.to_string()),
        None => (String::new(), label.to_string()),
    };
    let leaf_priority: u8 = if leaf.eq_ignore_ascii_case("edited") { 0 } else { 1 };
    (1, parent.to_lowercase(), leaf_priority, leaf.to_lowercase())
}

/// Returns the stored password (trimmed) for a job, or None if no `.password`
/// file exists. The caller is responsible for the verify; this is kept as a
/// separate step so callers can refuse to authorize when no file is set.
/// The file a job's display title is read from, at the job root.
///
/// Not a dotfile, unlike `.password`: this one is meant to be seen and edited,
/// and a client never sees the folder anyway. It is skipped by everything that
/// walks a job — the section builder and the counters take JPEGs and raws by
/// extension, and the zip builder does the same — so it never appears in a
/// gallery or an archive.
pub const TITLE_FILE: &str = "title.txt";

/// Longest display title accepted from [`TITLE_FILE`], in characters.
///
/// The title is the header's centre column and the stem of the `<title>`, and
/// both have a length past which they stop being a title and start being a
/// layout problem. Generous enough that a real one is never touched — the
/// longest job name on disk is 20 characters.
const TITLE_MAX_CHARS: usize = 120;

/// The job's own title, or `None` to fall back to its folder name.
///
/// First non-empty line, trimmed. A title is one line by definition, and taking
/// only the first means a file with a trailing newline — which is every file
/// written by an editor — behaves the same as one without, and a stray second
/// line cannot inject a line break into the `<title>`.
fn read_title_blocking(work_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(work_dir.join(TITLE_FILE)).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() > TITLE_MAX_CHARS {
        Some(line.chars().take(TITLE_MAX_CHARS).collect())
    } else {
        Some(line.to_string())
    }
}

pub async fn read_password(photos_root: PathBuf, name: String) -> Result<Option<String>> {
    tokio::task::spawn_blocking(move || {
        let dir = work_root(&photos_root).join(&name);
        Ok(read_password_blocking(&dir))
    })
    .await
    .context("read_password task panicked")?
}

fn read_password_blocking(work_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(work_dir.join(".password")).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Constant-time equality so a bad guess can't be timing-distinguished from
/// a near-miss. Plaintext on disk by design — see the project README.
pub fn verify(expected: &str, submitted: &str) -> bool {
    let a = expected.as_bytes();
    let b = submitted.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn is_jpeg_name(name: &str) -> bool {
    has_ext(name, &["jpg", "jpeg"])
}

pub fn is_raw_name(name: &str) -> bool {
    has_ext(name, RAW_EXTS)
}

fn has_ext(name: &str, exts: &[&str]) -> bool {
    let ext = match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return false,
    };
    exts.iter().any(|w| ext.eq_ignore_ascii_case(w))
}

/// Build (or reuse) the cached zip archive for one job. The zip lives at
/// `cache_root/work/<name>-<scope>-<kind>.zip` and is rebuilt when any
/// source file is newer than the cached zip's mtime. JPEGs and RAWs are
/// stored without compression — both are already compressed, so DEFLATE
/// costs CPU without shrinking the archive meaningfully.
pub async fn build_or_get_zip(
    photos_root: PathBuf,
    cache_root: PathBuf,
    name: String,
    scope: Scope,
    kind: DownloadKind,
) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || {
        build_or_get_zip_blocking(&photos_root, &cache_root, &name, scope, kind)
    })
    .await
    .context("build_or_get_zip task panicked")?
}

fn build_or_get_zip_blocking(
    photos_root: &Path,
    cache_root: &Path,
    name: &str,
    scope: Scope,
    kind: DownloadKind,
) -> Result<PathBuf> {
    let work_dir = work_root(photos_root).join(name);
    let cache_dir = cache_root.join("work");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating {}", cache_dir.display()))?;

    let safe_name = sanitize_filename(name);
    let zip_path = cache_dir.join(format!(
        "{safe_name}-{}-{}.zip",
        scope.slug(),
        kind.slug()
    ));

    let sources = collect_sources(&work_dir, scope, kind)?;
    if sources.is_empty() {
        anyhow::bail!(
            "no files for scope {:?} kind {:?} in job",
            scope.slug(),
            kind.slug()
        );
    }

    let newest_source = sources
        .iter()
        .filter_map(|(_, p)| std::fs::metadata(p).ok())
        .filter_map(|m| m.modified().ok())
        .max()
        .unwrap_or_else(SystemTime::now);

    let zip_mtime = std::fs::metadata(&zip_path)
        .ok()
        .and_then(|m| m.modified().ok());
    let fresh = zip_mtime.map(|t| t >= newest_source).unwrap_or(false);
    if fresh {
        return Ok(zip_path);
    }

    write_zip(&zip_path, &sources)?;
    Ok(zip_path)
}

/// Walks the whole job tree and returns `(zip_entry_name, abs_path)` pairs,
/// where `zip_entry_name` is the forward-slash subpath from the job root —
/// so the archive preserves the digital/film/edited grouping the client
/// sees on the page. Files are filtered by both `scope` and `kind`.
fn collect_sources(
    work_dir: &Path,
    scope: Scope,
    kind: DownloadKind,
) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![work_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let read = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read {
            let entry = entry?;
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if !kind.matches(&name) {
                continue;
            }
            let path = entry.path();
            let subpath = match relative_subpath(work_dir, &path) {
                Some(s) => s,
                None => continue,
            };
            if !scope.includes(&subpath) {
                continue;
            }
            out.push((subpath, path));
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    Ok(out)
}

fn write_zip(zip_path: &Path, sources: &[(String, PathBuf)]) -> Result<()> {
    // Write to a temp file in the same directory, then rename — partial
    // writes on crash never become a "valid" cached zip.
    let parent = zip_path
        .parent()
        .context("zip path has no parent")?
        .to_path_buf();
    let file_name = zip_path
        .file_name()
        .context("zip path has no file name")?
        .to_string_lossy()
        .into_owned();
    let tmp = parent.join(format!(".{file_name}.tmp"));

    let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut writer = ZipWriter::new(file);
    let opts: SimpleFileOptions = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true);

    for (name, path) in sources {
        writer
            .start_file(name.as_str(), opts)
            .with_context(|| format!("zip start_file {name}"))?;
        let mut src = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = src
                .read(&mut buf)
                .with_context(|| format!("reading {}", path.display()))?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .with_context(|| format!("writing zip entry {name}"))?;
        }
    }

    writer.finish().context("finalizing zip")?;
    std::fs::rename(&tmp, zip_path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), zip_path.display()))?;
    Ok(())
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_detection() {
        assert!(is_jpeg_name("IMG_0001.JPG"));
        assert!(is_jpeg_name("foo.jpeg"));
        assert!(!is_jpeg_name("foo.cr3"));
        assert!(!is_jpeg_name("foo"));
    }

    #[test]
    fn raw_detection_is_allowlist() {
        assert!(is_raw_name("a.cr3"));
        assert!(is_raw_name("a.NEF"));
        assert!(is_raw_name("a.dng"));
        assert!(is_raw_name("a.psd")); // edit master ships as the "raw"
        assert!(is_raw_name("a.PSB"));
        assert!(!is_raw_name("a.xmp")); // sidecar, not a deliverable
        assert!(!is_raw_name("a.txt"));
        assert!(!is_raw_name("a.jpg"));
    }

    #[test]
    fn constant_time_verify() {
        assert!(verify("hunter2", "hunter2"));
        assert!(!verify("hunter2", "hunter3"));
        assert!(!verify("hunter2", "hunter22"));
        assert!(!verify("", "x"));
    }

    #[test]
    fn sanitize_strips_unsafe() {
        assert_eq!(sanitize_filename("wedding-2026"), "wedding-2026");
        assert_eq!(sanitize_filename("a b/c"), "a_b_c");
    }

    #[test]
    fn parses_download_kind() {
        assert!(matches!(
            DownloadKind::parse("jpeg"),
            Some(DownloadKind::Jpeg)
        ));
        assert!(matches!(
            DownloadKind::parse("jpg"),
            Some(DownloadKind::Jpeg)
        ));
        assert!(matches!(
            DownloadKind::parse("raw"),
            Some(DownloadKind::Raw)
        ));
        // "both" builds a single merged JPEG+RAW archive for the Both button.
        assert!(matches!(
            DownloadKind::parse("both"),
            Some(DownloadKind::Both)
        ));
        assert!(DownloadKind::parse("png").is_none());
    }

    #[test]
    fn scope_includes_is_segmentwise() {
        assert!(Scope::All.includes("anything/at/all.jpg"));
        assert!(Scope::Original.includes("digital/original/foo.jpg"));
        assert!(Scope::Original.includes("ORIGINAL/x.jpg"));
        assert!(!Scope::Original.includes("digital/originals/foo.jpg")); // segmentwise
        assert!(Scope::Edited.includes("digital/edited/foo.jpg"));
        assert!(!Scope::Edited.includes("digital/foo.jpg"));
    }

    #[test]
    fn kind_both_matches_either() {
        assert!(DownloadKind::Both.matches("a.jpg"));
        assert!(DownloadKind::Both.matches("a.cr3"));
        assert!(!DownloadKind::Both.matches("a.xmp"));
    }

    #[test]
    fn parent_path_works() {
        assert_eq!(parent_path("digital/file.jpg"), "digital");
        assert_eq!(
            parent_path("film/medium-format/positive/x.jpg"),
            "film/medium-format/positive"
        );
        assert_eq!(parent_path("flat.jpg"), "");
    }

    #[test]
    fn edited_detection_is_segmentwise() {
        assert!(label_is_edited("edited"));
        assert!(label_is_edited("digital/edited"));
        assert!(label_is_edited("film/edited/2025"));
        assert!(label_is_edited("EDITED")); // case-insensitive
        assert!(!label_is_edited("edits")); // not a whole segment
        assert!(!label_is_edited("original"));
        assert!(!label_is_edited(""));
    }

    #[test]
    fn sort_pairs_siblings_with_edited_first() {
        let mut labels = vec![
            "digital/original",
            "small-format/positive/original",
            "digital/edited",
            "medium-format/positive/edited",
            "small-format/positive/edited",
            "medium-format/positive/original",
            "large-format/positive/edited",
            "",
        ];
        labels.sort_by(|a, b| section_sort_key(a).cmp(&section_sort_key(b)));
        assert_eq!(
            labels,
            vec![
                "",
                "digital/edited",
                "digital/original",
                "large-format/positive/edited",
                "medium-format/positive/edited",
                "medium-format/positive/original",
                "small-format/positive/edited",
                "small-format/positive/original",
            ]
        );
    }
}
