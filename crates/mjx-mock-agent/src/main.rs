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
    /// Every conversation this agent has, so it can list one and load it back.
    ///
    /// A `std::sync::Mutex` rather than tokio's, because [`Agent::update`] is
    /// synchronous and that is the funnel every update passes through — which is
    /// what makes it impossible to emit something and forget to record it. The
    /// guard is never held across an await: a replay clones the transcript out
    /// first.
    sessions: std::sync::Mutex<HashMap<String, MockSession>>,
}

/// One conversation the agent remembers.
struct MockSession {
    /// Where it was started. `session/list` can be filtered by this.
    cwd: String,
    /// Taken from the first thing the user said, the way a real agent titles a
    /// conversation.
    title: Option<String>,
    updated_at: String,
    /// The `update` object of every `session/update` sent for this session, in
    /// order, so `session/load` can stream the conversation back.
    transcript: Vec<Value>,
}

impl Agent {
    /// Sends a notification to the client.
    pub fn notify(&self, method: &str, params: Value) {
        let frame = Frame::notification(method, &params).expect("json! values always serialize");
        let _ = self.outbox.send(frame);
    }

    /// Sends a `session/update` notification, and remembers it.
    ///
    /// Remembering here rather than at the call sites is deliberate: the whole
    /// point of a transcript is that it is complete, and one step of the script
    /// that reached for [`Agent::emit`] instead would leave a hole in the
    /// history that only shows up when somebody loads the session back.
    pub fn update(&self, session_id: &str, update: Value) {
        self.record(session_id, update.clone());
        self.emit(session_id, update);
    }

    /// Sends a `session/update` *without* recording it.
    ///
    /// Only a replay may do this: the updates it streams are already in the
    /// transcript, and recording them again would make every load of a session
    /// longer than the last.
    fn emit(&self, session_id: &str, update: Value) {
        self.notify(
            method::client::SESSION_UPDATE,
            json!({ "sessionId": session_id, "update": update }),
        );
    }

