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
}
