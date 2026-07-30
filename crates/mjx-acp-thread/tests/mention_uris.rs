//! Parses the same mention URIs the TypeScript port parses.
//!
//! `fixtures/mention-uris.json` is read by this test and by
//! `web/src/acp/mention.test.ts`, the way `session-updates.jsonl` is read by
//! both thread models. Two ports of the same parser are only worth having if
//! something notices when they disagree — and where they will disagree is
//! percent-encoding, which no amount of care in either language prevents.
//!
//! Every case is Unix-styled. Windows spellings stay in each side's own unit
//! tests; the fixture cannot express two path styles without becoming a second
//! dialect.

use std::path::PathBuf;

use mjx_acp_thread::mention::MentionUri;
use mjx_acp_thread::paths::PathStyle;
use serde::Deserialize;
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("../../../fixtures/mention-uris.json");

#[derive(Deserialize)]
struct Case {
    name: String,
    input: String,
    mode: String,
    expect: Value,
    #[serde(default)]
    uri: Option<String>,
}

fn cases() -> Vec<Case> {
    serde_json::from_str(FIXTURE).expect("the mention fixture must parse")
}

/// The comparable shape of a mention. Only the keys a case names are checked,
/// so a case says exactly what it is about.
fn shape(mention: &MentionUri) -> Value {
    let mut shape = json!({
        "variant": variant_name(mention),
        "label": mention.name(),
    });
    let object = shape.as_object_mut().unwrap();

    match mention {
        MentionUri::File { abs_path } | MentionUri::Directory { abs_path } => {
            object.insert("absPath".into(), path_value(abs_path));
        }
        MentionUri::Symbol {
            abs_path,
            line_range,
            ..
        } => {
            object.insert("absPath".into(), path_value(abs_path));
            object.insert("lineRange".into(), json!([line_range.start(), line_range.end()]));
        }
        MentionUri::Selection {
            abs_path,
            line_range,
            column,
        } => {
            object.insert(
                "absPath".into(),
                abs_path.as_ref().map_or(Value::Null, |p| path_value(p)),
            );
            object.insert("lineRange".into(), json!([line_range.start(), line_range.end()]));
            object.insert("column".into(), json!(column));
        }
        MentionUri::Thread { id, .. } => {
            object.insert("id".into(), json!(id.to_string()));
        }
        MentionUri::Rule { id, .. } => {
            let uuid = id
                .get("User")
                .and_then(|user| user.get("uuid"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            object.insert("id".into(), json!(uuid));
        }
        MentionUri::Diagnostics {
            include_errors,
            include_warnings,
        } => {
            object.insert("includeErrors".into(), json!(include_errors));
            object.insert("includeWarnings".into(), json!(include_warnings));
        }
        MentionUri::Fetch { url } => {
            object.insert("url".into(), json!(url.to_string()));
        }
        MentionUri::TerminalSelection { line_count } => {
            object.insert("lineCount".into(), json!(line_count));
        }
        MentionUri::GitDiff { base_ref } => {
            object.insert("baseRef".into(), json!(base_ref));
        }
        MentionUri::MergeConflict { file_path } => {
            object.insert("filePath".into(), json!(file_path));
        }
        MentionUri::Skill {
            source,
            skill_file_path,
            ..
        } => {
            object.insert("source".into(), json!(source));
            object.insert("absPath".into(), path_value(skill_file_path));
        }
        MentionUri::PastedImage { .. } => {}
    }

    shape
}

fn path_value(path: &PathBuf) -> Value {
    json!(path.to_string_lossy())
}

fn variant_name(mention: &MentionUri) -> &'static str {
    match mention {
        MentionUri::File { .. } => "file",
        MentionUri::PastedImage { .. } => "pastedImage",
        MentionUri::Directory { .. } => "directory",
        MentionUri::Symbol { .. } => "symbol",
        MentionUri::Thread { .. } => "thread",
        MentionUri::Rule { .. } => "rule",
        MentionUri::Diagnostics { .. } => "diagnostics",
        MentionUri::Selection { .. } => "selection",
        MentionUri::Fetch { .. } => "fetch",
        MentionUri::TerminalSelection { .. } => "terminalSelection",
        MentionUri::GitDiff { .. } => "gitDiff",
        MentionUri::MergeConflict { .. } => "mergeConflict",
        MentionUri::Skill { .. } => "skill",
    }
}

#[test]
fn every_case_is_read() {
    // Asserted on both sides, so a case added to the fixture and read by only
    // one of the two ports fails rather than passing quietly.
    assert_eq!(cases().len(), 49);
}

#[test]
fn the_shared_cases_parse_the_same_way_on_both_sides() {
    for case in cases() {
        let style = PathStyle::Unix;
        let parsed = match case.mode.as_str() {
            "parse" => MentionUri::parse(&case.input, style).ok(),
            "parseHyperlink" => MentionUri::parse_hyperlink(&case.input, style).ok(),
            "parseHyperlinkLiteral" => MentionUri::parse_hyperlink_literal(&case.input, style),
            other => panic!("{}: unknown mode {other}", case.name),
        };

        if case.expect.get("error") == Some(&json!(true))
            || case.expect.get("none") == Some(&json!(true))
        {
            assert!(
                parsed.is_none(),
                "{}: expected nothing, got {parsed:?}",
                case.name
            );
            continue;
        }

        let parsed = parsed.unwrap_or_else(|| panic!("{}: did not parse", case.name));
        let shape = shape(&parsed);
        for (key, expected) in case.expect.as_object().unwrap() {
            assert_eq!(
                shape.get(key),
                Some(expected),
                "{}: {key} — whole shape was {shape}",
                case.name
            );
        }

        if let Some(uri) = &case.uri {
            assert_eq!(
                &parsed.to_uri().to_string(),
                uri,
                "{}: serialized differently",
                case.name
            );
        }
    }
}