    /// Appends to a session's transcript, and marks it as touched just now.
    fn record(&self, session_id: &str, update: Value) {
        let mut sessions = self.sessions();
        // An update for a session we never opened belongs to no conversation,
        // which is not an error worth failing a turn over.
        if let Some(session) = sessions.get_mut(session_id) {
            session.transcript.push(update);
            session.updated_at = now();
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, MockSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    /// Opens a conversation the agent will remember.
    fn open_session(&self, session_id: &str, cwd: &str, title: Option<String>, from: &[Value]) {
        self.sessions().insert(
            session_id.to_string(),
            MockSession {
                cwd: cwd.to_string(),
                title,
                updated_at: now(),
                transcript: from.to_vec(),
            },
        );
    }

    /// What `session/list` says, newest first.
    ///
    /// Sorted by `updatedAt` descending, which works on the string because ISO
    /// 8601 in UTC sorts the same way the instants do.
    fn list_sessions(&self, cwd: Option<&str>) -> Vec<Value> {
        let sessions = self.sessions();
        let mut listed: Vec<Value> = sessions
            .iter()
            .filter(|(_, session)| cwd.is_none_or(|cwd| session.cwd == cwd))
            .map(|(id, session)| {
                let mut info = json!({
                    "sessionId": id,
                    "cwd": session.cwd,
                    "updatedAt": session.updated_at,
                });
                // Omitted rather than null when the conversation has not been
                // said anything to yet: the schema reads both the same way, and
                // a client showing an empty title has nothing to show.
                if let Some(title) = &session.title {
                    info["title"] = json!(title);
                }
                info
            })
            .collect();
        listed.sort_by(|a, b| b["updatedAt"].as_str().cmp(&a["updatedAt"].as_str()));
        listed
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
        sessions: std::sync::Mutex::new(HashMap::new()),
    });
    seed_yesterdays_conversation(&agent).await;

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
                    "loadSession": true,
                    "promptCapabilities": {
                        "image": false, "audio": false, "embeddedContext": true
                    },
                    // `{}` and not `true` for each: the schema reads anything
                    // that is not an object as "not supported", so a client
                    // would quietly offer none of this.
                    "sessionCapabilities": {
                        "list": {}, "delete": {}, "fork": {}, "resume": {}, "close": {}
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
            agent.open_session(&session_id, cwd_of(&params).as_str(), None, &[]);
            Ok(json!({
                "sessionId": session_id,
                "modes": modes(),
                "configOptions": config_options(&default_config_values())
            }))
        }

        m::SESSION_LIST => {
            let sessions = agent.list_sessions(params["cwd"].as_str());
            // No `nextCursor`: everything this agent knows fits in one page, and
            // claiming otherwise would send a client asking for a page that
            // does not exist.
            Ok(json!({ "sessions": sessions }))
        }

        m::SESSION_LOAD => {
            let session_id = session_id_of(&params)?;
            let transcript = {
                let sessions = agent.sessions();
                let session = sessions
                    .get(&session_id)
                    .ok_or_else(|| JsonRpcError::invalid_params("no such session"))?;
                // Cloned out from under the lock: the replay below awaits, and
                // this guard must not be held across that.
                session.transcript.clone()
            };

            // The whole conversation, as the notifications that built it. They
            // go out *before* this response, which is the ordering a client has
            // to survive — see the client-side note in `web/src/acp/session.ts`.
            for update in transcript {
                agent.emit(&session_id, update);
                beat(15).await;
            }

            Ok(json!({
                "modes": modes(),
                "configOptions": agent.config_options(&session_id).await
            }))
        }

        m::SESSION_RESUME => {
            let session_id = session_id_of(&params)?;
            if !agent.sessions().contains_key(&session_id) {
                return Err(JsonRpcError::invalid_params("no such session"));
            }
            // Deliberately silent. A resume reactivates a session without
            // replaying it, which is the only thing that makes it different
            // from a load.
            Ok(json!({
                "modes": modes(),
                "configOptions": agent.config_options(&session_id).await
            }))
        }

        m::SESSION_FORK => {
            let session_id = session_id_of(&params)?;
            let (cwd, title, transcript) = {
                let sessions = agent.sessions();
                let source = sessions
                    .get(&session_id)
                    .ok_or_else(|| JsonRpcError::invalid_params("no such session"))?;
                (
                    source.cwd.clone(),
                    source.title.clone(),
                    source.transcript.clone(),
                )
            };

            let values = agent
                .config
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .unwrap_or_else(default_config_values);
            let fork_id = format!("mock-{}", short_id());
            agent
                .config
                .lock()
                .await
                .insert(fork_id.clone(), values.clone());
            // A fork carries the history of the session it came from — that is
            // what distinguishes it from a new session — but is its own
            // conversation from here on.
            agent.open_session(
                &fork_id,
                &cwd,
                Some(match title {
                    Some(title) => format!("Fork of {title}"),
                    None => format!("Fork of {session_id}"),
                }),
                &transcript,
            );

            Ok(json!({
                "sessionId": fork_id,
                "modes": modes(),
                "configOptions": config_options(&values)
            }))
        }

        m::SESSION_DELETE => {
            let session_id = session_id_of(&params)?;
            agent.sessions().remove(&session_id);
            agent.config.lock().await.remove(&session_id);
            Ok(json!({}))
        }

        m::SESSION_CLOSE => {
            // Nothing to free in a mock, and nothing to forget: closing a
            // session releases its resources, and `session/delete` is what
            // removes it from the list. Keeping the entry is what lets the
            // client show a conversation it has finished with.
            session_id_of(&params)?;
            Ok(json!({}))
        }

        m::SESSION_PROMPT => {
            let session_id = params["sessionId"]
                .as_str()
                .ok_or_else(|| JsonRpcError::invalid_params("sessionId is required"))?
                .to_string();
            // Recorded before the turn runs, and titled from it if this is the
            // first thing said: a conversation nobody can tell apart in a list
            // is one nobody will open.
            title_and_record_prompt(agent, &session_id, &params);
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

        m::AUTHENTICATE => Ok(json!({})),

        other => Err(JsonRpcError::method_not_found(other)),
    }
}

/// The `sessionId` a lifecycle request names.
fn session_id_of(params: &Value) -> Result<String, JsonRpcError> {
    params["sessionId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| JsonRpcError::invalid_params("sessionId is required"))
}

/// The directory a session belongs to, falling back to the one we were started
/// in — which, for an agent spawned per workspace, is the same thing.
fn cwd_of(params: &Value) -> String {
    match params["cwd"].as_str() {
        Some(cwd) if !cwd.is_empty() => cwd.to_string(),
        _ => working_directory(),
    }
}

fn working_directory() -> String {
    std::env::current_dir()
        .map(|cwd| cwd.display().to_string())
        .unwrap_or_default()
}

/// The modes every session here offers.
fn modes() -> Value {
    json!({
        "currentModeId": "code",
        "availableModes": [
            { "id": "ask",  "name": "Ask",  "description": "Answer questions, change nothing." },
            { "id": "code", "name": "Code", "description": "Read and edit files." },
        ]
    })
}

/// Puts the user's prompt in the session's history, and titles the session with
/// it if this is the first thing said.
///
/// Recorded but not emitted: the client shows its own prompt the moment it sends
/// it, so echoing it back live would say it twice. It has to be in the
/// transcript all the same, or a loaded conversation is only the agent's half of
/// it.
fn title_and_record_prompt(agent: &Agent, session_id: &str, params: &Value) {
    let text: String = params["prompt"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        return;
    }

    agent.record(
        session_id,
        json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text }
        }),
    );

    let mut sessions = agent.sessions();
    if let Some(session) = sessions.get_mut(session_id)
        && session.title.is_none()
    {
        session.title = Some(summarise(&text));
    }
}

