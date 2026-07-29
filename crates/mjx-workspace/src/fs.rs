//! The filesystem capability, confined to the workspace roots.
//!
//! The browser is the ACP client but has no filesystem, so the server answers
//! `fs/read_text_file` and `fs/write_text_file` on its behalf. That makes this
//! module the boundary between an agent and the disk, and the jail below is the
//! only thing standing between a mistaken path and the rest of the machine.

use std::path::{Path, PathBuf};

use crate::WorkspaceError;

/// Resolves `path` and confirms it is inside one of `roots`.
///
/// Both sides are canonicalized first, so `..` segments and symlinks are
/// resolved *before* the comparison. Checking the textual path instead would be
/// defeated by either.
///
/// `must_exist` is false for writes, where the file is allowed not to exist
/// yet; in that case the parent directory is what gets checked.
pub fn resolve_within(roots: &[PathBuf], path: &Path, must_exist: bool) -> Result<PathBuf, WorkspaceError> {
    if !path.is_absolute() {
        return Err(WorkspaceError::NotAbsolute(path.to_path_buf()));
    }

    let resolved = if must_exist {
        path.canonicalize()
            .map_err(|_| WorkspaceError::NotFound(path.to_path_buf()))?
    } else {
        // The file may not exist, but its directory must, and it is the
        // directory that decides whether we are inside a root.
        let parent = path
            .parent()
            .ok_or_else(|| WorkspaceError::NotAbsolute(path.to_path_buf()))?;
        let parent = parent
            .canonicalize()
            .map_err(|_| WorkspaceError::NotFound(parent.to_path_buf()))?;
        match path.file_name() {
            Some(name) => parent.join(name),
            None => parent,
        }
    };

    let allowed = roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|root| resolved.starts_with(&root))
    });

    if allowed {
        Ok(resolved)
    } else {
        Err(WorkspaceError::OutsideWorkspace(resolved))
    }
}

