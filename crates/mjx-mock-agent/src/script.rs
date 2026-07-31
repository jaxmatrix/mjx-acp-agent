//! The scripted turn.
//!
//! Ordered so a viewer developer sees each UI surface appear one at a time:
//! thought → text → read tool call → plan → diff → permission → terminal →
//! elicitation. Every step checks for cancellation first, so `session/cancel`
//! takes effect within one beat instead of at the end of the script.
//!
//! The last step runs only if the client declared it can be elicited. That is
//! not defensiveness — it is what the protocol requires, and it is why the
//! integration tests, which declare nothing, never see one.

use mjx_acp_core::method;
use serde_json::{Value, json};

use crate::{Agent, beat};

/// Runs one turn and returns the ACP `stopReason`.
pub async fn run_turn(agent: &Agent, session_id: &str, params: &Value) -> &'static str {
    let prompt = prompt_text(params);

    macro_rules! bail_if_cancelled {
        () => {
            if agent.is_cancelled(session_id).await {
                return "cancelled";
            }
        };
    }

    // The commands the UI offers in its slash-command menu.
    agent.update(
        session_id,
        json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "explain", "description": "Explain the current file" },
                { "name": "test", "description": "Run the test suite",
                  "input": { "hint": "optional test name filter" } },
            ]
        }),
    );

    // 1. A thought block. Streamed, so the UI's collapsed-thinking state has
    //    something to grow.
    for chunk in [
        "Let me look at what the user is asking for. ",
        "They said: \"",
        &prompt,
        "\". ",
        "I'll read the file first, then propose a fix.",
    ] {
        bail_if_cancelled!();
        agent.update(
            session_id,
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": chunk }
            }),
        );
        beat(120).await;
    }

    // 2. Streaming assistant text, word by word.
    beat(200).await;
    stream_text(
        agent,
        session_id,
        "I'll take a look at `stats.js` and check the `median` implementation.\n\n",
    )
    .await;
    bail_if_cancelled!();

    // 3. A read tool call. This actually calls `fs/read_text_file`, so it
    //    exercises the server's filesystem jail rather than faking a result.
    let path = format!("{}/stats.js", cwd(params));
    agent.update(
        session_id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_read",
            "title": "Read stats.js",
            "kind": "read",
            "status": "in_progress",
            "locations": [{ "path": path }],
            "rawInput": { "path": path }
        }),
    );
    beat(300).await;

    let file = agent
        .request(
            method::client::FS_READ_TEXT_FILE,
            json!({ "sessionId": session_id, "path": path }),
        )
        .await;
    let contents = match &file {
        Ok(result) => serde_json::from_str::<Value>(result.get())
            .ok()
            .and_then(|v| v["content"].as_str().map(str::to_owned))
            .unwrap_or_default(),
        // A client that doesn't offer the fs capability is a legitimate
        // configuration, not a crash. Report the tool call as failed and keep
        // going so the rest of the script still runs.
        Err(err) => {
            agent.update(
                session_id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call_read",
                    "status": "failed",
                    "content": [{ "type": "content", "content":
                        { "type": "text", "text": format!("Could not read the file: {}", err.message) } }]
                }),
            );
            String::new()
        }
    };

    if file.is_ok() {
        agent.update(
            session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_read",
                "status": "completed",
                "content": [{ "type": "content", "content":
                    { "type": "text", "text": numbered(&contents) } }],
                "rawOutput": { "bytes": contents.len() }
            }),
        );
    }
    bail_if_cancelled!();

    // 4. A plan. The UI shows it live and ticks entries off as we go.
    beat(300).await;
    agent.update(session_id, plan(&["in_progress", "pending", "pending"]));

    // 4a. Having seen the shape of the work, the agent moves itself up to a
    //     more capable model. Agents do change their own settings mid-turn, and
    //     it is the only way `config_option_update` gets into the recorded
    //     fixture — the selector in the sidebar is expected to follow.
    beat(200).await;
    let options = agent
        .set_config_option(session_id, "model", json!("mock-opus"))
        .await;
    agent.update(
        session_id,
        json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
    );

    beat(300).await;
    stream_text(
        agent,
        session_id,
        "Found it. `median()` returns the upper middle element for even-length \
         input instead of averaging the two middle values.\n\n",
    )
    .await;
    bail_if_cancelled!();

    // 5. An edit tool call carrying a diff, then the write that applies it.
    //    Actually writing matters: the tests in step 7 only pass if the fix is
    //    really on disk, so a demo that faked this would end by contradicting
    //    itself.
    let (old_text, new_text) = patched(&contents);
    agent.update(
        session_id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_edit",
            "title": "Fix the off-by-one in median()",
            "kind": "edit",
            "status": "in_progress",
            "locations": [{ "path": path, "line": 8 }],
            "content": [{ "type": "diff", "path": path,
                          "oldText": old_text, "newText": new_text }]
        }),
    );
    beat(700).await;

    let wrote = file.is_ok()
        && agent
            .request(
                method::client::FS_WRITE_TEXT_FILE,
                json!({ "sessionId": session_id, "path": path, "content": new_text }),
            )
            .await
            .is_ok();

    agent.update(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_edit",
            "status": if wrote { "completed" } else { "failed" }
        }),
    );
    agent.update(session_id, plan(&["completed", "in_progress", "pending"]));
    bail_if_cancelled!();

    // 6. A permission request. Blocks until the human answers, which is the
    //    whole point of the surface.
    beat(300).await;
    stream_text(agent, session_id, "Let me run the tests to confirm.\n\n").await;

    let approved = ask_permission(agent, session_id).await;
    if !approved {
        agent.update(
            session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_test",
                "status": "failed"
            }),
        );
        stream_text(
            agent,
            session_id,
            "\nUnderstood, I won't run anything. The fix above is ready when you are.",
        )
        .await;
        agent.update(session_id, plan(&["completed", "completed", "pending"]));
        return "end_turn";
    }
    bail_if_cancelled!();

    // 7. A live terminal.
    let passed = run_in_terminal(agent, session_id, params).await;
    agent.update(session_id, plan(&["completed", "completed", "completed"]));

    beat(200).await;
    // Report what actually happened. Claiming success over a red test run is
    // the one thing a coding agent must never do.
    stream_text(
        agent,
        session_id,
        if passed {
            "\nAll four tests pass now. The `median` fix is in place."
        } else {
            "\nThe tests still aren't green — see the terminal output above for \
             what failed."
        },
    )
    .await;

    // 8. Two elicitations: a structured question that is not a permission
    //    decision. Both modes, because a form built from a schema and a link to
    //    go away and come back from are separate rendering paths.
    bail_if_cancelled!();
    ask_how_to_record(agent, session_id).await;
    bail_if_cancelled!();
    confirm_out_of_band(agent, session_id).await;

    // Token accounting, for the usage bar.
    agent.update(
        session_id,
        json!({
            "sessionUpdate": "usage_update",
            "used": 4_812,
            "size": 200_000,
            "cost": { "amount": 0.0143, "currency": "USD" }
        }),
    );

    "end_turn"
}

