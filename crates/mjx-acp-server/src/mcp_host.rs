//! Answering `mcp/connect`, `mcp/disconnect` and `mcp/message`.
//!
//! For a server configured `transport = "acp"`, *this* process holds the MCP
//! connection and the agent reaches it through ACP. The agent is still the MCP
//! client; this is only the transport under it, which is why the work here is
//! bookkeeping rather than protocol:
//!
//! * the child is spawned on `mcp/connect` and reaped on `mcp/disconnect`, or
//!   when the browser's connection ends;
//! * an `mcp/message` from the agent becomes an MCP JSON-RPC message on the
//!   child's stdin, and the child's answer becomes the ACP response;
//! * anything the child says unprompted goes the other way, to the agent.
//!
//! Ids never cross. The agent's belong to the browser's id space by way of the
//! relay, the MCP server's belong to `mjx-mcp`, and the requests this module puts
//! *to* the agent get ids of their own — see [`AGENT_ID_PREFIX`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mjx_acp_core::{Frame, JsonRpcError, RequestId, ResponsePayload, acp, method};
use mjx_mcp::{FromServer, McpServer};
use serde_json::{Value, json};

use crate::config::{McpLaunch, McpServerConfig, McpServerKind};
use crate::relay::Outbox;

/// Marks a request this module put to the agent.
///
/// The relay maps agent ids back to browser ids and drops what it cannot place;
/// these have no browser behind them, so they are claimed by prefix before it
/// gets that far. A string, because every id the relay mints is a number and the
/// two must never collide.
const AGENT_ID_PREFIX: &str = "mjx-mcp-";

/// The MCP servers this connection holds on the agent's behalf.
pub struct McpHost {
    /// Configured `acp` servers, by the id the agent was given — which is the
    /// name from `mjx.toml`.
    configured: HashMap<String, McpLaunch>,
    /// Where a spawned server runs: the session's directory, as a terminal's is.
    cwd: PathBuf,
    /// Live connections, by the id handed back from `mcp/connect`.
    live: std::sync::Mutex<HashMap<String, Arc<McpServer>>>,
    /// Requests forwarded to the agent, so its answer can be routed back to the
    /// MCP server that asked: our id → (that server, the id *it* used).
    awaiting_agent: std::sync::Mutex<HashMap<RequestId, (Arc<McpServer>, RequestId)>>,
    next_id: AtomicU64,
}

impl McpHost {
    /// Builds a host over whichever configured servers are `transport = "acp"`.
    pub fn new(servers: &[McpServerConfig], cwd: PathBuf) -> Self {
        let configured = servers
            .iter()
            // A server that cannot work was not offered to the agent either, so
            // it can never be connected to.
            .filter(|server| server.unavailable.is_none())
            .filter_map(|server| match &server.kind {
                McpServerKind::Acp(launch) => Some((server.name.clone(), launch.clone())),
                _ => None,
            })
            .collect();
        Self {
            configured,
            cwd,
            live: std::sync::Mutex::new(HashMap::new()),
            awaiting_agent: std::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Whether anything is configured to be hosted here.
    ///
    /// With nothing configured the interceptor forwards `mcp/*` untouched, so an
    /// agent that reached for MCP-over-ACP uninvited meets a client that does not
    /// implement it rather than a server pretending to.
    pub fn is_configured(&self) -> bool {
        !self.configured.is_empty()
    }

    /// Answers one `mcp/*` request from the agent.
    pub async fn handle(
        self: &Arc<Self>,
        method: &str,
        params: &Value,
        outbox: &Outbox,
    ) -> Result<Value, JsonRpcError> {
        use mjx_acp_core::method::client as m;

        match method {
            m::MCP_CONNECT => self.connect(params, outbox),
            m::MCP_DISCONNECT => self.disconnect(params).await,
            m::MCP_MESSAGE => self.message(params).await,
            other => Err(JsonRpcError::method_not_found(other)),
        }
    }

    /// Starts the named server and hands back a connection id.
    ///
    /// One child per `mcp/connect` rather than one per configured server: an
    /// agent that connects twice gets two connections, which is what the
    /// protocol says it gets, and neither can see the other's state.
    fn connect(self: &Arc<Self>, params: &Value, outbox: &Outbox) -> Result<Value, JsonRpcError> {
        let request: acp::ConnectMcpRequest = serde_json::from_value(params.clone())
            .map_err(|err| JsonRpcError::invalid_params(format!("{err}")))?;
        let server_id = request.server_id.0.to_string();

        let launch = self.configured.get(&server_id).ok_or_else(|| {
            // Not an internal error: the agent asked for something that was
            // never offered, which is the one thing it could have got wrong.
            JsonRpcError::invalid_params(format!(
                "no MCP server called `{server_id}` is configured"
            ))
        })?;

        let (server, mut events) = McpServer::spawn(
            &server_id,
            &launch.command,
            &launch.args,
            &launch.env,
            &self.cwd,
        )
        .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;

        let connection_id = format!("mcp-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.live().insert(connection_id.clone(), server.clone());

        // Everything the server says unprompted goes to the agent, which is the
        // MCP client and the only thing that can act on it.
        let host = self.clone();
        let outbox = outbox.clone();
        let connection = connection_id.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                host.to_agent(&outbox, &connection, &server, event);
            }
        });

        tracing::info!(
            server = %server_id,
            connection = %connection_id,
            "holding an MCP server for the agent"
        );
        Ok(json!({ "connectionId": connection_id }))
    }

