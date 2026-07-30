//! What an `@`-mention points at.
//!
//! Port of Zed's `crates/acp_thread/src/mention.rs`
//! (`reference/zed-acp/acp_thread/src/mention.rs`). A mention travels over ACP
//! as a `resource_link` content block, and its `uri` is the whole of what it
//! means — so both ends of this project need the same parser, and the parser
//! has to be exact or a link stops round-tripping.
//!
//! Nothing on the server calls this yet. It exists because the browser needs
//! the same parser (`web/src/acp/mention.ts`), and this project keeps its two
//! models symmetrical and pinned to a shared fixture
//! (`fixtures/mention-uris.json`); when the server starts validating or
//! rendering mentions itself, this is what it will use.
//!
//! Two things are kept that might look like they should have been changed:
//!
//! * **The `zed:///agent/...` scheme, verbatim.** Renaming it would make our
//!   URIs unintelligible to every agent that already knows Zed's, and would
//!   throw away the only strong test available here — round-tripping Zed's own
//!   literal strings.
//! * **The Windows branches.** They are pure string logic, they are half the
//!   ported tests, and they cost nothing on Linux.
//!
//! Deliberately not ported: `icon_path` (the sole GPUI and `file_icons`
//! dependency — which icon to draw is the browser's decision), and
//! `disambiguated_name`, which needs `project::path_suffix` and a detail level
//! nothing here computes. `SharedString` becomes `String` throughout.
//!
//! The tests below are Zed's, renamed to this repository's sentence
//! convention. The assertions are unchanged.

use std::borrow::Cow;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use mjx_acp_core::acp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::paths::{PathStyle, PathWithPosition, is_absolute};

/// Why a string was not a mention URI.
///
/// Zed uses `anyhow` here; this crate uses `thiserror`, and rewriting every
/// `.context()` would make 800 ported lines undiffable against upstream. So the
/// error is a message, and the two helpers below let the ported bodies stay as
/// they are.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MentionUriError(String);

type Result<T> = std::result::Result<T, MentionUriError>;

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(MentionUriError(format!($($arg)*)))
    };
}

impl From<url::ParseError> for MentionUriError {
    fn from(err: url::ParseError) -> Self {
        Self(err.to_string())
    }
}

