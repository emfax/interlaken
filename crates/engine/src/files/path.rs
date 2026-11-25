//! Paths of files and directories (ADR-0044).

use crate::EngineError;

/// The longest name a file or directory can have, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// Split `path`, such as `/uploads/report.pdf`, into its names. `/` is the
/// root, with no names; a trailing `/` is allowed.
///
/// # Errors
///
/// Returns [`EngineError::InvalidPath`] for a path that doesn't start with `/`, or has an empty, `.` or `..`
/// name, a NUL, or a name longer than [`MAX_NAME_BYTES`].
pub(crate) fn split(path: &str) -> Result<Vec<String>, EngineError> {
    let invalid = |why: &str| EngineError::InvalidPath(format!("{path:?}: {why}"));
    let Some(rest) = path.strip_prefix('/') else {
        return Err(invalid("a path starts with /"));
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    rest.split('/')
        .map(|name| match name {
            "" => Err(invalid("empty name")),
            "." | ".." => Err(invalid("names can't be . or ..")),
            _ if name.contains('\0') => Err(invalid("names can't hold NUL")),
            _ if name.len() > MAX_NAME_BYTES => Err(invalid("a name is longer than 255 bytes")),
            _ => Ok(name.to_owned()),
        })
        .collect()
}

/// Split `path` into its parent's names and its own name.
///
/// # Errors
///
/// As for [`split`], and [`EngineError::InvalidPath`] for the root.
pub(crate) fn split_last(path: &str) -> Result<(Vec<String>, String), EngineError> {
    let mut names = split(path)?;
    let name = names
        .pop()
        .ok_or_else(|| EngineError::InvalidPath(format!("{path:?}: the root has no name")))?;
    Ok((names, name))
}

#[cfg(test)]
mod tests {
    use super::{split, split_last};

    #[test]
    /// Assert paths split into names, and malformed ones are refused.
    fn paths_split() {
        assert!(split("/").unwrap().is_empty());
        assert_eq!(split("/a/b/").unwrap(), ["a", "b"]);
        assert_eq!(
            split_last("/a/b").unwrap(),
            (vec!["a".to_owned()], "b".to_owned())
        );
        for bad in ["", "a", "/a//b", "/a/../b", "/./a", "/a\0"] {
            assert!(split(bad).is_err(), "{bad:?}");
        }
        assert!(split_last("/").is_err());
        assert!(split(&format!("/{}", "x".repeat(256))).is_err());
    }
}
