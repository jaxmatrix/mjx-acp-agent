//! Answers the client methods the browser cannot.
//!
//! The browser is the ACP client, but the workspace is on the server, so
//! `fs/*` and `terminal/*` are handled here and never forwarded. `mcp/*` joins
//! them when an `acp`-transport server is configured, for the same reason: a
//! browser cannot spawn an MCP server either. Two consequences follow, and both
//! are handled below:
//!
//! * The browser's `initialize` has to be rewritten to declare capabilities it
//!   does not itself implement, or the agent would never call them.
//! * The UI would otherwise have no idea any of this happened, so every
//!   outcome is mirrored to the browser as an `_mjx/*` notification.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use mjx_acp_core::{Frame, JsonRpcError, RequestId, acp, ext, method};
use mjx_workspace::{Workspace, WorkspaceError, WorkspaceEvent};
use serde_json::json;

use crate::config::McpServerConfig;
use crate::mcp_host::McpHost;
use crate::relay::{Disposition, Interceptor, Outbox};

/// Serves the filesystem, terminal and MCP capabilities for one connection.
pub struct WorkspaceInterceptor {
    workspace: Arc<Workspace>,
    /// Taken by [`Interceptor::start`], which turns workspace events into
    /// `_mjx/*` notifications.
    events: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<WorkspaceEvent>>>,
    /// The MCP servers held here rather than by the agent. Idle unless something
    /// in `mjx.toml` asked for it.
    mcp: Arc<McpHost>,
}

impl WorkspaceInterceptor {
    /// Builds an interceptor over `roots`, with `cwd` as the default working
    /// directory for terminals and for any MCP server hosted here.
    pub fn new(roots: Vec<PathBuf>, cwd: PathBuf, mcp_servers: &[McpServerConfig]) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            workspace: Arc::new(Workspace::new(roots, cwd.clone(), tx)),
            events: std::sync::Mutex::new(Some(rx)),
            mcp: Arc::new(McpHost::new(mcp_servers, cwd)),
        }
    }

    /// The workspace this interceptor serves.
    ///
    /// Shared with the auth interceptor, which starts login terminals in it. One
    /// workspace per connection is the point: a login terminal has to be in the
    /// same set as the agent's, or it would not be released when the connection
    /// ends and would outlive everything that could stop it.
    pub fn workspace(&self) -> Arc<Workspace> {
        self.workspace.clone()
    }

    /// Whether this frame is MCP-over-ACP traffic we have undertaken to answer.
    fn is_ours(&self, method: &str) -> bool {
        method::is_server_provided_capability(method)
            || (method::is_mcp_over_acp(method) && self.mcp.is_configured())
    }
}