/// Mirrors `anyhow::Context` for the two shapes the port uses, so the ported
/// bodies stay comparable with Zed's line for line.
trait Context<T> {
    fn context(self, message: impl fmt::Display) -> Result<T>;
    fn with_context<D: fmt::Display>(self, message: impl FnOnce() -> D) -> Result<T>;
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| MentionUriError(message.to_string()))
    }

    fn with_context<D: fmt::Display>(self, message: impl FnOnce() -> D) -> Result<T> {
        self.ok_or_else(|| MentionUriError(message().to_string()))
    }
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.map_err(|_| MentionUriError(message.to_string()))
    }

    fn with_context<D: fmt::Display>(self, message: impl FnOnce() -> D) -> Result<T> {
        self.map_err(|_| MentionUriError(message().to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum MentionUri {
    File {
        abs_path: PathBuf,
    },
    PastedImage {
        name: String,
    },
    Directory {
        abs_path: PathBuf,
    },
    Symbol {
        abs_path: PathBuf,
        name: String,
        line_range: RangeInclusive<u32>,
    },
    Thread {
        id: acp::SessionId,
        name: String,
    },
    /// Deprecated: kept so threads from before rules became skills still
    /// deserialize. `id` (an opaque `prompt_store::PromptId`) is preserved
    /// verbatim so re-saved threads stay loadable by older Zed versions.
    Rule {
        #[serde(default = "default_deprecated_rule_id")]
        id: serde_json::Value,
        name: String,
    },
    Diagnostics {
        #[serde(default = "default_include_errors")]
        include_errors: bool,
        #[serde(default)]
        include_warnings: bool,
    },
    Selection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abs_path: Option<PathBuf>,
        line_range: RangeInclusive<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
    Fetch {
        url: Url,
    },
    TerminalSelection {
        line_count: u32,
    },
    GitDiff {
        base_ref: String,
    },
    MergeConflict {
        file_path: String,
    },
    Skill {
        name: String,
        source: String,
        skill_file_path: PathBuf,
    },
}

impl MentionUri {
    pub fn parse(input: &str, path_style: PathStyle) -> Result<Self> {
        let input = input
            .strip_prefix('`')
            .and_then(|input| input.strip_suffix('`'))
            .unwrap_or(input);

        let parse_column =
            |input: Option<String>| -> Option<u32> { input?.parse::<u32>().ok()?.checked_sub(1) };
        let validate_query_params = |url: &Url, allowed: &[&str]| -> Result<()> {
            for (key, _) in url.query_pairs() {
                if !allowed.contains(&key.as_ref()) {
                    bail!("invalid query parameter")
                }
            }
            Ok(())
        };

        if is_absolute(input, path_style) && !input.contains("://") {
            return parse_absolute_path(input)
                .with_context(|| format!("Invalid absolute path mention URI: {input}"));
        }

        let url = Url::parse(input)?;
        let path = url.path();
        match url.scheme() {
            "file" => {
                let trimmed = if path_style.is_windows() {
                    path.trim_start_matches("/")
                } else {
                    path
                };
                let decoded = percent_decode(trimmed);
                let normalized: Cow<str> = if path_style.is_windows() {
                    match to_native_windows_path(&decoded) {
                        Some(native) => Cow::Owned(native),
                        None => decoded,
                    }
                } else {
                    decoded
                };
                let path = normalized.as_ref();

                if let Some(fragment) = url.fragment() {
                    validate_query_params(&url, &["symbol", "column"])?;
                    let line_range = parse_line_range(fragment).ok().unwrap_or(1..=1);
                    let column = parse_column(query_param(&url, "column"));
                    if let Some(name) = query_param(&url, "symbol") {
                        Ok(Self::Symbol {
                            name,
                            abs_path: path.into(),
                            line_range,
                        })
                    } else {
                        Ok(Self::Selection {
                            abs_path: Some(path.into()),
                            line_range,
                            column,
                        })
                    }
                } else if input.ends_with("/") {
                    Ok(Self::Directory {
                        abs_path: path.into(),
                    })
                } else {
                    Ok(Self::File {
                        abs_path: path.into(),
                    })
                }
            }
            "zed" => {
                if let Some(thread_id) = path.strip_prefix("/agent/thread/") {
                    let name = single_query_param(&url, "name")?.context("Missing thread name")?;
                    Ok(Self::Thread {
                        id: acp::SessionId::new(thread_id),
                        name,
                    })
                } else if let Some(rule_id) = path.strip_prefix("/agent/rule/") {
                    // Deprecated: parses legacy rule mentions.
                    let name = single_query_param(&url, "name")?.context("Missing rule name")?;
                    let id = if rule_id.is_empty() {
                        default_deprecated_rule_id()
                    } else {
                        serde_json::json!({ "User": { "uuid": rule_id } })
                    };
                    Ok(Self::Rule { id, name })
                } else if path == "/agent/diagnostics" {
                    let mut include_errors = default_include_errors();
                    let mut include_warnings = false;
                    for (key, value) in url.query_pairs() {
                        match key.as_ref() {
                            "include_warnings" => include_warnings = value == "true",
                            "include_errors" => include_errors = value == "true",
                            _ => bail!("invalid query parameter"),
                        }
                    }
                    Ok(Self::Diagnostics {
                        include_errors,
                        include_warnings,
                    })
                } else if path.starts_with("/agent/pasted-image") {
                    let name =
                        single_query_param(&url, "name")?.unwrap_or_else(|| "Image".to_string());
                    Ok(Self::PastedImage { name })
                } else if path.starts_with("/agent/untitled-buffer") {
                    let fragment = url
                        .fragment()
                        .context("Missing fragment for untitled buffer selection")?;
                    let line_range = parse_line_range(fragment)?;
                    validate_query_params(&url, &["column"])?;
                    Ok(Self::Selection {
                        abs_path: None,
                        line_range,
                        column: parse_column(query_param(&url, "column")),
                    })
                } else if let Some(name) = path.strip_prefix("/agent/symbol/") {
                    let fragment = url
                        .fragment()
                        .context("Missing fragment for untitled buffer selection")?;
                    let line_range = parse_line_range(fragment)?;
                    let path =
                        single_query_param(&url, "path")?.context("Missing path for symbol")?;
                    Ok(Self::Symbol {
                        name: name.to_string(),
                        abs_path: path.into(),
                        line_range,
                    })
                } else if path.starts_with("/agent/file") {
                    let path =
                        single_query_param(&url, "path")?.context("Missing path for file")?;
                    Ok(Self::File {
                        abs_path: path.into(),
                    })
                } else if path.starts_with("/agent/directory") {
                    let path =
                        single_query_param(&url, "path")?.context("Missing path for directory")?;
                    Ok(Self::Directory {
                        abs_path: path.into(),
                    })
                } else if path.starts_with("/agent/selection") {
                    validate_query_params(&url, &["path", "column"])?;
                    let fragment = url.fragment().context("Missing fragment for selection")?;
                    let line_range = parse_line_range(fragment)?;
                    let column = parse_column(query_param(&url, "column"));
                    let path = query_param(&url, "path").context("Missing path for selection")?;
                    Ok(Self::Selection {
                        abs_path: Some(path.into()),
                        line_range,
                        column,
                    })
                } else if path.starts_with("/agent/terminal-selection") {
                    let line_count = single_query_param(&url, "lines")?
                        .unwrap_or_else(|| "0".to_string())
                        .parse::<u32>()
                        .unwrap_or(0);
                    Ok(Self::TerminalSelection { line_count })
                } else if path.starts_with("/agent/git-diff") {
                    let base_ref =
                        single_query_param(&url, "base")?.unwrap_or_else(|| "main".to_string());
                    Ok(Self::GitDiff { base_ref })
                } else if path.starts_with("/agent/merge-conflict") {
                    let file_path = single_query_param(&url, "path")?.unwrap_or_default();
                    Ok(Self::MergeConflict { file_path })
                } else if path.starts_with("/agent/skill") {
                    let mut name = None;
                    let mut source = None;
                    let mut skill_file_path = None;

                    for (key, value) in url.query_pairs() {
                        match key.as_ref() {
                            "name" => {
                                if name.replace(value.to_string()).is_some() {
                                    bail!("duplicate skill name query parameter");
                                }
                            }
                            "source" => {
                                if source.replace(value.to_string()).is_some() {
                                    bail!("duplicate skill source query parameter");
                                }
                            }
                            "path" => {
                                if skill_file_path
                                    .replace(PathBuf::from(value.to_string()))
                                    .is_some()
                                {
                                    bail!("duplicate skill file path query parameter");
                                }
                            }
                            _ => bail!("invalid query parameter"),
                        }
                    }

                    Ok(Self::Skill {
                        name: name.context("missing skill name")?,
                        source: source.context("missing skill source")?,
                        skill_file_path: skill_file_path.context("missing skill file path")?,
                    })
                } else {
                    bail!("invalid zed url: {:?}", input);
                }
            }
            "http" | "https" => Ok(MentionUri::Fetch { url }),
            other => bail!("unrecognized scheme {:?}", other),
        }
    }

    /// Parses a hyperlink target from agent-authored Markdown.
    ///
    /// Unlike [`MentionUri::parse`] — which stays strict so canonical mention
    /// URIs round-trip verbatim — bare path targets are normalized first:
    /// percent escapes are decoded (see [`decode_path_escapes`]) and
    /// Windows-compatible spellings like `/C:/foo` or `/c/foo` become native
    /// paths (see [`to_native_windows_path`]).
    pub fn parse_hyperlink(input: &str, path_style: PathStyle) -> Result<Self> {
        if let Some(target) = bare_path_target(input, path_style) {
            return parse_hyperlink_path(target, path_style, DecodePercentEscapes::Yes)
                .with_context(|| format!("Invalid hyperlink path target: {input}"));
        }
        Self::parse(input, path_style)
    }

    /// Returns the literal (un-decoded) interpretation of a bare-path
    /// hyperlink target, for files whose names literally contain an escape
    /// sequence (e.g. `a%20b.rs`). Returns `None` when this wouldn't differ
    /// from [`MentionUri::parse_hyperlink`], including for URLs, whose
    /// escapes are unambiguous.
    pub fn parse_hyperlink_literal(input: &str, path_style: PathStyle) -> Option<Self> {
        let target = bare_path_target(input, path_style)?;
        let (path_input, _) = split_path_fragment(target);
        if !matches!(decode_path_escapes(path_input), Cow::Owned(_)) {
            return None;
        }
        parse_hyperlink_path(target, path_style, DecodePercentEscapes::No).ok()
    }

    /// The absolute path this mention refers to, if it refers to one.
    pub fn abs_path(&self) -> Option<&Path> {
        match self {
            MentionUri::File { abs_path }
            | MentionUri::Directory { abs_path }
            | MentionUri::Symbol { abs_path, .. } => Some(abs_path),
            MentionUri::Selection { abs_path, .. } => abs_path.as_deref(),
            MentionUri::Skill {
                skill_file_path, ..
            } => Some(skill_file_path),
            MentionUri::PastedImage { .. }
            | MentionUri::Thread { .. }
            | MentionUri::Rule { .. }
            | MentionUri::Diagnostics { .. }
            | MentionUri::Fetch { .. }
            | MentionUri::TerminalSelection { .. }
            | MentionUri::GitDiff { .. }
            | MentionUri::MergeConflict { .. } => None,
        }
    }

    /// A label for this mention. Total over every variant, so a chip always has
    /// something to show.
    pub fn name(&self) -> String {
        match self {
            MentionUri::File { abs_path, .. } | MentionUri::Directory { abs_path, .. } => abs_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            MentionUri::PastedImage { name } => name.clone(),
            MentionUri::Symbol { name, .. } => name.clone(),
            MentionUri::Thread { name, .. } => name.clone(),
            MentionUri::Rule { name, .. } => name.clone(),
            MentionUri::Diagnostics { .. } => "Diagnostics".to_string(),
            MentionUri::TerminalSelection { line_count } => {
                if *line_count == 1 {
                    "Terminal (1 line)".to_string()
                } else {
                    format!("Terminal ({line_count} lines)")
                }
            }
            MentionUri::GitDiff { base_ref } => format!("Branch Diff ({base_ref})"),
            MentionUri::MergeConflict { file_path } => {
                let name = Path::new(file_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                format!("Merge Conflict ({name})")
            }
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => selection_name(path.as_deref(), line_range),
            MentionUri::Fetch { url } => url.to_string(),
            MentionUri::Skill { name, .. } => name.clone(),
        }
    }

    pub fn tooltip_text(&self) -> Option<String> {
        match self {
            MentionUri::File { abs_path } | MentionUri::Directory { abs_path } => {
                Some(abs_path.to_string_lossy().into_owned())
            }
            MentionUri::Symbol {
                abs_path,
                line_range,
                ..
            } => Some(format!(
                "{}:{}-{}",
                abs_path.display(),
                line_range.start(),
                line_range.end()
            )),
            MentionUri::Selection {
                abs_path: Some(path),
                line_range,
                ..
            } => Some(format!(
                "{}:{}-{}",
                path.display(),
                line_range.start(),
                line_range.end()
            )),
            MentionUri::Skill {
                skill_file_path, ..
            } => Some(skill_file_path.to_string_lossy().into_owned()),
            _ => None,
        }
    }

    pub fn as_link(&self) -> MentionLink<'_> {
        MentionLink(self)
    }

    pub fn to_uri(&self) -> Url {
        match self {
            MentionUri::File { abs_path } => {
                let mut url = Url::parse("file:///").unwrap();
                url.set_path(&abs_path.to_string_lossy());
                url
            }
            MentionUri::PastedImage { name } => {
                let mut url = Url::parse("zed:///agent/pasted-image").unwrap();
                url.query_pairs_mut().append_pair("name", name);
                url
            }
            MentionUri::Directory { abs_path } => {
                let mut url = Url::parse("file:///").unwrap();
                let mut path = abs_path.to_string_lossy().into_owned();
                if !path.ends_with('/') && !path.ends_with('\\') {
                    path.push('/');
                }
                url.set_path(&path);
                url
            }
            MentionUri::Symbol {
                abs_path,
                name,
                line_range,
                ..
            } => {
                let mut url = Url::parse("file:///").unwrap();
                url.set_path(&abs_path.to_string_lossy());
                url.query_pairs_mut().append_pair("symbol", name);
                url.set_fragment(Some(&format!(
                    "L{}:{}",
                    line_range.start() + 1,
                    line_range.end() + 1
                )));
                url
            }
            MentionUri::Selection {
                abs_path,
                line_range,
                column,
            } => {
                let mut url = if let Some(path) = abs_path {
                    let mut url = Url::parse("file:///").unwrap();
                    url.set_path(&path.to_string_lossy());
                    url
                } else {
                    let mut url = Url::parse("zed:///").unwrap();
                    url.set_path("/agent/untitled-buffer");
                    url
                };
                if let Some(column) = column {
                    url.query_pairs_mut()
                        .append_pair("column", &(column + 1).to_string());
                }
                url.set_fragment(Some(&format!(
                    "L{}:{}",
                    line_range.start() + 1,
                    line_range.end() + 1
                )));
                url
            }
            MentionUri::Thread { name, id } => {
                let mut url = Url::parse("zed:///").unwrap();
                url.set_path(&format!("/agent/thread/{id}"));
                url.query_pairs_mut().append_pair("name", name);
                url
            }
            MentionUri::Rule { id, name } => {
                let mut url = Url::parse("zed:///").unwrap();
                let rule_id = id
                    .get("User")
                    .and_then(|user| user.get("uuid"))
                    .and_then(|uuid| uuid.as_str())
                    .unwrap_or_default();
                url.set_path(&format!("/agent/rule/{rule_id}"));
                url.query_pairs_mut().append_pair("name", name);
                url
            }
            MentionUri::Diagnostics {
                include_errors,
                include_warnings,
            } => {
                let mut url = Url::parse("zed:///").unwrap();
                url.set_path("/agent/diagnostics");
                if *include_warnings {
                    url.query_pairs_mut()
                        .append_pair("include_warnings", "true");
                }
                if !include_errors {
                    url.query_pairs_mut().append_pair("include_errors", "false");
                }
                url
            }
            MentionUri::Fetch { url } => url.clone(),
            MentionUri::TerminalSelection { line_count } => {
                let mut url = Url::parse("zed:///agent/terminal-selection").unwrap();
                url.query_pairs_mut()
                    .append_pair("lines", &line_count.to_string());
                url
            }
            MentionUri::GitDiff { base_ref } => {
                let mut url = Url::parse("zed:///agent/git-diff").unwrap();
                url.query_pairs_mut().append_pair("base", base_ref);
                url
            }
            MentionUri::MergeConflict { file_path } => {
                let mut url = Url::parse("zed:///agent/merge-conflict").unwrap();
                url.query_pairs_mut().append_pair("path", file_path);
                url
            }
            MentionUri::Skill {
                name,
                source,
                skill_file_path,
            } => {
                let mut url = Url::parse("zed:///").unwrap();
                url.set_path("/agent/skill");
                url.query_pairs_mut()
                    .append_pair("name", name)
                    .append_pair("source", source)
                    .append_pair("path", &skill_file_path.to_string_lossy());
                url
            }
        }
    }
}

/// A mention written the way it appears in Markdown: `[@name](uri)`.
pub struct MentionLink<'a>(&'a MentionUri);

impl fmt::Display for MentionLink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[@{}]({})", self.0.name(), self.0.to_uri())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecodePercentEscapes {
    Yes,
    No,
}

/// Decodes a whole URL path component, escapes and all.
///
/// Zed uses `urlencoding::decode`; `percent-encoding` is already in this
/// workspace's lock file and does the same thing for this input.
fn percent_decode(input: &str) -> Cow<'_, str> {
    percent_encoding::percent_decode_str(input)
        .decode_utf8()
        .unwrap_or(Cow::Borrowed(input))
}

