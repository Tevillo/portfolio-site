use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path contains invalid component")]
    Invalid,
    #[error("path escapes the photos root")]
    Escape,
    #[error("path not found")]
    NotFound,
}

/// Reject any user-supplied path that contains `..`, root anchors, or null bytes
/// before we even touch the filesystem.
pub fn precheck(user_path: &str) -> Result<PathBuf, PathError> {
    if user_path.as_bytes().contains(&0) {
        return Err(PathError::Invalid);
    }
    let candidate = Path::new(user_path);
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            // A leading "/" or trailing "/" is harmless and produces a CurDir/RootDir
            // we strip below; everything else is rejected.
            Component::CurDir => {}
            _ => return Err(PathError::Invalid),
        }
    }
    Ok(candidate
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect())
}

/// Join `user_path` under `root`, canonicalize it, and verify it stays inside `root`.
/// Used for both directories (browse) and files (image, thumb source).
pub async fn safe_resolve(root: &Path, user_path: &str) -> Result<PathBuf, PathError> {
    let safe = precheck(user_path)?;
    let candidate = root.join(safe);
    let resolved = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|_| PathError::NotFound)?;
    if !resolved.starts_with(root) {
        return Err(PathError::Escape);
    }
    Ok(resolved)
}

/// The leading run of digits in a folder name, when it parses as a number.
///
/// The archive files photographs under a year at the top level, so "2024" keys
/// on 2024 and "2024-summer" keys on 2024 too. Returning `None` for a name with
/// no leading digits (or a digit run too long to be a year) keeps one-off
/// buckets like "misc" out of the numeric order entirely, rather than
/// pretending they are year 0.
///
/// This is the archive's naming convention rather than a containment rule, and
/// it lives here because two pages now depend on agreeing about it: `/all`
/// orders its top level by it, and a person's page orders their photographs by
/// the year segment of each album path. One definition, so the two cannot
/// drift apart on what counts as a year.
pub fn leading_year(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// True when `name`'s extension is one of `exts`, ignoring case.
pub fn has_ext(name: &str, exts: &[&str]) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| exts.iter().any(|w| ext.eq_ignore_ascii_case(w)))
}

pub fn is_jpeg(name: &str) -> bool {
    has_ext(name, &["jpg", "jpeg"])
}

/// A file is "hidden" if its basename contains the substring "hidden"
/// (case-insensitive). Applied on top of the .jpg/.jpeg filter everywhere —
/// browse, `/all`, people, and a work job's gallery, counts, archives and
/// per-file downloads. In work it is how a cover cropped for the `/work` card
/// stays out of the job; see `handlers::is_hidden_work_cover`.
pub fn is_hidden(name: &str) -> bool {
    name.to_ascii_lowercase().contains("hidden")
}

/// Directories the lister and subtree scanners should pretend don't exist.
pub fn is_skipped_dir(name: &str) -> bool {
    name.eq_ignore_ascii_case("negative")
}

/// A JPEG the site will show: the filter every photo listing and walk applies.
pub fn is_visible_photo(name: &str) -> bool {
    is_jpeg(name) && !is_hidden(name)
}

/// Forward-slash parent of a relative path, or "" for a file at the top.
/// "digital/file.jpg" -> "digital"; "flat.jpg" -> "".
pub fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// `parent/child`, or just `child` when `parent` is empty. Slashes around
/// `parent` are trimmed, so digiKam's `/2024/roll` album paths join the same
/// way as the site's own `2024/roll` relative paths.
pub fn join_rel(parent: &str, child: &str) -> String {
    let parent = parent.trim_matches('/');
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent() {
        assert!(precheck("../etc/passwd").is_err());
        assert!(precheck("foo/../../etc").is_err());
    }

    #[test]
    fn rejects_root() {
        assert!(precheck("/etc/passwd").is_err());
    }

    #[test]
    fn rejects_null() {
        assert!(precheck("foo\0bar").is_err());
    }

    #[test]
    fn accepts_normal() {
        assert_eq!(precheck("portfolio").unwrap(), PathBuf::from("portfolio"));
        assert_eq!(
            precheck("sketches/2025/winter").unwrap(),
            PathBuf::from("sketches/2025/winter")
        );
    }

    #[test]
    fn accepts_trailing_slash() {
        assert_eq!(precheck("portfolio/").unwrap(), PathBuf::from("portfolio"));
    }

    #[test]
    fn leading_year_reads_the_digit_prefix() {
        assert_eq!(leading_year("2024"), Some(2024));
        assert_eq!(leading_year("2024-summer"), Some(2024));
        assert_eq!(leading_year("misc"), None);
        assert_eq!(leading_year(""), None);
        // Long enough to overflow a u32, so it is not a year.
        assert_eq!(leading_year("99999999999999999999"), None);
    }

    #[test]
    fn jpeg_detection() {
        assert!(is_jpeg("IMG_0001.JPG"));
        assert!(is_jpeg("foo.jpeg"));
        assert!(!is_jpeg("foo.cr3"));
        assert!(!is_jpeg("foo"));
        assert!(!is_visible_photo("hidden-album-cover.jpg"));
    }

    #[test]
    fn parent_and_join_are_inverse() {
        assert_eq!(parent_rel("digital/file.jpg"), "digital");
        assert_eq!(
            parent_rel("film/medium-format/positive/x.jpg"),
            "film/medium-format/positive"
        );
        assert_eq!(parent_rel("flat.jpg"), "");
        assert_eq!(join_rel("", "flat.jpg"), "flat.jpg");
        assert_eq!(join_rel("/2024/roll/", "a.jpg"), "2024/roll/a.jpg");
        assert_eq!(
            join_rel(parent_rel("2024/roll/a.jpg"), "a.cr3"),
            "2024/roll/a.cr3"
        );
    }
}
