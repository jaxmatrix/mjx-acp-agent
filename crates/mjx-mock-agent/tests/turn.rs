//! Drives the mock agent over real stdio, the same way the server will.
//!
//! This is the first end-to-end check in the project: it spawns the actual
//! binary, speaks newline-delimited JSON-RPC to it, answers the client requests
//! it makes, and asserts a complete turn comes back.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use mjx_acp_core::{Frame, RequestId, ResponsePayload, acp, method};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Anything the driver saw the agent do, in order.
#[derive(Default)]
struct Transcript {
    /// The `update` object of each `session/update` notification.
    updates: Vec<Value>,
    /// Methods of the client requests the agent made.
    client_requests: Vec<String>,
    /// Params of the client requests the agent made, by method.
    request_params: HashMap<String, Vec<Value>>,
    /// Params of every `elicitation/complete` the agent sent.
    completed_elicitations: Vec<Value>,
}

impl Transcript {
    /// The `sessionUpdate` discriminant of every update, in order.
    fn kinds(&self) -> Vec<&str> {
        self.updates
            .iter()
            .filter_map(|u| u["sessionUpdate"].as_str())
            .collect()
    }

    /// Updates of one kind.
    fn of_kind(&self, kind: &str) -> Vec<&Value> {
        self.updates
            .iter()
            .filter(|u| u["sessionUpdate"] == kind)
            .collect()
    }

    /// All `agent_message_chunk` text, concatenated the way a viewer would.
    fn assistant_text(&self) -> String {
        self.of_kind("agent_message_chunk")
            .iter()
            .filter_map(|u| u["content"]["text"].as_str())
            .collect()
    }
}

/// A minimal ACP client speaking to the agent over its stdio.
struct Driver {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: i64,
    transcript: Transcript,
    /// Canned answers for the client requests the agent makes.
    answers: HashMap<String, Value>,
    /// If set, sent as a `session/cancel` the first time an update arrives.
    cancel_on_first_update: Option<String>,
    /// If set, the named request is left unanswered and a `session/cancel` for
    /// this session is sent instead — a user walking away from a question.
    cancel_when_asked: Option<(String, String)>,
}

