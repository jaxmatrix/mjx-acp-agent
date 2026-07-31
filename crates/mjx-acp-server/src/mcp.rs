//! Offering the configured MCP servers to the agent.
//!
//! The browser is the ACP client, so `mcpServers` is nominally its to fill in —
//! but the servers are configured in `mjx.toml`, their paths are resolved
//! against it, and their credentials must not travel to a browser to be sent
//! straight back. So the relay adds them on the way past, the same way it adds
//! the capabilities the server provides on the browser's behalf.
//!
//! Everything here is pure: a frame in, a frame out, and no I/O.

use mjx_acp_core::{Frame, acp, method};
use serde_json::Value;

use crate::config::{McpServerConfig, McpServerKind};

/// Which MCP transports the agent said it supports.
///
/// Stdio is absent because it is not optional: every ACP agent must support it,
/// so there is nothing to record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transports {
    pub http: bool,
    pub sse: bool,
    /// MCP over ACP — the agent will call `mcp/connect` rather than connect to
    /// anything itself.
    pub acp: bool,
}

impl Transports {
    /// Reads the transports out of an `initialize` result.
    ///
    /// Navigates the JSON rather than deserializing `InitializeResponse`: this
    /// came off a socket, and an agent whose response we cannot fully model must
    /// still get its stdio servers rather than none.
    pub fn from_initialize_result(result: &serde_json::value::RawValue) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(result.get()) else {
            return Self::default();
        };
        let caps = &value["agentCapabilities"]["mcpCapabilities"];
        Self {
            http: caps["http"] == Value::Bool(true),
            sse: caps["sse"] == Value::Bool(true),
            acp: caps["acp"] == Value::Bool(true),
        }
    }
}

/// Why a configured server cannot be offered to this agent, if it cannot.
///
/// A reason rather than a bare bool, because the agent silently drops entries it
/// does not understand (`mcpServers` deserializes with `VecSkipError`), so this
/// is the only place anyone can be told.
pub fn skip_reason(server: &McpServerConfig, transports: &Transports) -> Option<String> {
    if let Some(reason) = &server.unavailable {
        return Some(reason.clone());
    }
    let supported = match server.kind {
        // Mandatory for every agent, so there is nothing to check.
        McpServerKind::Stdio(_) => true,
        McpServerKind::Http(_) => transports.http,
        McpServerKind::Sse(_) => transports.sse,
        McpServerKind::Acp(_) => transports.acp,
    };
    if supported {
        None
    } else {
        // Deliberately not downgraded to stdio. An `acp` server is configured
        // that way precisely so its credentials stay here; handing the agent the
        // command instead would give away the thing the setting protects.
        Some(format!(
            "this agent did not declare the `{}` MCP transport",
            server.transport()
        ))
    }
}

/// One configured server as the protocol spells it.
///
/// Built through the schema's own types so the wire shape is theirs and not
/// ours — note that stdio is untagged and so carries no `type`, while the others
/// do.
fn to_wire(server: &McpServerConfig) -> Option<Value> {
    let wire = match &server.kind {
        McpServerKind::Stdio(launch) => acp::McpServer::Stdio(
            acp::McpServerStdio::new(&server.name, launch.command.clone())
                .args(launch.args.clone())
                .env(
                    launch
                        .env
                        .iter()
                        .map(|(name, value)| acp::EnvVariable::new(name, value))
                        .collect(),
                ),
        ),
        McpServerKind::Http(endpoint) => acp::McpServer::Http(
            acp::McpServerHttp::new(&server.name, &endpoint.url).headers(headers(endpoint)),
        ),
        McpServerKind::Sse(endpoint) => acp::McpServer::Sse(
            acp::McpServerSse::new(&server.name, &endpoint.url).headers(headers(endpoint)),
        ),
        // The name is the id: it is unique by construction, and `mcp/connect`
        // then arrives naming something we can look up.
        McpServerKind::Acp(_) => acp::McpServer::Acp(acp::McpServerAcp::new(
            &server.name,
            acp::McpServerAcpId::new(server.name.clone()),
        )),
    };
    serde_json::to_value(wire).ok()
}

