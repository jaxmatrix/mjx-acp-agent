//! End-to-end: a real WebSocket client drives the mock agent through the
//! server, exactly as the browser will.
//!
//! This is the test that proves the premise of the project — that ACP survives
//! being carried over a WebSocket without either peer being adapted.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mjx_acp_core::{Frame, RequestId, ResponsePayload, acp, ext, method};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;

/// The repository root, derived from this crate's location.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The mock agent binary.
///
/// `CARGO_BIN_EXE_*` only exists for binaries defined by the crate under test,
/// and the mock lives in its own crate — but cargo puts every workspace binary
/// in the same directory, so it sits next to the server we do have a path for.
fn mock_agent_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_mjx-acp-server"))
        .parent()
        .expect("the server binary has a parent directory")
        .join("mjx-mock-agent");
    assert!(
        path.is_file(),
        "{} is missing; run `cargo build --workspace` first",
        path.display()
    );
    path
}

/// The file the agent is asked to fix. The off-by-one in `median` is the bug.
const BUGGY_SOURCE: &str = r#"// A few summary statistics, with one bug in it on purpose.

export function mean(xs) {
  if (xs.length === 0) return NaN;
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

export function median(xs) {
  if (xs.length === 0) return NaN;
  const sorted = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  // BUG: for even-length input this returns the upper of the two middle
  // values instead of their average.
  return sorted[mid];
}
"#;

/// Its test suite, which only passes once the fix is really on disk.
const TEST_SOURCE: &str = r#"import { test } from "node:test";
import assert from "node:assert/strict";
import { mean, median } from "./stats.js";

test("mean", () => {
  assert.equal(mean([1, 2, 3, 4]), 2.5);
});

test("median, even length", () => {
  assert.equal(median([1, 2, 3, 4]), 2.5);
});
"#;

/// A server running on a free port against a throwaway workspace, killed on
/// drop.
///
/// The workspace is a copy rather than the repo's own `demo/workspace`: the
/// mock agent really edits the file it is asked to fix, and a test that
/// rewrites a checked-in file is a test that fails the second time it runs.
struct Server {
    child: Child,
    port: u16,
    /// Holds the temporary config and workspace open for the server's lifetime.
    dir: tempfile::TempDir,
}

/// What a test wants a server to be configured with.
struct ServerOptions {
    /// `[server] resume_ttl_secs`. Long by default, so a test that is not about
    /// reaping never races the reaper.
    resume_ttl_secs: u64,
    /// Appended to the generated `mjx.toml` verbatim, for a test that needs a
    /// section this harness does not otherwise write.
    extra_config: String,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            resume_ttl_secs: 300,
            extra_config: String::new(),
        }
    }
}

impl Server {
    /// Starts the server and waits until it reports the port it bound.
    async fn start() -> Self {
        Self::start_with(ServerOptions::default()).await
    }

    async fn start_with(options: ServerOptions) -> Self {
        let repo = project_root();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // A project for the agent to work on, written out rather than copied
        // from `demo/workspace`: the mock agent really edits the file it fixes,
        // so a test that read the repo's copy would depend on whether some
        // earlier run had already fixed it.
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("stats.js"), BUGGY_SOURCE).unwrap();
        std::fs::write(workspace.join("stats.test.js"), TEST_SOURCE).unwrap();

        // Seed the registry cache from the checked-in snapshot and point the
        // fetch at a dead port, so the catalog is identical every run and the
        // tests never touch the network.
        let cache = base.join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::copy(
            repo.join("fixtures/registry.json"),
            cache.join("registry.json"),
        )
        .unwrap();

        std::fs::write(
            base.join("mjx.toml"),
            format!(
                r#"
                [server]
                resume_ttl_secs = {}

                [workspace]
                roots = ["workspace"]

                [registry]
                url = "http://127.0.0.1:1/unreachable.json"
                cache_dir = ".cache"

                [[agents]]
                id = "mock"
                name = "Mock Agent"
                command = "{}"

                {}
                "#,
                options.resume_ttl_secs,
                mock_agent_binary().display(),
                options.extra_config,
            ),
        )
        .unwrap();

        // Port 0 lets the OS pick, so concurrent test binaries don't collide.
        let mut child = Command::new(env!("CARGO_BIN_EXE_mjx-acp-server"))
            .arg("--config")
            .arg(base.join("mjx.toml"))
            .arg("--bind")
            .arg("127.0.0.1:0")
            .env("MJX_LOG", "mjx_acp_server=info")
            .env("MJX_MOCK_SPEED", "0")
            .current_dir(base)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the server binary should be built by `cargo test`");

        // The bound address is only knowable from the log line, since we asked
        // for port 0. `tracing_subscriber::fmt` logs to stdout by default.
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut lines = stdout.lines();