impl Driver {
    fn spawn(speed: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mjx-mock-agent"))
            .env("MJX_MOCK_SPEED", speed)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Without this a failing assertion leaves the agent running, since
            // the panic unwinds past `shutdown`.
            .kill_on_drop(true)
            .spawn()
            .expect("the mock agent binary should be built by `cargo test`");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();

        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            transcript: Transcript::default(),
            answers: HashMap::new(),
            cancel_on_first_update: None,
            cancel_when_asked: None,
        }
    }

    /// Registers the result to return for a client method.
    fn answer(&mut self, method: &str, result: Value) -> &mut Self {
        self.answers.insert(method.to_string(), result);
        self
    }

    async fn send(&mut self, frame: &Frame) {
        let mut line = frame.to_line();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Sends a request and pumps the connection until its response arrives,
    /// answering anything the agent asks for along the way.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        let frame = Frame::Request {
            id: id.clone(),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        };
        self.send(&frame).await;

        loop {
            let frame = self.next_frame().await;
            match frame {
                Frame::Response {
                    id: ref got,
                    ref payload,
                } if *got == id => {
                    return match payload {
                        ResponsePayload::Result(result) => {
                            serde_json::from_str(result.get()).unwrap()
                        }
                        ResponsePayload::Error(error) => {
                            panic!("{method} failed: {} ({})", error.message, error.code)
                        }
                    };
                }
                other => self.handle(other).await,
            }
        }
    }

    /// Reads one frame, failing the test rather than hanging if the agent
    /// stops talking.
    async fn next_frame(&mut self) -> Frame {
        let line = tokio::time::timeout(Duration::from_secs(30), self.stdout.next_line())
            .await
            .expect("the agent went quiet for 30s")
            .expect("stdout is readable")
            .expect("the agent closed stdout mid-turn");
        Frame::parse(&line).unwrap_or_else(|e| panic!("agent emitted a bad frame: {e}\n{line}"))
    }

    /// Records a notification, or answers a request the agent made of us.
    async fn handle(&mut self, frame: Frame) {
        match frame {
            // A URL-mode elicitation is finished by a notification rather than
            // by the response, so this is not only `session/update`.
            Frame::Notification { method, params }
                if method == method::client::ELICITATION_COMPLETE =>
            {
                let params: Value =
                    serde_json::from_str(params.expect("the notification has params").get())
                        .unwrap();
                serde_json::from_value::<acp::CompleteElicitationNotification>(params.clone())
                    .unwrap_or_else(|e| panic!("invalid elicitation/complete: {e}\n{params:#}"));
                self.transcript.completed_elicitations.push(params);
            }
            Frame::Notification { method, params } => {
                assert_eq!(method, method::client::SESSION_UPDATE, "unexpected notification");
                let params: Value =
                    serde_json::from_str(params.expect("session/update has params").get()).unwrap();

                // Everything the agent streams must be a valid SessionUpdate.
                serde_json::from_value::<acp::SessionNotification>(params.clone())
                    .unwrap_or_else(|e| panic!("invalid session/update: {e}\n{params:#}"));

                if let Some(session_id) = self.cancel_on_first_update.take() {
                    let cancel = Frame::notification(
                        method::agent::SESSION_CANCEL,
                        &json!({ "sessionId": session_id }),
                    )
                    .unwrap();
                    self.send(&cancel).await;
                }

                self.transcript.updates.push(params["update"].clone());
            }
            Frame::Request { id, method, params } => {
                self.transcript.client_requests.push(method.clone());
                let asked: Value = params
                    .as_deref()
                    .and_then(|p| serde_json::from_str(p.get()).ok())
                    .unwrap_or(Value::Null);
                self.transcript
                    .request_params
                    .entry(method.clone())
                    .or_default()
                    .push(asked);

                // The user walking away from a question: nothing is answered,
                // and the turn is cancelled instead.
                if self
                    .cancel_when_asked
                    .as_ref()
                    .is_some_and(|(asked, _)| *asked == method)
                {
                    let (_, session_id) = self.cancel_when_asked.take().expect("just matched");
                    let cancel = Frame::notification(
                        method::agent::SESSION_CANCEL,
                        &json!({ "sessionId": session_id }),
                    )
                    .unwrap();
                    self.send(&cancel).await;
                    return;
                }

                let result = self.answers.get(&method).cloned().unwrap_or_else(|| {
                    panic!("agent called {method}, which the driver has no answer for")
                });
                let response = Frame::result(id, &result).unwrap();
                self.send(&response).await;
            }
            Frame::Response { id, .. } => panic!("unsolicited response to {id}"),
        }
    }

    async fn shutdown(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

/// Handshake plus session creation, shared by the tests below.
async fn connect(driver: &mut Driver) -> String {
    connect_declaring(
        driver,
        json!({
            "fs": { "readTextFile": true, "writeTextFile": true },
            "terminal": true
        }),
    )
    .await
}

/// The same, with the client declaring exactly `capabilities`.
///
/// Which capabilities the client offers is not decoration: the agent asks only
/// for what it was offered, so this is what decides whether a step runs at all.
async fn connect_declaring(driver: &mut Driver, capabilities: Value) -> String {
    let init = driver
        .request(
            method::agent::INITIALIZE,
            json!({
                "protocolVersion": mjx_acp_core::PROTOCOL_VERSION,
                "clientCapabilities": capabilities
            }),
        )
        .await;
    serde_json::from_value::<acp::InitializeResponse>(init.clone()).expect("valid InitializeResponse");
    assert_eq!(init["protocolVersion"], mjx_acp_core::PROTOCOL_VERSION);

    let session = driver
        .request(
            method::agent::SESSION_NEW,
            json!({ "cwd": env!("CARGO_MANIFEST_DIR"), "mcpServers": [] }),
        )
        .await;
    serde_json::from_value::<acp::NewSessionResponse>(session.clone()).expect("valid NewSessionResponse");
    session["sessionId"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_full_turn_exercises_every_ui_surface() {
    let mut driver = Driver::spawn("0");
    driver
        .answer(
            method::client::FS_READ_TEXT_FILE,
            json!({ "content": include_str!("../../../demo/pristine/stats.js") }),
        )
        .answer(method::client::FS_WRITE_TEXT_FILE, json!({}))
        .answer(
            method::client::SESSION_REQUEST_PERMISSION,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        )
        .answer(method::client::TERMINAL_CREATE, json!({ "terminalId": "t1" }))
        .answer(method::client::TERMINAL_WAIT_FOR_EXIT, json!({ "exitCode": 0 }))
        .answer(method::client::TERMINAL_RELEASE, json!({}));

    let session_id = connect(&mut driver).await;
    let response = driver
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;

    assert_eq!(response["stopReason"], "end_turn");
    let t = &driver.transcript;

    // Every surface the viewer has to render showed up.
    let kinds = t.kinds();
    for required in [
        "available_commands_update",
        "agent_thought_chunk",
        "agent_message_chunk",
        "tool_call",
        "tool_call_update",
        "plan",
        "usage_update",
    ] {
        assert!(kinds.contains(&required), "no {required} in {kinds:?}");
    }

    // ...and so did every client capability the server will have to provide.
    for required in [
        method::client::FS_READ_TEXT_FILE,
        // The agent must actually apply its fix, not just describe it: the
        // test run at the end of the script only passes if the edit is real.
        method::client::FS_WRITE_TEXT_FILE,
        method::client::SESSION_REQUEST_PERMISSION,
        method::client::TERMINAL_CREATE,
        method::client::TERMINAL_WAIT_FOR_EXIT,
    ] {
        assert!(
            t.client_requests.iter().any(|m| m == required),
            "the agent never called {required}: {:?}",
            t.client_requests
        );
    }

    // The thought block arrives before the visible answer, which is what lets
    // the UI collapse it as "thinking" rather than interleave it with prose.
    let first_thought = kinds.iter().position(|k| *k == "agent_thought_chunk");
    let first_message = kinds.iter().position(|k| *k == "agent_message_chunk");
    assert!(first_thought < first_message, "{kinds:?}");

    // Streamed chunks reassemble into coherent prose.
    let text = t.assistant_text();
    assert!(text.contains("stats.js"), "{text:?}");
    assert!(text.contains("median()"), "{text:?}");

    // A diff reached the client with real before/after text.
    let diff = t
        .updates
        .iter()
        .filter_map(|u| u["content"].as_array())
        .flatten()
        .find(|c| c["type"] == "diff")
        .expect("no diff in the transcript");
    assert!(diff["oldText"].as_str().unwrap().contains("return sorted[mid];"));
    assert!(diff["newText"].as_str().unwrap().contains("/ 2"));

    // A terminal was attached to a tool call, so the UI renders a live console
    // rather than a wall of text.
    assert!(
        t.updates
            .iter()
            .filter_map(|u| u["content"].as_array())
            .flatten()
            .any(|c| c["type"] == "terminal" && c["terminalId"] == "t1"),
        "no terminal content attached to a tool call"
    );

    // The plan ends fully ticked off.
    let last_plan = t.of_kind("plan").last().copied().expect("no plan").clone();
    let statuses: Vec<&str> = last_plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["completed", "completed", "completed"]);

    driver.shutdown().await;
}

#[tokio::test]
async fn declining_permission_stops_short_of_the_terminal() {
    let mut driver = Driver::spawn("0");
    driver
        .answer(
            method::client::FS_READ_TEXT_FILE,
            json!({ "content": include_str!("../../../demo/pristine/stats.js") }),
        )
        .answer(method::client::FS_WRITE_TEXT_FILE, json!({}))
        .answer(
            method::client::SESSION_REQUEST_PERMISSION,
            json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }),
        );

    let session_id = connect(&mut driver).await;
    let response = driver
        .request(
            method::agent::SESSION_PROMPT,
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] }),
        )
        .await;

    // A refusal ends the turn normally; it is not an error.
    assert_eq!(response["stopReason"], "end_turn");
    assert!(
        !driver
            .transcript
            .client_requests
            .iter()
            .any(|m| m == method::client::TERMINAL_CREATE),
        "the agent ran a command after being told not to"
    );
    assert!(driver.transcript.assistant_text().contains("won't run"));

    driver.shutdown().await;
}