/// The form the client is asked to fill in.
///
/// One property of every type the schema allows, so the viewer's form has no
/// untested branch: a plain string, a titled single-select, a multi-select, an
/// integer, a float, and a boolean.
fn record_schema() -> Value {
    json!({
        "type": "object",
        "title": "Record the fix",
        "description": "Nothing is pushed until you say so.",
        "properties": {
            "branch": {
                "type": "string",
                "title": "Branch",
                "description": "Where the commit goes.",
                "default": "fix/median",
                "minLength": 1,
                "maxLength": 80
            },
            "remote": {
                "type": "string",
                "title": "Remote",
                "oneOf": [
                    { "const": "origin", "title": "origin",
                      "description": "Your fork." },
                    { "const": "upstream", "title": "upstream",
                      "description": "The shared repository." }
                ]
            },
            "reviewers": {
                "type": "array",
                "title": "Reviewers",
                "description": "Nobody is required.",
                "maxItems": 2,
                "items": { "anyOf": [
                    { "const": "ana", "title": "Ana" },
                    { "const": "bo", "title": "Bo" },
                    { "const": "cy", "title": "Cy" }
                ]}
            },
            "attempts": {
                "type": "integer",
                "title": "Retries",
                "description": "How many times to retry a rejected push.",
                "minimum": 0,
                "maximum": 5,
                "default": 1
            },
            "timeout": {
                "type": "number",
                "title": "Timeout (seconds)",
                "minimum": 0.5,
                "maximum": 120.0,
                "default": 30.0
            },
            "squash": {
                "type": "boolean",
                "title": "Squash the commits",
                "default": true
            }
        },
        "required": ["branch", "remote"]
    })
}

