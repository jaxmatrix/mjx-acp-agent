//! Folds a recorded turn and pins the result.
//!
//! `fixtures/session-updates.jsonl` is a real `session/update` stream, captured
//! from a live run by `web/scripts/capture-fixture.mjs`. The TypeScript model
//! in `web/src/acp/thread.test.ts` folds the same file and asserts the same
//! numbers, so the two implementations cannot drift apart silently.
//!
//! Regenerate with:
//!     node web/scripts/capture-fixture.mjs

use mjx_acp_core::acp;
use mjx_acp_thread::{ChunkKind, Entry, Thread};

const FIXTURE: &str = include_str!("../../../fixtures/session-updates.jsonl");

fn folded() -> Thread {
    let mut thread = Thread::new();
    // The capture starts after the prompt was sent, so mirror what the client
    // does: the user's message is on screen before the agent says anything.
    thread.push_user_prompt(vec![acp::ContentBlock::from(
        "fix the median bug in stats.js",
    )]);

    for (line_number, line) in FIXTURE.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let update: acp::SessionUpdate = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!(
                "fixture line {} is not a SessionUpdate: {e}",
                line_number + 1
            )
        });
        thread.apply(&update);
    }
    thread
}

#[test]
fn every_recorded_update_parses() {
    // The fixture is real agent output. If the schema types can't read it back,
    // one of the two is wrong.
    assert_eq!(FIXTURE.lines().filter(|l| !l.trim().is_empty()).count(), 74);
    let _ = folded();
}

#[test]
fn the_recorded_turn_folds_to_a_stable_shape() {
    let thread = folded();

    // These numbers are the contract with the TypeScript model. If a change to
    // the folding rules moves one, the same number must move on both sides.
    assert_eq!(thread.entries.len(), 8, "entry count");

    let kinds: Vec<&str> = thread
        .entries
        .iter()
        .map(|entry| match entry {
            Entry::User(_) => "user",
            Entry::Assistant(_) => "assistant",
            Entry::ToolCall(_) => "toolCall",
            Entry::Elicitation(_) => "elicitation",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "user",
            "assistant",
            "toolCall",
            "assistant",
            "toolCall",
            "assistant",
            "toolCall",
            "assistant",
        ]
    );

    // Hundreds of streamed chunks collapse into a handful of runs.
    let Entry::Assistant(first) = &thread.entries[1] else {
        panic!("expected an assistant message");
    };
    assert_eq!(first.chunks.len(), 2, "a thought run then a prose run");
    assert_eq!(first.chunks[0].kind, ChunkKind::Thought);
    assert_eq!(first.chunks[1].kind, ChunkKind::Message);
    assert!(first.chunks[0].text().starts_with("Let me look at"));
    assert!(first.chunks[1].text().contains("stats.js"));

    // Three tool calls, all finished.
    let calls: Vec<_> = thread
        .entries
        .iter()
        .filter_map(Entry::as_tool_call)
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["call_read", "call_edit", "call_test"]
    );
    for call in &calls {
        assert_eq!(
            call.status,
            acp::ToolCallStatus::Completed,
            "{} did not finish",
            call.id
        );
    }

    // The edit carries a real diff.
    let edit = thread.tool_call("call_edit").unwrap();
    let Some(acp::ToolCallContent::Diff(diff)) = edit.content.first() else {
        panic!("the edit tool call has no diff");
    };
    assert!(
        diff.old_text
            .as_deref()
            .is_some_and(|t| t.contains("return sorted[mid];"))
    );
    assert!(diff.new_text.contains("/ 2"));

    // The plan finished fully ticked off.
    assert_eq!(thread.plan.len(), 3);
    assert!(
        thread
            .plan
            .iter()
            .all(|entry| entry.status == acp::PlanEntryStatus::Completed)
    );

    let usage = thread.usage.as_ref().expect("no usage reported");
    assert_eq!(usage.used, 4_812);
    assert_eq!(usage.size, 200_000);

    assert_eq!(thread.available_commands.len(), 2);

    // The agent moved itself onto a more capable model partway through, and the
    // fold followed it.
    assert_eq!(thread.config_options.len(), 3);
    let acp::SessionConfigKind::Select(model) = &thread.config_options[0].kind else {
        panic!("the model option is not a select");
    };
    assert_eq!(model.current_value.0.as_ref(), "mock-opus");
}

#[test]
fn the_thread_survives_a_replay_round_trip() {
    // Replay serializes the thread and the browser reads it back. Anything
    // lost here is a reload that shows less than the live page did.
    let thread = folded();
    let json = serde_json::to_string(&thread).unwrap();
    let restored: Thread = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.entries.len(), thread.entries.len());
    assert_eq!(restored.plan.len(), thread.plan.len());
    assert_eq!(
        restored.usage.as_ref().map(|u| u.used),
        thread.usage.as_ref().map(|u| u.used)
    );

    let restored_edit = restored.tool_call("call_edit").unwrap();
    assert_eq!(
        restored_edit.content.len(),
        1,
        "the diff was lost in replay"
    );

    let Entry::Assistant(first) = &restored.entries[1] else {
        panic!("expected an assistant message");
    };
    assert_eq!(first.chunks.len(), 2);
}