    /// Ends a connection and reaps its child.
    async fn disconnect(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let server = self.take(params)?;
        server.shutdown().await;
        Ok(json!({}))
    }

    /// Carries one MCP message to the server, and its answer back.
    async fn message(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let server = self.find(params)?;
        let method = params["method"]
            .as_str()
            .ok_or_else(|| JsonRpcError::invalid_params("`method` is required"))?;

        let result = server.request(method, inner_params(params)).await?;
        // The response *is* the inner MCP result: `MessageMcpResponse` is
        // `#[serde(transparent)]`, so there is no envelope to add.
        serde_json::from_str(result.get()).map_err(|err| {
            JsonRpcError::internal(format!("the MCP server answered unreadable JSON: {err}"))
        })
    }

    /// Carries a one-way MCP message to the server. Nothing comes back, so a
    /// message for a connection that has gone is dropped rather than reported.
    pub fn notify(&self, params: &Value) {
        let Ok(server) = self.find(params) else {
            tracing::debug!("an MCP notification for a connection that is not open");
            return;
        };
        if let Some(method) = params["method"].as_str() {
            server.notify(method, inner_params(params));
        }
    }

    /// Sends the agent something the MCP server said on its own.
    fn to_agent(
        &self,
        outbox: &Outbox,
        connection_id: &str,
        server: &Arc<McpServer>,
        event: FromServer,
    ) {
        match event {
            FromServer::Notification { method, params } => {
                let envelope = envelope(connection_id, &method, params.as_deref());
                match Frame::notification(method::agent::MCP_MESSAGE, &envelope) {
                    Ok(frame) => outbox.to_agent(&frame),
                    Err(err) => tracing::error!(%err, "could not build an mcp/message"),
                }
            }
            FromServer::Request {
                id: server_id,
                method,
                params,
            } => {
                let id = RequestId::String(format!(
                    "{AGENT_ID_PREFIX}{}",
                    self.next_id.fetch_add(1, Ordering::Relaxed)
                ));
                let envelope = envelope(connection_id, &method, params.as_deref());
                self.awaiting_agent
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.clone(), (server.clone(), server_id));
                outbox.to_agent(&Frame::Request {
                    id,
                    method: method::agent::MCP_MESSAGE.into(),
                    params: serde_json::value::to_raw_value(&envelope).ok(),
                });
            }
        }
    }

    /// Whether `id` answers a request this module put to the agent, routing it
    /// back to the MCP server that asked if it does.
    pub fn claim_response(&self, id: &RequestId, payload: &ResponsePayload) -> bool {
        let RequestId::String(text) = id else {
            return false;
        };
        if !text.starts_with(AGENT_ID_PREFIX) {
            return false;
        }
        // Ours by name but not on the list means it was already answered, or the
        // connection went away underneath it. Either way it is not the browser's,
        // so claim it rather than let it be forwarded.
        if let Some((server, server_id)) = self
            .awaiting_agent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id)
        {
            server.answer(
                server_id,
                match payload {
                    ResponsePayload::Result(result) => Ok(result.clone()),
                    ResponsePayload::Error(error) => Err(error.clone()),
                },
            );
        }
        true
    }

    /// Ends every connection. Called when the browser's connection does.
    pub async fn shutdown_all(&self) {
        let servers: Vec<Arc<McpServer>> = self.live().drain().map(|(_, server)| server).collect();
        for server in servers {
            server.shutdown().await;
        }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<McpServer>>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The server a message names.
    fn find(&self, params: &Value) -> Result<Arc<McpServer>, JsonRpcError> {
        let id = params["connectionId"].as_str().unwrap_or_default();
        self.live().get(id).cloned().ok_or_else(|| {
            JsonRpcError::invalid_params(format!("no MCP connection called `{id}` is open"))
        })
    }

    /// The same, removing it.
    fn take(&self, params: &Value) -> Result<Arc<McpServer>, JsonRpcError> {
        let id = params["connectionId"].as_str().unwrap_or_default();
        self.live().remove(id).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("no MCP connection called `{id}` is open"))
        })
    }
}

