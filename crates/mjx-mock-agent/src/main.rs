//! A scripted ACP agent that needs no credentials and no network.
//!
//! It exists so `./scripts/demo.sh` works on a machine with nothing installed,
//! and so the integration tests have a deterministic peer. One turn walks
//! through every client UI surface in order: a thought block, streaming text, a
//! read tool call, a plan, an edit tool call carrying a diff, a permission
//! request, and a live terminal.
//!
//! It speaks the wire format directly via `serde_json::json!` rather than
//! through the schema types. That is deliberate: the mock is a *fixture*, and
//! writing the JSON by hand means the tests check our understanding of the
//! protocol rather than checking a serializer against itself. The tests in
//! `script.rs` re-parse everything it emits with the real schema types, so a
//! typo fails the build rather than the demo.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use anyhow::Result;
use mjx_acp_core::{Frame, JsonRpcError, RequestId, ResponsePayload, method};
use serde_json::{Value, json, value::RawValue};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

mod script;

/// Wall-clock pacing. Tests set `MJX_MOCK_SPEED=0` to run the script instantly;
/// the demo leaves it at 1 so streaming is visible.
fn speed() -> f32 {
    std::env::var("MJX_MOCK_SPEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

/// Sleeps for `ms` scaled by [`speed`].
pub async fn beat(ms: u64) {
    let scaled = (ms as f32 * speed()) as u64;
    if scaled > 0 {
        tokio::time::sleep(Duration::from_millis(scaled)).await;
    }
}

/// What a client request came back with.
pub type Reply = Result<Box<RawValue>, JsonRpcError>;

/// The agent's shared state: an outbox, and the bookkeeping for requests we
/// have sent the client and are waiting on.
pub struct Agent {
    outbox: mpsc::UnboundedSender<Frame>,
    next_request_id: AtomicI64,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Reply>>>,
    /// One flag per session, raised by `session/cancel`. The script checks it
    /// between steps.
    cancelled: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// The current value of each config option, per session. Only the values
    /// are kept: what is *offered* is fixed, and [`config_options`] rebuilds
    /// the advertised set from these so the two cannot drift.
    config: Mutex<HashMap<String, HashMap<String, Value>>>,
    /// Whether the client said it can render a form, from `initialize`.
    ///
    /// A conformant agent asks only for what the client offered: an elicitation
    /// sent to a client that declared none comes straight back as an error, and
    /// the user is left looking at a turn that failed for no visible reason.
    elicitation_form: AtomicBool,
    /// Whether the client said it can send the user to a URL.
    elicitation_url: AtomicBool,
}

impl Agent {
    /// Sends a notification to the client.
    pub fn notify(&self, method: &str, params: Value) {
        let frame = Frame::notification(method, &params).expect("json! values always serialize");
        let _ = self.outbox.send(frame);
    }

    /// Sends a `session/update` notification.
    pub fn update(&self, session_id: &str, update: Value) {
        self.notify(
            method::client::SESSION_UPDATE,
            json!({ "sessionId": session_id, "update": update }),
        );
    }

    /// Calls a client method and waits for the response.
    pub async fn request(&self, method: &str, params: Value) -> Reply {
        let id = RequestId::Number(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let frame = Frame::Request {
            id: id.clone(),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).expect("json! serializes")),
        };
        if self.outbox.send(frame).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(JsonRpcError::internal("connection closed"));
        }

        rx.await
            .unwrap_or_else(|_| Err(JsonRpcError::internal("connection closed")))
    }

    /// Calls a client method, giving up if the turn is cancelled first.
    ///
    /// `None` means it gave up. A real agent stops waiting on a question the
    /// user has walked away from; without this a `session/cancel` would have no
    /// effect until somebody answered a form nobody cares about any more.
    ///
    /// The abandoned entry stays in `pending` until the process exits. That is a
    /// leak, and a deliberate one: this is a fixture with a lifetime measured in
    /// seconds, and reaping it would need the request future to own its own
    /// cleanup for no benefit anyone can observe.
    pub async fn request_until_cancelled(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Option<Reply> {
        tokio::select! {
            reply = self.request(method, params) => Some(reply),
            () = self.until_cancelled(session_id) => None,
        }
    }

    /// Resolves once this session's turn has been cancelled.
    ///
    /// Polled rather than signalled: cancellation is already an `AtomicBool` the
    /// script checks between steps, and one waiter does not earn a channel.
    async fn until_cancelled(&self, session_id: &str) {
        while !self.is_cancelled(session_id).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether the client can render a form we ask it to.
    pub fn can_elicit_form(&self) -> bool {
        self.elicitation_form.load(Ordering::Relaxed)
    }

    /// Whether the client can send the user to a URL for us.
    pub fn can_elicit_url(&self) -> bool {
        self.elicitation_url.load(Ordering::Relaxed)
    }

    /// Whether the given session's turn has been cancelled.
    pub async fn is_cancelled(&self, session_id: &str) -> bool {
        self.cancelled
            .lock()
            .await
            .get(session_id)
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    async fn begin_turn(&self, session_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancelled
            .lock()
            .await
            .insert(session_id.to_string(), flag.clone());
        flag
    }

    /// This session's config options as they currently stand.
    pub async fn config_options(&self, session_id: &str) -> Value {
        let config = self.config.lock().await;
        match config.get(session_id) {
            Some(values) => config_options(values),
            None => config_options(&default_config_values()),
        }
    }

    /// Records a new value and returns the whole refreshed set.
    async fn set_config_option(&self, session_id: &str, config_id: &str, value: Value) -> Value {
        let mut config = self.config.lock().await;
        let values = config
            .entry(session_id.to_string())
            .or_insert_with(default_config_values);
        // An id we do not offer is ignored rather than stored. Storing it would
        // have no effect on what is advertised and would only leak memory.
        if values.contains_key(config_id) {
            values.insert(config_id.to_string(), value);
        }
        config_options(values)
    }

    async fn resolve(&self, id: &RequestId, reply: Reply) {
        if let Some(tx) = self.pending.lock().await.remove(id) {
            let _ = tx.send(reply);
        } else {
            tracing::warn!(%id, "response to a request we never sent");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr; stdout is the protocol channel and must stay clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MJX_MOCK_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let (outbox, mut outbox_rx) = mpsc::unbounded_channel::<Frame>();
    let agent = Arc::new(Agent {
        outbox,
        next_request_id: AtomicI64::new(1),
        pending: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(HashMap::new()),
        config: Mutex::new(HashMap::new()),
        elicitation_form: AtomicBool::new(false),
        elicitation_url: AtomicBool::new(false),
    });

    // A single writer task owns stdout, so concurrent handlers can't interleave
    // halves of two frames onto the same line.
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = outbox_rx.recv().await {
            let mut line = frame.to_line();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match Frame::parse(&line) {
            Ok(frame) => dispatch(agent.clone(), frame).await,
            Err(err) => tracing::warn!(%err, line, "unparseable frame"),
        }
    }

    drop(agent);
    let _ = writer.await;
    Ok(())
}

async fn dispatch(agent: Arc<Agent>, frame: Frame) {
    match frame {
        Frame::Response { id, payload } => {
            let reply = match payload {
                ResponsePayload::Result(result) => Ok(result),
                ResponsePayload::Error(error) => Err(error),
            };
            agent.resolve(&id, reply).await;
        }
        Frame::Notification { method, params } => {
            if method == method::agent::SESSION_CANCEL {
                let session_id = params
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<Value>(p.get()).ok())
                    .and_then(|p| p["sessionId"].as_str().map(str::to_owned));
                if let Some(session_id) = session_id
                    && let Some(flag) = agent.cancelled.lock().await.get(&session_id)
                {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        }
        // Requests are spawned rather than awaited inline: a `session/prompt`
        // runs for seconds, and `session/cancel` has to be read off stdin while
        // it does.
        Frame::Request { id, method, params } => {
            tokio::spawn(async move {
                let params: Value = params
                    .as_deref()
                    .and_then(|p| serde_json::from_str(p.get()).ok())
                    .unwrap_or(Value::Null);
                let reply = handle(&agent, &method, params).await;
                let frame = match reply {
                    Ok(result) => Frame::result(id, &result).expect("results serialize"),
                    Err(error) => Frame::error(id, error),
                };
                let _ = agent.outbox.send(frame);
            });
        }
    }
}

async fn handle(agent: &Arc<Agent>, method: &str, params: Value) -> Result<Value, JsonRpcError> {
    use mjx_acp_core::method::agent as m;
    match method {
        m::INITIALIZE => {
            // Remember what the client offered, because the script has steps
            // that must not run otherwise. Tested for being an *object*: the
            // schema spells "supported" as `{}` and "not" as absent or null, so
            // a bare `true` means the client got it wrong and gets nothing.
            let elicitation = &params["clientCapabilities"]["elicitation"];
            agent
                .elicitation_form
                .store(elicitation["form"].is_object(), Ordering::Relaxed);
            agent
                .elicitation_url
                .store(elicitation["url"].is_object(), Ordering::Relaxed);

            Ok(json!({
                "protocolVersion": mjx_acp_core::PROTOCOL_VERSION,
                "agentInfo": {
                    "name": "mjx-mock-agent", "version": env!("CARGO_PKG_VERSION")
                },
                "agentCapabilities": {
                    "loadSession": false,
                    "promptCapabilities": {
                        "image": false, "audio": false, "embeddedContext": true
                    }
                },
                // No auth methods: this agent is the whole point of "works out
                // of the box".
                "authMethods": []
            }))
        }

        m::SESSION_NEW => {
            let session_id = format!("mock-{}", short_id());
            agent
                .config
                .lock()
                .await
                .insert(session_id.clone(), default_config_values());
            Ok(json!({
                "sessionId": session_id,
                "modes": {
                    "currentModeId": "code",
                    "availableModes": [
                        { "id": "ask",  "name": "Ask",  "description": "Answer questions, change nothing." },
                        { "id": "code", "name": "Code", "description": "Read and edit files." },
                    ]
                },
                "configOptions": config_options(&default_config_values())
            }))
        }

        m::SESSION_PROMPT => {
            let session_id = params["sessionId"]
                .as_str()
                .ok_or_else(|| JsonRpcError::invalid_params("sessionId is required"))?
                .to_string();
            let flag = agent.begin_turn(&session_id).await;
            let stop_reason = script::run_turn(agent, &session_id, &params).await;
            flag.store(false, Ordering::Relaxed);
            Ok(json!({ "stopReason": stop_reason }))
        }

        m::SESSION_SET_MODE => {
            let mode = params["modeId"].as_str().unwrap_or("code").to_string();
            if let Some(session_id) = params["sessionId"].as_str() {
                agent.update(
                    session_id,
                    json!({ "sessionUpdate": "current_mode_update", "currentModeId": mode }),
                );
            }
            Ok(json!({}))
        }
        m::SESSION_SET_CONFIG_OPTION => {
            let session_id = params["sessionId"]
                .as_str()
                .ok_or_else(|| JsonRpcError::invalid_params("sessionId is required"))?;
            let config_id = params["configId"]
                .as_str()
                .ok_or_else(|| JsonRpcError::invalid_params("configId is required"))?;

            // No `config_option_update` notification here on purpose. The
            // response already carries the whole refreshed set, so a
            // notification would say the same thing twice. The notification is
            // for changes the *agent* makes on its own, which the script does.
            let options = agent
                .set_config_option(session_id, config_id, params["value"].clone())
                .await;
            Ok(json!({ "configOptions": options }))
        }

        m::AUTHENTICATE | m::SESSION_CLOSE => Ok(json!({})),

        other => Err(JsonRpcError::method_not_found(other)),
    }
}

/// What every session starts with.
fn default_config_values() -> HashMap<String, Value> {
    HashMap::from([
        ("model".to_string(), json!("mock-sonnet")),
        ("thought_level".to_string(), json!("balanced")),
        ("web_search".to_string(), json!(false)),
    ])
}

/// The advertised config options, with `values` filled in as current.
///
/// One of each control the client can render: a *grouped* select, because a
/// real model list is grouped and that is a separate rendering path; a flat
/// select; and a boolean, which is the only variant a client must declare a
/// capability for. Between them the viewer's config UI has no untested branch.
fn config_options(values: &HashMap<String, Value>) -> Value {
    let current = |id: &str| values.get(id).cloned().unwrap_or(Value::Null);

    json!([
        {
            "id": "model",
            "name": "Model",
            "description": "Which model answers.",
            "category": "model",
            "type": "select",
            "currentValue": current("model"),
            "options": [
                { "group": "fast", "name": "Fast", "options": [
                    { "value": "mock-haiku", "name": "Mock Haiku",
                      "description": "Quick, and cheap to be wrong with." }
                ]},
                { "group": "capable", "name": "Capable", "options": [
                    { "value": "mock-sonnet", "name": "Mock Sonnet",
                      "description": "The balanced default." },
                    { "value": "mock-opus", "name": "Mock Opus",
                      "description": "Slower, and thinks harder about it." }
                ]}
            ]
        },
        {
            "id": "thought_level",
            "name": "Thinking",
            "category": "thought_level",
            "type": "select",
            "currentValue": current("thought_level"),
            "options": [
                { "value": "off", "name": "Off" },
                { "value": "balanced", "name": "Balanced" },
                { "value": "deep", "name": "Deep" }
            ]
        },
        {
            "id": "web_search",
            "name": "Web search",
            "description": "Let the agent look things up.",
            "type": "boolean",
            "currentValue": current("web_search")
        }
    ])
}

/// A short id, unique enough within one process run. Saves a uuid dependency
/// for something that never leaves this binary.
fn short_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicI64 = AtomicI64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{n:x}")
}
