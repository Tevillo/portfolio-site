use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use exif::{In, Reader as ExifReader, Tag};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Extensions we treat as "raw" deliverables. Anything else that is not a
/// JPEG (and not a sidecar like .xmp or a .DS_Store/Thumbs.db) is ignored
/// when building the raw archive — the allowlist keeps junk out of the zip
/// clients download.
const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "nef", "nrw", "arw", "raf", "dng", "orf", "rw2", "pef", "srw", "x3f", "raw",
    "rwl", "iiq", "3fr",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DownloadKind {
    Jpeg,
    Raw,
}

impl DownloadKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jpeg" | "jpg" => Some(DownloadKind::Jpeg),
            "raw" => Some(DownloadKind::Raw),
            _ => None,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            DownloadKind::Jpeg => "jpeg",
            DownloadKind::Raw => "raw",
        }
    }
}

pub struct JobSummary {
    pub name: String,
    pub jpeg_count: u32,
    pub raw_count: u32,
}

pub struct JobPhoto {
    /// Filename inside the job folder, e.g. "IMG_001.jpg".
    pub name: String,
    /// Path relative to the photos root, e.g. "jobs/wedding/IMG_001.jpg".
    pub rel: String,
    pub exif: ExifInfo,
}

#[derive(Default)]
pub struct ExifInfo {
    pub datetime: Option<String>,
    pub camera: Option<String>,
}

pub struct JobDetail {
    pub jpegs: Vec<JobPhoto>,
    /// Raw filenames (no EXIF parsing — they may not be readable by exif).
    pub raws: Vec<String>,
    /// True when a `.password` file exists in the job folder. False means
    /// downloads are effectively locked (no password set yet).
    pub has_password: bool,
}

/// `photos_root/jobs/`. Returns Ok(None) if the directory doesn't exist.
pub fn jobs_root(photos_root: &Path) -> PathBuf {
    photos_root.join("jobs")
}

pub async fn list_jobs(photos_root: PathBuf) -> Result<Vec<JobSummary>> {
    tokio::task::spawn_blocking(move || list_jobs_blocking(&photos_root))
        .await
        .context("list_jobs task panicked")?
}

fn list_jobs_blocking(photos_root: &Path) -> Result<Vec<JobSummary>> {
    let root = jobs_root(photos_root);
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
        let (jpeg_count, raw_count) = count_files(&entry.path())?;
        out.push(JobSummary {
            name,
            jpeg_count,
            raw_count,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

fn count_files(dir: &Path) -> Result<(u32, u32)> {
    let mut j = 0;
    let mut r = 0;
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok((0, 0)),
    };
    for entry in read {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        if is_jpeg_name(&name) {
            j += 1;
        } else if is_raw_name(&name) {
            r += 1;
        }
    }
    Ok((j, r))
}

pub async fn read_job(photos_root: PathBuf, name: String) -> Result<Option<JobDetail>> {
    tokio::task::spawn_blocking(move || read_job_blocking(&photos_root, &name))
        .await
        .context("read_job task panicked")?
}

fn read_job_blocking(photos_root: &Path, name: &str) -> Result<Option<JobDetail>> {
    let dir = jobs_root(photos_root).join(name);
    let meta = match std::fs::metadata(&dir) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if !meta.is_dir() {
        return Ok(None);
    }

    let mut jpegs: Vec<JobPhoto> = Vec::new();
    let mut raws: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let fname = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if fname.starts_with('.') {
            continue;
        }
        if is_jpeg_name(&fname) {
            let path = entry.path();
            let exif = read_exif(&path).unwrap_or_default();
            jpegs.push(JobPhoto {
                rel: format!("jobs/{}/{}", name, fname),
                name: fname,
                exif,
            });
        } else if is_raw_name(&fname) {
            raws.push(fname);
        }
    }

    jpegs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    raws.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));

    let has_password = read_password_blocking(&dir).is_some();

    Ok(Some(JobDetail {
        jpegs,
        raws,
        has_password,
    }))
}

/// Returns the stored password (trimmed) for a job, or None if no `.password`
/// file exists. The caller is responsible for the verify; this is kept as a
/// separate step so callers can refuse to authorize when no file is set.
pub async fn read_password(photos_root: PathBuf, name: String) -> Result<Option<String>> {
    tokio::task::spawn_blocking(move || {
        let dir = jobs_root(&photos_root).join(&name);
        Ok(read_password_blocking(&dir))
    })
    .await
    .context("read_password task panicked")?
}

fn read_password_blocking(job_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(job_dir.join(".password")).ok()?;
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

fn read_exif(path: &Path) -> Result<ExifInfo> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(&file);
    let exif = ExifReader::new()
        .read_from_container(&mut reader)
        .with_context(|| format!("parsing exif {}", path.display()))?;
    let datetime = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY))
        .map(|f| f.display_value().to_string())
        .map(|s| s.trim_matches('"').to_string());
    let make = exif
        .get_field(Tag::Make, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .map(|s| s.trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty());
    let model = exif
        .get_field(Tag::Model, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .map(|s| s.trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty());
    let camera = match (make, model) {
        (Some(make), Some(model)) => {
            if model.to_lowercase().starts_with(&make.to_lowercase()) {
                Some(model)
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (Some(make), None) => Some(make),
        (None, Some(model)) => Some(model),
        (None, None) => None,
    };
    Ok(ExifInfo { datetime, camera })
}

/// Build (or reuse) the cached zip archive for one job. The zip lives at
/// `cache_root/jobs/<name>-<kind>.zip` and is rebuilt when any source file
/// is newer than the cached zip's mtime. JPEGs and RAWs are stored
/// without compression — both are already compressed, so DEFLATE costs CPU
/// without shrinking the archive meaningfully.
pub async fn build_or_get_zip(
    photos_root: PathBuf,
    cache_root: PathBuf,
    name: String,
    kind: DownloadKind,
) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || build_or_get_zip_blocking(&photos_root, &cache_root, &name, kind))
        .await
        .context("build_or_get_zip task panicked")?
}

fn build_or_get_zip_blocking(
    photos_root: &Path,
    cache_root: &Path,
    name: &str,
    kind: DownloadKind,
) -> Result<PathBuf> {
    let job_dir = jobs_root(photos_root).join(name);
    let cache_dir = cache_root.join("jobs");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating {}", cache_dir.display()))?;

    let safe_name = sanitize_filename(name);
    let zip_path = cache_dir.join(format!("{safe_name}-{}.zip", kind.slug()));

    let sources = collect_sources(&job_dir, kind)?;
    if sources.is_empty() {
        anyhow::bail!("no files of kind {:?} in job", kind.slug());
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

fn collect_sources(job_dir: &Path, kind: DownloadKind) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let read = std::fs::read_dir(job_dir)
        .with_context(|| format!("reading {}", job_dir.display()))?;
    for entry in read {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        let matches = match kind {
            DownloadKind::Jpeg => is_jpeg_name(&name),
            DownloadKind::Raw => is_raw_name(&name),
        };
        if matches {
            out.push((name, entry.path()));
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
            .start_file(name, opts)
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
        assert!(matches!(DownloadKind::parse("jpeg"), Some(DownloadKind::Jpeg)));
        assert!(matches!(DownloadKind::parse("jpg"), Some(DownloadKind::Jpeg)));
        assert!(matches!(DownloadKind::parse("raw"), Some(DownloadKind::Raw)));
        assert!(DownloadKind::parse("png").is_none());
    }
}