/// The inner MCP params, which are absent as often as not.
fn inner_params(params: &Value) -> Option<Box<serde_json::value::RawValue>> {
    match &params["params"] {
        Value::Null => None,
        inner => serde_json::value::to_raw_value(inner).ok(),
    }
}

/// The ACP envelope for a message going to the agent.
fn envelope(
    connection_id: &str,
    method: &str,
    params: Option<&serde_json::value::RawValue>,
) -> Value {
    let mut envelope = json!({ "connectionId": connection_id, "method": method });
    if let Some(params) = params
        && let Ok(params) = serde_json::from_str::<Value>(params.get())
        && !params.is_null()
    {
        envelope["params"] = params;
    }
    envelope
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpEndpoint;

    fn over_acp(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            kind: McpServerKind::Acp(McpLaunch {
                command: command.into(),
                args: vec![],
                env: vec![],
            }),
            unavailable: None,
        }
    }

    fn host(servers: &[McpServerConfig]) -> Arc<McpHost> {
        Arc::new(McpHost::new(servers, PathBuf::from(".")))
    }

    #[test]
    fn only_acp_servers_are_hosted() {
        // The others are the agent's to connect to; hosting one here would mean
        // spawning a server the agent is also spawning.
        let stdio = McpServerConfig {
            name: "git".into(),
            kind: McpServerKind::Stdio(McpLaunch {
                command: "npx".into(),
                args: vec![],
                env: vec![],
            }),
            unavailable: None,
        };
        let http = McpServerConfig {
            name: "docs".into(),
            kind: McpServerKind::Http(McpEndpoint {
                url: "https://h/mcp".into(),
                headers: vec![],
            }),
            unavailable: None,
        };
        assert!(!host(&[stdio, http]).is_configured());
        assert!(host(&[over_acp("private", "/bin/true")]).is_configured());

        // A server missing its credential is not connectable either: it was
        // never offered, so an agent naming it is naming something that does not
        // exist as far as it is concerned.
        let mut broken = over_acp("private", "/bin/true");
        broken.unavailable = Some("`KEY` is not set".into());
        assert!(!host(&[broken]).is_configured());
    }

    #[tokio::test]
    async fn a_server_that_was_never_configured_is_refused_as_invalid_params() {
        let host = host(&[over_acp("private", "/bin/true")]);
        let (outbox, _rx) = crate::relay::Outbox::for_test();

        let err = host
            .handle(
                method::client::MCP_CONNECT,
                &json!({ "serverId": "somebody-elses" }),
                &outbox,
            )
            .await
            .unwrap_err();
        // -32602: the agent asked wrongly, nothing here is broken.
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("somebody-elses"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_message_for_a_connection_that_is_not_open_is_refused() {
        let host = host(&[over_acp("private", "/bin/true")]);
        let (outbox, _rx) = crate::relay::Outbox::for_test();

        for method in [method::client::MCP_MESSAGE, method::client::MCP_DISCONNECT] {
            let err = host
                .handle(
                    method,
                    &json!({ "connectionId": "mcp-99", "method": "tools/list" }),
                    &outbox,
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, -32602, "{method}");
        }
    }

    #[test]
    fn only_our_own_ids_are_claimed_back_from_the_agent() {
        let host = host(&[over_acp("private", "/bin/true")]);
        let payload = ResponsePayload::Result(serde_json::value::to_raw_value(&json!({})).unwrap());

        // A number is always the relay's, and belongs to a browser.
        assert!(!host.claim_response(&RequestId::Number(1), &payload));
        assert!(!host.claim_response(&RequestId::String("elicit-3".into()), &payload));
        // Ours by prefix, even with nothing on the list: it has no browser
        // behind it, so forwarding it could only confuse one.
        assert!(host.claim_response(&RequestId::String("mjx-mcp-7".into()), &payload));
    }

    #[test]
    fn the_envelope_omits_params_it_does_not_have() {
        let bare = envelope("mcp-1", "tools/list", None);
        assert_eq!(
            bare.to_string(),
            r#"{"connectionId":"mcp-1","method":"tools/list"}"#
        );

        let with = envelope(
            "mcp-1",
            "tools/call",
            Some(&serde_json::value::to_raw_value(&json!({ "name": "stat" })).unwrap()),
        );
        assert_eq!(with["params"]["name"], "stat");

        // An explicit null is the same as absent, which is what the schema says:
        // "if omitted or set to null, the inner MCP message has no params".
        let nulled = envelope(
            "mcp-1",
            "ping",
            Some(&serde_json::value::to_raw_value(&Value::Null).unwrap()),
        );
        assert!(nulled.get("params").is_none(), "{nulled}");
    }
}