#[tokio::test]
async fn cancelling_mid_turn_stops_within_a_beat() {
    // Real pacing, so there is a turn in flight to interrupt.
    let mut driver = Driver::spawn("1");
    let session_id = connect(&mut driver).await;
    driver.cancel_on_first_update = Some(session_id.clone());

    let response = driver
        .request(
            method::agent::SESSION_PROMPT,
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] }),
        )
        .await;

    assert_eq!(response["stopReason"], "cancelled");
    // It stopped early rather than running the script to completion.
    assert!(
        !driver.transcript.kinds().contains(&"usage_update"),
        "the turn ran to completion despite being cancelled: {:?}",
        driver.transcript.kinds()
    );

    driver.shutdown().await;
}

#[tokio::test]
async fn a_client_without_fs_or_terminal_still_gets_a_turn() {
    // Not every ACP client offers every capability. The agent must degrade
    // rather than hang or crash.
    let mut driver = Driver::spawn("0");
    driver.answers.clear();

    let session_id = connect(&mut driver).await;

    // Answer every client request with an error instead of a result.
    let id = RequestId::Number(driver.next_id);
    driver.next_id += 1;
    let prompt = Frame::Request {
        id: id.clone(),
        method: method::agent::SESSION_PROMPT.into(),
        params: Some(
            serde_json::value::to_raw_value(
                &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] }),
            )
            .unwrap(),
        ),
    };
    driver.send(&prompt).await;

    let stop_reason = loop {
        match driver.next_frame().await {
            Frame::Response {
                id: ref got,
                payload: ResponsePayload::Result(ref result),
            } if *got == id => {
                let v: Value = serde_json::from_str(result.get()).unwrap();
                break v["stopReason"].as_str().unwrap().to_string();
            }
            Frame::Request { id, .. } => {
                let refusal = Frame::error(
                    id,
                    mjx_acp_core::JsonRpcError::method_not_found("unsupported here"),
                );
                driver.send(&refusal).await;
            }
            other => driver.transcript_only(other),
        }
    };

    assert_eq!(stop_reason, "end_turn");
    // It still said something useful rather than dying silently.
    assert!(!driver.transcript.assistant_text().is_empty());

    driver.shutdown().await;
}