/// A one-line title from what the user said.
fn summarise(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    match line.char_indices().nth(60) {
        Some((cut, _)) => format!("{}…", &line[..cut].trim_end()),
        None => line.to_string(),
    }
}

/// A conversation from before this run, so the history is worth opening the
/// first time the demo starts.
///
/// It is written as the update stream that produced it rather than as a
/// finished thread, because that is what `session/load` has to replay and what
/// a client has to fold — a canned thread would prove nothing about either.
async fn seed_yesterdays_conversation(agent: &Arc<Agent>) {
    let session_id = "mock-yesterday".to_string();
    agent.config.lock().await.insert(
        session_id.clone(),
        HashMap::from([
            ("model".to_string(), json!("mock-haiku")),
            ("thought_level".to_string(), json!("balanced")),
            ("web_search".to_string(), json!(false)),
        ]),
    );

    agent.open_session(
        &session_id,
        &working_directory(),
        Some("Rename the helper in stats.js".to_string()),
        &[
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "rename `avg` to `mean` in stats.js" }
            }),
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "One definition and two call sites." }
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "yesterday-1",
                "title": "Rename avg to mean",
                "kind": "edit",
                "status": "completed",
                "content": [{
                    "type": "diff",
                    "path": "stats.js",
                    "oldText": "export function avg(xs) {\n",
                    "newText": "export function mean(xs) {\n"
                }]
            }),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Renamed it, and both call sites with it." }
            }),
        ],
    );

    // Older than anything this run will produce, so the list has an order worth
    // sorting.
    if let Some(session) = agent.sessions().get_mut(&session_id) {
        session.updated_at = iso8601(unix_seconds().saturating_sub(26 * 60 * 60));
    }
}

/// Now, as ACP wants a timestamp: ISO 8601 in UTC.
fn now() -> String {
    iso8601(unix_seconds())
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Formats a Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Written out rather than pulled in: a date crate for one timestamp in a
/// fixture agent is a dependency the whole workspace then carries. The civil
/// calendar conversion is Howard Hinnant's `civil_from_days`, which is exact for
/// every date this will ever see.
fn iso8601(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;

    // Shift the epoch to 0000-03-01, so leap days land at the end of the cycle
    // and the year/month arithmetic below needs no special cases.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;

    let day = (day_of_year - (153 * march_month + 2) / 5 + 1) as u32;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    } as u32;
    let year = era * 400 + year_of_era + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_iso_8601_in_utc() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // A leap day, which is the case the calendar arithmetic exists for.
        assert_eq!(iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(iso8601(1_767_225_599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn timestamps_sort_the_way_the_instants_do() {
        // `session/list` orders by this string, so the two orders have to agree.
        assert!(iso8601(1_709_164_800) < iso8601(1_767_225_599));
    }

    #[test]
    fn a_title_is_one_line_and_short_enough_to_read() {
        assert_eq!(summarise("fix the median bug\nand the mean"), "fix the median bug");
        let long = summarise(&"a".repeat(200));
        assert!(long.ends_with('…') && long.chars().count() == 61, "{long}");
    }
}