fn headers(endpoint: &crate::config::McpEndpoint) -> Vec<acp::HttpHeader> {
    endpoint
        .headers
        .iter()
        .map(|(name, value)| acp::HttpHeader::new(name, value))
        .collect()
}

/// Whether `method` is a request that opens a session, and so carries
/// `mcpServers`.
///
/// All four, not just `session/new`: the schema puts `mcpServers` on every one
/// of them and documents it as the complete resulting list, so a fork or a
/// resume that omitted it would quietly leave the conversation with no tools.
fn opens_a_session(method: &str) -> bool {
    matches!(
        method,
        method::agent::SESSION_NEW
            | method::agent::SESSION_LOAD
            | method::agent::SESSION_FORK
            | method::agent::SESSION_RESUME
    )
}

/// Builds the rewrite that adds the configured servers to a session request, or
/// `None` if there is nothing to add.
///
/// Merges by name rather than replacing, so a client that configures its own
/// servers keeps them and never receives two entries with one name. Edits the
/// JSON as a value rather than through `NewSessionRequest`, so `_meta` and
/// anything else the client sent survives the round trip.
pub fn merge_mcp_servers(
    frame: &Frame,
    servers: &[McpServerConfig],
    transports: &Transports,
) -> Option<Frame> {
    let Frame::Request { id, method, params } = frame else {
        return None;
    };
    if servers.is_empty() || !opens_a_session(method) {
        return None;
    }

    let mut value: Value = params
        .as_deref()
        .and_then(|params| serde_json::from_str(params.get()).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let object = value.as_object_mut()?;

    // `session/fork` and `session/resume` are sent without the key at all, so it
    // has to be created; `session/new` sends an empty array.
    let existing = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(existing) = existing.as_array_mut() else {
        // Not an array: the client is speaking a shape we do not model, and
        // rewriting it would be a guess.
        tracing::warn!(method, "`mcpServers` is not an array; leaving it alone");
        return None;
    };

    let declared: Vec<String> = existing
        .iter()
        .filter_map(|server| server["name"].as_str().map(str::to_owned))
        .collect();

    let mut added = 0;
    for server in servers {
        if declared.iter().any(|name| name == &server.name) {
            continue;
        }
        if let Some(reason) = skip_reason(server, transports) {
            tracing::warn!(server = %server.name, %reason, "not offering an MCP server");
            continue;
        }
        if let Some(wire) = to_wire(server) {
            existing.push(wire);
            added += 1;
        }
    }
    if added == 0 {
        return None;
    }

    Some(Frame::Request {
        id: id.clone(),
        method: method.clone(),
        params: Some(serde_json::value::to_raw_value(&value).ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpEndpoint, McpLaunch};
    use mjx_acp_core::RequestId;
    use serde_json::json;

    fn stdio(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            kind: McpServerKind::Stdio(McpLaunch {
                command: "npx".into(),
                args: vec!["-y".into(), "server".into()],
                env: vec![("TOKEN".into(), "sh-hh".into())],
            }),
            unavailable: None,
        }
    }

    fn http(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            kind: McpServerKind::Http(McpEndpoint {
                url: "https://example.com/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer t".into())],
            }),
            unavailable: None,
        }
    }

    fn sse(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            kind: McpServerKind::Sse(McpEndpoint {
                url: "https://example.com/sse".into(),
                headers: vec![],
            }),
            unavailable: None,
        }
    }

    fn over_acp(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            kind: McpServerKind::Acp(McpLaunch {
                command: "/opt/private-mcp".into(),
                args: vec![],
                env: vec![("API_KEY".into(), "secret".into())],
            }),
            unavailable: None,
        }
    }

    fn request(method: &str, params: Value) -> Frame {
        Frame::Request {
            id: RequestId::Number(1),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        }
    }

    fn everything() -> Transports {
        Transports {
            http: true,
            sse: true,
            acp: true,
        }
    }

    fn servers_of(frame: &Frame) -> Vec<Value> {
        let params: Value = serde_json::from_str(frame.params().unwrap().get()).unwrap();
        params["mcpServers"].as_array().cloned().unwrap()
    }

    #[test]
    fn a_configured_server_is_added_in_the_shape_the_schema_defines() {
        let frame = request(
            method::agent::SESSION_NEW,
            json!({ "cwd": "/w", "mcpServers": [] }),
        );
        let rewritten =
            merge_mcp_servers(&frame, &[stdio("git"), http("docs")], &everything()).unwrap();

        let servers = servers_of(&rewritten);
        // The answer must deserialize as the protocol's own type, or we have
        // invented a dialect.
        let parsed: Vec<acp::McpServer> = serde_json::from_value(json!(servers)).unwrap();
        assert_eq!(parsed.len(), 2);

        // Stdio is untagged, so it carries no discriminator; the others do.
        assert!(servers[0].get("type").is_none(), "{}", servers[0]);
        assert_eq!(servers[0]["name"], "git");
        assert_eq!(servers[0]["command"], "npx");
        assert_eq!(servers[0]["env"][0]["name"], "TOKEN");
        assert_eq!(servers[1]["type"], "http");
        assert_eq!(servers[1]["headers"][0]["name"], "Authorization");
    }

    #[test]
    fn the_client_keeps_what_it_declared_and_never_sees_a_name_twice() {
        let frame = request(
            method::agent::SESSION_NEW,
            json!({
                "cwd": "/w",
                "mcpServers": [{ "name": "git", "command": "/usr/bin/its-own-git", "args": [], "env": [] }],
                "_meta": { "keep": true },
            }),
        );
        let rewritten =
            merge_mcp_servers(&frame, &[stdio("git"), stdio("other")], &everything()).unwrap();

        let servers = servers_of(&rewritten);
        assert_eq!(servers.len(), 2, "{servers:?}");
        // The client's own entry wins: it is the one nearer the user.
        assert_eq!(servers[0]["command"], "/usr/bin/its-own-git");
        assert_eq!(servers[1]["name"], "other");

        // Anything else the client sent is still there.
        let params: Value = serde_json::from_str(rewritten.params().unwrap().get()).unwrap();
        assert_eq!(params["_meta"]["keep"], true);
        assert_eq!(params["cwd"], "/w");
    }

    #[test]
    fn every_session_opening_method_is_rewritten_and_nothing_else_is() {
        for method in [
            method::agent::SESSION_NEW,
            method::agent::SESSION_LOAD,
            method::agent::SESSION_FORK,
            method::agent::SESSION_RESUME,
        ] {
            // Fork and resume send no `mcpServers` at all, so the key has to be
            // created rather than merged into.
            let frame = request(method, json!({ "sessionId": "s1" }));
            let rewritten = merge_mcp_servers(&frame, &[stdio("git")], &everything())
                .unwrap_or_else(|| panic!("{method} should carry the configured servers"));
            assert_eq!(servers_of(&rewritten).len(), 1, "{method}");
        }

        for method in [
            method::agent::SESSION_PROMPT,
            method::agent::INITIALIZE,
            method::agent::SESSION_LIST,
        ] {
            let frame = request(method, json!({ "sessionId": "s1" }));
            assert!(
                merge_mcp_servers(&frame, &[stdio("git")], &everything()).is_none(),
                "{method} must be left alone"
            );
        }
    }

    #[test]
    fn a_transport_the_agent_did_not_declare_is_skipped_with_a_reason() {
        // What an agent that declared no `mcpCapabilities` at all supports.
        let stdio_only = Transports::default();

        let frame = request(method::agent::SESSION_NEW, json!({ "mcpServers": [] }));
        let rewritten = merge_mcp_servers(
            &frame,
            &[stdio("git"), http("docs"), sse("feed"), over_acp("private")],
            &stdio_only,
        )
        .unwrap();
        let servers = servers_of(&rewritten);
        assert_eq!(servers.len(), 1, "only stdio survives: {servers:?}");
        assert_eq!(servers[0]["name"], "git");

        for (server, transport) in [
            (http("docs"), "http"),
            (sse("feed"), "sse"),
            (over_acp("private"), "acp"),
        ] {
            let reason = skip_reason(&server, &stdio_only).unwrap();
            assert!(reason.contains(transport), "{reason}");
        }
        // Stdio is mandatory, so it is never the thing that was missing.
        assert!(skip_reason(&stdio("git"), &stdio_only).is_none());
    }

    #[test]
    fn a_server_missing_its_credential_is_skipped_and_says_which_one() {
        let mut server = http("docs");
        server.unavailable = Some("`DOCS_TOKEN` is not set".into());

        let reason = skip_reason(&server, &everything()).unwrap();
        assert!(reason.contains("DOCS_TOKEN"), "{reason}");

        let frame = request(method::agent::SESSION_NEW, json!({ "mcpServers": [] }));
        assert!(
            merge_mcp_servers(&frame, &[server], &everything()).is_none(),
            "a server that cannot work must not be offered"
        );
    }

    #[test]
    fn an_acp_server_is_offered_by_id_and_never_by_command() {
        let frame = request(method::agent::SESSION_NEW, json!({ "mcpServers": [] }));
        let rewritten = merge_mcp_servers(&frame, &[over_acp("private")], &everything()).unwrap();

        let servers = servers_of(&rewritten);
        assert_eq!(servers[0]["type"], "acp");
        assert_eq!(servers[0]["serverId"], "private");
        // The whole point of the transport: the agent learns a name, not a
        // command and not a key.
        let line = rewritten.to_line();
        assert!(!line.contains("private-mcp"), "{line}");
        assert!(!line.contains("secret"), "{line}");
    }

    #[test]
    fn nothing_configured_means_nothing_touched() {
        let frame = request(method::agent::SESSION_NEW, json!({ "mcpServers": [] }));
        assert!(merge_mcp_servers(&frame, &[], &everything()).is_none());

        // Neither is a notification, or a response, whatever it is named.
        let note = Frame::notification(method::agent::SESSION_NEW, &json!({})).unwrap();
        assert!(merge_mcp_servers(&note, &[stdio("git")], &everything()).is_none());
    }

    #[test]
    fn a_shape_we_do_not_model_is_forwarded_untouched() {
        for params in [
            json!({ "mcpServers": "all of them" }),
            json!("not an object"),
        ] {
            let frame = request(method::agent::SESSION_NEW, params.clone());
            assert!(
                merge_mcp_servers(&frame, &[stdio("git")], &everything()).is_none(),
                "{params} should be left alone"
            );
        }

        // No params at all is not a malformed shape, though: a `session/new`
        // with nothing in it still wants its servers.
        let bare = Frame::Request {
            id: RequestId::Number(1),
            method: method::agent::SESSION_NEW.into(),
            params: None,
        };
        let rewritten = merge_mcp_servers(&bare, &[stdio("git")], &everything()).unwrap();
        assert_eq!(servers_of(&rewritten).len(), 1);
    }

    #[test]
    fn the_transports_are_read_out_of_an_initialize_result() {
        let result = serde_json::value::to_raw_value(&json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "mcpCapabilities": { "http": true, "sse": false, "acp": true },
            },
        }))
        .unwrap();
        let transports = Transports::from_initialize_result(&result);
        assert_eq!(
            transports,
            Transports {
                http: true,
                sse: false,
                acp: true
            }
        );

        // An agent that says nothing supports stdio and nothing else — and so
        // does one whose response we cannot parse at all.
        for result in [json!({ "agentCapabilities": {} }), json!("nonsense")] {
            let result = serde_json::value::to_raw_value(&result).unwrap();
            assert_eq!(
                Transports::from_initialize_result(&result),
                Transports::default()
            );
        }
    }
}