fn parse_line_range(fragment: &str) -> Result<RangeInclusive<u32>> {
    let range = fragment.strip_prefix("L").unwrap_or(fragment);

    let (start, end) = if let Some((start, end)) = range.split_once(":") {
        (start, end)
    } else if let Some((start, end)) = range.split_once("-") {
        // Also handle L10-20 or L10-L20 format
        (start, end.strip_prefix("L").unwrap_or(end))
    } else {
        // Single line number like L1872 - treat as a range of one line
        (range, range)
    };

    let start_line = start
        .parse::<u32>()
        .context("Parsing line range start")?
        .checked_sub(1)
        .context("Line numbers should be 1-based")?;
    let end_line = end
        .parse::<u32>()
        .context("Parsing line range end")?
        .checked_sub(1)
        .context("Line numbers should be 1-based")?;

    Ok(start_line..=end_line)
}

/// Returns the mention target as a bare absolute path (not a URL), with the
/// backticks agents sometimes add stripped.
fn bare_path_target(input: &str, path_style: PathStyle) -> Option<&str> {
    let input = input
        .strip_prefix('`')
        .and_then(|input| input.strip_suffix('`'))
        .unwrap_or(input);
    (is_absolute(input, path_style) && !input.contains("://")).then_some(input)
}

fn split_path_fragment(input: &str) -> (&str, Option<&str>) {
    input
        .split_once('#')
        .map_or((input, None), |(path, fragment)| (path, Some(fragment)))
}