impl Interceptor for WorkspaceInterceptor {
    fn start(&self, outbox: &Outbox) {
        let Some(mut events) = self.events.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let outbox = outbox.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                mirror(&outbox, event);
            }
        });
    }

    fn on_client_frame(&self, frame: &Frame, _outbox: &Outbox) -> Disposition {
        // Declare the capabilities we provide on the browser's behalf. Without
        // this the agent would never call fs/* or terminal/* at all.
        match crate::relay::merge_client_capabilities(frame, true, true) {
            Some(rewritten) => Disposition::Rewrite(rewritten),
            None => Disposition::Forward,
        }
    }

    fn on_agent_frame(&self, frame: &Frame, outbox: &Outbox) -> Disposition {
        match frame {
            // A one-way MCP message. There is nothing to answer, so it is
            // delivered here and goes no further: the browser has no MCP server
            // to give it to.
            Frame::Notification { method, params } if self.is_ours(method) => {
                let params: serde_json::Value = params
                    .as_deref()
                    .and_then(|params| serde_json::from_str(params.get()).ok())
                    .unwrap_or(serde_json::Value::Null);
                self.mcp.notify(&params);
                return Disposition::Intercept;
            }
            // The agent answering something this server asked *it* — an MCP
            // server's own request, forwarded on. The browser never saw the
            // question and must not see the answer.
            Frame::Response { id, payload } if self.mcp.claim_response(id, payload) => {
                return Disposition::Intercept;
            }
            _ => {}
        }

        let Frame::Request { id, method, .. } = frame else {
            return Disposition::Forward;
        };
        if !self.is_ours(method) {
            return Disposition::Forward;
        }

        // Spawned rather than awaited: `terminal/wait_for_exit` blocks for as
        // long as the command runs, and the relay must keep pumping frames
        // meanwhile — not least so the agent can be cancelled.
        let workspace = self.workspace.clone();
        let mcp = self.mcp.clone();
        let outbox = outbox.clone();
        let id = id.clone();
        let method = method.clone();
        let params = frame.params().map(|p| p.to_owned());

        tokio::spawn(async move {
            let frame = Frame::Request {
                id: id.clone(),
                method: method.clone(),
                params,
            };
            let reply = match run(&workspace, &mcp, &method, &frame, &outbox).await {
                Ok(result) => Frame::result(id, &result).unwrap_or_else(|err| {
                    Frame::error(RequestId::Null, JsonRpcError::internal(err))
                }),
                Err(error) => {
                    tracing::debug!(%method, error = %error.message, "refused a client request");
                    Frame::error(id, error)
                }
            };
            outbox.to_agent(&reply);
        });

        Disposition::Intercept
    }

    fn on_extension_request(&self, frame: &Frame, outbox: &Outbox) -> bool {
        let Frame::Request { id, method, .. } = frame else {
            return false;
        };

        // Driving a terminal the browser is already watching. Answered here
        // rather than in the auth interceptor that opens the login, because the
        // terminals are this one's — and because the *refusal* is the important
        // half: `Terminals` allows a write only on one this server opened for a
        // login, so a browser cannot type into anything the agent started.
        let outcome = match method.as_str() {
            ext::TERMINAL_INPUT => frame
                .params_as::<ext::TerminalInput>()
                .map_err(JsonRpcError::invalid_params)
                .and_then(|params| {
                    let params =
                        params.ok_or_else(|| JsonRpcError::invalid_params("missing params"))?;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&params.bytes)
                        .map_err(JsonRpcError::invalid_params)?;
                    self.workspace
                        .write_terminal(&params.terminal_id, &bytes)
                        .map_err(to_rpc_error)
                }),
            ext::TERMINAL_RESIZE => frame
                .params_as::<ext::TerminalResize>()
                .map_err(JsonRpcError::invalid_params)
                .and_then(|params| {
                    let params =
                        params.ok_or_else(|| JsonRpcError::invalid_params("missing params"))?;
                    self.workspace
                        .resize_terminal(&params.terminal_id, params.rows, params.cols)
                        .map_err(to_rpc_error)
                }),
            _ => return false,
        };

        let reply = match outcome {
            Ok(()) => Frame::result(id.clone(), &json!({}))
                .unwrap_or_else(|err| Frame::error(id.clone(), JsonRpcError::internal(err))),
            Err(error) => {
                tracing::debug!(%method, error = %error.message, "refused a terminal request");
                Frame::error(id.clone(), error)
            }
        };
        outbox.to_browser(&reply);
        true
    }

    fn stop(&self) {
        // Redundant with `Workspace`'s own Drop, but the interceptor may outlive
        // the connection by a moment and a stray build should not.
        self.workspace.release_all_terminals();

        // An MCP server outliving the agent that asked for it would be a leaked
        // subprocess holding whatever it holds. Spawned rather than awaited
        // because `stop` cannot block: it is called from the relay's shutdown
        // path, which the reaper drives.
        let mcp = self.mcp.clone();
        tokio::spawn(async move { mcp.shutdown_all().await });
    }
}

/// Runs one client request against whichever of the two owns it.
async fn run(
    workspace: &Workspace,
    mcp: &Arc<McpHost>,
    method: &str,
    frame: &Frame,
    outbox: &Outbox,
) -> Result<serde_json::Value, JsonRpcError> {
    if method::is_mcp_over_acp(method) {
        let params: serde_json::Value = frame
            .params()
            .and_then(|params| serde_json::from_str(params.get()).ok())
            .unwrap_or(serde_json::Value::Null);
        return mcp.handle(method, &params, outbox).await;
    }
    handle(workspace, method, frame).await
}

