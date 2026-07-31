//! `mjx-mock-agent --mcp`: a scripted MCP server.
//!
//! The same idea as the ACP script next door, one protocol down. It exists so
//! the MCP-over-ACP host can be tested end to end with no network, no `npx` and
//! no credentials — and, like the ACP mock, it writes the wire JSON by hand so
//! the tests check our reading of the MCP spec rather than a serializer against
//! itself.
//!
//! One tool, and one thing worth proving with it: `mock_stat` reports whether
//! `MJX_MOCK_MCP_TOKEN` reached it. That is the whole claim of the `acp`
//! transport — the server gets its credential from the process holding it, and
//! the agent never sees the value.

use anyhow::Result;
use mjx_acp_core::{Frame, JsonRpcError, RequestId, ResponsePayload};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The MCP version this speaks. Old enough to be uninteresting, which is the
/// point: nothing here depends on the version.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The environment variable the one tool looks for.
pub const TOKEN_VARIABLE: &str = "MJX_MOCK_MCP_TOKEN";

/// The id this server uses for the one request it makes of its client.
const ROOTS_REQUEST_ID: &str = "mock-mcp-roots";

/// Reads MCP frames on stdin and answers them on stdout, until stdin closes.
pub async fn serve() -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame = match Frame::parse(&line) {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!(%err, line, "unparseable MCP frame");
                continue;
            }
        };

        let replies = match frame {
            // The handshake is the one exchange that goes both ways: this server
            // asks its client for the roots *before* answering, and reports
            // whether it got them. An answer means the whole return path works —
            // a request from a server the client is holding, put to the agent and
            // routed back — and asking mid-handshake makes it deterministic
            // rather than a race with whatever the agent does next.
            Frame::Request { id, method, .. } if method == "initialize" => {
                let roots = ask_for_roots(&mut stdout, &mut lines).await?;
                handshake_reply(id, roots)
            }
            Frame::Request { id, method, params } => {
                let params: Value = params
                    .as_deref()
                    .and_then(|p| serde_json::from_str(p.get()).ok())
                    .unwrap_or(Value::Null);
                answer(id, &method, &params)
            }
            // Notifications are one-way by definition, `notifications/initialized`
            // included: an MCP client that got an answer to one would be right to
            // complain.
            Frame::Notification { .. } => Vec::new(),
            Frame::Response { .. } => {
                tracing::warn!("a response to something this server never asked");
                Vec::new()
            }
        };

        for reply in replies {
            let mut line = reply.to_line();
            line.push('\n');
            stdout.write_all(line.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// What the handshake answers, and what it says straight after.
fn handshake_reply(id: RequestId, roots_answered: bool) -> Vec<Frame> {
    vec![
        result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "mjx-mock-mcp", "version": env!("CARGO_PKG_VERSION") },
                "_meta": { "mjx.rootsAnswered": roots_answered }
            }),
        ),
        // Unprompted, and immediately: the host has to be able to carry a
        // server-initiated *notification* as well as a request, and a
        // `listChanged` right after the handshake is the cheapest way to make it
        // do so.
        Frame::notification("notifications/tools/list_changed", &json!({}))
            .expect("json! serializes"),
    ]
}