fn parse_absolute_path(input: &str) -> Result<MentionUri> {
    let (path_input, fragment) = split_path_fragment(input);
    absolute_path_mention(path_input, fragment)
}

/// Like [`parse_absolute_path`], but normalizes hyperlink spellings first.
fn parse_hyperlink_path(
    input: &str,
    path_style: PathStyle,
    decode_escapes: DecodePercentEscapes,
) -> Result<MentionUri> {
    let (path_input, fragment) = split_path_fragment(input);
    let path_input = normalize_path_mention(path_input, path_style, decode_escapes);
    absolute_path_mention(&path_input, fragment)
}

fn absolute_path_mention(path_input: &str, fragment: Option<&str>) -> Result<MentionUri> {
    if let Some(fragment) = fragment.and_then(|fragment| parse_line_range(fragment).ok()) {
        return Ok(MentionUri::Selection {
            abs_path: Some(path_input.into()),
            line_range: fragment,
            column: None,
        });
    }

    let path_with_position = PathWithPosition::parse_str(path_input);
    let abs_path = path_with_position.path;
    if let Some(row) = path_with_position.row {
        let line = row
            .checked_sub(1)
            .context("Line numbers should be 1-based")?;
        Ok(MentionUri::Selection {
            abs_path: Some(abs_path),
            line_range: line..=line,
            column: path_with_position
                .column
                .map(|column| column.saturating_sub(1)),
        })
    } else {
        Ok(MentionUri::File { abs_path })
    }
}

fn normalize_path_mention(
    input: &str,
    path_style: PathStyle,
    decode_escapes: DecodePercentEscapes,
) -> Cow<'_, str> {
    let decoded = match decode_escapes {
        DecodePercentEscapes::Yes => decode_path_escapes(input),
        DecodePercentEscapes::No => Cow::Borrowed(input),
    };
    if !path_style.is_windows() {
        return decoded;
    }
    match to_native_windows_path(&decoded) {
        Some(native) => Cow::Owned(native),
        None => decoded,
    }
}