/// Asks the client to fill in a form, and says what came back.
///
/// Skipped unless the client declared it can render one. A conformant agent does
/// not elicit from a client that never offered to: the request would come back
/// as a plain "method not found" and the user would see a turn fail for no
/// visible reason.
async fn ask_how_to_record(agent: &Agent, session_id: &str) {
    if !agent.can_elicit_form() {
        return;
    }

    beat(300).await;
    let reply = agent
        .request_until_cancelled(
            session_id,
            method::client::ELICITATION_CREATE,
            json!({
                "mode": "form",
                "sessionId": session_id,
                // Tied to the edit, so a client that shows which call an
                // elicitation belongs to has something to show.
                "toolCallId": "call_edit",
                "message": "How should I record this fix?",
                "requestedSchema": record_schema()
            }),
        )
        .await;

    // Cancelled with the form still open. Say nothing further: the turn is over
    // and the caller reports it as cancelled.
    let Some(reply) = reply else { return };

    let answered = reply
        .ok()
        .and_then(|result| serde_json::from_str::<Value>(result.get()).ok());
    let summary = match answered.as_ref().map(|value| &value["action"]) {
        Some(action) if action == "accept" => {
            let content = &answered.as_ref().expect("just matched")["content"];
            format!(
                "\n\nPushing to `{}/{}`.",
                content["remote"].as_str().unwrap_or("origin"),
                content["branch"].as_str().unwrap_or("fix/median")
            )
        }
        Some(action) if action == "decline" => "\n\nLeaving it uncommitted, then.".to_string(),
        // Cancelled, refused, or a client that could not answer at all. Saying
        // nothing was recorded is true in every one of those cases.
        _ => "\n\nNothing recorded.".to_string(),
    };
    stream_text(agent, session_id, &summary).await;
}

/// Sends the client to a URL, then says the exchange is finished.
///
/// URL mode does not end with a response the way a form does — the client is
/// told the elicitation completed by a notification. Here the agent completes it
/// itself after a beat, which is what makes the demo deterministic: a real agent
/// would be waiting on whatever the user does at the far end.
async fn confirm_out_of_band(agent: &Agent, session_id: &str) {
    if !agent.can_elicit_url() {
        return;
    }

    beat(300).await;
    let elicitation_id = "mock-elicitation-1";
    let asked = agent.request(
        method::client::ELICITATION_CREATE,
        json!({
            "mode": "url",
            "sessionId": session_id,
            "elicitationId": elicitation_id,
            "url": "https://agentclientprotocol.com/protocol/elicitation",
            "message": "Read the elicitation spec, then come back."
        }),
    );

    // Concurrently, because the request is only answered once the client has
    // been told the exchange is over.
    let finish = async {
        beat(1_200).await;
        agent.notify(
            method::client::ELICITATION_COMPLETE,
            json!({ "elicitationId": elicitation_id }),
        );
    };

    let (_, ()) = tokio::join!(asked, finish);
}

/// Sends `text` as `agent_message_chunk`s, one word at a time.
async fn stream_text(agent: &Agent, session_id: &str, text: &str) {
    for word in split_keeping_whitespace(text) {
        if agent.is_cancelled(session_id).await {
            return;
        }
        agent.update(
            session_id,
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": word }
            }),
        );
        beat(45).await;
    }
}

/// Asks the client to authorize running the test suite. Returns whether it may.
async fn ask_permission(agent: &Agent, session_id: &str) -> bool {
    let tool_call = json!({
        "toolCallId": "call_test",
        "title": "Run `node --test`",
        "kind": "execute",
        "status": "pending",
        "rawInput": { "command": "node", "args": ["--test"] }
    });
    agent.update(
        session_id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_test",
            "title": "Run `node --test`",
            "kind": "execute",
            "status": "pending"
        }),
    );

    let reply = agent
        .request(
            method::client::SESSION_REQUEST_PERMISSION,
            json!({
                "sessionId": session_id,
                "toolCall": tool_call,
                "options": [
                    { "optionId": "allow_once",   "name": "Allow",            "kind": "allow_once" },
                    { "optionId": "allow_always", "name": "Always allow",     "kind": "allow_always" },
                    { "optionId": "reject_once",  "name": "Don't run it",     "kind": "reject_once" },
                ]
            }),
        )
        .await;

    let Ok(result) = reply else { return false };
    let Ok(value) = serde_json::from_str::<Value>(result.get()) else {
        return false;
    };
    // `{"outcome": {"outcome": "selected", "optionId": "..."}}`
    let outcome = &value["outcome"];
    outcome["outcome"] == "selected"
        && outcome["optionId"]
            .as_str()
            .is_some_and(|id| id.starts_with("allow"))
}

