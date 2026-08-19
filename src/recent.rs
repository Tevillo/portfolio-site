//! The "recent" set — which folders make up the current drop.
//!
//! Declared in `photos/.recent`, one folder path relative to the photos root
//! per line. This is a deliberate statement ("these folders are the new
//! photos"), not something inferred from the filesystem: EXIF and mtimes both
//! lie about publication, since a negative shot in 2019 can be scanned and
//! uploaded today.
//!
//! It lives in a runtime file rather than a constant for the same reason
//! `.password` does (see `set_password.sh`): adding photos needs only a file
//! copy plus `update_db.sh`, with no `cargo build`, and requiring a rebuild to
//! change which folders are current would put a deploy in the middle of the
//! step that happens most often.
//!
//! One set, two readers: `/recent` renders it, and `notify` announces it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::paths::safe_resolve;

/// Basename of the declaration file, directly inside the photos root.
pub const FILE_NAME: &str = ".recent";

pub fn file_path(photos_root: &Path) -> PathBuf {
    photos_root.join(FILE_NAME)
}

/// Split the file into candidate folder paths. Blank lines and `#` comments are
/// skipped, surrounding whitespace and slashes trimmed, and duplicates dropped
/// while keeping the author's order — the order is the render order on
/// `/recent`, so it is the one thing about the file that is purely the owner's
/// call.
///
/// Pure and syntactic: nothing here touches the filesystem, which is what makes
/// it testable without a photos tree.
pub fn parse(contents: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = match line.split_once('#') {
            Some((before, _)) => before,
            None => line,
        };
        let rel = line.trim().trim_matches('/').trim();
        if rel.is_empty() {
            continue;
        }
        if seen.insert(rel.to_string()) {
            out.push(rel.to_string());
        }
    }
    out
}

/// The current set, ready to render.
///
/// Every failure is soft: a missing file, an unreadable one, or a line naming a
/// folder that has since been renamed all collapse to "that entry is not in the
/// set". `/recent` showing two of three rolls beats it returning a 500 because
/// one folder moved, and the warning is the actionable half anyway.
pub async fn load(photos_root: &Path) -> Vec<String> {
    let path = file_path(photos_root);
    let contents = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "reading .recent failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for rel in parse(&contents) {
        match resolve_folder(photos_root, &rel).await {
            Ok(_) => out.push(rel),
            Err(e) => warn!(rel = %rel, error = %e, "skipping .recent entry"),
        }
    }
    out
}

/// Resolve one entry to an absolute path, rejecting anything that is not a
/// directory inside the photos root. `safe_resolve` is the same containment
/// check every user-supplied path on the site goes through, so a `..` or an
/// absolute path in the file is refused exactly as it would be in a URL.
async fn resolve_folder(photos_root: &Path, rel: &str) -> Result<PathBuf> {
    let abs = safe_resolve(photos_root, rel)
        .await
        .with_context(|| format!("resolving {rel}"))?;
    if !tokio::fs::metadata(&abs)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        bail!("{rel} is not a directory");
    }
    Ok(abs)
}

/// Strict validation for `recent set`: every folder must resolve and hold at
/// least one visible photo. Unlike [`load`], a bad entry is an error rather
/// than a skip — the point of the subcommand is to catch a typo at the moment
/// it is made, instead of silently writing a set that renders short.
pub async fn validate(photos_root: &Path, folders: &[String]) -> Result<Vec<String>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for folder in folders {
        let rel = folder.trim().trim_matches('/').trim().to_string();
        if rel.is_empty() {
            bail!("empty folder path");
        }
        let abs = resolve_folder(photos_root, &rel).await?;
        if !crate::handlers::subtree_has_jpeg(&abs).await {
            bail!("{rel} holds no visible photos");
        }
        if seen.insert(rel.clone()) {
            out.push(rel);
        }
    }
    Ok(out)
}

/// Replace the set. Written temp-then-rename so a live server never reads a
/// half-written file, matching how the zip cache and the warm command write.
pub async fn write(photos_root: &Path, folders: &[String]) -> Result<()> {
    let path = file_path(photos_root);
    let tmp = path.with_extension("recent.tmp");
    let mut body = String::new();
    for folder in folders {
        body.push_str(folder);
        body.push('\n');
    }
    tokio::fs::write(&tmp, body.as_bytes())
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_blanks_and_comments() {
        let got = parse("# a comment\n\n2026/utopia\n   \n2026/caldwell-35 # trailing\n");
        assert_eq!(got, vec!["2026/utopia", "2026/caldwell-35"]);
    }

    #[test]
    fn trims_slashes_and_dedupes_keeping_order() {
        let got = parse("/2026/utopia/\n2026/caldwell-35\n2026/utopia\n");
        assert_eq!(got, vec!["2026/utopia", "2026/caldwell-35"]);
    }

    #[test]
    fn empty_file_is_an_empty_set() {
        assert!(parse("").is_empty());
        assert!(parse("#only a comment\n").is_empty());
    }
}
