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
}
