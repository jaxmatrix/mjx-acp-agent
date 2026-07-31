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
/// Server to browser: the agent will do nothing until it is authenticated, and
/// here is what it offered.
///
/// The agent's own `-32000` reaches the browser too, carrying the same detail —
/// this exists beside it because a refusal belongs to the connection, not to the
/// one request that happened to provoke it, and the panel that renders it must
/// outlive that request.
pub const AUTH_REQUIRED: &str = "_mjx/auth/required";
/// Browser to server: what is the authentication state of this connection?
///
/// A *pull*, deliberately. The refusal may have arrived while a different
/// browser was attached, or none at all, and a notification sent then is gone.
pub const AUTH_STATE: &str = "_mjx/auth/state";
/// Browser to server: authenticate with this method.
pub const AUTH_ATTEMPT: &str = "_mjx/auth/attempt";
/// Server to browser: how an attempt is going.
pub const AUTH_PROGRESS: &str = "_mjx/auth/progress";
/// Browser to server: these keystrokes go to that terminal.
///
/// Only terminals the server opened for a login accept this. One the *agent*
/// asked for stays read-only: the agent owns that process, and a client typing
/// into it is not something ACP has any notion of.
pub const TERMINAL_INPUT: &str = "_mjx/terminal/input";
/// Browser to server: the terminal is being shown at this size.
pub const TERMINAL_RESIZE: &str = "_mjx/terminal/resize";
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
    /// The MCP servers configured for this agent, and whether it got them.
    ///
    /// Here rather than in a notification of its own because the browser must be
    /// told on every attachment, and this is the message that is already sent
    /// once per attachment — including to one that reattached, where nothing
    /// else runs.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerInfo>,
}

/// One configured MCP server, as the sidebar shows it.
///
/// Deliberately not the configuration. A `headers` or `env` *value* is a
/// credential, and the browser is the one place it must never travel — the whole
/// reason the server injects these rather than asking the browser to send them.
/// The names are here because "this server carries a token called X" is worth
/// seeing; the values are not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerInfo {
    /// The name the agent knows it by.
    pub name: String,
    /// `stdio`, `http`, `sse` or `acp`.
    pub transport: String,
    /// The command line or the URL. Never a credential.
    pub target: String,
    /// The names — never the values — of the environment variables or headers it
    /// carries.
    pub secrets: Vec<String>,
    /// Why this server was not offered to this agent, if it was not: an
    /// unsupported transport, or a credential that is not in the environment.
    /// `None` means the agent has it.
    pub unavailable: Option<String>,
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

/// Payload of the [`TERMINAL_INPUT`] request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInput {
    /// Which terminal.
    pub terminal_id: String,
    /// The bytes to write, base64 — the same encoding [`TerminalOutput`] uses,
    /// and for the same reason: a keystroke is not necessarily a character.
    pub bytes: String,
}

/// Payload of the [`TERMINAL_RESIZE`] request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResize {
    /// Which terminal.
    pub terminal_id: String,
    /// Rows the browser is showing.
    pub rows: u16,
    /// Columns the browser is showing.
    pub cols: u16,
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

/// What the browser is told about one method the agent offered.
///
/// Everything here is a *name* or a *reason*. There is deliberately nowhere to
/// put a credential: the browser is the one place a value must never travel, and
/// the server holding them is the whole reason it doesn't have to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethodInfo {
    /// The `methodId` to pass back in an [`AuthAttemptRequest`].
    pub id: String,
    /// Display name, from the agent.
    pub name: String,
    /// Longer description, from the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which shape of method this is: `envVar`, `terminal` or `agent`.
    pub kind: String,
    /// The provider that would handle it, if one would.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// What the operator must do, when the server cannot get any further alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Documentation the agent pointed at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The names — never the values — of the variables this method needs, and
    /// whether each is set. Named for the same reason [`McpServerInfo::secrets`]
    /// is: "this wants a token called X, and X is missing" is worth seeing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<AuthSecret>,
    /// Why each provider that looked at this method passed on it, in the order
    /// they were asked. Kept rather than reduced to "unsupported", because a
    /// silently shorter answer is a support call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declines: Vec<AuthDecline>,
    /// True once this method has authenticated the agent.
    pub satisfied: bool,
}

/// One variable a method needs, and whether the server has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSecret {
    /// The environment variable's name.
    pub name: String,
    /// The label the agent gave it, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether the server's environment has it. Not what it is.
    pub present: bool,
    /// Whether the method works without it.
    pub optional: bool,
}

/// One provider's reason for not handling a method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthDecline {
    /// Which provider.
    pub provider: String,
    /// Why. Written for the operator, not for a log.
    pub reason: String,
}

