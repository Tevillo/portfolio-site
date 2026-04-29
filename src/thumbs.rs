use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use image::ImageReader;

const THUMB_MAX_DIM: u32 = 400;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const PRESERVED_CHUNKS: &[&[u8; 4]] = &[b"iCCP", b"sRGB", b"gAMA", b"cHRM"];

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
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);

    let mut thumb_bytes = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut thumb_bytes), image::ImageFormat::Png)
        .with_context(|| format!("encoding thumb for {}", src.display()))?;

    let final_bytes = splice_color_chunks(&thumb_bytes, &src_bytes)
        .with_context(|| format!("splicing color chunks for {}", src.display()))?;

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

/// Copy iCCP/sRGB/gAMA/cHRM chunks from `source` PNG into `thumb` PNG so
/// browsers color-manage the thumbnail the same way as the original.
/// Returns `thumb` unchanged if neither input is a PNG or the source has
/// none of the preserved chunks.
fn splice_color_chunks(thumb: &[u8], source: &[u8]) -> Result<Vec<u8>> {
    if !thumb.starts_with(&PNG_SIGNATURE) || !source.starts_with(&PNG_SIGNATURE) {
        return Ok(thumb.to_vec());
    }

    let preserved = extract_preserved_chunks(source)?;
    if preserved.is_empty() {
        return Ok(thumb.to_vec());
    }

    let preserved_total: usize = preserved.iter().map(|c| c.len()).sum();
    let mut out = Vec::with_capacity(thumb.len() + preserved_total);
    out.extend_from_slice(&PNG_SIGNATURE);

    let ihdr_end = chunk_end(thumb, 8)?;
    if &thumb[12..16] != b"IHDR" {
        anyhow::bail!("thumb does not start with IHDR");
    }
    out.extend_from_slice(&thumb[8..ihdr_end]);

    for chunk in preserved {
        out.extend_from_slice(chunk);
    }

    let mut idx = ihdr_end;
    while idx < thumb.len() {
        let end = chunk_end(thumb, idx)?;
        let ctype = &thumb[idx + 4..idx + 8];
        if !is_preserved_type(ctype) {
            out.extend_from_slice(&thumb[idx..end]);
        }
        idx = end;
    }

    Ok(out)
}

fn extract_preserved_chunks(buf: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut idx = 8usize;
    while idx < buf.len() {
        let end = chunk_end(buf, idx)?;
        let ctype = &buf[idx + 4..idx + 8];
        if ctype == b"IEND" {
            break;
        }
        if is_preserved_type(ctype) {
            out.push(&buf[idx..end]);
        }
        idx = end;
    }
    Ok(out)
}

fn chunk_end(buf: &[u8], idx: usize) -> Result<usize> {
    if idx + 8 > buf.len() {
        anyhow::bail!("truncated PNG chunk header at {idx}");
    }
    let len = u32::from_be_bytes(buf[idx..idx + 4].try_into().unwrap()) as usize;
    let end = idx + 8 + len + 4;
    if end > buf.len() {
        anyhow::bail!("truncated PNG chunk body at {idx}");
    }
    Ok(end)
}

fn is_preserved_type(ctype: &[u8]) -> bool {
    PRESERVED_CHUNKS.iter().any(|w| w.as_slice() == ctype)
}