/// Reads a text file, optionally a slice of its lines.
///
/// `line` is 1-based, matching the protocol. `limit` counts lines, not bytes.
pub fn read_text_file(
    roots: &[PathBuf],
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, WorkspaceError> {
    let path = resolve_within(roots, path, true)?;
    let contents = std::fs::read_to_string(&path)
        .map_err(|err| WorkspaceError::Io(path.clone(), err.to_string()))?;

    if line.is_none() && limit.is_none() {
        return Ok(contents);
    }

    // `line` is 1-based; line 0 and line 1 both mean "from the start".
    let skip = line.unwrap_or(1).saturating_sub(1) as usize;
    let mut selected: Vec<&str> = contents.lines().skip(skip).collect();
    if let Some(limit) = limit {
        selected.truncate(limit as usize);
    }

    let mut out = selected.join("\n");
    // Keep the trailing newline when the slice runs to the end of a file that
    // had one, so a read/write round trip doesn't quietly strip it.
    if contents.ends_with('\n') && !out.is_empty() && limit.is_none_or(|l| selected.len() < l as usize)
    {
        out.push('\n');
    }
    Ok(out)
}

/// Writes a text file, creating it and any missing parent directories.
///
/// Returns the previous contents, so the caller can show the browser a diff.
/// `None` means the file was created.
pub fn write_text_file(
    roots: &[PathBuf],
    path: &Path,
    contents: &str,
) -> Result<Option<String>, WorkspaceError> {
    let path = resolve_within(roots, path, false)?;

    let previous = std::fs::read_to_string(&path).ok();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| WorkspaceError::Io(parent.to_path_buf(), err.to_string()))?;
    }
    std::fs::write(&path, contents)
        .map_err(|err| WorkspaceError::Io(path.clone(), err.to_string()))?;

    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(outside.join("secret.txt"), "classified").unwrap();
        Fixture {
            _dir: dir,
            root,
            outside,
        }
    }

    #[test]
    fn reads_a_whole_file() {
        let f = fixture();
        let contents = read_text_file(std::slice::from_ref(&f.root), &f.root.join("a.txt"), None, None).unwrap();
        assert_eq!(contents, "one\ntwo\nthree\nfour\n");
    }

    #[test]
    fn reads_a_line_range() {
        let f = fixture();
        let roots = [f.root.clone()];
        let path = f.root.join("a.txt");

        assert_eq!(
            read_text_file(&roots, &path, Some(2), Some(2)).unwrap(),
            "two\nthree"
        );
        // Line 1 and line 0 both mean "from the start".
        assert_eq!(
            read_text_file(&roots, &path, Some(1), Some(1)).unwrap(),
            "one"
        );
        assert_eq!(
            read_text_file(&roots, &path, Some(0), Some(1)).unwrap(),
            "one"
        );
        // A limit past the end is not an error.
        assert_eq!(
            read_text_file(&roots, &path, Some(3), Some(99)).unwrap(),
            "three\nfour\n"
        );
    }

    #[test]
    fn a_path_outside_every_root_is_refused() {
        let f = fixture();
        let err = read_text_file(std::slice::from_ref(&f.root), &f.outside.join("secret.txt"), None, None)
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
    }

    #[test]
    fn traversal_out_of_a_root_is_refused() {
        let f = fixture();
        // Textually this starts with the root; it must still be rejected,
        // which is why the check canonicalizes first.
        let sneaky = f.root.join("../outside/secret.txt");
        let err = read_text_file(std::slice::from_ref(&f.root), &sneaky, None, None).unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_a_root_is_refused() {
        let f = fixture();
        let link = f.root.join("escape.txt");
        std::os::unix::fs::symlink(f.outside.join("secret.txt"), &link).unwrap();

        let err = read_text_file(std::slice::from_ref(&f.root), &link, None, None).unwrap_err();
        assert!(
            matches!(err, WorkspaceError::OutsideWorkspace(_)),
            "a symlink must not be a way out of the jail: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlinked_directory_is_refused() {
        let f = fixture();
        let link_dir = f.root.join("escape-dir");
        std::os::unix::fs::symlink(&f.outside, &link_dir).unwrap();

        let err = write_text_file(std::slice::from_ref(&f.root), &link_dir.join("new.txt"), "x").unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
        assert!(
            !f.outside.join("new.txt").exists(),
            "the write escaped the jail"
        );
    }

    #[test]
    fn relative_paths_are_refused() {
        let f = fixture();
        let err = read_text_file(std::slice::from_ref(&f.root), Path::new("a.txt"), None, None).unwrap_err();
        assert!(matches!(err, WorkspaceError::NotAbsolute(_)), "{err}");
    }

    #[test]
    fn writing_returns_the_previous_contents_for_a_diff() {
        let f = fixture();
        let roots = [f.root.clone()];
        let path = f.root.join("a.txt");

        let previous = write_text_file(&roots, &path, "replaced\n").unwrap();
        assert_eq!(previous.as_deref(), Some("one\ntwo\nthree\nfour\n"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced\n");
    }

    #[test]
    fn creating_a_new_file_reports_no_previous_contents() {
        let f = fixture();
        let path = f.root.join("nested/new.txt");
        assert_eq!(write_text_file(std::slice::from_ref(&f.root), &path, "fresh").unwrap(), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
    }

    #[test]
    fn writing_into_a_missing_directory_inside_a_root_fails_cleanly() {
        // The parent has to exist for the jail check to mean anything, so this
        // is a not-found rather than a silent mkdir outside the root.
        let f = fixture();
        let err =
            write_text_file(std::slice::from_ref(&f.root), &f.root.join("no/such/dir/x.txt"), "x").unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound(_)), "{err}");
    }

    #[test]
    fn several_roots_are_all_honoured() {
        let f = fixture();
        let roots = [f.root.clone(), f.outside.clone()];
        assert!(read_text_file(&roots, &f.outside.join("secret.txt"), None, None).is_ok());
        assert!(read_text_file(&roots, &f.root.join("a.txt"), None, None).is_ok());
    }

    #[test]
    fn no_roots_means_nothing_is_readable() {
        let f = fixture();
        assert!(read_text_file(&[], &f.root.join("a.txt"), None, None).is_err());
    }
}