/// Decodes percent escapes in a path, leaving separator escapes (`%2F`,
/// `%5C`) encoded so decoding can't change which directories the path
/// traverses. Invalid sequences and non-UTF-8 results leave the input
/// unchanged. Returns `Cow::Owned` iff decoding changed the input
/// (`parse_hyperlink_literal` relies on this).
pub fn decode_path_escapes(input: &str) -> Cow<'_, str> {
    fn hex_digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    if !input.contains('%') {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(high) = bytes.get(index + 1).copied().and_then(hex_digit)
            && let Some(low) = bytes.get(index + 2).copied().and_then(hex_digit)
        {
            let byte = (high << 4) | low;
            if byte != b'/' && byte != b'\\' {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    if decoded == bytes {
        return Cow::Borrowed(input);
    }
    match String::from_utf8(decoded) {
        Ok(decoded) => Cow::Owned(decoded),
        Err(_) => Cow::Borrowed(input),
    }
}

/// Converts Windows-compatible path spellings into a native Windows path,
/// normalizing separators to backslashes and drive letters to uppercase so
/// parsed paths compare equal to worktree paths. Returns `None` when the
/// input needs no changes.
fn to_native_windows_path(path: &str) -> Option<String> {
    fn join_drive(drive: char, rest: &str) -> String {
        format!(
            "{}:\\{}",
            drive.to_ascii_uppercase(),
            rest.replace('/', "\\")
        )
    }

    if let Some(rest) = path.strip_prefix('/') {
        // URL-style path with a leading slash before the drive: `/C:/foo`.
        let mut chars = rest.chars();
        if let (Some(drive), Some(':'), Some('/' | '\\')) =
            (chars.next(), chars.next(), chars.next())
            && drive.is_ascii_alphabetic()
        {
            return Some(join_drive(drive, chars.as_str()));
        }

        // MSYS/Git Bash style: `/c/foo`. Lowercase-only, since that's what
        // those shells emit and uppercase risks misreading real directories.
        let mut chars = rest.chars();
        if let (Some(drive), Some('/' | '\\')) = (chars.next(), chars.next())
            && drive.is_ascii_lowercase()
        {
            return Some(join_drive(drive, chars.as_str()));
        }
    }

    // A native path with a drive prefix: uppercase the drive and normalize
    // separators, e.g. `c:/foo` or `c:\foo`.
    let mut chars = path.chars();
    if let (Some(drive), Some(':')) = (chars.next(), chars.next())
        && drive.is_ascii_alphabetic()
    {
        if drive.is_ascii_uppercase() && !path.contains('/') {
            return None;
        }
        return Some(format!(
            "{}:{}",
            drive.to_ascii_uppercase(),
            chars.as_str().replace('/', "\\")
        ));
    }

    if path.contains('/') {
        return Some(path.replace('/', "\\"));
    }

    None
}

fn default_include_errors() -> bool {
    true
}

/// Placeholder rule `id` for legacy mentions missing one, shaped so older Zed
/// versions can still deserialize it as a `prompt_store::PromptId`.
fn default_deprecated_rule_id() -> serde_json::Value {
    serde_json::json!({ "User": { "uuid": "00000000-0000-0000-0000-000000000000" } })
}

fn query_param(url: &Url, name: &'static str) -> Option<String> {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.to_string()))
}

fn single_query_param(url: &Url, name: &'static str) -> Result<Option<String>> {
    let pairs = url.query_pairs().collect::<Vec<_>>();
    match pairs.as_slice() {
        [] => Ok(None),
        [(k, v)] => {
            if k != name {
                bail!("invalid query parameter")
            }

            Ok(Some(v.to_string()))
        }
        _ => bail!("too many query pairs"),
    }
}

pub fn selection_name(path: Option<&Path>, line_range: &RangeInclusive<u32>) -> String {
    format!(
        "{} ({}:{})",
        path.and_then(|path| path.file_name())
            .unwrap_or("Untitled".as_ref())
            .display(),
        *line_range.start() + 1,
        *line_range.end() + 1
    )
}

/// Formats a 0-based, inclusive line range as a 1-based path suffix: `:5` for a
/// single line or `:5-9` for a span. Used for `path:line` mentions in text.
pub fn line_range_suffix(line_range: &RangeInclusive<u32>) -> String {
    let start = *line_range.start() + 1;
    let end = *line_range.end() + 1;
    if start == end {
        format!(":{start}")
    } else {
        format!(":{start}-{end}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zed's `util::path!` / `util::uri!`, which are not in `reference/`.
    ///
    /// They rewrite a Unix test literal into its Windows spelling when the
    /// tests are compiled for Windows, so one assertion covers both hosts.
    #[cfg(not(windows))]
    macro_rules! path {
        ($literal:expr) => {
            $literal
        };
    }
    #[cfg(windows)]
    macro_rules! path {
        ($literal:expr) => {
            concat!("C:", $literal).replace('/', "\\").as_str()
        };
    }
    #[cfg(not(windows))]
    macro_rules! uri {
        ($literal:expr) => {
            $literal
        };
    }
    #[cfg(windows)]
    macro_rules! uri {
        ($literal:expr) => {
            concat!("file:///C:", $literal.trim_start_matches("file://"))
        };
    }

    #[test]
    fn a_file_uri_parses_to_a_file_mention() {
        let file_uri = uri!("file:///path/to/file.rs");
        let parsed = MentionUri::parse(file_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, Path::new(path!("/path/to/file.rs")));
            }
            _ => panic!("Expected File variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), file_uri);
    }

    #[test]
    fn a_trailing_slash_means_a_directory() {
        let file_uri = uri!("file:///path/to/dir/");
        let parsed = MentionUri::parse(file_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Directory { abs_path } => {
                assert_eq!(abs_path, Path::new(path!("/path/to/dir/")));
            }
            _ => panic!("Expected Directory variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), file_uri);
    }

    #[test]
    fn file_uris_use_native_separators_on_windows() {
        let parsed = MentionUri::parse("file:///C:/path/to/file.rs", PathStyle::Windows).unwrap();
        match parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, PathBuf::from("C:\\path\\to\\file.rs"));
            }
            other => panic!("Expected File variant, got {other:?}"),
        }

        let parsed = MentionUri::parse("file:///C:/path/to/dir/", PathStyle::Windows).unwrap();
        match parsed {
            MentionUri::Directory { abs_path } => {
                assert_eq!(abs_path, PathBuf::from("C:\\path\\to\\dir\\"));
            }
            other => panic!("Expected Directory variant, got {other:?}"),
        }

        let parsed = MentionUri::parse(
            "file:///C:/path/to/file.rs?symbol=MySymbol#L10:20",
            PathStyle::Windows,
        )
        .unwrap();
        match parsed {
            MentionUri::Symbol { abs_path, .. } => {
                assert_eq!(abs_path, PathBuf::from("C:\\path\\to\\file.rs"));
            }
            other => panic!("Expected Symbol variant, got {other:?}"),
        }

        let parsed =
            MentionUri::parse("file:///C:/path/to/file.rs#L5:15", PathStyle::Windows).unwrap();
        match parsed {
            MentionUri::Selection {
                abs_path: Some(abs_path),
                ..
            } => {
                assert_eq!(abs_path, PathBuf::from("C:\\path\\to\\file.rs"));
            }
            other => panic!("Expected Selection variant, got {other:?}"),
        }
    }

    #[test]
    fn a_file_uri_with_spaces_round_trips() {
        let parsed =
            MentionUri::parse("file:///C:/path%20with%20space/file.rs", PathStyle::Windows)
                .unwrap();
        match parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, PathBuf::from("C:\\path with space\\file.rs"));
            }
            other => panic!("Expected File variant, got {other:?}"),
        }
        assert_eq!(
            MentionUri::File {
                abs_path: PathBuf::from("C:\\path with space\\file.rs")
            }
            .to_uri()
            .to_string(),
            "file:///C:/path%20with%20space/file.rs"
        );
    }

    #[test]
    fn a_windows_drive_path_with_a_leading_slash_and_a_line_parses() {
        let parsed = MentionUri::parse_hyperlink(
            "/C:/Projects/Example Workspace/Cargo.toml:2",
            PathStyle::Windows,
        )
        .unwrap();
        match parsed {
            MentionUri::Selection {
                abs_path: Some(abs_path),
                line_range,
                ..
            } => {
                assert_eq!(
                    abs_path,
                    PathBuf::from("C:\\Projects\\Example Workspace\\Cargo.toml")
                );
                assert_eq!(line_range, 1..=1);
            }
            other => panic!("Expected Selection variant, got {other:?}"),
        }
    }

    #[test]
    fn a_windows_path_with_percent_escaped_spaces_and_a_line_parses() {
        let parsed = MentionUri::parse_hyperlink(
            "C:\\Projects\\Example%20Workspace\\path\\to\\filename.ext:42",
            PathStyle::Windows,
        )
        .unwrap();
        match parsed {
            MentionUri::Selection {
                abs_path: Some(abs_path),
                line_range,
                ..
            } => {
                assert_eq!(
                    abs_path,
                    PathBuf::from("C:\\Projects\\Example Workspace\\path\\to\\filename.ext")
                );
                assert_eq!(line_range, 41..=41);
            }
            other => panic!("Expected Selection variant, got {other:?}"),
        }
    }

    #[test]
    fn a_windows_compat_path_with_spaces_parses() {
        let parsed = MentionUri::parse_hyperlink(
            "/c/Projects/Example Workspace/AGENTS.md",
            PathStyle::Windows,
        )
        .unwrap();
        match parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(
                    abs_path,
                    PathBuf::from("C:\\Projects\\Example Workspace\\AGENTS.md")
                );
            }
            other => panic!("Expected File variant, got {other:?}"),
        }
    }

    #[test]
    fn a_windows_drive_path_with_a_leading_slash_and_a_fragment_line_parses() {
        let parsed =
            MentionUri::parse_hyperlink("/C:/Projects/Cargo.toml#L4", PathStyle::Windows).unwrap();
        match parsed {
            MentionUri::Selection {
                abs_path: Some(abs_path),
                line_range,
                ..
            } => {
                assert_eq!(abs_path, PathBuf::from("C:\\Projects\\Cargo.toml"));
                assert_eq!(line_range, 3..=3);
            }
            other => panic!("Expected Selection variant, got {other:?}"),
        }
    }

    #[test]
    fn a_windows_drive_path_with_a_leading_slash_round_trips() {
        let parsed = MentionUri::parse_hyperlink("/C:/dir/file.rs", PathStyle::Windows).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("C:\\dir\\file.rs")
            }
        );
        let uri = parsed.to_uri().to_string();
        assert_eq!(uri, "file:///C:/dir/file.rs");
        assert_eq!(MentionUri::parse(&uri, PathStyle::Windows).unwrap(), parsed);
    }

    #[test]
    fn a_windows_unc_path_parses() {
        let parsed =
            MentionUri::parse_hyperlink("//server/share/dir/file.rs", PathStyle::Windows).unwrap();
        match parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, PathBuf::from("\\\\server\\share\\dir\\file.rs"));
            }
            other => panic!("Expected File variant, got {other:?}"),
        }
    }

    #[test]
    fn windows_drive_letters_are_uppercased() {
        for input in [
            "file:///c:/foo/bar.rs",
            "/c:/foo/bar.rs",
            "/c/foo/bar.rs",
            "c:\\foo\\bar.rs",
            "c:/foo/bar.rs",
        ] {
            let parsed = MentionUri::parse_hyperlink(input, PathStyle::Windows).unwrap();
            assert_eq!(
                parsed,
                MentionUri::File {
                    abs_path: PathBuf::from("C:\\foo\\bar.rs")
                },
                "input: {input}"
            );
        }
    }

    #[test]
    fn msys_style_paths_require_a_lowercase_drive() {
        // Uppercase `/C/foo` is more likely a real directory than a drive.
        let parsed = MentionUri::parse_hyperlink("/C/Users/readme.md", PathStyle::Windows).unwrap();
        match parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, PathBuf::from("\\C\\Users\\readme.md"));
            }
            other => panic!("Expected File variant, got {other:?}"),
        }
    }

    #[test]
    fn posix_paths_are_not_rewritten_as_windows_drives() {
        let parsed = MentionUri::parse_hyperlink("/c/Projects/AGENTS.md", PathStyle::Unix).unwrap();
        match parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, PathBuf::from("/c/Projects/AGENTS.md"));
            }
            other => panic!("Expected File variant, got {other:?}"),
        }
    }

    #[test]
    fn decoding_a_hyperlink_path_cannot_introduce_a_traversal() {
        let parsed = MentionUri::parse_hyperlink("/tmp/a%20b.rs", PathStyle::Unix).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("/tmp/a b.rs")
            }
        );

        // Invalid escape sequences pass through unchanged.
        let parsed =
            MentionUri::parse_hyperlink("C:\\dir\\100%_done.txt", PathStyle::Windows).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("C:\\dir\\100%_done.txt")
            }
        );

        // Separator escapes stay encoded (no introduced path traversal).
        let parsed = MentionUri::parse_hyperlink("/tmp/a%2Fb.rs", PathStyle::Unix).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("/tmp/a%2Fb.rs")
            }
        );
        let parsed = MentionUri::parse_hyperlink("/tmp/..%2F..%2Fsecret", PathStyle::Unix).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("/tmp/..%2F..%2Fsecret")
            }
        );
    }

    #[test]
    fn parse_keeps_bare_path_targets_verbatim() {
        let parsed = MentionUri::parse("/tmp/a%20b.rs", PathStyle::Unix).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("/tmp/a%20b.rs")
            }
        );

        let parsed = MentionUri::parse("/c/Projects/AGENTS.md", PathStyle::Windows).unwrap();
        assert_eq!(
            parsed,
            MentionUri::File {
                abs_path: PathBuf::from("/c/Projects/AGENTS.md")
            }
        );
    }

    #[test]
    fn a_literal_hyperlink_keeps_its_percent_escapes() {
        let literal =
            MentionUri::parse_hyperlink_literal("/tmp/a%20b.rs", PathStyle::Unix).unwrap();
        assert_eq!(
            literal,
            MentionUri::File {
                abs_path: PathBuf::from("/tmp/a%20b.rs")
            }
        );

        // Line suffixes still parse.
        let literal =
            MentionUri::parse_hyperlink_literal("/tmp/a%20b.rs:42", PathStyle::Unix).unwrap();
        assert_eq!(
            literal,
            MentionUri::Selection {
                abs_path: Some(PathBuf::from("/tmp/a%20b.rs")),
                line_range: 41..=41,
                column: None,
            }
        );

        // Windows normalization still applies.
        let literal =
            MentionUri::parse_hyperlink_literal("/C:/dir/a%20b.rs", PathStyle::Windows).unwrap();
        assert_eq!(
            literal,
            MentionUri::File {
                abs_path: PathBuf::from("C:\\dir\\a%20b.rs")
            }
        );
    }

    #[test]
    fn a_literal_hyperlink_is_none_when_it_would_not_differ() {
        // No percent escapes: identical to `parse_hyperlink`.
        assert_eq!(
            MentionUri::parse_hyperlink_literal("/tmp/a b.rs", PathStyle::Unix),
            None
        );
        // Invalid escape sequences are also left alone by `parse_hyperlink`.
        assert_eq!(
            MentionUri::parse_hyperlink_literal("/tmp/100%_done.txt", PathStyle::Unix),
            None
        );
        // Separator escapes are never decoded, so they're not ambiguous.
        assert_eq!(
            MentionUri::parse_hyperlink_literal("/tmp/a%2Fb.rs", PathStyle::Unix),
            None
        );
        // URLs are spec-encoded, not ambiguous.
        assert_eq!(
            MentionUri::parse_hyperlink_literal("file:///tmp/a%20b.rs", PathStyle::Unix),
            None
        );
        // Relative paths are not bare-path mentions.
        assert_eq!(
            MentionUri::parse_hyperlink_literal("tmp/a%20b.rs", PathStyle::Unix),
            None
        );
    }

    #[test]
    fn a_directory_uri_keeps_its_trailing_slash() {
        let uri = MentionUri::Directory {
            abs_path: PathBuf::from(path!("/path/to/dir/")),
        };
        let expected = uri!("file:///path/to/dir/");
        assert_eq!(uri.to_uri().to_string(), expected);
    }

    #[test]
    fn a_directory_without_a_trailing_slash_still_round_trips() {
        let uri = MentionUri::Directory {
            abs_path: PathBuf::from(path!("/path/to/dir")),
        };
        let serialized = uri.to_uri().to_string();
        assert!(serialized.ends_with('/'), "directory URI must end with /");
        let parsed = MentionUri::parse(&serialized, PathStyle::local()).unwrap();
        assert!(
            matches!(parsed, MentionUri::Directory { .. }),
            "expected Directory variant, got {parsed:?}"
        );
    }

    #[test]
    fn a_symbol_uri_carries_its_name_and_line_range() {
        let symbol_uri = uri!("file:///path/to/file.rs?symbol=MySymbol#L10:20");
        let parsed = MentionUri::parse(symbol_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Symbol {
                abs_path: path,
                name,
                line_range,
                ..
            } => {
                assert_eq!(path, Path::new(path!("/path/to/file.rs")));
                assert_eq!(name, "MySymbol");
                assert_eq!(line_range.start(), &9);
                assert_eq!(line_range.end(), &19);
            }
            _ => panic!("Expected Symbol variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), symbol_uri);
    }

    #[test]
    fn a_fragment_without_a_symbol_is_a_selection() {
        let selection_uri = uri!("file:///path/to/file.rs#L5:15");
        let parsed = MentionUri::parse(selection_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new(path!("/path/to/file.rs")));
                assert_eq!(line_range.start(), &4);
                assert_eq!(line_range.end(), &14);
            }
            _ => panic!("Expected Selection variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), selection_uri);
    }

    #[test]
    fn a_non_ascii_filename_round_trips() {
        let file_uri = uri!("file:///path/to/%E6%97%A5%E6%9C%AC%E8%AA%9E.txt");
        let parsed = MentionUri::parse(file_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, Path::new(path!("/path/to/日本語.txt")));
            }
            _ => panic!("Expected File variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), file_uri);
    }

    #[test]
    fn an_untitled_buffer_selection_has_no_path() {
        let selection_uri = "zed:///agent/untitled-buffer#L1:10";
        let parsed = MentionUri::parse(selection_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: None,
                line_range,
                ..
            } => {
                assert_eq!(line_range.start(), &0);
                assert_eq!(line_range.end(), &9);
            }
            _ => panic!("Expected Selection variant without path"),
        }
        assert_eq!(parsed.to_uri().to_string(), selection_uri);
    }

    #[test]
    fn a_thread_uri_carries_its_session_id_and_name() {
        let thread_uri = "zed:///agent/thread/session123?name=Thread+name";
        let parsed = MentionUri::parse(thread_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Thread {
                id: thread_id,
                name,
            } => {
                assert_eq!(thread_id.to_string(), "session123");
                assert_eq!(name, "Thread name");
            }
            _ => panic!("Expected Thread variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), thread_uri);
    }

    #[test]
    fn a_legacy_rule_uri_still_parses() {
        let rule_uri = "zed:///agent/rule/d8694ff2-90d5-4b6f-be33-33c1763acd52?name=Some+rule";
        let parsed = MentionUri::parse(rule_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Rule { name, .. } => assert_eq!(name, "Some rule"),
            _ => panic!("Expected Rule variant"),
        }
        // The id round-trips through the URI.
        assert_eq!(parsed.to_uri().to_string(), rule_uri);
    }

    #[test]
    fn a_legacy_rule_mention_preserves_its_id() {
        // The `id` older Zed versions require must survive a load + save.
        let json = r#"{"Rule":{"id":{"User":{"uuid":"d8694ff2-90d5-4b6f-be33-33c1763acd52"}},"name":"Some rule"}}"#;
        let parsed: MentionUri = serde_json::from_str(json).unwrap();
        match &parsed {
            MentionUri::Rule { name, .. } => assert_eq!(name, "Some rule"),
            _ => panic!("Expected Rule variant"),
        }
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(
            reserialized["Rule"]["id"]["User"]["uuid"],
            "d8694ff2-90d5-4b6f-be33-33c1763acd52"
        );
    }

    #[test]
    fn a_legacy_rule_mention_without_an_id_gets_a_placeholder() {
        // A mention missing its id still serializes a valid id for older versions.
        let json = r#"{"Rule":{"name":"Some rule"}}"#;
        let parsed: MentionUri = serde_json::from_str(json).unwrap();
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert!(reserialized["Rule"]["id"]["User"]["uuid"].is_string());
    }

    #[test]
    fn a_skill_uri_round_trips() {
        let skill_uri = MentionUri::Skill {
            name: "rust-best-practices".to_string(),
            source: "my-personal-project".to_string(),
            skill_file_path: PathBuf::from(path!("/path/to/skills/rust-best-practices/SKILL.md")),
        };

        let serialized = skill_uri.to_uri().to_string();
        let parsed = MentionUri::parse(&serialized, PathStyle::local()).unwrap();

        assert_eq!(parsed, skill_uri);
    }

    #[test]
    fn an_http_url_is_a_fetch_mention() {
        let http_uri = "http://example.com/path?query=value#fragment";
        let parsed = MentionUri::parse(http_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Fetch { url } => {
                assert_eq!(url.to_string(), http_uri);
            }
            _ => panic!("Expected Fetch variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), http_uri);
    }

    #[test]
    fn an_https_url_is_a_fetch_mention() {
        let https_uri = "https://example.com/api/endpoint";
        let parsed = MentionUri::parse(https_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Fetch { url } => {
                assert_eq!(url.to_string(), https_uri);
            }
            _ => panic!("Expected Fetch variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), https_uri);
    }

    #[test]
    fn a_diagnostics_uri_defaults_to_errors_only() {
        let uri = "zed:///agent/diagnostics?include_warnings=true";
        let parsed = MentionUri::parse(uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Diagnostics {
                include_errors,
                include_warnings,
            } => {
                assert!(include_errors);
                assert!(include_warnings);
            }
            _ => panic!("Expected Diagnostics variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), uri);
    }

    #[test]
    fn a_diagnostics_uri_can_ask_for_warnings_only() {
        let uri = "zed:///agent/diagnostics?include_warnings=true&include_errors=false";
        let parsed = MentionUri::parse(uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Diagnostics {
                include_errors,
                include_warnings,
            } => {
                assert!(!include_errors);
                assert!(include_warnings);
            }
            _ => panic!("Expected Diagnostics variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), uri);
    }

    #[test]
    fn an_unrecognized_scheme_is_refused() {
        assert!(MentionUri::parse("ftp://example.com", PathStyle::local()).is_err());
        assert!(MentionUri::parse("ssh://example.com", PathStyle::local()).is_err());
        assert!(MentionUri::parse("unknown://example.com", PathStyle::local()).is_err());
    }

    #[test]
    fn an_unrecognized_zed_path_is_refused() {
        assert!(MentionUri::parse("zed:///invalid/path", PathStyle::local()).is_err());
        assert!(MentionUri::parse("zed:///agent/unknown/test", PathStyle::local()).is_err());
    }

    #[test]
    fn a_bare_absolute_path_is_a_file_mention() {
        let file_path = path!("/path/to/file.rs");
        let parsed = MentionUri::parse(file_path, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, Path::new(file_path));
            }
            _ => panic!("Expected File variant"),
        }
    }

    #[test]
    fn a_bare_path_with_a_row_is_a_selection() {
        let file_path = "/path/to/file.rs:42";
        let parsed = MentionUri::parse(file_path, PathStyle::Unix).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new("/path/to/file.rs"));
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_bare_path_with_a_row_and_a_column_round_trips() {
        let file_path = "/path/to/file.rs:42:5";
        let parsed = MentionUri::parse(file_path, PathStyle::Unix).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                column,
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new("/path/to/file.rs"));
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
                assert_eq!(column, &Some(4));

                let parsed_again = MentionUri::parse(parsed.to_uri().as_ref(), PathStyle::Unix)
                    .expect("selection URI with column should parse");
                assert_eq!(parsed_again, parsed.clone());
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_bare_path_with_a_fragment_line_is_a_selection() {
        let file_path = "/path/to/file.rs#L42";
        let parsed = MentionUri::parse(file_path, PathStyle::Unix).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new("/path/to/file.rs"));
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_bare_windows_path_is_a_file_mention() {
        let file_path = "C:\\Users\\zed\\project\\main.rs";
        let parsed = MentionUri::parse(file_path, PathStyle::Windows).unwrap();
        match &parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, Path::new("C:\\Users\\zed\\project\\main.rs"));
            }
            _ => panic!("Expected File variant"),
        }
    }

    #[test]
    fn a_bare_windows_path_with_a_row_is_a_selection() {
        let file_path = "C:\\Users\\zed\\project\\main.rs:42";
        let parsed = MentionUri::parse(file_path, PathStyle::Windows).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(
                    path.as_ref().unwrap(),
                    Path::new("C:\\Users\\zed\\project\\main.rs")
                );
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_bare_windows_path_with_a_fragment_line_is_a_selection() {
        let file_path = "C:\\Users\\zed\\project\\main.rs#L42";
        let parsed = MentionUri::parse(file_path, PathStyle::Windows).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(
                    path.as_ref().unwrap(),
                    Path::new("C:\\Users\\zed\\project\\main.rs")
                );
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn the_backticks_an_agent_adds_are_stripped() {
        let file_path = "`/path/to/file.rs`";
        let parsed = MentionUri::parse(file_path, PathStyle::Unix).unwrap();
        match &parsed {
            MentionUri::File { abs_path } => {
                assert_eq!(abs_path, Path::new("/path/to/file.rs"));
            }
            _ => panic!("Expected File variant"),
        }
    }

    #[test]
    fn a_backticked_path_with_a_fragment_line_is_a_selection() {
        let file_path = "`/path/to/file.rs#L42`";
        let parsed = MentionUri::parse(file_path, PathStyle::Unix).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new("/path/to/file.rs"));
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_backticked_windows_path_with_a_fragment_line_is_a_selection() {
        let file_path = "`C:\\Users\\zed\\project\\main.rs#L42`";
        let parsed = MentionUri::parse(file_path, PathStyle::Windows).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(
                    path.as_ref().unwrap(),
                    Path::new("C:\\Users\\zed\\project\\main.rs")
                );
                assert_eq!(line_range.start(), &41);
                assert_eq!(line_range.end(), &41);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_single_line_number_is_a_one_line_range() {
        // https://github.com/zed-industries/zed/issues/46114
        let uri = uri!("file:///path/to/file.rs#L1872");
        let parsed = MentionUri::parse(uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new(path!("/path/to/file.rs")));
                assert_eq!(line_range.start(), &1871);
                assert_eq!(line_range.end(), &1871);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_dash_separated_line_range_parses() {
        let uri = uri!("file:///path/to/file.rs#L10-20");
        let parsed = MentionUri::parse(uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new(path!("/path/to/file.rs")));
                assert_eq!(line_range.start(), &9);
                assert_eq!(line_range.end(), &19);
            }
            _ => panic!("Expected Selection variant"),
        }

        // Also test L10-L20 format
        let uri = uri!("file:///path/to/file.rs#L10-L20");
        let parsed = MentionUri::parse(uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => {
                assert_eq!(path.as_ref().unwrap(), Path::new(path!("/path/to/file.rs")));
                assert_eq!(line_range.start(), &9);
                assert_eq!(line_range.end(), &19);
            }
            _ => panic!("Expected Selection variant"),
        }
    }

    #[test]
    fn a_terminal_selection_names_its_line_count() {
        let terminal_uri = "zed:///agent/terminal-selection?lines=42";
        let parsed = MentionUri::parse(terminal_uri, PathStyle::local()).unwrap();
        match &parsed {
            MentionUri::TerminalSelection { line_count } => {
                assert_eq!(*line_count, 42);
            }
            _ => panic!("Expected TerminalSelection variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), terminal_uri);
        assert_eq!(parsed.name(), "Terminal (42 lines)");

        // Test single line
        let single_line_uri = "zed:///agent/terminal-selection?lines=1";
        let parsed_single = MentionUri::parse(single_line_uri, PathStyle::local()).unwrap();
        assert_eq!(parsed_single.name(), "Terminal (1 line)");
    }

    #[test]
    fn a_mention_link_is_markdown() {
        let mention = MentionUri::File {
            abs_path: PathBuf::from(path!("/path/to/stats.js")),
        };
        assert_eq!(
            mention.as_link().to_string(),
            format!("[@stats.js]({})", uri!("file:///path/to/stats.js"))
        );
    }

    #[test]
    fn every_variant_has_a_name_to_show() {
        // A chip can never fall back to rendering `[resource_link]`, so this
        // has to be total.
        let cases = [
            (
                MentionUri::File {
                    abs_path: PathBuf::from(path!("/a/stats.js")),
                },
                "stats.js",
            ),
            (
                MentionUri::Directory {
                    abs_path: PathBuf::from(path!("/a/src")),
                },
                "src",
            ),
            (
                MentionUri::PastedImage {
                    name: "Image".into(),
                },
                "Image",
            ),
            (
                MentionUri::Diagnostics {
                    include_errors: true,
                    include_warnings: false,
                },
                "Diagnostics",
            ),
            (
                MentionUri::GitDiff {
                    base_ref: "main".into(),
                },
                "Branch Diff (main)",
            ),
            (
                MentionUri::MergeConflict {
                    file_path: "/a/stats.js".into(),
                },
                "Merge Conflict (stats.js)",
            ),
            (
                MentionUri::TerminalSelection { line_count: 3 },
                "Terminal (3 lines)",
            ),
            (
                MentionUri::Selection {
                    abs_path: Some(PathBuf::from(path!("/a/stats.js"))),
                    line_range: 4..=14,
                    column: None,
                },
                "stats.js (5:15)",
            ),
        ];

        for (mention, expected) in cases {
            assert_eq!(mention.name(), expected, "{mention:?}");
        }
    }

    #[test]
    fn a_line_range_suffix_is_one_based() {
        assert_eq!(line_range_suffix(&(4..=4)), ":5");
        assert_eq!(line_range_suffix(&(4..=8)), ":5-9");
    }
}
