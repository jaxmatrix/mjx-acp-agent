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
pub fn resolve_within(
    roots: &[PathBuf],
    path: &Path,
    must_exist: bool,
) -> Result<PathBuf, WorkspaceError> {
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
    if contents.ends_with('\n')
        && !out.is_empty()
        && limit.is_none_or(|l| selected.len() < l as usize)
    {
        out.push('\n');
    }
    Ok(out)
}

/// A file or directory offered to the composer as a mention candidate.
///
/// A name, and nothing else. The listing exists so the browser can offer an
/// `@`-mention; anything more would be telling a caller about the workspace
/// that reading the file would have told them anyway, only without the jail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The root this was found under, so the caller can relativize it.
    pub root: PathBuf,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// The answer to one enumeration, and whether it was cut short.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// A budget ran out. The caller should narrow its query rather than page.
    pub truncated: bool,
}

/// Directories never worth offering as a mention, skipped whole.
///
/// Not a `.gitignore` reading: that would depend on git state in a way nothing
/// else here does, and it would hide generated files people legitimately
/// mention. This is a fixed list of directories that are never the answer.
const SKIPPED: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
    ".mjx-cache",
];

/// How many directory entries a single listing will look at.
const MAX_VISITS: usize = 20_000;
/// How deep below a root a listing will go.
const MAX_DEPTH: usize = 12;

