//! Path spellings, without a filesystem.
//!
//! Port of the parts of Zed's `util::paths` that [`crate::mention`] needs:
//! `PathStyle`, `is_absolute` and `PathWithPosition`. Zed's `util` crate is not
//! in `reference/zed-acp/` — only the ACP crates are — so this is written from
//! the call sites rather than copied, and it is deliberately no larger than
//! those call sites require.
//!
//! Everything here is string logic. A mention URI names a path on the machine
//! the *server* runs on, which is not necessarily the machine doing the
//! parsing, so the style has to be a parameter rather than a `cfg`.

use std::path::PathBuf;

/// Which spelling of a path a string is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathStyle {
    Unix,
    Windows,
}

impl PathStyle {
    /// The style of the machine this is compiled for.
    pub const fn local() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }

    pub const fn is_windows(&self) -> bool {
        matches!(self, Self::Windows)
    }
}

/// Whether `path` is absolute *in the given style*, textually.
///
/// `std::path::Path::is_absolute` answers for the host, which is the wrong
/// question: a Windows path is still a Windows path when a Linux server parses
/// it.
pub fn is_absolute(path: &str, style: PathStyle) -> bool {
    match style {
        PathStyle::Unix => path.starts_with('/'),
        // `/C:/foo` and `//server/share` are both spellings Windows tooling
        // emits, so a leading separator of either kind counts, as does a bare
        // drive prefix.
        PathStyle::Windows => {
            path.starts_with('/') || path.starts_with('\\') || has_drive_prefix(path)
        }
    }
}

fn has_drive_prefix(path: &str) -> bool {
    let mut chars = path.chars();
    matches!((chars.next(), chars.next()), (Some(drive), Some(':')) if drive.is_ascii_alphabetic())
}

/// A path that may carry a `:row` or `:row:column` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathWithPosition {
    pub path: PathBuf,
    /// 1-based, as written.
    pub row: Option<u32>,
    /// 1-based, as written.
    pub column: Option<u32>,
}

impl PathWithPosition {
    /// Splits a trailing `:row` or `:row:column` off a path.
    ///
    /// A suffix only counts when it parses as a number, which is what keeps a
    /// Windows drive letter (`C:\dir\file.rs`) from being read as a position:
    /// `\dir\file.rs` is not a number.
    ///
    /// Zed's version also understands `(row, column)` and a trailing colon;
    /// neither shape reaches a mention URI, so neither is ported.
    pub fn parse_str(input: &str) -> Self {
        let bare = |path: &str| Self {
            path: PathBuf::from(path),
            row: None,
            column: None,
        };

        let Some((head, last)) = input.rsplit_once(':') else {
            return bare(input);
        };
        let Ok(last) = last.parse::<u32>() else {
            return bare(input);
        };

        // One number found. A second one before it makes the first a column.
        if let Some((rest, middle)) = head.rsplit_once(':')
            && let Ok(middle) = middle.parse::<u32>()
        {
            return Self {
                path: PathBuf::from(rest),
                row: Some(middle),
                column: Some(last),
            };
        }

        Self {
            path: PathBuf::from(head),
            row: Some(last),
            column: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_path_is_absolute_only_under_the_windows_style() {
        assert!(is_absolute("C:\\dir\\file.rs", PathStyle::Windows));
        assert!(!is_absolute("C:\\dir\\file.rs", PathStyle::Unix));
        assert!(is_absolute("//server/share/file.rs", PathStyle::Windows));
        assert!(is_absolute("/C:/dir/file.rs", PathStyle::Windows));
    }

    #[test]
    fn a_posix_path_is_absolute_when_it_starts_at_the_root() {
        assert!(is_absolute("/tmp/a.rs", PathStyle::Unix));
        assert!(!is_absolute("tmp/a.rs", PathStyle::Unix));
        assert!(!is_absolute("", PathStyle::Unix));
    }

    #[test]
    fn a_path_with_no_suffix_keeps_every_character() {
        let parsed = PathWithPosition::parse_str("/path/to/file.rs");
        assert_eq!(parsed.path, PathBuf::from("/path/to/file.rs"));
        assert_eq!(parsed.row, None);
        assert_eq!(parsed.column, None);
    }

    #[test]
    fn a_row_and_a_column_are_split_off_the_end() {
        let parsed = PathWithPosition::parse_str("/path/to/file.rs:42");
        assert_eq!(parsed.path, PathBuf::from("/path/to/file.rs"));
        assert_eq!((parsed.row, parsed.column), (Some(42), None));

        let parsed = PathWithPosition::parse_str("/path/to/file.rs:42:5");
        assert_eq!(parsed.path, PathBuf::from("/path/to/file.rs"));
        assert_eq!((parsed.row, parsed.column), (Some(42), Some(5)));
    }

    #[test]
    fn a_windows_drive_letter_is_not_a_row() {
        // The whole reason a suffix must parse as a number.
        let parsed = PathWithPosition::parse_str("C:\\Users\\zed\\main.rs");
        assert_eq!(parsed.path, PathBuf::from("C:\\Users\\zed\\main.rs"));
        assert_eq!(parsed.row, None);

        let parsed = PathWithPosition::parse_str("C:\\Users\\zed\\main.rs:42");
        assert_eq!(parsed.path, PathBuf::from("C:\\Users\\zed\\main.rs"));
        assert_eq!((parsed.row, parsed.column), (Some(42), None));
    }

    #[test]
    fn a_trailing_colon_with_no_number_is_part_of_the_path() {
        let parsed = PathWithPosition::parse_str("/path/to/odd:");
        assert_eq!(parsed.path, PathBuf::from("/path/to/odd:"));
        assert_eq!(parsed.row, None);
    }
}