/// A driver that can answer everything the script asks, elicitation included.
fn driver_answering_everything() -> Driver {
    let mut driver = Driver::spawn("0");
    driver
        .answer(
            method::client::FS_READ_TEXT_FILE,
            json!({ "content": include_str!("../../../demo/pristine/stats.js") }),
        )
        .answer(method::client::FS_WRITE_TEXT_FILE, json!({}))
        .answer(
            method::client::SESSION_REQUEST_PERMISSION,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        )
        .answer(
            method::client::TERMINAL_CREATE,
            json!({ "terminalId": "t1" }),
        )
        .answer(
            method::client::TERMINAL_WAIT_FOR_EXIT,
            json!({ "exitCode": 0 }),
        )
        .answer(method::client::TERMINAL_RELEASE, json!({}))
        .answer(
            method::client::ELICITATION_CREATE,
            json!({ "action": "accept",
                    "content": { "branch": "fix/median", "remote": "upstream" } }),
        );
    driver
}

/// The capabilities a client that can be elicited declares.
///
/// `{}` and not `true`: the schema types these as objects, and a lenient
/// deserializer reads `true` as absent, which turns the feature off silently.
fn elicitable() -> Value {
    json!({
        "fs": { "readTextFile": true, "writeTextFile": true },
        "terminal": true,
        "elicitation": { "form": {}, "url": {} }
    })
}

#[tokio::test]
async fn a_client_that_can_be_elicited_is_asked_in_both_modes() {
    let mut driver = driver_answering_everything();
    let session_id = connect_declaring(&mut driver, elicitable()).await;
    let response = driver
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;
    assert_eq!(response["stopReason"], "end_turn");

    let asked = driver
        .transcript
        .request_params
        .get(method::client::ELICITATION_CREATE)
        .expect("the agent never elicited");
    let modes: Vec<&str> = asked
        .iter()
        .filter_map(|params| params["mode"].as_str())
        .collect();
    assert_eq!(modes, ["form", "url"]);

    // Both are tied to the session, which is what puts them in a thread. Only
    // the form names a tool call.
    assert_eq!(asked[0]["sessionId"], session_id.as_str());
    assert_eq!(asked[0]["toolCallId"], "call_edit");
    assert_eq!(asked[1]["sessionId"], session_id.as_str());

    // The URL exchange is finished by a notification, not by the response.
    assert_eq!(
        driver
            .transcript
            .completed_elicitations
            .iter()
            .filter_map(|params| params["elicitationId"].as_str())
            .collect::<Vec<_>>(),
        [asked[1]["elicitationId"].as_str().unwrap()]
    );

    // What the user filled in is read, not ignored.
    let text = driver.transcript.assistant_text();
    assert!(text.contains("upstream/fix/median"), "{text:?}");

    driver.shutdown().await;
}

