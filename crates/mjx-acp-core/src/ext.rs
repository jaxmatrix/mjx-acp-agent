//! The `_mjx/*` extension namespace.
//!
//! ACP routes any method with a leading underscore to its extension mechanism,
//! so these coexist with the protocol rather than colliding with it. Agents
//! never see them: they exist only on the browser-to-server hop.
//!
//! They cover the gap created by the server answering `fs/*` and `terminal/*`
//! on the browser's behalf. Those requests never reach the browser, but the UI
//! still has to *show* what happened — a terminal that streams, a diff for a
//! file the agent rewrote. So the server mirrors the outcome here.

use serde::{Deserialize, Serialize};

/// Server to browser: which agent this connection is talking to. Sent once,
/// immediately after the agent subprocess starts.
pub const AGENT_INFO: &str = "_mjx/agent/info";
/// Server to browser: a line the agent wrote to stderr. Surfaced so a crashing
/// agent is visible instead of silent.
pub const AGENT_STDERR: &str = "_mjx/agent/stderr";
/// Server to browser: the agent asked for a terminal and we started one.
pub const TERMINAL_CREATED: &str = "_mjx/terminal/created";
/// Server to browser: more bytes from a terminal.
pub const TERMINAL_OUTPUT: &str = "_mjx/terminal/output";
/// Server to browser: a terminal's process exited.
pub const TERMINAL_EXIT: &str = "_mjx/terminal/exit";
/// Server to browser: the agent rewrote a file, with before and after.
pub const FS_WROTE: &str = "_mjx/fs/wrote";
/// Server to browser: a frame the browser never saw, for the inspector.
///
/// The browser is the ACP client, so it already sees every frame it exchanges
/// and can log those itself. The exception is the `fs/*` and `terminal/*`
/// traffic the server answers on its behalf, which would otherwise be a blind
/// spot in a tool whose whole job is showing the protocol.
pub const INSPECTOR_FRAME: &str = "_mjx/inspector/frame";
/// Browser to server: send me the whole thread again (used after a reload).
pub const SESSION_REPLAY: &str = "_mjx/session/replay";
/// Server to browser: a turn started on an earlier socket has ended.
///
/// ACP signals the end of a turn with the response to `session/prompt`, and
/// that response is owed to whichever browser sent the prompt. When that
/// browser has gone, nothing in the protocol tells the one now watching that
/// the turn it inherited is over, and it would show "generating" forever.
pub const SESSION_TURN_ENDED: &str = "_mjx/session/turn_ended";
/// Server to browser: another socket has taken this connection over, and this
/// one is about to close.
///
/// Sent with an empty payload: the browser being told already knows which
/// connection it is on, and telling it anything about the socket that displaced
/// it would say more than it needs to know.
pub const CONNECTION_TAKEN_OVER: &str = "_mjx/connection/taken_over";

/// Payload of [`AGENT_INFO`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    /// Catalog id, e.g. `claude-acp`.
    pub agent_id: String,
    /// Display name.
    pub name: String,
    /// The command line we spawned, for the inspector.
    pub command: Vec<String>,
    /// Working directory the session runs in.
    pub cwd: String,
    /// Pass this back as `?resume=` to rejoin this agent after a reload.
    pub connection_id: String,
    /// True when this socket rejoined an agent that was already running, so the
    /// handshake was answered from what the agent said the first time round.
    /// The browser reads it to decide whether to ask for a replay.
    pub resumed: bool,
}

/// Payload of [`AGENT_STDERR`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStderr {
    /// One line, newline stripped.
    pub line: String,
}

/// Payload of [`TERMINAL_CREATED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCreated {
    /// Matches the `terminalId` the agent will reference in tool call content.
    pub terminal_id: String,
    /// Program name.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: String,
}

/// Payload of [`TERMINAL_OUTPUT`].
///
/// Incremental, not cumulative: each notification carries only the bytes
/// produced since the last one, so a long build doesn't resend its whole log on
/// every tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutput {
    /// Which terminal.
    pub terminal_id: String,
    /// New bytes, base64. Base64 rather than a string because PTY output is
    /// arbitrary bytes — escape sequences, partial UTF-8 at a chunk boundary —
    /// and xterm.js wants the bytes, not a lossy decode.
    pub chunk: String,
    /// Set once the terminal has hit its `outputByteLimit` and older output is
    /// being discarded.
    pub truncated: bool,
}

/// Payload of [`TERMINAL_EXIT`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalExit {
    /// Which terminal.
    pub terminal_id: String,
    /// Exit code, absent if the process was signalled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Signal name, absent on a normal exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

/// Payload of [`FS_WROTE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWrote {
    /// Absolute path.
    pub path: String,
    /// Contents before the write; `None` if the file was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// Contents after the write.
    pub new_text: String,
}

/// Payload of [`INSPECTOR_FRAME`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorFrame {
    /// `clientToAgent` or `agentToClient`.
    pub direction: String,
    /// The frame, verbatim, as a single line of JSON.
    pub line: String,
    /// The method this frame is or answers, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// True when the server answered this itself instead of forwarding it.
    pub intercepted: bool,
}

/// Payload of [`SESSION_TURN_ENDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTurnEnded {
    /// Which session's turn ended.
    pub session_id: String,
    /// Why it ended, spelled as ACP spells it in a `PromptResponse`.
    pub stop_reason: String,
}

/// Payload of the [`SESSION_REPLAY`] request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReplayRequest {
    /// Which session to replay.
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method;

    #[test]
    fn every_method_is_in_the_extension_namespace() {
        for m in [
            AGENT_INFO,
            AGENT_STDERR,
            TERMINAL_CREATED,
            TERMINAL_OUTPUT,
            TERMINAL_EXIT,
            FS_WROTE,
            INSPECTOR_FRAME,
            SESSION_REPLAY,
            SESSION_TURN_ENDED,
            CONNECTION_TAKEN_OVER,
        ] {
            assert!(method::is_extension(m), "{m} must start with _");
            assert!(m.starts_with("_mjx/"), "{m} must be namespaced to _mjx");
            // An extension must never shadow a real ACP method.
            assert_eq!(method::side(m), None, "{m}");
        }
    }

    #[test]
    fn payloads_are_camel_case_on_the_wire() {
        let json = serde_json::to_string(&TerminalExit {
            terminal_id: "t1".into(),
            exit_code: Some(0),
            signal: None,
        })
        .unwrap();
        // camelCase to match ACP's own convention, and `signal` is omitted
        // rather than sent as null.
        assert_eq!(json, r#"{"terminalId":"t1","exitCode":0}"#);
    }

    #[test]
    fn agent_info_carries_the_handle_a_browser_resumes_with() {
        let json = serde_json::to_string(&AgentInfo {
            agent_id: "mock".into(),
            name: "Mock Agent".into(),
            command: vec!["mjx-mock-agent".into()],
            cwd: "/w".into(),
            connection_id: "c1".into(),
            resumed: true,
        })
        .unwrap();
        assert!(json.contains(r#""connectionId":"c1""#), "{json}");
        assert!(json.contains(r#""resumed":true"#), "{json}");
    }
}