/// Payload of [`AUTH_REQUIRED`] and result of an [`AUTH_STATE`] request.
///
/// Sent when the agent refuses to work until it is authenticated, and readable
/// on demand because the browser attached now may not be the one that was there
/// when the refusal arrived.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthState {
    /// Whether the agent has refused something for want of authentication.
    pub required: bool,
    /// Whether one of the methods below has since succeeded.
    pub authenticated: bool,
    /// What the agent offered, in the order it offered them.
    #[serde(default)]
    pub methods: Vec<AuthMethodInfo>,
    /// The method the agent named when it refused, if it named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused_method: Option<String>,
}

/// Payload of the [`AUTH_ATTEMPT`] request.
///
/// A method id and nothing else. A browser cannot hand the server a credential
/// through this — by construction, not by validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthAttemptRequest {
    /// Which of the offered methods to use.
    pub method_id: String,
}

/// Result of an [`AUTH_ATTEMPT`] request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthAttemptResult {
    /// Whether the agent is now authenticated.
    pub authenticated: bool,
    /// What happened, for the operator to read.
    pub message: String,
    /// A terminal the server opened for this attempt, whose output the browser
    /// should show and can type into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
}

/// Payload of [`AUTH_PROGRESS`].
///
/// A login that runs in a terminal outlives the request that started it, so
/// there has to be a way to say how it ended that is not a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthProgress {
    /// Which method is being attempted.
    pub method_id: String,
    /// What is happening, for the operator to read.
    pub message: String,
    /// Set once the attempt has finished, either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authenticated: Option<bool>,
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
            AUTH_REQUIRED,
            AUTH_STATE,
            AUTH_ATTEMPT,
            AUTH_PROGRESS,
            TERMINAL_INPUT,
            TERMINAL_RESIZE,
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
            mcp_servers: vec![],
        })
        .unwrap();
        assert!(json.contains(r#""connectionId":"c1""#), "{json}");
        assert!(json.contains(r#""resumed":true"#), "{json}");
    }

    #[test]
    fn an_mcp_server_is_described_without_its_credentials() {
        let json = serde_json::to_string(&McpServerInfo {
            name: "github".into(),
            transport: "http".into(),
            target: "https://api.example.test/mcp".into(),
            secrets: vec!["Authorization".into()],
            unavailable: None,
        })
        .unwrap();
        assert!(json.contains(r#""secrets":["Authorization"]"#), "{json}");
        // The struct has nowhere to put a value, which is the point: this is the
        // one payload that reaches a browser, and a token that got this far would
        // be on the wire the injection exists to keep it off.
        assert!(!json.contains("Bearer"), "{json}");
    }

    #[test]
    fn an_auth_method_is_described_without_its_credentials() {
        // The same rule as the MCP payload above, for the same reason. An auth
        // method is *about* a credential, so this is the payload where getting
        // it wrong would be easiest.
        let json = serde_json::to_string(&AuthMethodInfo {
            id: "env".into(),
            name: "API key".into(),
            description: None,
            kind: "envVar".into(),
            provider: Some("environment".into()),
            instructions: Some("set OPENAI_API_KEY and reconnect".into()),
            link: Some("https://example.test/keys".into()),
            secrets: vec![AuthSecret {
                name: "OPENAI_API_KEY".into(),
                label: None,
                present: false,
                optional: false,
            }],
            declines: vec![AuthDecline {
                provider: "terminal".into(),
                reason: "this method is not a terminal login".into(),
            }],
            satisfied: false,
        })
        .unwrap();

        assert!(json.contains(r#""name":"OPENAI_API_KEY""#), "{json}");
        assert!(json.contains(r#""present":false"#), "{json}");
        // Whether it is set is the browser's business. What it is, is not, and
        // `AuthSecret` has no field that could carry it.
        assert!(!json.contains("sk-"), "{json}");
    }

    #[test]
    fn an_attempt_can_only_name_a_method() {
        // By construction rather than by validation: there is no field on this
        // request for a browser to put a credential in, so there is no path by
        // which one travels inward.
        let request: AuthAttemptRequest =
            serde_json::from_str(r#"{"methodId":"env","password":"hunter2"}"#).unwrap();
        assert_eq!(request.method_id, "env");
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"methodId":"env"}"#
        );
    }

    #[test]
    fn an_unauthenticated_connection_has_a_state_to_report() {
        let json = serde_json::to_string(&AuthState::default()).unwrap();
        // The default is the honest one for an agent that has never refused
        // anything: nothing is required and nothing has been authenticated.
        assert_eq!(
            json,
            r#"{"required":false,"authenticated":false,"methods":[]}"#
        );
    }
}