#[tokio::test]
async fn cancelling_while_a_form_is_open_gives_up_on_it() {
    // Without this the turn would sit on the question forever, and
    // `session/cancel` would only take effect once somebody answered a form
    // they had already walked away from.
    let mut driver = driver_answering_everything();
    let session_id = connect_declaring(&mut driver, elicitable()).await;
    driver.cancel_when_asked = Some((
        method::client::ELICITATION_CREATE.to_string(),
        session_id.clone(),
    ));

    let response = driver
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;

    assert_eq!(response["stopReason"], "cancelled");
    // It stopped at the form rather than carrying on to the URL exchange.
    assert_eq!(
        driver
            .transcript
            .request_params
            .get(method::client::ELICITATION_CREATE)
            .map(Vec::len),
        Some(1)
    );

    driver.shutdown().await;
}

#[tokio::test]
async fn a_client_that_declared_nothing_is_never_elicited() {
    // The protocol's rule, not caution: eliciting from a client that did not
    // offer to would come straight back as an error, and the user would watch a
    // turn fail for no visible reason.
    let mut driver = driver_answering_everything();
    let session_id = connect(&mut driver).await;
    driver
        .request(
            method::agent::SESSION_PROMPT,
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "fix the median bug" }]
            }),
        )
        .await;

    assert!(
        !driver
            .transcript
            .client_requests
            .iter()
            .any(|m| m == method::client::ELICITATION_CREATE),
        "elicited a client that never said it could render one: {:?}",
        driver.transcript.client_requests
    );
    assert!(driver.transcript.completed_elicitations.is_empty());

    driver.shutdown().await;
}

impl Driver {
    /// Records a notification without answering anything.
    fn transcript_only(&mut self, frame: Frame) {
        if let Frame::Notification { params, .. } = frame
            && let Some(params) = params
            && let Ok(params) = serde_json::from_str::<Value>(params.get())
        {
            self.transcript.updates.push(params["update"].clone());
        }
    }
}

/// The sessions the agent lists, newest first.
async fn list_sessions(driver: &mut Driver, params: Value) -> Vec<Value> {
    let listed = driver.request(method::agent::SESSION_LIST, params).await;
    serde_json::from_value::<acp::ListSessionsResponse>(listed.clone())
        .unwrap_or_else(|e| panic!("invalid ListSessionsResponse: {e}\n{listed:#}"));
    listed["sessions"].as_array().cloned().unwrap_or_default()
}

/// The one listed session with this id, if it is still listed.
fn listed<'a>(sessions: &'a [Value], session_id: &str) -> Option<&'a Value> {
    sessions.iter().find(|s| s["sessionId"] == session_id)
}

#[tokio::test]
async fn the_agent_offers_the_session_lifecycle_it_implements() {
    // A client renders what the agent advertises and nothing else, so what is
    // claimed here has to match what the handlers below actually answer.
    let mut driver = Driver::spawn("0");
    let init = driver
        .request(
            method::agent::INITIALIZE,
            json!({ "protocolVersion": mjx_acp_core::PROTOCOL_VERSION, "clientCapabilities": {} }),
        )
        .await;

    let capabilities = &init["agentCapabilities"];
    assert_eq!(capabilities["loadSession"], true);
    for offered in ["list", "delete", "fork", "resume", "close"] {
        assert!(
            capabilities["sessionCapabilities"][offered].is_object(),
            "{offered} must be advertised as an object, not {:?}",
            capabilities["sessionCapabilities"][offered]
        );
    }

    driver.shutdown().await;
}