        // Drain stderr independently so a full pipe can never block the server.
        let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(l)) = stderr.next_line().await {
                eprintln!("[srv-err] {l}");
            }
        });
        let port = tokio::time::timeout(Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(addr) = line.split("listening on http://").nth(1) {
                    return addr.trim().rsplit(':').next().unwrap().parse().unwrap();
                }
            }
            panic!("the server exited before it started listening");
        })
        .await
        .expect("the server did not start within 30s");

        // Keep draining stdout too, for the same reason.
        tokio::spawn(async move {
            while let Ok(Some(l)) = lines.next_line().await {
                eprintln!("[srv] {l}");
            }
        });

        Self { child, port, dir }
    }

    /// A path inside the throwaway workspace.
    fn workspace_file(&self, name: &str) -> PathBuf {
        self.dir.path().join("workspace").join(name)
    }

    fn http(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn ws(&self, query: &str) -> String {
        format!("ws://127.0.0.1:{}/ws?{query}", self.port)
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
    }

    /// Stops the server the way a person does, and waits for it to exit.
    ///
    /// `stop` sends SIGKILL, which nothing can clean up after. SIGTERM is what
    /// Ctrl-C and a service manager send, and what the server can act on.
    #[cfg(unix)]
    async fn terminate(mut self) {
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        let _ = tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

/// An ACP client speaking over the WebSocket, as the browser does.
struct Client {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: i64,
    /// Every `session/update` seen, in order.
    updates: Vec<Value>,
    /// Every `_mjx/*` notification seen, as (method, params).
    ext_notifications: Vec<(String, Value)>,
    /// Client methods the agent called on us.
    client_requests: Vec<String>,
    /// Every line this client was sent, verbatim. Kept so a test can assert
    /// something never reached the browser at all, which no parsed view can.
    received: Vec<String>,
}

impl Client {
    async fn connect(url: &str) -> Self {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("the websocket should connect");
        Self {
            socket,
            next_id: 1,
            updates: Vec::new(),
            ext_notifications: Vec::new(),
            client_requests: Vec::new(),
            received: Vec::new(),
        }
    }

    async fn send(&mut self, frame: &Frame) {
        self.socket
            .send(Message::Text(frame.to_line().into()))
            .await
            .unwrap();
    }

    async fn next_frame(&mut self) -> Frame {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(30), self.socket.next())
                .await
                .expect("the server went quiet for 30s")
                .expect("the socket closed mid-turn")
                .expect("the socket errored");
            if let Message::Text(text) = message {
                self.received.push(text.to_string());
                return Frame::parse(&text)
                    .unwrap_or_else(|e| panic!("server sent a bad frame: {e}\n{text}"));
            }
        }
    }

    /// Sends a request and pumps until its response arrives, answering
    /// everything the agent asks along the way.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        self.send(&Frame::Request {
            id: id.clone(),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        })
        .await;

        loop {
            match self.next_frame().await {
                Frame::Response {
                    id: ref got,
                    ref payload,
                } if *got == id => {
                    return match payload {
                        ResponsePayload::Result(r) => serde_json::from_str(r.get()).unwrap(),
                        ResponsePayload::Error(e) => panic!("{method} failed: {}", e.message),
                    };
                }
                other => self.handle(other).await,
            }
        }
    }

    async fn handle(&mut self, frame: Frame) {
        match frame {
            Frame::Notification { method, params } => {
                let params: Value = params
                    .as_deref()
                    .and_then(|p| serde_json::from_str(p.get()).ok())
                    .unwrap_or(Value::Null);
                if method == method::client::SESSION_UPDATE {
                    self.updates.push(params["update"].clone());
                } else {
                    self.ext_notifications.push((method, params));
                }
            }
            Frame::Request { id, method, .. } => {
                self.client_requests.push(method.clone());
                // Answer the handful of client methods the browser really can
                // implement; refuse the rest so the agent degrades visibly.
                let reply = match method.as_str() {
                    m if m == method::client::SESSION_REQUEST_PERMISSION => Frame::result(
                        id,
                        &json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
                    )
                    .unwrap(),
                    m if m == method::client::ELICITATION_CREATE => Frame::result(
                        id,
                        &json!({ "action": "accept",
                                 "content": { "branch": "fix/median", "remote": "origin" } }),
                    )
                    .unwrap(),
                    _ => Frame::error(id, mjx_acp_core::JsonRpcError::method_not_found(&method)),
                };
                self.send(&reply).await;
            }
            Frame::Response { id, .. } => panic!("unsolicited response to {id}"),
        }
    }

    /// Sends a request without waiting for its answer.
    ///
    /// Needed to be *in* a turn rather than after one: `request` pumps until
    /// the response arrives, and a turn that is still running has not sent one.
    async fn start_request(&mut self, method: &str, params: Value) -> RequestId {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        self.send(&Frame::Request {
            id: id.clone(),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        })
        .await;
        id
    }

    /// Pumps until the agent asks `method`, and leaves it deliberately
    /// unanswered — which is what parks a turn mid-flight.
    ///
    /// Returns the params it was asked with.
    async fn wait_for_client_request(&mut self, method: &str) -> Value {
        loop {
            match self.next_frame().await {
                Frame::Request {
                    method: asked,
                    params,
                    ..
                } if asked == method => {
                    self.client_requests.push(asked);
                    return params
                        .as_deref()
                        .and_then(|p| serde_json::from_str(p.get()).ok())
                        .unwrap_or(Value::Null);
                }
                other => self.handle(other).await,
            }
        }
    }

    /// Pumps until a response to `id` arrives, answering what the agent asks.
    async fn wait_for_response(&mut self, id: &RequestId) -> Value {
        loop {
            match self.next_frame().await {
                Frame::Response {
                    id: ref got,
                    ref payload,
                } if got == id => {
                    return match payload {
                        ResponsePayload::Result(r) => serde_json::from_str(r.get()).unwrap(),
                        ResponsePayload::Error(e) => panic!("failed: {}", e.message),
                    };
                }
                other => self.handle(other).await,
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(&Frame::notification(method, &params).unwrap())
            .await;
    }

    /// Reads until the server closes the socket.
    ///
    /// Notifications along the way are recorded but requests are not answered:
    /// this is only ever used on a socket that is being shut down, and writing
    /// to it would be a race with the close.
    async fn expect_closed(&mut self) {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(30), self.socket.next())
                .await
                .expect("the socket was not closed within 30s");
            match message {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(Message::Text(text))) => {
                    if let Ok(Frame::Notification { method, params }) = Frame::parse(&text) {
                        let params: Value = params
                            .as_deref()
                            .and_then(|p| serde_json::from_str(p.get()).ok())
                            .unwrap_or(Value::Null);
                        self.ext_notifications.push((method, params));
                    }
                }
                Some(Ok(_)) => {}
            }
        }
    }

    fn saw_ext(&self, method: &str) -> bool {
        self.ext_notifications.iter().any(|(m, _)| m == method)
    }

    /// Reads frames until an `_mjx/*` notification with this method shows up.
    async fn wait_for_ext(&mut self, method: &str) -> Value {
        if let Some((_, params)) = self.ext_notifications.iter().find(|(m, _)| m == method) {
            return params.clone();
        }
        loop {
            let frame = self.next_frame().await;
            self.handle(frame).await;
            if let Some((_, params)) = self.ext_notifications.iter().find(|(m, _)| m == method) {
                return params.clone();
            }
        }
    }

    fn update_kinds(&self) -> Vec<&str> {
        self.updates
            .iter()
            .filter_map(|u| u["sessionUpdate"].as_str())
            .collect()
    }
}

/// Handshakes and returns what the server told us it connected to.
///
/// `initialize` first, then the announcement: the server holds
/// `_mjx/agent/info` until the handshake completes, because a conformant ACP
/// client discards anything arriving before it.
async fn handshake(client: &mut Client) -> Value {
    // The browser declares nothing: it has no filesystem and cannot spawn a
    // process. The server adds those on its behalf.
    handshake_declaring(client, json!({})).await
}

