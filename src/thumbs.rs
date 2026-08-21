use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use exif::{In, Reader as ExifReader, Tag, Value};
use image::{DynamicImage, ImageReader};

const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];
const APP2_MARKER: u8 = 0xE2;
const ICC_IDENTIFIER: &[u8] = b"ICC_PROFILE\0";

/// Which rendition to produce. Each kind owns its own cache subdirectory
/// (`cache/<subdir>/<same layout as photos>/file.jpg`) and target dimension.
#[derive(Clone, Copy, Debug)]
pub enum ThumbKind {
    /// 400px grid thumbnail.
    Grid,
    /// 800px, the second `srcset` candidate for the natural-ratio grids.
    ///
    /// Sized from the measured tile: /recent lays out at 252-327 CSS px across
    /// every viewport, so 400px covers a 1x screen and nothing more — on a 2x
    /// phone a portrait scan's 272px width against a 327px slot is a 0.42x
    /// upscale. 800px puts the same portrait at 544px, which is 0.96x for a 2x
    /// laptop and 0.83x for a 2x phone, at roughly a quarter of Preview's bytes.
    Medium,
    /// 1600px medium-size preview, used by the work feed so the page can
    /// show "big" photos without serving the multi-megabyte originals.
    Preview,
}

impl ThumbKind {
    pub(crate) fn subdir(self) -> &'static str {
        match self {
            ThumbKind::Grid => "thumbs",
            ThumbKind::Medium => "medium",
            ThumbKind::Preview => "preview",
        }
    }

    /// URL prefix this rendition is served under. Deliberately not `subdir`:
    /// the Grid rendition caches to `thumbs/` but serves from `/thumb/`, and
    /// having the two derived from one `match` is what stops a page linking a
    /// route that does not exist.
    pub(crate) fn route(self) -> &'static str {
        match self {
            ThumbKind::Grid => "thumb",
            ThumbKind::Medium => "medium",
            ThumbKind::Preview => "preview",
        }
    }

    /// Every rendition, so the warm and prune passes cannot silently skip one
    /// that was added later. Both used to carry their own array literal.
    pub(crate) const ALL: [ThumbKind; 3] = [ThumbKind::Grid, ThumbKind::Medium, ThumbKind::Preview];

    pub(crate) fn max_dim(self) -> u32 {
        match self {
            ThumbKind::Grid => 400,
            ThumbKind::Medium => 800,
            ThumbKind::Preview => 1600,
        }
    }
}

pub struct ThumbInfo {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
    /// True if this call actually (re)rendered the file, false if a fresh
    /// cached rendition already existed. Let cache-warming report progress;
    /// request handlers ignore it.
    pub rebuilt: bool,
}

/// Cheap pre-flight: read just the JPEG header + EXIF orientation from `src`
/// and return the (w, h) the `kind` rendition will end up with after
/// orientation rotation and downscaling to fit within `kind.max_dim()`.
/// No image decode happens; only ~headers are read off disk, so this is
/// safe to call per-photo during page render to populate `<img width
/// height>` attributes that prevent layout shift.
///
/// `kind` has to be the rendition the page actually links, not whichever one
/// is largest: the attributes are a claim about the bytes the browser is
/// about to fetch. The natural-ratio masonry only reads the *ratio* out of
/// them, so a mismatched scale would lay out correctly and still be a lie.
pub fn rendition_dimensions(src: &Path, kind: ThumbKind) -> Result<(u32, u32)> {
    Ok(scale_to(oriented_dimensions(src)?, kind.max_dim()))
}

/// The source's pixel dimensions after EXIF-orientation rotation, before any
/// downscaling. Split out from [`rendition_dimensions`] so a page needing the
/// dimensions of *several* renditions of the same photo — a `srcset` needs one
/// `w` descriptor per candidate — pays for one header read rather than one per
/// candidate. [`scale_to`] turns this into a given rendition's size and touches
/// no disk at all.
pub fn oriented_dimensions(src: &Path) -> Result<(u32, u32)> {
    let (raw_w, raw_h) = image::image_dimensions(src)
        .with_context(|| format!("reading dimensions of {}", src.display()))?;
    let orientation = read_orientation(src).unwrap_or(1);
    Ok(if (5..=8).contains(&orientation) {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    })
}

/// Fit `(w, h)` inside a `max` x `max` box, preserving the aspect ratio and
/// never enlarging. Pure arithmetic, and the same rule `ensure_thumb` renders
/// by, which is what lets the `<img>` attributes describe the real file.
pub fn scale_to((w, h): (u32, u32), max: u32) -> (u32, u32) {
    if w <= max && h <= max {
        return (w, h);
    }
    if w >= h {
        let ratio = max as f64 / w as f64;
        (max, ((h as f64) * ratio).round().max(1.0) as u32)
    } else {
        let ratio = max as f64 / h as f64;
        (((w as f64) * ratio).round().max(1.0) as u32, max)
    }
}

fn read_orientation(path: &Path) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = ExifReader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(Tag::Orientation, In::PRIMARY)?;
    match &field.value {
        Value::Short(v) => v.first().map(|n| *n as u32),
        _ => None,
    }
}

