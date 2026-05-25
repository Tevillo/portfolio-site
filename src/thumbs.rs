use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use exif::{In, Reader as ExifReader, Tag, Value};
use image::{DynamicImage, ImageReader};

const THUMB_MAX_DIM: u32 = 400;
const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];
const APP2_MARKER: u8 = 0xE2;
const ICC_IDENTIFIER: &[u8] = b"ICC_PROFILE\0";

pub struct ThumbInfo {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
}

/// Ensure a fresh thumbnail exists for `source` under `cache_root`,
/// mirroring the path layout of `photos_root`. Returns the cached path
/// plus metadata for ETag/Last-Modified headers.
pub async fn ensure_thumb(
    source: &Path,
    photos_root: &Path,
    cache_root: &Path,
) -> Result<ThumbInfo> {
    let rel = source
        .strip_prefix(photos_root)
        .context("source not under photos_root")?
        .to_path_buf();
    let cache_path = cache_root.join(&rel);

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
        tokio::task::spawn_blocking(move || render_thumb(&src, &dst))
            .await
            .context("thumbnail task panicked")??;
    }

    let final_meta = tokio::fs::metadata(&cache_path).await?;
    Ok(ThumbInfo {
        path: cache_path,
        mtime: final_meta.modified()?,
        size: final_meta.len(),
    })
}

fn render_thumb(src: &Path, dst: &Path) -> Result<()> {
    let src_bytes =
        std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;

    let img = ImageReader::new(Cursor::new(&src_bytes))
        .with_guessed_format()
        .with_context(|| format!("guessing format for {}", src.display()))?
        .decode()
        .with_context(|| format!("decoding {}", src.display()))?;
    let orientation = read_exif_orientation(&src_bytes);
    let thumb = apply_orientation(img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM), orientation);

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