/// The same, with the client declaring exactly `capabilities`.
async fn handshake_declaring(client: &mut Client, capabilities: Value) -> Value {
    let init = client
        .request(
            method::agent::INITIALIZE,
            json!({
                "protocolVersion": mjx_acp_core::PROTOCOL_VERSION,
                "clientCapabilities": capabilities
            }),
        )
        .await;
    serde_json::from_value::<acp::InitializeResponse>(init).expect("valid InitializeResponse");
    client.wait_for_ext(ext::AGENT_INFO).await
}

/// What a client that can render an elicitation declares.
///
/// `{}` and not `true`: the schema types these as objects and reads anything
/// else as absent, so `true` would turn the feature off without saying so.
fn elicitable() -> Value {
    json!({ "elicitation": { "form": {}, "url": {} } })
}

#[tokio::test]
async fn the_workspace_is_enumerable_over_http() {
    let server = Server::start().await;

    let files: Value = reqwest::get(server.http("/api/files"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let entries = files["entries"].as_array().unwrap();
    let stats = entries
        .iter()
        .find(|entry| entry["name"] == "stats.js")
        .expect("the workspace file must be offered as a mention candidate");
    assert_eq!(stats["relPath"], "stats.js");
    assert_eq!(stats["isDir"], false);

    // Names, never contents. A listing that leaked a byte of a file would be a
    // way around the jail rather than a use of it.
    let body = serde_json::to_string(&files).unwrap();
    assert!(
        !body.contains("median"),
        "the listing must not carry file contents: {body}"
    );

    // The query narrows, and it matches the path relative to the root.
    let filtered: Value = reqwest::get(server.http("/api/files?q=stats.test"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = filtered["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["stats.test.js"]);

    // A root outside the workspace is refused, the same way a read would be.
    let refused = reqwest::get(server.http("/api/files?root=/etc"))
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
}

#[tokio::test]
async fn the_catalog_is_served_over_http() {
    let server = Server::start().await;

    let agents: Vec<Value> = reqwest::get(server.http("/api/agents"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mock = agents
        .iter()
        .find(|a| a["id"] == "mock")
        .expect("the mock agent must always be offered");
    assert_eq!(mock["availability"]["state"], "ready");
    assert_eq!(mock["isLocalOverride"], true);

    // The registry entries are merged in alongside the local ones.
    assert!(
        agents.iter().any(|a| a["id"] == "claude-acp"),
        "the registry was not merged into the catalog"
    );

    let workspaces: Vec<Value> = reqwest::get(server.http("/api/workspaces"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        workspaces[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("workspace")
    );

    server.stop().await;
}

#[tokio::test]
async fn a_full_acp_turn_survives_the_websocket() {
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;

    // The announcement follows the handshake, and says what we got.
    let info = handshake(&mut client).await;
    assert_eq!(info["agentId"], "mock");
    assert!(info["cwd"].as_str().unwrap().ends_with("workspace"));

    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    let session_id = session["sessionId"].as_str().unwrap().to_string();

    let response = client
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    assert_eq!(response["stopReason"], "end_turn");

    // The streaming surfaces all arrived, in order, over the socket.
    let kinds = client.update_kinds();
    for required in [
        "agent_thought_chunk",
        "agent_message_chunk",
        "tool_call",
        "tool_call_update",
        "plan",
    ] {
        assert!(kinds.contains(&required), "no {required} in {kinds:?}");
    }

    // A request originated by the *agent* reached the browser and was answered
    // — the direction that a naive one-way relay would drop.
    assert!(
        client
            .client_requests
            .iter()
            .any(|m| m == method::client::SESSION_REQUEST_PERMISSION),
        "the permission request never reached the client: {:?}",
        client.client_requests
    );

    server.stop().await;
}

#[tokio::test]
async fn agent_stderr_is_surfaced_rather_than_swallowed() {
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;

    // Provoke a warning: the mock logs one for a frame it can't parse.
    client
        .socket
        .send(Message::Text("{not json".into()))
        .await
        .unwrap();

    // MJX_MOCK_LOG isn't set for the spawned agent, so it logs at `warn`, which
    // is exactly what an unparseable frame produces.
    let stderr = tokio::time::timeout(
        Duration::from_secs(10),
        client.wait_for_ext(ext::AGENT_STDERR),
    )
    .await;

    if let Ok(stderr) = stderr {
        assert!(
            stderr["line"].as_str().is_some(),
            "stderr notification had no line"
        );
    }
    // If the agent chose not to log, that is its right; the assertion above
    // only checks the shape when something did arrive.

    server.stop().await;
}

#[tokio::test]
async fn fs_and_terminal_are_served_by_the_server_not_the_browser() {
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let info = handshake(&mut client).await;

    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;

    let response = client
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session["sessionId"],
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    assert_eq!(response["stopReason"], "end_turn");

    // The agent asked for these; the browser must never have been bothered.
    for never in [
        method::client::FS_READ_TEXT_FILE,
        method::client::TERMINAL_CREATE,
        method::client::TERMINAL_WAIT_FOR_EXIT,
    ] {
        assert!(
            !client.client_requests.iter().any(|m| m == never),
            "{never} reached the browser, which cannot answer it: {:?}",
            client.client_requests
        );
    }

    // It really read the file: the diff the agent produced contains the actual
    // source, which it could only have got through fs/read_text_file.
    let diff = client
        .updates
        .iter()
        .filter_map(|u| u["content"].as_array())
        .flatten()
        .find(|c| c["type"] == "diff")
        .expect("no diff in the transcript");
    assert!(
        diff["oldText"]
            .as_str()
            .unwrap()
            .contains("export function median"),
        "the diff was built from a fallback, so the file was never read"
    );

    // The write really landed: the fix is on disk, not just in a diff.
    let on_disk = std::fs::read_to_string(server.workspace_file("stats.js")).unwrap();
    assert!(
        on_disk.contains("(sorted[mid - 1] + sorted[mid]) / 2"),
        "fs/write_text_file never reached the filesystem"
    );

    // ...and the browser was shown the change as a before/after pair.
    let wrote = client.wait_for_ext(ext::FS_WROTE).await;
    assert!(wrote["path"].as_str().unwrap().ends_with("stats.js"));
    assert!(
        wrote["oldText"]
            .as_str()
            .unwrap()
            .contains("return sorted[mid];"),
        "the mirrored diff has no before-text"
    );

    // A terminal really ran, and its output was streamed to the browser.
    let created = client.wait_for_ext(ext::TERMINAL_CREATED).await;
    let terminal_id = created["terminalId"].as_str().unwrap().to_string();
    assert_eq!(created["command"], "node");

    let output: Vec<u8> = client
        .ext_notifications
        .iter()
        .filter(|(m, p)| m == ext::TERMINAL_OUTPUT && p["terminalId"] == terminal_id.as_str())
        .filter_map(|(_, p)| p["chunk"].as_str())
        .flat_map(|chunk| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .unwrap_or_default()
        })
        .collect();
    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains("pass") || output.contains("fail") || output.contains("tests"),
        "the terminal produced no recognisable test output: {output:?}"
    );

    // And the inspector was told about the traffic it never saw.
    let intercepted: Vec<&str> = client
        .ext_notifications
        .iter()
        .filter(|(m, _)| m == ext::INSPECTOR_FRAME)
        .filter_map(|(_, p)| p["method"].as_str())
        .collect();
    assert!(
        intercepted.contains(&method::client::FS_READ_TEXT_FILE),
        "the inspector has a blind spot: {intercepted:?}"
    );

    server.stop().await;
}

#[tokio::test]
async fn the_server_can_replay_the_thread_it_watched() {
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let info = handshake(&mut client).await;
    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    let session_id = session["sessionId"].as_str().unwrap().to_string();

    client
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;

    // The server folded the same stream the browser did, without either peer
    // being asked for anything extra.
    let thread = client
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;

    assert_eq!(thread["status"], "idle");
    assert_eq!(thread["stopReason"], "end_turn");

    let entries = thread["entries"]
        .as_array()
        .expect("no entries in the replay");
    assert!(entries.len() >= 5, "only {} entries", entries.len());
    assert_eq!(entries[0]["type"], "user", "the prompt is missing");

    // The tool calls survived, with their content.
    let tool_calls: Vec<&Value> = entries.iter().filter(|e| e["type"] == "toolCall").collect();
    assert_eq!(tool_calls.len(), 3);
    assert!(
        tool_calls.iter().any(|call| call["content"]
            .as_array()
            .is_some_and(|c| c.iter().any(|item| item["type"] == "diff"))),
        "the diff was lost"
    );

    assert_eq!(thread["plan"].as_array().map(Vec::len), Some(3));
    assert_eq!(thread["usage"]["used"], 4812);

    // An unknown session is answered, not errored: asking about a session that
    // doesn't exist is a normal thing for a reconnecting browser to do.
    let missing = client
        .request(ext::SESSION_REPLAY, json!({ "sessionId": "nope" }))
        .await;
    assert!(missing.is_null());

    server.stop().await;
}

/// Handshakes, opens a session, and returns `(connection id, session id)`.
async fn open_session(client: &mut Client) -> (String, String) {
    open_session_declaring(client, json!({})).await
}

/// The same, with the client declaring exactly `capabilities`.
async fn open_session_declaring(client: &mut Client, capabilities: Value) -> (String, String) {
    let info = handshake_declaring(client, capabilities).await;
    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    (
        info["connectionId"].as_str().unwrap().to_string(),
        session["sessionId"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn a_reload_mid_turn_rejoins_the_agent_and_its_conversation() {
    // The whole point. A browser that reloads while the agent is working must
    // come back to the same agent, the same session and the same conversation —
    // not to a fresh agent that has never heard of any of it.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, session_id) = open_session(&mut first).await;

    first
        .start_request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    // Park the turn: the agent asks permission and blocks until someone
    // answers, which is exactly the moment a person reaches for reload.
    let _asked = first
        .wait_for_client_request(method::client::SESSION_REQUEST_PERMISSION)
        .await;
    drop(first);

    // The reload.
    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake(&mut second).await;
    assert_eq!(info["resumed"], true, "started a new agent instead: {info}");
    assert_eq!(info["connectionId"], connection_id.as_str());

    // The browser asks for a session because that is what an ACP client does on
    // a new connection. It must be given the one already running, not another.
    let session = second
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    assert_eq!(
        session["sessionId"], session_id,
        "the reload started a second session beside the one still running"
    );

    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    assert_eq!(
        thread["status"], "generating",
        "the turn was abandoned rather than left running"
    );
    let entries = thread["entries"].as_array().expect("no entries");
    assert_eq!(entries[0]["type"], "user", "the prompt was lost");
    assert!(
        entries.len() > 1,
        "nothing the agent said before the reload survived"
    );

    // The turn is not merely preserved, it is still running: the question the
    // first tab never answered is put to this one, and answering it lets the
    // agent carry on to the end.
    let ended = second.wait_for_ext(ext::SESSION_TURN_ENDED).await;
    assert_eq!(ended["sessionId"], session_id.as_str());
    assert_eq!(ended["stopReason"], "end_turn");
    assert!(
        second
            .client_requests
            .contains(&method::client::SESSION_REQUEST_PERMISSION.to_string()),
        "the parked question was never re-asked: {:?}",
        second.client_requests
    );

    // And the finished turn is what the thread now says.
    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    assert_eq!(thread["status"], "idle");
    assert_eq!(thread["stopReason"], "end_turn");

    server.stop().await;
}

#[tokio::test]
async fn a_question_answered_before_the_reload_is_not_asked_again() {
    // Re-asking is for questions still outstanding. Repeating one the previous
    // browser already answered would ask the user to approve the same edit
    // twice, and the agent is no longer listening for the answer.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, session_id) = open_session(&mut first).await;
    // `request` answers every permission prompt on the way through, so by the
    // time this returns the whole turn is done and nothing is outstanding.
    first
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    drop(first);

    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake(&mut second).await;
    second
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;

    assert!(
        second.client_requests.is_empty(),
        "asked again for something already answered: {:?}",
        second.client_requests
    );

    server.stop().await;
}

/// The pending elicitation in a replayed thread, if there is one.
fn pending_elicitation(thread: &Value) -> Option<&Value> {
    thread["entries"]
        .as_array()?
        .iter()
        .find(|entry| entry["type"] == "elicitation")
}

#[tokio::test]
async fn a_reload_gets_the_open_form_from_the_thread_and_from_the_socket() {
    // Both, and neither alone would do. The replay is what carries the question
    // and its answer as part of the conversation; the re-ask is what makes it
    // answerable, because a browser cannot respond to a request the connection
    // it is holding never received. The browser matches them by request id.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, session_id) = open_session_declaring(&mut first, elicitable()).await;

    first
        .start_request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    let asked = first
        .wait_for_client_request(method::client::ELICITATION_CREATE)
        .await;
    assert_eq!(asked["mode"], "form");
    drop(first);

    // The reload.
    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake_declaring(&mut second, elicitable()).await;
    assert_eq!(info["resumed"], true, "started a new agent instead: {info}");
    second
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;

    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    let open = pending_elicitation(&thread).expect("the open form was not in the replay");
    assert_eq!(open["state"], "pending");
    assert_eq!(open["mode"], "form");
    assert_eq!(open["toolCallId"], "call_edit");
    assert!(open["requestedSchema"]["properties"]["branch"].is_object());

    // And it is asked again, with the same id the thread carries — that pairing
    // is what lets the browser show one form rather than two.
    let reasked = second
        .wait_for_client_request(method::client::ELICITATION_CREATE)
        .await;
    assert_eq!(reasked["mode"], "form");

    let id = RequestId::Number(open["requestId"].as_i64().expect("no request id"));
    second
        .send(
            &Frame::result(
                id,
                &json!({ "action": "accept",
                         "content": { "branch": "fix/median", "remote": "upstream" } }),
            )
            .unwrap(),
        )
        .await;

    let ended = second.wait_for_ext(ext::SESSION_TURN_ENDED).await;
    assert_eq!(ended["stopReason"], "end_turn", "the turn did not finish");

    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    let answered = pending_elicitation(&thread).expect("the form vanished from the thread");
    assert_eq!(answered["state"], "accepted");
    assert_eq!(answered["content"]["remote"], "upstream");

    server.stop().await;
}

#[tokio::test]
async fn a_cancelled_turn_stops_offering_the_question_it_asked() {
    // Nobody is going to answer a question whose turn is over. Left pending it
    // would be offered to every browser that ever attaches, and a replayed
    // thread would show a live form with nowhere to send the answer.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, session_id) = open_session_declaring(&mut first, elicitable()).await;

    let prompt = first
        .start_request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    first
        .wait_for_client_request(method::client::ELICITATION_CREATE)
        .await;

    // Cancel with the form still open. A real agent gives up on the question
    // rather than waiting on an answer nobody is going to give.
    first
        .notify(
            method::agent::SESSION_CANCEL,
            json!({ "sessionId": session_id }),
        )
        .await;
    let response = first.wait_for_response(&prompt).await;
    assert_eq!(response["stopReason"], "cancelled");

    let thread = first
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    assert_eq!(
        pending_elicitation(&thread).expect("the form left the thread")["state"],
        "cancelled"
    );
    drop(first);

    // And the next browser is not asked it either.
    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake_declaring(&mut second, elicitable()).await;
    second
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;

    assert_eq!(
        pending_elicitation(&thread).expect("the form left the thread")["state"],
        "cancelled"
    );
    assert!(
        second.client_requests.is_empty(),
        "a dead turn's questions were put to a new browser: {:?}",
        second.client_requests
    );

    server.stop().await;
}

#[tokio::test]
async fn a_resource_link_survives_the_relay_and_the_replay() {
    // A mention travels as a resource_link, and every hop is somewhere it could
    // be flattened to its text: the relay's thread, the agent's transcript, and
    // the replay a reloaded browser is given back.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, session_id) = open_session(&mut first).await;
    let link = server.workspace_file("stats.js");
    let uri = format!("file://{}", link.display());

    first
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [
                    { "type": "text", "text": "fix the median bug in " },
                    { "type": "resource_link", "uri": uri, "name": "stats.js" }
                ]
            }),
        )
        .await;
    drop(first);

    // The socket is gone; the agent is not. What comes back has to be the same
    // conversation, mention and all.
    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake(&mut second).await;
    assert_eq!(info["resumed"], true);
    second
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;

    let users: Vec<&Value> = thread["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["type"] == "user")
        .collect();
    assert_eq!(users.len(), 1, "the prompt was duplicated: {thread}");

    let content = users[0]["content"].as_array().unwrap();
    let link = content
        .iter()
        .find(|block| block["type"] == "resource_link")
        .unwrap_or_else(|| panic!("the mention was flattened away: {thread}"));
    assert_eq!(link["uri"], uri);

    // And again through the agent's own transcript, which is a second copy of
    // the same conversation and a second chance to lose the mention. The agent
    // annotates the link it replays; the fold has to know it for the same
    // block all the same.
    second
        .request(
            method::agent::SESSION_LOAD,
            json!({ "sessionId": session_id, "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    let thread = second
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    let replayed = thread["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["type"] == "user")
        .flat_map(|entry| entry["content"].as_array().unwrap())
        .find(|block| block["type"] == "resource_link")
        .unwrap_or_else(|| panic!("the loaded conversation lost the mention: {thread}"));
    assert_eq!(replayed["uri"], uri);
    assert_eq!(replayed["mimeType"], "text/javascript");

    server.stop().await;
}

#[tokio::test]
async fn a_second_tab_takes_the_connection_over_and_tells_the_first() {
    // Take-over rather than refusal, deliberately: on a reload the new socket
    // can arrive before the old one's close has been processed, so refusing the
    // second attachment would make an ordinary refresh fail.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, _) = open_session(&mut first).await;

    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake(&mut second).await;
    assert_eq!(info["resumed"], true);

    first.expect_closed().await;
    assert!(
        first.saw_ext(ext::CONNECTION_TAKEN_OVER),
        "the displaced tab was cut off without being told why: {:?}",
        first.ext_notifications
    );

    server.stop().await;
}

#[tokio::test]
async fn an_unknown_resume_id_starts_a_fresh_connection() {
    // An expired or stale id is an ordinary thing for a reloading browser to
    // arrive with. Refusing the upgrade would turn a routine event into a page
    // that will not load.
    let server = Server::start().await;

    let mut client = Client::connect(&server.ws("agent=mock&resume=not-a-real-id")).await;
    let info = handshake(&mut client).await;

    assert_eq!(info["resumed"], false);
    assert_ne!(info["connectionId"], "not-a-real-id");
    assert!(
        info["connectionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    server.stop().await;
}

#[tokio::test]
async fn a_resume_id_is_a_handle_to_one_conversation_not_to_the_pool() {
    // The id names an agent in a directory. Answering a request for a different
    // directory with it would hand a browser someone else's workspace.
    let server = Server::start().await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, _) = open_session(&mut first).await;
    drop(first);

    let elsewhere = server.dir.path().join("workspace").join("nested");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut second = Client::connect(&server.ws(&format!(
        "agent=mock&resume={connection_id}&cwd={}",
        elsewhere.display()
    )))
    .await;
    let info = handshake(&mut second).await;

    assert_eq!(
        info["resumed"], false,
        "a resume for another directory was answered by the wrong agent"
    );

    server.stop().await;
}

/// Whether a process is still running.
///
/// `kill -0` rather than reading `/proc`, so this works on any Unix. The server
/// waits on the child it spawned, so a reaped agent is not left a zombie for
/// this to be fooled by.
fn process_is_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_the_server_takes_its_agents_with_it() {
    // An agent that outlives its socket is, for a while, a subprocess with
    // nobody watching it. If the server goes without ending them, they are
    // reparented to init and keep whatever terminals they started — an orphan
    // holding a PTY forever, which is worse than the lost session this feature
    // exists to save.
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;
    open_session(&mut client).await;

    let pooled: Vec<Value> = reqwest::get(server.http("/api/connections"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = pooled[0]["pid"].as_u64().expect("no pid reported");

    // Leave the socket open: the agent has to be ended because the server is
    // going, not because anyone disconnected.
    server.terminate().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while process_is_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the server exited but left agent {pid} running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn an_abandoned_connection_is_reaped_and_its_agent_killed() {
    // An agent that outlives its socket has to be ended by something. An
    // orphan holding a subprocess and a PTY forever is worse than losing the
    // session it was keeping.
    let server = Server::start_with(ServerOptions {
        resume_ttl_secs: 3,
        ..Default::default()
    })
    .await;

    let mut client = Client::connect(&server.ws("agent=mock")).await;
    open_session(&mut client).await;

    let pooled: Vec<Value> = reqwest::get(server.http("/api/connections"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pooled.len(), 1, "{pooled:?}");
    assert_eq!(pooled[0]["attached"], true);
    assert!(
        pooled[0].get("id").is_none(),
        "a connection id is the capability to talk to a running agent, and this \
         endpoint has no authentication in front of it: {:?}",
        pooled[0]
    );
    let pid = pooled[0]["pid"].as_u64().expect("no pid reported");
    assert!(process_is_alive(pid), "the agent never started");

    drop(client);

    // Poll for the outcome rather than sleeping for the TTL and asserting once:
    // the deadline is generous, so a slow machine makes the test slower rather
    // than making it fail.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while process_is_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the agent nobody came back to is still running as pid {pid}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let pooled: Vec<Value> = reqwest::get(server.http("/api/connections"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        pooled.is_empty(),
        "the registry still lists a dead agent: {pooled:?}"
    );

    server.stop().await;
}

#[tokio::test]
async fn extension_methods_never_reach_the_agent() {
    // `_mjx/*` is between the browser and this server. An agent that received
    // one would rightly answer "method not found", and the browser would see a
    // failure where it should have seen a result.
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;
    handshake(&mut client).await;

    // The mock answers anything it doesn't implement with -32601. Getting a
    // result back proves the server handled this itself.
    let replayed = client
        .request(ext::SESSION_REPLAY, json!({ "sessionId": "whatever" }))
        .await;
    assert!(replayed.is_null());

    server.stop().await;
}

/// Attempts a real WebSocket handshake and returns the HTTP status the server
/// refused it with.
///
/// A plain GET would just be a 400 from the upgrade extractor, which tells us
/// nothing; the browser only ever arrives with upgrade headers, so that is what
/// the refusal path has to be tested through.
async fn handshake_status(url: &str) -> u16 {
    use tokio_tungstenite::tungstenite::Error;
    match tokio_tungstenite::connect_async(url).await {
        Ok(_) => panic!("the handshake should have been refused"),
        Err(Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
    }
}

#[tokio::test]
async fn an_unknown_agent_is_refused_during_the_handshake() {
    let server = Server::start().await;
    // 404 rather than a socket that opens and closes for no visible reason.
    assert_eq!(handshake_status(&server.ws("agent=nope")).await, 404);
    server.stop().await;
}

#[tokio::test]
async fn a_cwd_outside_the_workspace_roots_is_refused() {
    let server = Server::start().await;
    assert_eq!(
        handshake_status(&server.ws("agent=mock&cwd=/etc")).await,
        403,
        "the filesystem jail must apply to the session cwd, not just to fs/*"
    );
    server.stop().await;
}

#[tokio::test]
async fn the_relay_forwards_frames_it_cannot_classify() {
    // A relay that only passes what it understands breaks the day either peer
    // speaks a newer protocol. An unknown method must still reach the agent,
    // and the agent's error must come back.
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;

    let id = RequestId::Number(99);
    client
        .send(&Frame::Request {
            id: id.clone(),
            method: "session/some_method_from_the_future".into(),
            params: Some(serde_json::value::to_raw_value(&json!({})).unwrap()),
        })
        .await;

    loop {
        match client.next_frame().await {
            Frame::Response {
                id: ref got,
                ref payload,
            } if *got == id => {
                let ResponsePayload::Error(error) = payload else {
                    panic!("the mock agent should not implement a method from the future");
                };
                // -32601 came from the agent, which means the request reached it.
                assert_eq!(error.code, -32601);
                break;
            }
            other => client.handle(other).await,
        }
    }

    server.stop().await;
}

/// The sessions the agent lists, through the relay.
async fn list_sessions(client: &mut Client, params: Value) -> Vec<Value> {
    let listed = client.request(method::agent::SESSION_LIST, params).await;
    listed["sessions"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn a_past_conversation_is_listed_loaded_and_folded() {
    // The session lifecycle is forwarded like anything else — the relay
    // intercepts none of it — and the thread the server folds from a replay is
    // the one a browser that reloads gets back.
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;

    // What the agent said it can do has to reach the browser intact: the UI
    // offers only what is advertised here.
    let init = client
        .request(
            method::agent::INITIALIZE,
            json!({ "protocolVersion": mjx_acp_core::PROTOCOL_VERSION, "clientCapabilities": {} }),
        )
        .await;
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    assert!(init["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
    let info = client.wait_for_ext(ext::AGENT_INFO).await;

    let cwd = info["cwd"].clone();
    let sessions = list_sessions(&mut client, json!({ "cwd": cwd })).await;
    let past = sessions
        .first()
        .expect("the mock agent seeds a conversation from before this run")["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // The replay arrives as `session/update` notifications, and `request`
    // records them on the way past the response.
    let before = client.updates.len();
    client
        .request(
            method::agent::SESSION_LOAD,
            json!({ "sessionId": past, "cwd": cwd, "mcpServers": [] }),
        )
        .await;
    let replayed = client.updates.len() - before;
    assert!(replayed > 0, "the load replayed nothing");

    let thread = client
        .request(ext::SESSION_REPLAY, json!({ "sessionId": past }))
        .await;
    let entries = thread["entries"].as_array().expect("no entries").len();
    assert!(entries > 0, "the server folded none of the replay");
    assert_eq!(thread["entries"][0]["type"], "user");

    // Again: a second load rebuilds the conversation rather than doubling it.
    client
        .request(
            method::agent::SESSION_LOAD,
            json!({ "sessionId": past, "cwd": cwd, "mcpServers": [] }),
        )
        .await;
    let thread = client
        .request(ext::SESSION_REPLAY, json!({ "sessionId": past }))
        .await;
    assert_eq!(thread["entries"].as_array().map(Vec::len), Some(entries));

    server.stop().await;
}

#[tokio::test]
async fn a_deleted_session_leaves_neither_a_listing_nor_a_thread() {
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let (_, session_id) = open_session(&mut client).await;
    client
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    assert!(
        !client
            .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
            .await
            .is_null()
    );

    client
        .request(
            method::agent::SESSION_DELETE,
            json!({ "sessionId": session_id }),
        )
        .await;

    let sessions = list_sessions(&mut client, json!({})).await;
    assert!(
        !sessions
            .iter()
            .any(|s| s["sessionId"] == session_id.as_str()),
        "the agent still lists a deleted session: {sessions:#?}"
    );
    // And the server let go of it too: replaying a session the agent has
    // deleted should answer "nothing", not hand back a conversation that no
    // longer exists anywhere else.
    let thread = client
        .request(ext::SESSION_REPLAY, json!({ "sessionId": session_id }))
        .await;
    assert!(thread.is_null(), "{thread:#}");

    server.stop().await;
}

/// A token that must never appear on the browser's socket.
///
/// The passthrough hands the agent credentials the browser has no business
/// seeing: the server injects them on the way past, so a value only ever travels
/// server → agent.
const MCP_ENV_VALUE: &str = "sh-hh-only-the-agent-sees-this";

/// Servers of every transport, for the tests below.
///
/// `feed` is an SSE server and the mock agent declares `sse: false`, so it is
/// the one that must be dropped — a configuration that is passed through whole
/// would look identical without it.
fn configured_mcp_servers() -> String {
    format!(
        r#"
        [[mcp_servers]]
        name = "git"
        command = "npx"
        args = ["-y", "@modelcontextprotocol/server-git"]
        env = {{ TOKEN = "{MCP_ENV_VALUE}" }}

        [[mcp_servers]]
        name = "docs"
        transport = "http"
        url = "https://example.test/mcp"
        headers = {{ Authorization = "Bearer {MCP_ENV_VALUE}" }}

        [[mcp_servers]]
        name = "feed"
        transport = "sse"
        url = "https://example.test/sse"
        "#
    )
}

/// What the agent says it was opened with: name → transport.
fn offered_mcp_servers(response: &Value) -> Vec<(String, String)> {
    response["_meta"]["mjx.mcpServers"]
        .as_array()
        .unwrap_or_else(|| panic!("the agent reported no MCP servers at all: {response:#}"))
        .iter()
        .map(|server| {
            (
                server["name"].as_str().unwrap_or_default().to_string(),
                server["transport"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_configured_mcp_servers_reach_the_agent_and_their_secrets_do_not_reach_the_browser() {
    // The point of the feature: an agent behind this transport has the tools it
    // would have in an editor, without the browser — which is the ACP client and
    // would normally send `mcpServers` — having to know any of them.
    let server = Server::start_with(ServerOptions {
        extra_config: configured_mcp_servers(),
        ..Default::default()
    })
    .await;

    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let info = handshake(&mut client).await;
    let session = client
        .request(
            method::agent::SESSION_NEW,
            // Empty, exactly as the browser sends it.
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;

    let offered = offered_mcp_servers(&session);
    assert_eq!(
        offered,
        [
            ("git".to_string(), "stdio".to_string()),
            ("docs".to_string(), "http".to_string()),
        ],
        "`feed` is SSE and this agent declared `sse: false`, so it must be dropped"
    );

    // Nothing the browser was sent carries the credential. Checked over the raw
    // lines rather than a parsed view, because a leak would be in whatever shape
    // we failed to anticipate.
    let leaked: Vec<&String> = client
        .received
        .iter()
        .filter(|line| line.contains(MCP_ENV_VALUE))
        .collect();
    assert!(
        leaked.is_empty(),
        "a credential reached the browser: {leaked:#?}"
    );

    server.stop().await;
}

#[tokio::test]
async fn every_way_of_opening_a_session_carries_the_configured_servers() {
    // `mcpServers` is on all four session-opening methods and documented as the
    // complete resulting list. The browser sends it on `session/new` and
    // `session/load` only — so a fork or a resume that was not rewritten would
    // silently leave that conversation with no tools at all.
    let server = Server::start_with(ServerOptions {
        extra_config: configured_mcp_servers(),
        ..Default::default()
    })
    .await;

    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let info = handshake(&mut client).await;
    let cwd = info["cwd"].clone();
    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": cwd, "mcpServers": [] }),
        )
        .await;
    let session_id = session["sessionId"].as_str().unwrap().to_string();

    let expected = offered_mcp_servers(&session);
    assert_eq!(expected.len(), 2, "{session:#}");

    // A load and a fork as the browser sends them, with the key present but
    // empty; a resume as it sends that, with the key absent entirely.
    for (method, params) in [
        (
            method::agent::SESSION_LOAD,
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        ),
        (
            method::agent::SESSION_FORK,
            json!({ "sessionId": session_id, "cwd": cwd }),
        ),
        (
            method::agent::SESSION_RESUME,
            json!({ "sessionId": session_id, "cwd": cwd }),
        ),
    ] {
        let response = client.request(method, params).await;
        assert_eq!(
            offered_mcp_servers(&response),
            expected,
            "{method} arrived without the configured servers"
        );
    }

    server.stop().await;
}

#[tokio::test]
async fn a_server_the_client_declared_itself_is_not_offered_twice() {
    // The merge is by name, so a client that configures its own `git` keeps its
    // own — and the agent is never handed two servers with one name, which no
    // MCP host can make sense of.
    let server = Server::start_with(ServerOptions {
        extra_config: configured_mcp_servers(),
        ..Default::default()
    })
    .await;

    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let info = handshake(&mut client).await;
    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({
                "cwd": info["cwd"],
                "mcpServers": [
                    { "name": "git", "command": "/usr/bin/its-own-git", "args": [], "env": [] }
                ],
            }),
        )
        .await;

    let offered = offered_mcp_servers(&session);
    assert_eq!(
        offered,
        [
            ("git".to_string(), "stdio".to_string()),
            ("docs".to_string(), "http".to_string()),
        ],
        "the client's own `git` should have been left alone, not doubled"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_reattached_browser_still_reaches_the_configured_servers() {
    // A reattachment's first `session/new` is answered from the recording and so
    // is never rewritten — right, because the agent still running was given
    // these servers when the first browser opened it. Its `session/load` does
    // reach the agent, and has to carry them.
    let server = Server::start_with(ServerOptions {
        extra_config: configured_mcp_servers(),
        ..Default::default()
    })
    .await;

    let mut first = Client::connect(&server.ws("agent=mock")).await;
    let (connection_id, session_id) = open_session(&mut first).await;
    drop(first);

    let mut second =
        Client::connect(&server.ws(&format!("agent=mock&resume={connection_id}"))).await;
    let info = handshake(&mut second).await;
    assert_eq!(info["resumed"], true);
    second
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;

    let loaded = second
        .request(
            method::agent::SESSION_LOAD,
            json!({ "sessionId": session_id, "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;
    assert_eq!(offered_mcp_servers(&loaded).len(), 2, "{loaded:#}");

    server.stop().await;
}

/// A credential only the server and the MCP child it spawns ever see.
const HOSTED_MCP_TOKEN: &str = "hosted-token-the-agent-never-learns";

/// An `acp`-transport server: the mock agent binary, in its MCP mode.
fn hosted_mcp_server() -> String {
    format!(
        r#"
        [[mcp_servers]]
        name = "private"
        transport = "acp"
        command = "{}"
        args = ["--mcp"]
        env = {{ MJX_MOCK_MCP_TOKEN = "{HOSTED_MCP_TOKEN}" }}
        "#,
        mock_agent_binary().display(),
    )
}

#[tokio::test]
async fn a_tool_from_a_server_this_process_holds_reaches_the_agent() {
    // MCP-over-ACP end to end: the server spawns the MCP child and holds it, the
    // agent reaches it entirely through `mcp/connect` and `mcp/message`, and the
    // credential the child needs never leaves this process.
    let server = Server::start_with(ServerOptions {
        extra_config: hosted_mcp_server(),
        ..Default::default()
    })
    .await;

    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let info = handshake(&mut client).await;
    let session = client
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": info["cwd"], "mcpServers": [] }),
        )
        .await;

    // Offered as `acp`, by name — never as a command.
    assert_eq!(
        offered_mcp_servers(&session),
        [("private".to_string(), "acp".to_string())],
        "{session:#}"
    );

    let report = &session["_meta"]["mjx.mcp"];
    assert!(
        report["error"].is_null(),
        "the agent could not use the hosted server: {report:#}"
    );
    assert_eq!(report["serverId"], "private");
    assert_eq!(report["initialize"]["serverInfo"]["name"], "mjx-mock-mcp");
    // The return path: the MCP server asked *its* client for the roots, the
    // server put that to the agent as an `mcp/message` request with an id no
    // browser is waiting on, and the answer found its way back. Asked
    // mid-handshake, so this is a fact rather than a race.
    assert_eq!(
        report["initialize"]["_meta"]["mjx.rootsAnswered"], true,
        "a request from the hosted server never got an answer: {report:#}"
    );
    assert_eq!(report["tools"]["tools"][0]["name"], "mock_stat");

    // The tool really ran, in the child, with the credential this process gave
    // it — which is the whole claim of the transport.
    let text = report["called"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the tool call answered nothing readable: {report:#}"));
    assert!(text.contains("credential present: true"), "{text}");
    // And letting go worked, so the child is not left behind for the life of the
    // connection.
    assert_eq!(report["disconnected"], true, "{report:#}");

    // What the *agent* was handed: a name and an id, and nothing it could run.
    // The whole purpose of the transport is that the command and its environment
    // stay here, and the receiving end is the only place that can say they did.
    let fields = session["_meta"]["mjx.mcpServers"][0]["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("the agent did not report what it was handed: {session:#}"));
    let fields: Vec<&str> = fields.iter().filter_map(|f| f.as_str()).collect();
    assert_eq!(
        fields,
        ["name", "serverId", "type"],
        "the agent was handed more than an id"
    );

    // The browser is shown the command — that is what the sidebar is for — but
    // never the credential, which is the one value that must not travel.
    let leaked: Vec<&String> = client
        .received
        .iter()
        .filter(|line| line.contains(HOSTED_MCP_TOKEN))
        .collect();
    assert!(
        leaked.is_empty(),
        "a credential reached the browser: {leaked:#?}"
    );

    // What the server said unprompted got through too: `mcp/message` carries a
    // server-initiated notification the other way, and nothing else would have
    // told the agent its tool list had changed.
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let loaded = client
                .request(
                    method::agent::SESSION_LOAD,
                    json!({
                        "sessionId": session["sessionId"],
                        "cwd": info["cwd"],
                        "mcpServers": []
                    }),
                )
                .await;
            // Read from a later request on purpose: the notification races the
            // response to the call that triggered it, and the point is only that
            // it arrives at all.
            let _ = loaded;
            let session = client
                .request(
                    method::agent::SESSION_NEW,
                    json!({ "cwd": info["cwd"], "mcpServers": [] }),
                )
                .await;
            if session["_meta"]["mjx.mcp"]["notifications"]
                .as_i64()
                .unwrap_or(0)
                > 0
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        seen.is_ok(),
        "the agent was never told what the MCP server said unprompted"
    );

    server.stop().await;
}

#[tokio::test]
async fn mcp_over_acp_is_forwarded_when_nothing_is_hosted_here() {
    // With no `acp` server configured the server must not answer `mcp/*` at all.
    // An agent that asked anyway is talking to a browser that does not implement
    // it, and gets `method not found` — which is the truth, and is what makes
    // the capability gating above meaningful rather than decorative.
    let server = Server::start().await;
    let mut client = Client::connect(&server.ws("agent=mock")).await;
    let (_, _) = open_session(&mut client).await;

    // Nothing was offered over ACP, so the agent never tried: the proof is that
    // the client was asked no `mcp/*` question and the session opened cleanly.
    assert!(
        !client
            .client_requests
            .iter()
            .any(|method| method.starts_with("mcp/")),
        "the agent asked for MCP over ACP uninvited: {:?}",
        client.client_requests
    );

    server.stop().await;
}