/// Runs one client request against the workspace.
async fn handle(
    workspace: &Workspace,
    method: &str,
    frame: &Frame,
) -> Result<serde_json::Value, JsonRpcError> {
    use mjx_acp_core::method::client as m;

    match method {
        m::FS_READ_TEXT_FILE => {
            let request: acp::ReadTextFileRequest = params(frame)?;
            let content = workspace
                .read_text_file(&request.path, request.line, request.limit)
                .map_err(to_rpc_error)?;
            Ok(json!({ "content": content }))
        }

        m::FS_WRITE_TEXT_FILE => {
            let request: acp::WriteTextFileRequest = params(frame)?;
            workspace
                .write_text_file(&request.path, &request.content)
                .map_err(to_rpc_error)?;
            Ok(json!({}))
        }

        m::TERMINAL_CREATE => {
            let request: acp::CreateTerminalRequest = params(frame)?;
            let env: Vec<(String, String)> =
                request.env.into_iter().map(|v| (v.name, v.value)).collect();
            let terminal_id = workspace
                .create_terminal(
                    &request.command,
                    &request.args,
                    &env,
                    request.cwd,
                    request.output_byte_limit.map(|n| n as usize),
                )
                .map_err(to_rpc_error)?;
            Ok(json!({ "terminalId": terminal_id }))
        }

        m::TERMINAL_OUTPUT => {
            let request: acp::TerminalOutputRequest = params(frame)?;
            let (output, truncated, exit) = workspace
                .terminal_output(request.terminal_id.0.as_ref())
                .map_err(to_rpc_error)?;
            let mut result = json!({ "output": output, "truncated": truncated });
            if let Some(exit) = exit {
                result["exitStatus"] = exit_status_json(&exit);
            }
            Ok(result)
        }

        m::TERMINAL_WAIT_FOR_EXIT => {
            let request: acp::WaitForTerminalExitRequest = params(frame)?;
            let exit = workspace
                .wait_for_terminal_exit(request.terminal_id.0.as_ref())
                .await
                .map_err(to_rpc_error)?;
            Ok(exit_status_json(&exit))
        }

        m::TERMINAL_KILL => {
            let request: acp::KillTerminalRequest = params(frame)?;
            workspace
                .kill_terminal(request.terminal_id.0.as_ref())
                .map_err(to_rpc_error)?;
            Ok(json!({}))
        }

        m::TERMINAL_RELEASE => {
            let request: acp::ReleaseTerminalRequest = params(frame)?;
            workspace
                .release_terminal(request.terminal_id.0.as_ref())
                .map_err(to_rpc_error)?;
            Ok(json!({}))
        }

        other => Err(JsonRpcError::method_not_found(other)),
    }
}

/// Deserializes a request's params, or reports why it couldn't.
fn params<T: serde::de::DeserializeOwned>(frame: &Frame) -> Result<T, JsonRpcError> {
    frame
        .params_as::<T>()
        .map_err(JsonRpcError::invalid_params)?
        .ok_or_else(|| JsonRpcError::invalid_params("missing params"))
}

fn to_rpc_error(error: WorkspaceError) -> JsonRpcError {
    JsonRpcError {
        code: error.code(),
        message: error.to_string(),
        data: None,
    }
}

/// The protocol's exit shape: `exitCode` or `signal`, omitting the other.
fn exit_status_json(exit: &mjx_workspace::ExitStatus) -> serde_json::Value {
    let mut value = serde_json::Map::new();
    if let Some(code) = exit.exit_code {
        value.insert("exitCode".into(), code.into());
    }
    if let Some(signal) = &exit.signal {
        value.insert("signal".into(), signal.clone().into());
    }
    serde_json::Value::Object(value)
}