/// Asks the client for its roots and waits for the answer.
///
/// Anything else arriving meanwhile is dropped: the client is waiting on
/// `initialize`, so it has nothing else to say, and a mock is not the place to
/// build a second queue.
async fn ask_for_roots(
    stdout: &mut tokio::io::Stdout,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> Result<bool> {
    let asked = Frame::Request {
        id: RequestId::String(ROOTS_REQUEST_ID.into()),
        method: "roots/list".into(),
        params: None,
    };
    stdout
        .write_all(format!("{}\n", asked.to_line()).as_bytes())
        .await?;
    stdout.flush().await?;

    while let Some(line) = lines.next_line().await? {
        match Frame::parse(&line) {
            Ok(Frame::Response { id, payload })
                if id == RequestId::String(ROOTS_REQUEST_ID.into()) =>
            {
                return Ok(matches!(payload, ResponsePayload::Result(_)));
            }
            _ => tracing::warn!(line, "ignored while waiting for the roots"),
        }
    }
    // The client went away mid-handshake, which is not this mock's problem to
    // solve — it just did not get its roots.
    Ok(false)
}

/// What this server says to one request. More than one frame, when answering it
/// also means saying something unprompted.
fn answer(id: RequestId, method: &str, params: &Value) -> Vec<Frame> {
    match method {
        "tools/list" => vec![result(
            id,
            json!({
                "tools": [{
                    "name": "mock_stat",
                    "description": "Reports whether this server got its credential.",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } }
                    }
                }]
            }),
        )],

        "tools/call" => {
            if params["name"] != json!("mock_stat") {
                return vec![Frame::error(
                    id,
                    JsonRpcError::invalid_params(format!("no tool named {}", params["name"])),
                )];
            }
            // Whether, not what. The value is a credential; a mock that echoed
            // it would put it on the very wire this transport exists to keep it
            // off.
            let token = std::env::var(TOKEN_VARIABLE).is_ok();
            vec![result(
                id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("mock_stat ran; credential present: {token}")
                    }],
                    "isError": false
                }),
            )]
        }

        other => vec![Frame::error(id, JsonRpcError::method_not_found(other))],
    }
}

fn result(id: RequestId, value: Value) -> Frame {
    Frame::Response {
        id,
        payload: ResponsePayload::Result(
            serde_json::value::to_raw_value(&value).expect("json! serializes"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(frames: Vec<Frame>) -> Value {
        let [frame] = frames.as_slice() else {
            panic!("expected exactly one frame, got {}", frames.len());
        };
        let Frame::Response {
            payload: ResponsePayload::Result(result),
            ..
        } = frame
        else {
            panic!("expected a result");
        };
        serde_json::from_str(result.get()).unwrap()
    }

    #[test]
    fn the_handshake_answers_and_then_speaks_unprompted() {
        let frames = handshake_reply(RequestId::Number(1), true);
        assert_eq!(frames.len(), 2, "an answer, and a notification after it");
        assert_eq!(frames[1].method(), Some("notifications/tools/list_changed"));
        assert!(frames[1].id().is_none(), "a notification carries no id");

        // Whether the client answered the request this server made *of it*, which
        // is what the end-to-end test reads to know the return path works.
        let Frame::Response {
            payload: ResponsePayload::Result(result),
            ..
        } = &frames[0]
        else {
            panic!("the handshake must answer with a result");
        };
        let value: Value = serde_json::from_str(result.get()).unwrap();
        assert_eq!(value["_meta"]["mjx.rootsAnswered"], true);
    }

    #[test]
    fn the_tool_is_listed_and_can_be_called() {
        let listed = only(answer(RequestId::Number(1), "tools/list", &json!({})));
        assert_eq!(listed["tools"][0]["name"], "mock_stat");

        let called = only(answer(
            RequestId::Number(2),
            "tools/call",
            &json!({ "name": "mock_stat", "arguments": {} }),
        ));
        let text = called["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("mock_stat ran"), "{text}");
        // Whether the credential arrived, never the credential.
        assert!(!text.contains("MJX_MOCK_MCP_TOKEN="), "{text}");
    }

    #[test]
    fn an_unknown_tool_and_an_unknown_method_are_both_errors() {
        for (method, params) in [
            ("tools/call", json!({ "name": "nope" })),
            ("telepathy/read", json!({})),
        ] {
            let frames = answer(RequestId::Number(1), method, &params);
            assert!(
                matches!(
                    frames.first(),
                    Some(Frame::Response {
                        payload: ResponsePayload::Error(_),
                        ..
                    })
                ),
                "{method} should be an error"
            );
        }
    }
}