#[test]
fn the_serialized_thread_matches_the_checked_in_fixture() {
    // `fixtures/session-thread.json` is this thread as `_mjx/session/replay`
    // puts it on the wire, and the browser reads the same file back through its
    // replay adapter. Pinning it here is what stops the two models drifting in
    // their *serialization* rather than in their folding: a renamed field or a
    // newly-omitted empty collection is invisible to every other test and shows
    // up in a real reload as a blank page.
    //
    // Regenerate with:  MJX_UPDATE_FIXTURES=1 cargo test -p mjx-acp-thread
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/session-thread.json");
    let serialized = format!("{}\n", serde_json::to_string_pretty(&folded()).unwrap());

    if std::env::var_os("MJX_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &serialized).expect("could not write the fixture");
        return;
    }

    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        checked_in, serialized,
        "the replay fixture is out of date; regenerate with MJX_UPDATE_FIXTURES=1"
    );
}

/// A thread holding one of every elicitation shape.
///
/// Hand-built rather than captured, because an elicitation is a *request* and
/// `fixtures/session-updates.jsonl` holds only notifications — the recorded turn
/// cannot contain one no matter how the mock is scripted. What can still be
/// pinned is the serialization, which is what the browser has to read back.
fn asked() -> Thread {
    let ask = |params: serde_json::Value| -> acp::CreateElicitationRequest {
        serde_json::from_value(params).expect("the fixture is a valid elicitation")
    };

    let mut thread = Thread::new();
    thread.push_elicitation(
        mjx_acp_core::RequestId::Number(7),
        &ask(serde_json::json!({
            "mode": "form",
            "sessionId": "s1",
            "toolCallId": "call_edit",
            "message": "How should I record this fix?",
            "requestedSchema": {
                "type": "object",
                "title": "Record the fix",
                "properties": {
                    "branch": { "type": "string", "title": "Branch", "default": "fix/median" },
                    "remote": { "type": "string", "title": "Remote", "oneOf": [
                        { "const": "origin", "title": "origin" }
                    ]},
                    "reviewers": { "type": "array", "title": "Reviewers", "maxItems": 2,
                                   "items": { "anyOf": [{ "const": "ana", "title": "Ana" }] } },
                    "attempts": { "type": "integer", "title": "Retries", "minimum": 0 },
                    "timeout": { "type": "number", "title": "Timeout", "minimum": 0.5 },
                    "squash": { "type": "boolean", "title": "Squash", "default": true }
                },
                "required": ["branch", "remote"]
            }
        })),
    );
    thread.settle_elicitation(
        &mjx_acp_core::RequestId::Number(7),
        mjx_acp_thread::ElicitationState::Accepted,
        Some(std::collections::BTreeMap::from([
            ("branch".to_string(), "fix/median".into()),
            (
                "attempts".to_string(),
                acp::ElicitationContentValue::Integer(2),
            ),
            (
                "squash".to_string(),
                acp::ElicitationContentValue::Boolean(true),
            ),
            (
                "reviewers".to_string(),
                acp::ElicitationContentValue::StringArray(vec!["ana".to_string()]),
            ),
        ])),
    );

    // A second one still waiting, in the other mode, so both render paths and
    // both lifecycle ends are in the file.
    thread.push_elicitation(
        mjx_acp_core::RequestId::String("a".into()),
        &ask(serde_json::json!({
            "mode": "url",
            "sessionId": "s1",
            "elicitationId": "el-1",
            "url": "https://agentclientprotocol.com/protocol/elicitation",
            "message": "Read the spec, then come back."
        })),
    );
    thread
}

#[test]
fn the_serialized_elicitations_match_the_checked_in_fixture() {
    // Same job as the test above, for the one entry kind the recorded turn can
    // never contain. The browser reads this file back through its replay
    // adapter, so a renamed field or a mode spelled differently fails there
    // rather than in a real reload.
    //
    // Regenerate with:  MJX_UPDATE_FIXTURES=1 cargo test -p mjx-acp-thread
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/session-elicitations.json");
    let serialized = format!("{}\n", serde_json::to_string_pretty(&asked()).unwrap());

    if std::env::var_os("MJX_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &serialized).expect("could not write the fixture");
        return;
    }

    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        checked_in, serialized,
        "the elicitation fixture is out of date; regenerate with MJX_UPDATE_FIXTURES=1"
    );
}

#[test]
fn folding_is_deterministic() {
    let a = serde_json::to_string(&folded()).unwrap();
    let b = serde_json::to_string(&folded()).unwrap();
    assert_eq!(a, b);
}