/// Starts a terminal, streams it via the tool call, and waits for it to exit.
/// Returns whether the command succeeded.
async fn run_in_terminal(agent: &Agent, session_id: &str, params: &Value) -> bool {
    let created = agent
        .request(
            method::client::TERMINAL_CREATE,
            json!({
                "sessionId": session_id,
                "command": "node",
                "args": ["--test"],
                "cwd": cwd(params),
                "outputByteLimit": 65_536
            }),
        )
        .await;

    let terminal_id = created
        .ok()
        .and_then(|r| serde_json::from_str::<Value>(r.get()).ok())
        .and_then(|v| v["terminalId"].as_str().map(str::to_owned));

    let Some(terminal_id) = terminal_id else {
        agent.update(
            session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_test",
                "status": "failed",
                "content": [{ "type": "content", "content":
                    { "type": "text", "text": "This client has no terminal capability." } }]
            }),
        );
        return false;
    };

    // Attaching the terminal to the tool call is what makes the UI render a
    // live console inside the card rather than a block of text at the end.
    agent.update(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_test",
            "status": "in_progress",
            "content": [{ "type": "terminal", "terminalId": terminal_id }]
        }),
    );

    let exit = agent
        .request(
            method::client::TERMINAL_WAIT_FOR_EXIT,
            json!({ "sessionId": session_id, "terminalId": terminal_id }),
        )
        .await;
    let exit_code = exit
        .ok()
        .and_then(|r| serde_json::from_str::<Value>(r.get()).ok())
        .and_then(|v| v["exitCode"].as_i64());

    agent.update(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_test",
            "status": if exit_code == Some(0) { "completed" } else { "failed" },
            "rawOutput": { "exitCode": exit_code }
        }),
    );

    // Release it so the server can reclaim the PTY. The terminal's output stays
    // on screen; release only ends our claim on it.
    let _ = agent
        .request(
            method::client::TERMINAL_RELEASE,
            json!({ "sessionId": session_id, "terminalId": terminal_id }),
        )
        .await;

    exit_code == Some(0)
}

/// The three-step plan, with the given statuses.
fn plan(statuses: &[&str; 3]) -> Value {
    let steps = [
        ("Read stats.js and find the bug", "high"),
        ("Fix the off-by-one in median()", "high"),
        ("Run the tests", "medium"),
    ];
    json!({
        "sessionUpdate": "plan",
        "entries": steps.iter().zip(statuses).map(|((content, priority), status)| json!({
            "content": content, "priority": priority, "status": status
        })).collect::<Vec<_>>()
    })
}

/// The user's prompt as plain text, for echoing back in the thought block.
fn prompt_text(params: &Value) -> String {
    params["prompt"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    b["text"].as_str().map(str::to_string).or_else(|| {
                        // A prompt of nothing but mentions still says something.
                        (b["type"] == "resource_link")
                            .then(|| b["name"].as_str().or_else(|| b["uri"].as_str()))
                            .flatten()
                            .map(|named| format!("@{named}"))
                    })
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "have a look at this project".into())
}

/// The session's working directory. `session/prompt` doesn't carry it, so fall
/// back to the process's own cwd, which the server sets when it spawns us.
fn cwd(params: &Value) -> String {
    params["cwd"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into())
        })
}