/// Ensure a fresh rendition exists for `source` under `cache_root/<kind>/`,
/// mirroring the path layout of `photos_root`. Returns the cached path plus
/// metadata for ETag/Last-Modified headers.
pub async fn ensure_thumb(
    source: &Path,
    photos_root: &Path,
    cache_root: &Path,
    kind: ThumbKind,
) -> Result<ThumbInfo> {
    let rel = source
        .strip_prefix(photos_root)
        .context("source not under photos_root")?
        .to_path_buf();
    let cache_path = cache_root.join(kind.subdir()).join(&rel);

    let source_meta = tokio::fs::metadata(source).await?;
    let source_mtime = source_meta.modified()?;

    let needs_rebuild = match tokio::fs::metadata(&cache_path).await {
        Ok(m) => match m.modified() {
            Ok(thumb_mtime) => thumb_mtime < source_mtime,
            Err(_) => true,
        },
        Err(_) => true,
    };

    if needs_rebuild {
        if let Some(parent) = cache_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let src = source.to_path_buf();
        let dst = cache_path.clone();
        let max_dim = kind.max_dim();
        tokio::task::spawn_blocking(move || render_thumb(&src, &dst, max_dim))
            .await
            .context("thumbnail task panicked")??;
    }

    let final_meta = tokio::fs::metadata(&cache_path).await?;
    Ok(ThumbInfo {
        path: cache_path,
        mtime: final_meta.modified()?,
        size: final_meta.len(),
        rebuilt: needs_rebuild,
    })
}

fn render_thumb(src: &Path, dst: &Path, max_dim: u32) -> Result<()> {
    let src_bytes =
        std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;

    let img = ImageReader::new(Cursor::new(&src_bytes))
        .with_guessed_format()
        .with_context(|| format!("guessing format for {}", src.display()))?
        .decode()
        .with_context(|| format!("decoding {}", src.display()))?;
    let orientation = read_exif_orientation(&src_bytes);
    let thumb = apply_orientation(img.thumbnail(max_dim, max_dim), orientation);

    let mut thumb_bytes = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut thumb_bytes), image::ImageFormat::Jpeg)
        .with_context(|| format!("encoding thumb for {}", src.display()))?;

    let final_bytes = splice_icc_profile(&thumb_bytes, &src_bytes)
        .with_context(|| format!("splicing ICC profile for {}", src.display()))?;

    let parent = dst.parent().context("thumb dst has no parent")?;
    let file_name = dst.file_name().context("thumb dst has no file name")?;
    let mut tmp = parent.to_path_buf();
    tmp.push(format!(".{}.tmp", file_name.to_string_lossy()));

    std::fs::write(&tmp, &final_bytes)
        .with_context(|| format!("writing tmp thumb {}", tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), dst.display()))?;
    Ok(())
}

/// Copy APP2 `ICC_PROFILE` segments from `source` JPEG into `thumb` JPEG so
/// browsers color-manage the thumbnail the same way as the original. ICC
/// profiles larger than ~64 KB span multiple APP2 segments; all are copied
/// in order so the decoder reassembles them via the sequence numbers in
/// each segment's identifier block. Returns `thumb` unchanged if either
/// input is not a JPEG or the source has no ICC profile.
fn splice_icc_profile(thumb: &[u8], source: &[u8]) -> Result<Vec<u8>> {
    if !thumb.starts_with(&JPEG_SOI) || !source.starts_with(&JPEG_SOI) {
        return Ok(thumb.to_vec());
    }

    let segments = extract_icc_segments(source)?;
    if segments.is_empty() {
        return Ok(thumb.to_vec());
    }

    let extra: usize = segments.iter().map(|s| s.len()).sum();
    let mut out = Vec::with_capacity(thumb.len() + extra);
    out.extend_from_slice(&JPEG_SOI);
    for seg in &segments {
        out.extend_from_slice(seg);
    }
    out.extend_from_slice(&thumb[2..]);
    Ok(out)
}

/// Walk the marker segments of a JPEG (between SOI and SOS), returning each
/// APP2 segment whose payload begins with the `ICC_PROFILE\0` identifier.
/// Each returned vector is a complete segment: `FF E2 len_hi len_lo payload`.
fn extract_icc_segments(buf: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut idx = 2usize;
    while idx < buf.len() {
        while idx < buf.len() && buf[idx] == 0xFF {
            idx += 1;
        }
        if idx >= buf.len() {
            anyhow::bail!("truncated JPEG marker");
        }
        let marker = buf[idx];
        idx += 1;
        match marker {
            // SOS / EOI — ICC must appear before compressed data.
            0xDA | 0xD9 => break,
            // Standalone markers with no payload.
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        if idx + 2 > buf.len() {
            anyhow::bail!("truncated JPEG segment length");
        }
        let seg_len = u16::from_be_bytes([buf[idx], buf[idx + 1]]) as usize;
        if seg_len < 2 {
            anyhow::bail!("invalid JPEG segment length");
        }
        let payload_end = idx + seg_len;
        if payload_end > buf.len() {
            anyhow::bail!("truncated JPEG segment");
        }
        if marker == APP2_MARKER {
            let payload = &buf[idx + 2..payload_end];
            if payload.starts_with(ICC_IDENTIFIER) {
                let mut seg = Vec::with_capacity(2 + seg_len);
                seg.push(0xFF);
                seg.push(marker);
                seg.extend_from_slice(&buf[idx..payload_end]);
                out.push(seg);
            }
        }
        idx = payload_end;
    }
    Ok(out)
}

/// Read the EXIF Orientation tag (1..=8) from a JPEG byte stream.
/// Returns 1 (normal) if the file has no EXIF or the tag is missing/invalid.
fn read_exif_orientation(buf: &[u8]) -> u32 {
    let mut cursor = Cursor::new(buf);
    let exif = match ExifReader::new().read_from_container(&mut cursor) {
        Ok(e) => e,
        Err(_) => return 1,
    };
    let field = match exif.get_field(Tag::Orientation, In::PRIMARY) {
        Some(f) => f,
        None => return 1,
    };
    match &field.value {
        Value::Short(v) => v.first().copied().unwrap_or(1) as u32,
        _ => 1,
    }
}

fn apply_orientation(img: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}
