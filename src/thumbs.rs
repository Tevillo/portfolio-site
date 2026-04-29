use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use image::ImageReader;

const THUMB_MAX_DIM: u32 = 400;

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
    let img = ImageReader::open(src)
        .with_context(|| format!("opening {}", src.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding {}", src.display()))?;
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);

    let parent = dst.parent().context("thumb dst has no parent")?;
    let file_name = dst.file_name().context("thumb dst has no file name")?;
    let mut tmp = parent.to_path_buf();
    tmp.push(format!(".{}.tmp", file_name.to_string_lossy()));

    thumb
        .save_with_format(&tmp, image::ImageFormat::Png)
        .with_context(|| format!("writing tmp thumb {}", tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), dst.display()))?;
    Ok(())
}