/// Turns a workspace event into the `_mjx/*` notification the UI listens for.
fn mirror(outbox: &Outbox, event: WorkspaceEvent) {
    match event {
        WorkspaceEvent::TerminalCreated {
            terminal_id,
            command,
            args,
            cwd,
        } => outbox.notify_browser(
            ext::TERMINAL_CREATED,
            &ext::TerminalCreated {
                terminal_id,
                command,
                args,
                cwd,
            },
        ),
        WorkspaceEvent::TerminalOutput {
            terminal_id,
            chunk,
            truncated,
        } => outbox.notify_browser(
            ext::TERMINAL_OUTPUT,
            &ext::TerminalOutput {
                terminal_id,
                // Base64 because PTY output is arbitrary bytes: escape
                // sequences, and partial UTF-8 at a chunk boundary. The
                // terminal emulator wants the bytes, not a lossy decode.
                chunk: base64::engine::general_purpose::STANDARD.encode(&chunk),
                truncated,
            },
        ),
        WorkspaceEvent::TerminalExit {
            terminal_id,
            status,
        } => outbox.notify_browser(
            ext::TERMINAL_EXIT,
            &ext::TerminalExit {
                terminal_id,
                exit_code: status.exit_code,
                signal: status.signal,
            },
        ),
        WorkspaceEvent::FileWritten {
            path,
            old_text,
            new_text,
        } => outbox.notify_browser(
            ext::FS_WROTE,
            &ext::FsWrote {
                path,
                old_text,
                new_text,
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: serde_json::Value) -> Frame {
        Frame::Request {
            id: RequestId::Number(1),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        workspace: Workspace,
        events: tokio::sync::mpsc::UnboundedReceiver<WorkspaceEvent>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        Fixture {
            workspace: Workspace::new(vec![root.clone()], root.clone(), tx),
            _dir: dir,
            root,
            events,
        }
    }

    #[tokio::test]
    async fn reads_a_file_and_answers_the_protocol_shape() {
        let f = fixture();
        let frame = request(
            method::client::FS_READ_TEXT_FILE,
            json!({ "sessionId": "s1", "path": f.root.join("a.txt") }),
        );
        let result = handle(&f.workspace, method::client::FS_READ_TEXT_FILE, &frame)
            .await
            .unwrap();

        // The answer must deserialize as the protocol's own response type.
        let response: acp::ReadTextFileResponse = serde_json::from_value(result).unwrap();
        assert_eq!(response.content, "one\ntwo\n");
    }

    #[tokio::test]
    async fn a_read_outside_the_workspace_is_refused_as_invalid_params() {
        let f = fixture();
        let frame = request(
            method::client::FS_READ_TEXT_FILE,
            json!({ "sessionId": "s1", "path": "/etc/passwd" }),
        );
        let err = handle(&f.workspace, method::client::FS_READ_TEXT_FILE, &frame)
            .await
            .unwrap_err();

        // -32602, not -32002: the request was wrong, the file is not "missing".
        // An agent told "not found" for a file it can see would keep retrying.
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("outside the workspace"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn writing_answers_and_mirrors_a_diff() {
        let mut f = fixture();
        let path = f.root.join("a.txt");
        let frame = request(
            method::client::FS_WRITE_TEXT_FILE,
            json!({ "sessionId": "s1", "path": path, "content": "rewritten\n" }),
        );
        let result = handle(&f.workspace, method::client::FS_WRITE_TEXT_FILE, &frame)
            .await
            .unwrap();
        serde_json::from_value::<acp::WriteTextFileResponse>(result).unwrap();

        let WorkspaceEvent::FileWritten { old_text, .. } = f.events.try_recv().unwrap() else {
            panic!("the browser was never told about the write");
        };
        assert_eq!(old_text.as_deref(), Some("one\ntwo\n"));
    }

    #[tokio::test]
    async fn a_terminal_runs_and_reports_its_exit() {
        let f = fixture();

        let create = request(
            method::client::TERMINAL_CREATE,
            json!({ "sessionId": "s1", "command": "echo", "args": ["hi"] }),
        );
        let result = handle(&f.workspace, method::client::TERMINAL_CREATE, &create)
            .await
            .unwrap();
        let created: acp::CreateTerminalResponse = serde_json::from_value(result).unwrap();
        let terminal_id = created.terminal_id.0.to_string();

        let wait = request(
            method::client::TERMINAL_WAIT_FOR_EXIT,
            json!({ "sessionId": "s1", "terminalId": terminal_id }),
        );
        let result = handle(&f.workspace, method::client::TERMINAL_WAIT_FOR_EXIT, &wait)
            .await
            .unwrap();
        let exit: acp::WaitForTerminalExitResponse = serde_json::from_value(result).unwrap();
        // `exitStatus` is flattened into the response, so `exitCode` sits at
        // the top level on the wire.
        assert_eq!(exit.exit_status.exit_code, Some(0));

        let output = request(
            method::client::TERMINAL_OUTPUT,
            json!({ "sessionId": "s1", "terminalId": terminal_id }),
        );
        let result = handle(&f.workspace, method::client::TERMINAL_OUTPUT, &output)
            .await
            .unwrap();
        let output: acp::TerminalOutputResponse = serde_json::from_value(result).unwrap();
        assert!(output.output.contains("hi"), "{:?}", output.output);
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn an_unknown_terminal_is_reported_as_not_found() {
        let f = fixture();
        let frame = request(
            method::client::TERMINAL_OUTPUT,
            json!({ "sessionId": "s1", "terminalId": "nope" }),
        );
        let err = handle(&f.workspace, method::client::TERMINAL_OUTPUT, &frame)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32002, "ACP's resource-not-found code");
    }

    #[tokio::test]
    async fn malformed_params_are_reported_as_invalid_params() {
        let f = fixture();
        let frame = request(method::client::FS_READ_TEXT_FILE, json!({ "wrong": true }));
        let err = handle(&f.workspace, method::client::FS_READ_TEXT_FILE, &frame)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn the_exit_shape_omits_what_did_not_happen() {
        let code_only = exit_status_json(&mjx_workspace::ExitStatus {
            exit_code: Some(0),
            signal: None,
        });
        assert_eq!(code_only.to_string(), r#"{"exitCode":0}"#);

        let signalled = exit_status_json(&mjx_workspace::ExitStatus {
            exit_code: None,
            signal: Some("SIGKILL".into()),
        });
        assert_eq!(signalled.to_string(), r#"{"signal":"SIGKILL"}"#);
    }

    #[tokio::test]
    async fn only_the_server_provided_methods_are_intercepted() {
        let interceptor = WorkspaceInterceptor::new(vec![], PathBuf::from("/tmp"), &[]);
        let (outbox, _rx) = crate::relay::Outbox::for_test();

        // These the browser cannot do; they must be intercepted.
        for method in [
            method::client::FS_READ_TEXT_FILE,
            method::client::TERMINAL_CREATE,
        ] {
            let frame = request(method, json!({}));
            assert!(
                matches!(
                    interceptor.on_agent_frame(&frame, &outbox),
                    Disposition::Intercept
                ),
                "{method} should be intercepted"
            );
        }

        // These need a human, so they must reach the browser.
        for method in [
            method::client::SESSION_REQUEST_PERMISSION,
            method::client::ELICITATION_CREATE,
        ] {
            let frame = request(method, json!({}));
            assert!(
                matches!(
                    interceptor.on_agent_frame(&frame, &outbox),
                    Disposition::Forward
                ),
                "{method} must reach the user"
            );
        }

        // Notifications are never intercepted, whatever they are named.
        let note = Frame::notification(method::client::SESSION_UPDATE, &json!({})).unwrap();
        assert!(matches!(
            interceptor.on_agent_frame(&note, &outbox),
            Disposition::Forward
        ));
    }

    #[tokio::test]
    async fn mcp_is_only_ours_when_something_is_configured_to_be_hosted_here() {
        let (outbox, _rx) = crate::relay::Outbox::for_test();
        let mcp_methods = [
            method::client::MCP_CONNECT,
            method::client::MCP_DISCONNECT,
            method::client::MCP_MESSAGE,
        ];

        // Nothing configured: forwarded, exactly as before this existed. An agent
        // reaching for MCP-over-ACP uninvited should meet a client that does not
        // implement it, not a server pretending to.
        let bare = WorkspaceInterceptor::new(vec![], PathBuf::from("/tmp"), &[]);
        for method in mcp_methods {
            let frame = request(method, json!({}));
            assert!(
                matches!(bare.on_agent_frame(&frame, &outbox), Disposition::Forward),
                "{method} must be forwarded when nothing is hosted here"
            );
        }

        let hosted = WorkspaceInterceptor::new(
            vec![],
            PathBuf::from("/tmp"),
            &[crate::config::McpServerConfig {
                name: "private".into(),
                kind: crate::config::McpServerKind::Acp(crate::config::McpLaunch {
                    command: "/bin/true".into(),
                    args: vec![],
                    env: vec![],
                }),
                unavailable: None,
            }],
        );
        for method in mcp_methods {
            let frame = request(method, json!({ "serverId": "private" }));
            assert!(
                matches!(
                    hosted.on_agent_frame(&frame, &outbox),
                    Disposition::Intercept
                ),
                "{method} must be answered here when a server is hosted"
            );
        }

        // A one-way MCP message has nothing to answer, and the browser has no
        // MCP server to give it to — so it stops here rather than being
        // forwarded to something that would ignore it.
        let note = Frame::notification(
            method::client::MCP_MESSAGE,
            &json!({ "connectionId": "mcp-1", "method": "notifications/cancelled" }),
        )
        .unwrap();
        assert!(matches!(
            hosted.on_agent_frame(&note, &outbox),
            Disposition::Intercept
        ));
        assert!(matches!(
            bare.on_agent_frame(&note, &outbox),
            Disposition::Forward
        ));
    }

    #[tokio::test]
    async fn initialize_is_rewritten_to_declare_what_the_server_provides() {
        let interceptor = WorkspaceInterceptor::new(vec![], PathBuf::from("/tmp"), &[]);
        let (outbox, _rx) = crate::relay::Outbox::for_test();

        let frame = request(
            method::agent::INITIALIZE,
            json!({ "protocolVersion": 1, "clientCapabilities": {} }),
        );
        let Disposition::Rewrite(rewritten) = interceptor.on_client_frame(&frame, &outbox) else {
            panic!("initialize must be rewritten");
        };
        let params: serde_json::Value =
            serde_json::from_str(rewritten.params().unwrap().get()).unwrap();
        assert_eq!(params["clientCapabilities"]["terminal"], true);
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], true);
    }
}