/// Renders file contents the way an agent usually reports a read: numbered
/// lines, truncated to something a chat bubble can hold.
fn numbered(contents: &str) -> String {
    contents
        .lines()
        .take(30)
        .enumerate()
        .map(|(i, line)| format!("{:>4}  {line}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The before/after pair for the diff. Falls back to a canned snippet when the
/// client couldn't give us the file, so the diff surface still renders.
fn patched(contents: &str) -> (String, String) {
    // Anchored on the return statement alone rather than on a block of
    // surrounding lines, so an extra comment in the file doesn't silently turn
    // the patch into a no-op.
    const OLD_LINE: &str = "  return sorted[mid];";
    const NEW_LINE: &str = "  return sorted.length % 2 === 0\n    ? (sorted[mid - 1] + sorted[mid]) / 2\n    : sorted[mid];";
    /// The comment describing the bug, which the fix makes untrue.
    const STALE_COMMENT: &str = "  // BUG: for even-length input this returns the upper of the two middle\n  // values instead of their average.\n";

    if contents.contains(OLD_LINE) {
        let fixed = contents
            .replacen(OLD_LINE, NEW_LINE, 1)
            // Leaving a comment that says the code is broken, next to the code
            // that fixes it, is the kind of thing a careless agent does.
            .replacen(STALE_COMMENT, "", 1);
        (contents.to_string(), fixed)
    } else {
        // The client couldn't give us the file. Show the change on its own so
        // the diff surface still renders.
        (
            format!("export function median(xs) {{\n{OLD_LINE}\n}}\n"),
            format!("export function median(xs) {{\n{NEW_LINE}\n}}\n"),
        )
    }
}

/// Splits into words, keeping the whitespace attached, so reassembling the
/// chunks reproduces the input exactly. A viewer that concatenates chunks
/// naively should still get correct text.
fn split_keeping_whitespace(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if ch == ' ' || ch == '\n' {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjx_acp_core::acp;

    /// The whole reason the script writes raw JSON: these assertions check that
    /// what it emits is what the protocol actually defines. A typo in a field
    /// name fails here.
    fn assert_is_session_update(update: &Value) {
        let notification = json!({ "sessionId": "s1", "update": update });
        serde_json::from_value::<acp::SessionNotification>(notification)
            .unwrap_or_else(|e| panic!("not a valid SessionUpdate: {e}\n{update:#}"));
    }

    #[test]
    fn every_plan_state_is_a_valid_session_update() {
        for statuses in [
            ["pending", "pending", "pending"],
            ["in_progress", "pending", "pending"],
            ["completed", "completed", "completed"],
        ] {
            assert_is_session_update(&plan(&statuses));
        }
    }

    #[test]
    fn message_thought_and_command_updates_are_valid() {
        assert_is_session_update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi" }
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" }
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "explain", "description": "Explain the current file" },
                { "name": "test", "description": "Run the test suite",
                  "input": { "hint": "optional test name filter" } },
            ]
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "current_mode_update", "currentModeId": "code"
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "usage_update", "used": 4_812, "size": 200_000,
            "cost": { "amount": 0.0143, "currency": "USD" }
        }));
    }

    /// Parsing alone is not enough here. The schema deserializes a config
    /// option list with `VecSkipError`, so a malformed option is *dropped*
    /// rather than rejected — a typo would leave a valid-looking update with
    /// nothing in it. Counting what survives is what actually checks the shape.
    #[test]
    fn the_advertised_config_options_survive_a_round_trip() {
        let options = crate::config_options(&crate::default_config_values());
        let update = json!({ "sessionUpdate": "config_option_update", "configOptions": options });
        assert_is_session_update(&update);

        let parsed: acp::SessionNotification =
            serde_json::from_value(json!({ "sessionId": "s1", "update": update })).unwrap();
        let acp::SessionUpdate::ConfigOptionUpdate(parsed) = parsed.update else {
            panic!("not a config option update");
        };
        assert_eq!(parsed.config_options.len(), 3, "an option was dropped");

        // The grouped variant is the one with no discriminator on the wire, so
        // it is the one that silently degrades if it is written wrong.
        let acp::SessionConfigKind::Select(model) = &parsed.config_options[0].kind else {
            panic!("the model option is not a select");
        };
        let acp::SessionConfigSelectOptions::Grouped(groups) = &model.options else {
            panic!("the model options were not read as grouped");
        };
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn tool_calls_with_every_content_kind_are_valid() {
        assert_is_session_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_read", "title": "Read stats.js", "kind": "read",
            "status": "in_progress",
            "locations": [{ "path": "/w/stats.js" }],
            "rawInput": { "path": "/w/stats.js" }
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_edit", "title": "Fix it", "kind": "edit",
            "status": "in_progress",
            "locations": [{ "path": "/w/stats.js", "line": 8 }],
            "content": [{ "type": "diff", "path": "/w/stats.js",
                          "oldText": "a", "newText": "b" }]
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_test", "status": "in_progress",
            "content": [{ "type": "terminal", "terminalId": "t1" }]
        }));
        assert_is_session_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_read", "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "x" } }],
            "rawOutput": { "bytes": 1 }
        }));
    }

    #[test]
    fn the_permission_request_matches_the_schema() {
        let request = json!({
            "sessionId": "s1",
            "toolCall": {
                "toolCallId": "call_test", "title": "Run `node --test`",
                "kind": "execute", "status": "pending",
                "rawInput": { "command": "node", "args": ["--test"] }
            },
            "options": [
                { "optionId": "allow_once",   "name": "Allow",        "kind": "allow_once" },
                { "optionId": "allow_always", "name": "Always allow", "kind": "allow_always" },
                { "optionId": "reject_once",  "name": "Don't run it", "kind": "reject_once" },
            ]
        });
        serde_json::from_value::<acp::RequestPermissionRequest>(request).unwrap();
    }

    #[test]
    fn the_form_elicitation_matches_the_schema() {
        let request = json!({
            "mode": "form",
            "sessionId": "s1",
            "toolCallId": "call_edit",
            "message": "How should I record this fix?",
            "requestedSchema": record_schema()
        });
        let parsed = serde_json::from_value::<acp::CreateElicitationRequest>(request).unwrap();

        // Reading it back as the real type is not enough on its own: the schema
        // keeps an unrecognised property as `Other` rather than rejecting it, so
        // a typo in a `type` would parse and then render as nothing. Naming the
        // variants is what makes the assertion mean something.
        let acp::ElicitationMode::Form(form) = &parsed.mode else {
            panic!("not read as a form: {:?}", parsed.mode);
        };
        let properties = &form.requested_schema.properties;
        let kinds: Vec<&str> = [
            "branch",
            "remote",
            "reviewers",
            "attempts",
            "timeout",
            "squash",
        ]
        .iter()
        .map(|name| match properties.get(*name) {
            Some(acp::ElicitationPropertySchema::String(_)) => "string",
            Some(acp::ElicitationPropertySchema::Integer(_)) => "integer",
            Some(acp::ElicitationPropertySchema::Number(_)) => "number",
            Some(acp::ElicitationPropertySchema::Boolean(_)) => "boolean",
            Some(acp::ElicitationPropertySchema::Array(_)) => "array",
            other => panic!("{name} was read as {other:?}"),
        })
        .collect();
        assert_eq!(
            kinds,
            ["string", "string", "array", "integer", "number", "boolean"]
        );
    }

    #[test]
    fn the_url_elicitation_matches_the_schema() {
        let request = json!({
            "mode": "url",
            "sessionId": "s1",
            "elicitationId": "mock-elicitation-1",
            "url": "https://agentclientprotocol.com/protocol/elicitation",
            "message": "Read the elicitation spec, then come back."
        });
        let parsed = serde_json::from_value::<acp::CreateElicitationRequest>(request).unwrap();
        assert!(matches!(parsed.mode, acp::ElicitationMode::Url(_)));

        let notification = json!({ "elicitationId": "mock-elicitation-1" });
        serde_json::from_value::<acp::CompleteElicitationNotification>(notification).unwrap();
    }

    #[test]
    fn chunks_reassemble_into_the_original_text() {
        let text = "I'll take a look at `stats.js`.\n\nThen fix it.";
        assert_eq!(split_keeping_whitespace(text).concat(), text);
    }

    #[test]
    fn the_patch_actually_fixes_the_demo_bug() {
        let source = include_str!("../../../demo/pristine/stats.js");
        let (old, new) = patched(source);
        assert_eq!(old, source, "old text must be the file as it was");
        assert_ne!(new, source, "the patch must change something");
        assert!(new.contains("(sorted[mid - 1] + sorted[mid]) / 2"));
        assert!(
            !new.contains("BUG:"),
            "the fix must not leave a comment claiming the code is broken"
        );
    }

    #[test]
    fn the_patch_falls_back_when_the_file_could_not_be_read() {
        let (old, new) = patched("");
        assert!(old.contains("return sorted[mid];"));
        assert!(new.contains("/ 2"));
    }

    #[test]
    fn prompt_text_reads_text_blocks_and_has_a_fallback() {
        let params = json!({ "prompt": [
            { "type": "text", "text": "fix" }, { "type": "text", "text": "median" }
        ]});
        assert_eq!(prompt_text(&params), "fix median");
        assert!(!prompt_text(&json!({ "prompt": [] })).is_empty());
    }

    #[test]
    fn a_prompt_of_only_mentions_still_says_something() {
        // "@stats.js" and no prose is a real prompt. Reading only text blocks
        // would leave the thought stream with nothing to work from.
        let params = json!({ "prompt": [
            { "type": "resource_link", "uri": "file:///w/stats.js", "name": "stats.js" }
        ]});
        assert_eq!(prompt_text(&params), "@stats.js");
    }
}
