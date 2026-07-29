//! ACP v1 method names, and which side of the connection answers them.
//!
//! Strings verified against `agent-client-protocol` 2.0.0's schema modules; the
//! upstream machine-readable list is `schema/v1/meta.unstable.json` in
//! <https://github.com/agentclientprotocol/agent-client-protocol>.

/// Methods the *agent* implements. The client calls these.
pub mod agent {
    /// Handshake: version and capability exchange.
    pub const INITIALIZE: &str = "initialize";
    /// Authenticate with one of the agent's advertised auth methods.
    pub const AUTHENTICATE: &str = "authenticate";
    /// Drop the current authentication.
    pub const LOGOUT: &str = "logout";
    /// Create a session.
    pub const SESSION_NEW: &str = "session/new";
    /// Reload a previously created session, replaying its history.
    pub const SESSION_LOAD: &str = "session/load";
    /// Enumerate sessions.
    pub const SESSION_LIST: &str = "session/list";
    /// Delete a session.
    pub const SESSION_DELETE: &str = "session/delete";
    /// Resume a session (unstable).
    pub const SESSION_RESUME: &str = "session/resume";
    /// Close a session.
    pub const SESSION_CLOSE: &str = "session/close";
    /// Branch a session (unstable).
    pub const SESSION_FORK: &str = "session/fork";
    /// Switch mode, e.g. "ask" to "code".
    pub const SESSION_SET_MODE: &str = "session/set_mode";
    /// Set a session config option, e.g. the model.
    pub const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    /// Run a turn.
    pub const SESSION_PROMPT: &str = "session/prompt";
    /// Notification: abort the running turn.
    pub const SESSION_CANCEL: &str = "session/cancel";
    /// MCP-over-ACP (unstable).
    pub const MCP_MESSAGE: &str = "mcp/message";
}

/// Methods the *client* implements. The agent calls these.
pub mod client {
    /// Notification: a streaming update to a session.
    pub const SESSION_UPDATE: &str = "session/update";
    /// Ask the user to authorize a tool call.
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    /// Read a text file from the workspace.
    pub const FS_READ_TEXT_FILE: &str = "fs/read_text_file";
    /// Write a text file to the workspace.
    pub const FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";
    /// Start a command in a terminal.
    pub const TERMINAL_CREATE: &str = "terminal/create";
    /// Read a terminal's accumulated output.
    pub const TERMINAL_OUTPUT: &str = "terminal/output";
    /// Signal a terminal's process.
    pub const TERMINAL_KILL: &str = "terminal/kill";
    /// Discard a terminal.
    pub const TERMINAL_RELEASE: &str = "terminal/release";
    /// Block until a terminal's process exits.
    pub const TERMINAL_WAIT_FOR_EXIT: &str = "terminal/wait_for_exit";
    /// Ask the user for structured input (unstable).
    pub const ELICITATION_CREATE: &str = "elicitation/create";
    /// Notification: an elicitation finished (unstable).
    pub const ELICITATION_COMPLETE: &str = "elicitation/complete";
    /// MCP-over-ACP (unstable).
    pub const MCP_CONNECT: &str = "mcp/connect";
    /// MCP-over-ACP (unstable).
    pub const MCP_DISCONNECT: &str = "mcp/disconnect";
    /// MCP-over-ACP (unstable).
    pub const MCP_MESSAGE: &str = "mcp/message";
}

/// Protocol-level methods, which either side may send.
pub mod protocol {
    /// Notification: cancel an in-flight request by id.
    pub const CANCEL_REQUEST: &str = "$/cancel_request";
}

/// Which peer implements a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The agent answers it.
    Agent,
    /// The client answers it.
    Client,
    /// Either peer may send it.
    Either,
}

/// Which peer implements `method`, or `None` for a method we don't know.
///
/// Unknown methods are expected, not exceptional: extensions use a `_` prefix
/// and new protocol versions add methods. Callers should forward what they
/// can't classify.
pub fn side(method: &str) -> Option<Side> {
    use {agent as a, client as c};
    Some(match method {
        a::INITIALIZE
        | a::AUTHENTICATE
        | a::LOGOUT
        | a::SESSION_NEW
        | a::SESSION_LOAD
        | a::SESSION_LIST
        | a::SESSION_DELETE
        | a::SESSION_RESUME
        | a::SESSION_CLOSE
        | a::SESSION_FORK
        | a::SESSION_SET_MODE
        | a::SESSION_SET_CONFIG_OPTION
        | a::SESSION_PROMPT
        | a::SESSION_CANCEL => Side::Agent,

        c::SESSION_UPDATE
        | c::SESSION_REQUEST_PERMISSION
        | c::FS_READ_TEXT_FILE
        | c::FS_WRITE_TEXT_FILE
        | c::TERMINAL_CREATE
        | c::TERMINAL_OUTPUT
        | c::TERMINAL_KILL
        | c::TERMINAL_RELEASE
        | c::TERMINAL_WAIT_FOR_EXIT
        | c::ELICITATION_CREATE
        | c::ELICITATION_COMPLETE
        | c::MCP_CONNECT
        | c::MCP_DISCONNECT => Side::Client,

        // `mcp/message` is the one method both sides implement, so it is
        // matched after the two lists above and resolves to `Either`.
        protocol::CANCEL_REQUEST | "mcp/message" => Side::Either,

        _ => return None,
    })
}

/// Whether `method` is a client method this server answers itself instead of
/// forwarding to the browser.
///
/// The browser is the ACP client, but it has no filesystem and no ability to
/// spawn processes — the workspace lives on the server. These are the methods
/// the server intercepts and services locally.
pub fn is_server_provided_capability(method: &str) -> bool {
    matches!(
        method,
        client::FS_READ_TEXT_FILE
            | client::FS_WRITE_TEXT_FILE
            | client::TERMINAL_CREATE
            | client::TERMINAL_OUTPUT
            | client::TERMINAL_KILL
            | client::TERMINAL_RELEASE
            | client::TERMINAL_WAIT_FOR_EXIT
    )
}

/// Whether `method` is an ACP extension. Extensions are namespaced by a leading
/// underscore, which is how this project's `_mjx/*` methods coexist with the
/// protocol.
pub fn is_extension(method: &str) -> bool {
    method.starts_with('_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_both_sides() {
        assert_eq!(side("session/prompt"), Some(Side::Agent));
        assert_eq!(side("session/update"), Some(Side::Client));
        assert_eq!(side("terminal/create"), Some(Side::Client));
        assert_eq!(side("$/cancel_request"), Some(Side::Either));
        assert_eq!(side("mcp/message"), Some(Side::Either));
    }

    #[test]
    fn unknown_and_extension_methods_are_unclassified() {
        assert_eq!(side("_mjx/terminal/output"), None);
        assert_eq!(side("providers/list"), None);
        assert!(is_extension("_mjx/agent/info"));
        assert!(!is_extension("session/update"));
    }

    #[test]
    fn server_provided_capabilities_are_exactly_fs_and_terminal() {
        for method in [
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/kill",
            "terminal/release",
            "terminal/wait_for_exit",
        ] {
            assert!(is_server_provided_capability(method), "{method}");
            assert_eq!(side(method), Some(Side::Client), "{method}");
        }
        // These stay with the browser: they need a human.
        assert!(!is_server_provided_capability("session/request_permission"));
        assert!(!is_server_provided_capability("elicitation/create"));
        assert!(!is_server_provided_capability("session/update"));
    }
}