/// Lists the files under `root`, or under every root when it is `None`.
///
/// `query` is a case-insensitive substring matched against the path *relative
/// to its root*, applied during the walk so that `limit` counts matches rather
/// than files looked at.
///
/// The walk is breadth-first on purpose: when a budget runs out, what survives
/// is the shallow entries, and shallow entries are what people mention.
pub fn list_within(
    roots: &[PathBuf],
    root: Option<&Path>,
    query: &str,
    limit: usize,
) -> Result<Listing, WorkspaceError> {
    // A caller-named root goes through the same jail a read does. Without
    // this, `?root=/etc` would enumerate the machine.
    let starts: Vec<PathBuf> = match root {
        Some(root) => vec![resolve_within(roots, root, true)?],
        None => roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect(),
    };

    let needle = query.to_lowercase();
    let mut listing = Listing::default();
    let mut visits = 0usize;

    for start in starts {
        // (directory, depth). BFS, so the queue is drained from the front.
        let mut queue = std::collections::VecDeque::from([(start.clone(), 0usize)]);

        while let Some((dir, depth)) = queue.pop_front() {
            let Ok(children) = std::fs::read_dir(&dir) else {
                // An unreadable directory is not an error for a listing; it
                // simply has nothing to offer.
                continue;
            };

            for child in children.flatten() {
                visits += 1;
                if visits > MAX_VISITS {
                    listing.truncated = true;
                    return Ok(listing);
                }

                let path = child.path();
                let name = child.file_name();
                let name = name.to_string_lossy();

                // `symlink_metadata` does not follow the link, so this is the
                // link itself rather than what it points at.
                let Ok(meta) = path.symlink_metadata() else {
                    continue;
                };

                // A symlinked directory is the one way a *listing* leaves a
                // jail that `resolve_within` would have caught at read time.
                // Never descend it, and do not offer it either — a mention of
                // it would name a tree that is not in the workspace. A
                // symlinked *file* is still listed: naming it is not reading
                // it, and the read is still jailed.
                if meta.file_type().is_symlink() && path.is_dir() {
                    continue;
                }

                let is_dir = meta.is_dir();
                if is_dir && SKIPPED.contains(&name.as_ref()) {
                    continue;
                }

                let matches = needle.is_empty()
                    || path
                        .strip_prefix(&start)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&needle);

                if matches {
                    if listing.entries.len() >= limit {
                        listing.truncated = true;
                        return Ok(listing);
                    }
                    listing.entries.push(Entry {
                        root: start.clone(),
                        path: path.clone(),
                        is_dir,
                    });
                }

                if is_dir {
                    if depth + 1 > MAX_DEPTH {
                        listing.truncated = true;
                        continue;
                    }
                    queue.push_back((path, depth + 1));
                }
            }
        }
    }

    Ok(listing)
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
        let contents = read_text_file(
            std::slice::from_ref(&f.root),
            &f.root.join("a.txt"),
            None,
            None,
        )
        .unwrap();
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
        let err = read_text_file(
            std::slice::from_ref(&f.root),
            &f.outside.join("secret.txt"),
            None,
            None,
        )
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

        let err = write_text_file(
            std::slice::from_ref(&f.root),
            &link_dir.join("new.txt"),
            "x",
        )
        .unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
        assert!(
            !f.outside.join("new.txt").exists(),
            "the write escaped the jail"
        );
    }

    #[test]
    fn relative_paths_are_refused() {
        let f = fixture();
        let err = read_text_file(
            std::slice::from_ref(&f.root),
            Path::new("a.txt"),
            None,
            None,
        )
        .unwrap_err();
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
        assert_eq!(
            write_text_file(std::slice::from_ref(&f.root), &path, "fresh").unwrap(),
            None
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
    }

    #[test]
    fn writing_into_a_missing_directory_inside_a_root_fails_cleanly() {
        // The parent has to exist for the jail check to mean anything, so this
        // is a not-found rather than a silent mkdir outside the root.
        let f = fixture();
        let err = write_text_file(
            std::slice::from_ref(&f.root),
            &f.root.join("no/such/dir/x.txt"),
            "x",
        )
        .unwrap_err();
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

    /// The relative paths a listing offered, sorted, for readable assertions.
    fn listed(listing: &Listing) -> Vec<String> {
        let mut names: Vec<String> = listing
            .entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .strip_prefix(&entry.root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_listing_only_contains_paths_inside_the_root() {
        let f = fixture();
        let listing = list_within(std::slice::from_ref(&f.root), None, "", 100).unwrap();
        assert!(!listing.entries.is_empty());
        for entry in &listing.entries {
            assert!(
                entry.path.starts_with(&f.root),
                "{} escaped the root",
                entry.path.display()
            );
        }
        assert!(listed(&listing).contains(&"a.txt".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_not_followed_out_of_the_workspace() {
        let f = fixture();
        std::os::unix::fs::symlink(&f.outside, f.root.join("escape-dir")).unwrap();

        let listing = list_within(std::slice::from_ref(&f.root), None, "", 100).unwrap();
        let names = listed(&listing);
        assert!(
            !names.iter().any(|name| name.contains("secret.txt")),
            "the listing followed a symlink out of the jail: {names:?}"
        );
        assert!(
            !names.contains(&"escape-dir".to_string()),
            "a symlinked directory is not a workspace directory: {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_still_offered() {
        // Naming a file is not reading it, and the read is still jailed — so
        // there is no reason to hide a link to a file inside the workspace.
        let f = fixture();
        std::os::unix::fs::symlink(f.root.join("a.txt"), f.root.join("alias.txt")).unwrap();

        let listing = list_within(std::slice::from_ref(&f.root), None, "", 100).unwrap();
        assert!(listed(&listing).contains(&"alias.txt".to_string()));
    }

    #[test]
    fn a_root_outside_the_configured_roots_is_refused() {
        let f = fixture();
        let err =
            list_within(std::slice::from_ref(&f.root), Some(&f.outside), "", 100).unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
    }

    #[test]
    fn noise_directories_are_skipped() {
        let f = fixture();
        std::fs::create_dir_all(f.root.join("node_modules/left-pad")).unwrap();
        std::fs::write(f.root.join("node_modules/left-pad/index.js"), "").unwrap();
        std::fs::write(f.root.join("nested/b.txt"), "").unwrap();

        let names = listed(&list_within(std::slice::from_ref(&f.root), None, "", 100).unwrap());
        assert!(
            !names.iter().any(|name| name.starts_with("node_modules")),
            "{names:?}"
        );
        assert!(names.contains(&"nested/b.txt".to_string()), "{names:?}");
    }

    #[test]
    fn the_query_matches_the_path_relative_to_its_root_case_insensitively() {
        let f = fixture();
        std::fs::write(f.root.join("nested/Stats.js"), "").unwrap();

        let names =
            listed(&list_within(std::slice::from_ref(&f.root), None, "stats", 100).unwrap());
        assert_eq!(names, vec!["nested/Stats.js".to_string()]);

        // The directory is part of the path, so it matches too.
        let names =
            listed(&list_within(std::slice::from_ref(&f.root), None, "nested/", 100).unwrap());
        assert!(names.contains(&"nested/Stats.js".to_string()), "{names:?}");
    }

    #[test]
    fn a_listing_past_the_cap_says_it_was_truncated() {
        let f = fixture();
        for i in 0..10 {
            std::fs::write(f.root.join(format!("f{i}.txt")), "").unwrap();
        }

        let listing = list_within(std::slice::from_ref(&f.root), None, "", 3).unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.truncated, "a capped listing must say so");

        let listing = list_within(std::slice::from_ref(&f.root), None, "", 100).unwrap();
        assert!(!listing.truncated);
    }
}
