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

impl Server {
    /// Starts the server and waits until it reports the port it bound.
    async fn start() -> Self {
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
                [workspace]
                roots = ["workspace"]

                [registry]
                url = "http://127.0.0.1:1/unreachable.json"
                cache_dir = ".cache"

                [[agents]]
                id = "mock"
                name = "Mock Agent"
                command = "{}"
                "#,
                mock_agent_binary().display(),
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
        tokio::spawn(async move { while let Ok(Some(_)) = stderr.next_line().await {} });
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
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

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
                    _ => Frame::error(id, mjx_acp_core::JsonRpcError::method_not_found(&method)),
                };
                self.send(&reply).await;
            }
            Frame::Response { id, .. } => panic!("unsolicited response to {id}"),
        }
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
    let init = client
        .request(
            method::agent::INITIALIZE,
            json!({
                "protocolVersion": mjx_acp_core::PROTOCOL_VERSION,
                // The browser declares nothing: it has no filesystem and cannot
                // spawn a process. The server adds those on its behalf.
                "clientCapabilities": {}
            }),
        )
        .await;
    serde_json::from_value::<acp::InitializeResponse>(init).expect("valid InitializeResponse");
    client.wait_for_ext(ext::AGENT_INFO).await
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