#[tokio::test]
async fn a_conversation_from_before_this_run_is_listed_and_titled() {
    // The seeded session is what makes the history worth opening the first time
    // the demo is run: without it the list holds only what you just started.
    let mut driver = Driver::spawn("0");
    let session_id = connect(&mut driver).await;

    let sessions = list_sessions(&mut driver, json!({})).await;
    let seeded = sessions
        .iter()
        .find(|s| s["sessionId"] != session_id)
        .expect("a session from before this run");
    serde_json::from_value::<acp::SessionInfo>(seeded.clone()).expect("valid SessionInfo");

    assert!(seeded["title"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(seeded["updatedAt"].as_str().is_some());
    // Newest first, and the one just created is the newest.
    assert_eq!(sessions[0]["sessionId"], session_id);

    driver.shutdown().await;
}

#[tokio::test]
async fn a_loaded_session_replays_its_history_before_it_answers() {
    // The ordering the client has to survive: `session/load` streams the whole
    // conversation back as notifications, and they arrive *during* the call.
    let mut driver = Driver::spawn("0");
    let session_id = connect(&mut driver).await;
    let sessions = list_sessions(&mut driver, json!({})).await;
    let seeded = sessions
        .iter()
        .find(|s| s["sessionId"] != session_id)
        .expect("a session from before this run")["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let before = driver.transcript.updates.len();
    let loaded = driver
        .request(
            method::agent::SESSION_LOAD,
            json!({ "sessionId": seeded, "cwd": env!("CARGO_MANIFEST_DIR"), "mcpServers": [] }),
        )
        .await;
    serde_json::from_value::<acp::LoadSessionResponse>(loaded.clone())
        .unwrap_or_else(|e| panic!("invalid LoadSessionResponse: {e}\n{loaded:#}"));

    let replayed = driver.transcript.updates[before..].to_vec();
    assert!(
        !replayed.is_empty(),
        "a load that replays nothing is a load that lost the conversation"
    );
    assert_eq!(replayed[0]["sessionUpdate"], "user_message_chunk");
    assert!(loaded["configOptions"].is_array());

    // And again: a second load replays the same history, not twice as much.
    let before = driver.transcript.updates.len();
    driver
        .request(
            method::agent::SESSION_LOAD,
            json!({ "sessionId": seeded, "cwd": env!("CARGO_MANIFEST_DIR"), "mcpServers": [] }),
        )
        .await;
    assert_eq!(driver.transcript.updates.len() - before, replayed.len());

    driver.shutdown().await;
}

#[tokio::test]
async fn a_resumed_session_answers_without_replaying() {
    // The difference from `session/load`, and the only reason both exist.
    let mut driver = Driver::spawn("0");
    let session_id = connect(&mut driver).await;

    let before = driver.transcript.updates.len();
    let resumed = driver
        .request(
            method::agent::SESSION_RESUME,
            json!({ "sessionId": session_id, "cwd": env!("CARGO_MANIFEST_DIR") }),
        )
        .await;

    assert_eq!(driver.transcript.updates.len(), before, "a resume replayed");
    assert!(resumed["configOptions"].is_array());

    driver.shutdown().await;
}

#[tokio::test]
async fn a_fork_is_a_second_session_carrying_the_first_ones_history() {
    let mut driver = Driver::spawn("0");
    let session_id = connect(&mut driver).await;

    let forked = driver
        .request(
            method::agent::SESSION_FORK,
            json!({ "sessionId": session_id, "cwd": env!("CARGO_MANIFEST_DIR") }),
        )
        .await;
    serde_json::from_value::<acp::ForkSessionResponse>(forked.clone())
        .unwrap_or_else(|e| panic!("invalid ForkSessionResponse: {e}\n{forked:#}"));

    let fork_id = forked["sessionId"].as_str().expect("a forked session id");
    assert_ne!(fork_id, session_id, "a fork is a new session, not the old one");

    let sessions = list_sessions(&mut driver, json!({})).await;
    assert!(listed(&sessions, fork_id).is_some(), "the fork is not listed");

    driver.shutdown().await;
}

#[tokio::test]
async fn deleting_forgets_a_session_and_closing_only_frees_it() {
    // The protocol keeps these apart: `session/close` frees the resources a
    // session is holding, `session/delete` is what removes it from the list.
    let mut driver = Driver::spawn("0");
    let session_id = connect(&mut driver).await;
    let doomed = driver
        .request(
            method::agent::SESSION_FORK,
            json!({ "sessionId": session_id, "cwd": env!("CARGO_MANIFEST_DIR") }),
        )
        .await["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    driver
        .request(method::agent::SESSION_CLOSE, json!({ "sessionId": session_id }))
        .await;
    let sessions = list_sessions(&mut driver, json!({})).await;
    assert!(
        listed(&sessions, &session_id).is_some(),
        "a closed session is still a session that happened"
    );

    driver
        .request(method::agent::SESSION_DELETE, json!({ "sessionId": &doomed }))
        .await;
    let sessions = list_sessions(&mut driver, json!({})).await;
    assert!(listed(&sessions, &doomed).is_none(), "a deleted session is still listed");

    driver.shutdown().await;
}

#[tokio::test]
async fn sessions_can_be_listed_by_the_directory_they_belong_to() {
    let mut driver = Driver::spawn("0");
    let session_id = connect(&mut driver).await;

    let mine = list_sessions(&mut driver, json!({ "cwd": env!("CARGO_MANIFEST_DIR") })).await;
    assert!(listed(&mine, &session_id).is_some());

    let elsewhere = list_sessions(&mut driver, json!({ "cwd": "/nowhere/in/particular" })).await;
    assert!(elsewhere.is_empty(), "{elsewhere:#?}");

    driver.shutdown().await;
}
