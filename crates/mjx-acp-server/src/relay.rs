//! The relay: one WebSocket to one agent subprocess.
//!
//! Both hops carry the same newline-delimited JSON-RPC, so the browser is an
//! ordinary ACP client and the agent is an ordinary ACP agent — neither knows
//! the other is remote. The relay's only jobs are to move frames between them
//! and to answer, on the browser's behalf, the client methods a browser
//! physically cannot implement.
//!
//! Anything it fails to classify is forwarded verbatim. A relay that only
//! passes messages it understands breaks the day either side speaks a newer
//! protocol version, so "forward it" is the default for everything.

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use mjx_acp_core::{Direction, Frame, JsonRpcError, MethodCorrelator, ext, method};

use crate::agent_process::{AgentHandle, AgentProcess};
use crate::id_bridge::IdBridge;
use crate::sessions::SessionStore;

/// How a frame should be handled.
///
/// Only `Forward` is used by the trait defaults; the other two exist for the
/// filesystem and terminal interceptor, which is the only thing that rewrites
/// or answers a frame.
#[allow(
    dead_code,
    reason = "Rewrite and Intercept are used by the fs/terminal interceptor"
)]
pub enum Disposition {
    /// Pass it on unchanged.
    Forward,
    /// Pass on this rewritten frame instead.
    Rewrite(Frame),
    /// Don't pass it on. Any reply is the interceptor's responsibility.
    Intercept,
}

/// Decides what happens to each frame, and services the client methods the
/// server owns.
///
/// The server installs exactly one: `WorkspaceInterceptor`. The trait exists
/// so the relay itself stays ignorant of what the filesystem and terminal
/// capabilities need.
pub trait Interceptor: Send + Sync + 'static {
    /// Called once before any frames flow, so an interceptor can start
    /// background work that needs to write to the connection.
    fn start(&self, outbox: &Outbox) {
        let _ = outbox;
    }

    /// Called for every frame from the browser, before it reaches the agent.
    fn on_client_frame(&self, frame: &Frame, outbox: &Outbox) -> Disposition {
        let _ = (frame, outbox);
        Disposition::Forward
    }

    /// Called for every frame from the agent, before it reaches the browser.
    ///
    /// Returning [`Disposition::Intercept`] makes the interceptor responsible
    /// for answering: the agent is waiting on a response that will never come
    /// from the browser.
    fn on_agent_frame(&self, frame: &Frame, outbox: &Outbox) -> Disposition {
        let _ = (frame, outbox);
        Disposition::Forward
    }

    /// Called once the connection has ended.
    fn stop(&self) {}
}

/// The browser end of an outbox: whichever socket is attached right now, if any.
///
/// An agent outlives the socket that started it, so the thing on the far end of
/// this changes over a connection's life — a reload detaches one browser and
/// attaches the next. The indirection lives *inside* the sink rather than around
/// [`Outbox`], because clones of the outbox are already out in detached tasks by
/// the time a swap happens (the interceptor's event mirror, and one task per
/// in-flight `fs/*` or `terminal/*` request) and those clones cannot be
/// re-pointed after the fact.
#[derive(Clone, Default)]
pub struct BrowserSink(Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>);

impl BrowserSink {
    /// Points the sink at a new socket, returning the receiver its writer task
    /// should drain.
    ///
    /// Any socket already attached is dropped, which closes its receiver and
    /// ends its writer. That is take-over, and it is deliberate: a browser that
    /// reloads can open the new socket before the old one's close has been
    /// processed, so refusing the second attachment would make an ordinary
    /// refresh fail.
    pub fn attach(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.slot() = Some(tx);
        rx
    }

    /// Forgets the current socket. Frames sent from now on go nowhere.
    #[allow(
        dead_code,
        reason = "used once a connection can outlive the socket it started on"
    )]
    pub fn detach(&self) {
        *self.slot() = None;
    }

    /// Whether a live socket is attached.
    ///
    /// A writer task that has ended has dropped its receiver, so the sender is
    /// closed even though the slot still holds it. Checking that here means a
    /// socket that went away is never reported as attached, without needing the
    /// code that noticed to reach back into the sink.
    #[allow(
        dead_code,
        reason = "used once a connection can outlive the socket it started on"
    )]
    pub fn is_attached(&self) -> bool {
        self.slot()
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }

    /// Sends one line to the attached socket, if there is one.
    ///
    /// A frame sent while detached is **dropped**, not queued. What the browser
    /// misses it gets back from `_mjx/session/replay`, which is a truthful
    /// snapshot; a queue would instead replay the whole detached period after
    /// the replay had already rebuilt the thread, duplicating it.
    fn send(&self, line: String) {
        let mut slot = self.slot();
        let Some(sender) = slot.as_ref() else {
            return;
        };
        if sender.send(line).is_err() {
            // The writer task is gone, so the socket is too. Clear the slot
            // rather than keep reporting ourselves attached to a dead socket.
            *slot = None;
        }
    }

    fn slot(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<tokio::sync::mpsc::UnboundedSender<String>>> {
        // A panic in one connection's send must not poison every other
        // connection's outbox: the value behind the lock is a plain `Option`
        // and cannot be left half-updated.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The two sinks a relay writes to.
#[derive(Clone)]
pub struct Outbox {
    to_browser: BrowserSink,
    to_agent: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Outbox {
    /// Sends a frame to the browser.
    pub fn to_browser(&self, frame: &Frame) {
        self.to_browser.send(frame.to_line());
    }

    /// Sends a frame to the agent. Used by an interceptor to answer a request
    /// the browser never saw.
    #[allow(dead_code, reason = "used by the fs/terminal interceptor")]
    pub fn to_agent(&self, frame: &Frame) {
        let _ = self.to_agent.send(frame.to_line());
    }

    /// An outbox whose agent-bound half can be inspected, for tests that
    /// exercise an interceptor without a live connection.
    #[cfg(test)]
    pub fn for_test() -> (Self, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (to_agent, to_agent_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                to_browser: BrowserSink::default(),
                to_agent,
            },
            to_agent_rx,
        )
    }

    /// Sends an already-serialized line towards one side.
    ///
    /// The relay forwards frames verbatim, including ones it could not parse,
    /// so this takes a line rather than a [`Frame`]. It exists so nothing
    /// reaches past [`BrowserSink`] into a sender that a reattach would replace.
    fn send_line(&self, direction: Direction, line: String) {
        match direction {
            Direction::ClientToAgent => {
                let _ = self.to_agent.send(line);
            }
            Direction::AgentToClient => self.to_browser.send(line),
        }
    }

    /// Sends an `_mjx/*` notification to the browser.
    pub fn notify_browser(&self, method: &str, params: &impl serde::Serialize) {
        match Frame::notification(method, params) {
            Ok(frame) => self.to_browser(&frame),
            Err(err) => tracing::error!(%err, method, "could not build a notification"),
        }
    }
}

/// Everything one connection needs.
pub struct Relay<I: Interceptor> {
    interceptor: Arc<I>,
    correlator: tokio::sync::Mutex<MethodCorrelator>,
    /// Browser request ids, mapped onto one id space the agent sees. See
    /// [`IdBridge`] for why an agent that outlives a socket needs one.
    ids: tokio::sync::Mutex<IdBridge>,
    outbox: Outbox,
    /// Held until the `initialize` handshake completes.
    /// See [`Relay::announce_after_handshake`].
    agent_info: tokio::sync::Mutex<Option<ext::AgentInfo>>,
    /// Thread state, so a browser that reloads can be given the conversation
    /// back instead of an empty page.
    sessions: tokio::sync::Mutex<SessionStore>,
}

/// Why an attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detached {
    /// The browser's socket closed. The agent is untouched and can be
    /// reattached to.
    BrowserLeft,
    /// The agent closed its stdout. Nothing can be resumed after this.
    AgentGone,
}

/// One agent and everything about it that outlives a browser socket.
///
/// The split is the whole point: closing a tab is not quitting an editor, so
/// the subprocess, the thread state and the workspace's running terminals stay
/// here while the socket comes and goes. Only the two tasks that touch the
/// socket itself are rebuilt per [`Connection::attach`].
pub struct Connection<I: Interceptor> {
    relay: Arc<Relay<I>>,
    /// The tasks that talk to the subprocess: the stdin writer, the stderr
    /// relay and the stdout reader. They keep running with nobody attached,
    /// which is what lets a turn continue across a reload.
    agent_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    handle: tokio::sync::Mutex<Option<AgentHandle>>,
    /// False once the agent's stdout closes or we shut it down. A `watch`
    /// rather than a `Notify` because `wait_for` also reports a value that was
    /// already set, so an agent that dies before anyone attaches is still seen.
    agent_alive: Arc<tokio::sync::watch::Sender<bool>>,
}

/// Starts an agent and everything that outlives a browser socket.
///
/// Nothing is attached yet: frames the agent produces before the first
/// [`Connection::attach`] are dropped, and recovered from the thread the
/// [`SessionStore`] folds.
pub fn start<I: Interceptor>(
    interceptor: Arc<I>,
    agent: AgentProcess,
    agent_info: ext::AgentInfo,
) -> Arc<Connection<I>> {
    let AgentProcess {
        handle,
        stdin: mut agent_stdin,
        stdout: mut agent_stdout,
        stderr: mut agent_stderr,
    } = agent;

    let (to_agent, mut to_agent_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let agent_alive = Arc::new(tokio::sync::watch::channel(true).0);

    let relay = Arc::new(Relay {
        interceptor,
        correlator: tokio::sync::Mutex::new(MethodCorrelator::new()),
        ids: tokio::sync::Mutex::new(IdBridge::new()),
        outbox: Outbox {
            to_browser: BrowserSink::default(),
            to_agent,
        },
        agent_info: tokio::sync::Mutex::new(Some(agent_info)),
        sessions: tokio::sync::Mutex::new(SessionStore::new()),
    });

    relay.interceptor.start(&relay.outbox);

    // Single writer per destination: handlers run concurrently, and two of them
    // interleaving halves of a frame onto one stream would corrupt both.
    let agent_writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(mut line) = to_agent_rx.recv().await {
            line.push('\n');
            if agent_stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = agent_stdin.flush().await;
        }
    });

    // Agent diagnostics, surfaced so a crashing agent is visible in the UI
    // rather than a connection that mysteriously goes quiet.
    let stderr_relay = {
        let relay = relay.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = agent_stderr.next_line().await {
                tracing::debug!(target: "agent_stderr", "{line}");
                relay
                    .outbox
                    .notify_browser(ext::AGENT_STDERR, &ext::AgentStderr { line });
            }
        })
    };

    let agent_to_browser = {
        let relay = relay.clone();
        let agent_alive = agent_alive.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = agent_stdout.next_line().await {
                relay.handle(Direction::AgentToClient, line).await;
            }
            tracing::debug!("agent closed stdout");
            let _ = agent_alive.send(false);
        })
    };

    Arc::new(Connection {
        relay,
        agent_tasks: std::sync::Mutex::new(vec![agent_writer, stderr_relay, agent_to_browser]),
        handle: tokio::sync::Mutex::new(Some(handle)),
        agent_alive,
    })
}

impl<I: Interceptor> Connection<I> {
    /// Serves one browser socket, returning when it — or the agent — goes away.
    ///
    /// `browser_rx` yields text frames from the WebSocket; `browser_tx` accepts
    /// them. Keeping this generic over the socket rather than taking an
    /// `axum::extract::ws::WebSocket` is what lets the tests drive it over an
    /// in-memory channel.
    pub async fn attach<Rx, Tx>(&self, mut browser_rx: Rx, mut browser_tx: Tx) -> Detached
    where
        Rx: futures::Stream<Item = String> + Unpin + Send + 'static,
        Tx: futures::Sink<String> + Unpin + Send + 'static,
    {
        // Whatever the previous browser left in flight is owed to a browser
        // that is no longer here, and the one arriving now numbers its own
        // requests from 1.
        self.relay.ids.lock().await.reattach();

        let mut to_browser_rx = self.relay.outbox.to_browser.attach();

        let browser_writer = tokio::spawn(async move {
            while let Some(line) = to_browser_rx.recv().await {
                if browser_tx.send(line).await.is_err() {
                    break;
                }
            }
        });

        let mut reader = {
            let relay = self.relay.clone();
            tokio::spawn(async move {
                while let Some(line) = browser_rx.next().await {
                    relay.handle(Direction::ClientToAgent, line).await;
                }
                tracing::debug!("browser disconnected");
            })
        };

        // A dead agent still ends the socket: a browser with nothing to talk to
        // is better told than left waiting. The converse is no longer true — an
        // agent with no browser now waits for the next one.
        let mut alive = self.agent_alive.subscribe();
        let detached = tokio::select! {
            _ = &mut reader => Detached::BrowserLeft,
            _ = alive.wait_for(|alive| !*alive) => Detached::AgentGone,
        };

        reader.abort();
        // Dropping the writer's receiver is also what marks the sink detached,
        // so a socket that has gone is never reported as still attached.
        browser_writer.abort();
        detached
    }

    /// Ends the agent and everything running on its behalf.
    pub async fn shutdown(&self) {
        let _ = self.agent_alive.send(false);

        // Resolved before the macro: an `.await` inside `tracing!` holds a
        // non-Send temporary across it, which makes the whole future non-Send.
        let session_count = self.relay.sessions.lock().await.len();
        tracing::debug!(sessions = session_count, "connection ending");
        self.relay.interceptor.stop();

        // Aborting the stdin writer drops the agent's stdin, which is the
        // signal most agents shut down on. It has to happen before we wait for
        // the exit.
        for task in self
            .agent_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
        {
            task.abort();
        }

        if let Some(handle) = self.handle.lock().await.take() {
            handle.shutdown().await;
        }
    }
}

/// Runs one connection until either side hangs up.
pub async fn run<I, Rx, Tx>(
    interceptor: Arc<I>,
    agent: AgentProcess,
    browser_rx: Rx,
    browser_tx: Tx,
    agent_info: ext::AgentInfo,
) where
    I: Interceptor,
    Rx: futures::Stream<Item = String> + Unpin + Send + 'static,
    Tx: futures::Sink<String> + Unpin + Send + 'static,
{
    let connection = start(interceptor, agent, agent_info);
    let detached = connection.attach(browser_rx, browser_tx).await;
    tracing::debug!(?detached, "connection ending");
    connection.shutdown().await;
}

impl<I: Interceptor> Relay<I> {
    /// Routes one line in one direction.
    async fn handle(&self, direction: Direction, line: String) {
        let frame = match Frame::parse(&line) {
            Ok(frame) => frame,
            Err(err) => {
                // Unparseable, but still forwarded: the peer that sent it may
                // be speaking a JSON-RPC shape we don't model, such as a batch,
                // and dropping it would break a connection that would
                // otherwise work.
                tracing::warn!(%err, ?direction, "forwarding an unrecognised frame");
                self.send(direction, line);
                return;
            }
        };

        // `_mjx/*` is between the browser and this server; the agent has never
        // heard of it and must not be sent it. Checked before anything else
        // observes the frame, because an extension request is answered with the
        // browser's own id and so must not be rebound below.
        if direction == Direction::ClientToAgent && frame.method().is_some_and(method::is_extension)
        {
            self.handle_extension(&frame).await;
            return;
        }

        // Rebind before the correlator and the session store see it, so both
        // are keyed on the ids the agent will actually answer with. Everything
        // from here on lives in the agent's id space; the browser's own ids
        // reappear only in `forward`.
        let frame = match (direction, &frame) {
            (Direction::ClientToAgent, Frame::Request { id, method, params }) => Frame::Request {
                id: self.ids.lock().await.for_agent(id.clone()),
                method: method.clone(),
                params: params.clone(),
            },
            _ => frame,
        };

        let method = self.correlator.lock().await.observe(direction, &frame);
        let label = frame
            .method()
            .map(str::to_owned)
            .or_else(|| method.map(|m| m.to_string()));

        {
            let mut sessions = self.sessions.lock().await;
            match direction {
                Direction::ClientToAgent => sessions.observe_from_client(&frame),
                Direction::AgentToClient => sessions.observe_from_agent(&frame),
            }
        }

        let disposition = match direction {
            Direction::ClientToAgent => self.interceptor.on_client_frame(&frame, &self.outbox),
            Direction::AgentToClient => self.interceptor.on_agent_frame(&frame, &self.outbox),
        };

        match disposition {
            Disposition::Forward => {
                self.forward(direction, &frame).await;
                self.announce_after_handshake(direction, &frame, label.as_deref())
                    .await;
            }
            Disposition::Rewrite(rewritten) => self.forward(direction, &rewritten).await,
            Disposition::Intercept => {
                // The browser never sees this frame, so mirror it to the
                // inspector; otherwise a tool whose job is showing the protocol
                // would have a blind spot exactly where the server is busiest.
                self.outbox.notify_browser(
                    ext::INSPECTOR_FRAME,
                    &ext::InspectorFrame {
                        direction: direction_name(direction).into(),
                        line: frame.to_line(),
                        method: label,
                        intercepted: true,
                    },
                );
            }
        }
    }

    /// Answers an `_mjx/*` request from the browser.
    async fn handle_extension(&self, frame: &Frame) {
        let Frame::Request { id, method, .. } = frame else {
            // Notifications in this namespace are ours to send, not to receive.
            return;
        };

        let reply = match method.as_str() {
            ext::SESSION_REPLAY => match frame.params_as::<ext::SessionReplayRequest>() {
                Ok(Some(request)) => {
                    let sessions = self.sessions.lock().await;
                    match sessions.thread(&request.session_id) {
                        Some(thread) => Frame::result(id.clone(), thread),
                        // Not an error: a browser may ask before the session
                        // exists, and an empty thread is the honest answer.
                        None => Frame::result(id.clone(), &serde_json::json!(null)),
                    }
                    .unwrap_or_else(|err| Frame::error(id.clone(), JsonRpcError::internal(err)))
                }
                Ok(None) => {
                    Frame::error(id.clone(), JsonRpcError::invalid_params("missing params"))
                }
                Err(err) => Frame::error(id.clone(), JsonRpcError::invalid_params(err)),
            },
            other => Frame::error(id.clone(), JsonRpcError::method_not_found(other)),
        };

        self.outbox.to_browser(&reply);
    }

    /// Sends `_mjx/agent/info` once the `initialize` handshake has completed.
    ///
    /// It cannot go out any earlier. ACP puts `initialize` first for a reason —
    /// nothing about the connection is agreed until it returns — and a
    /// conformant client discards frames that arrive before its response. The
    /// browser would silently never learn which agent it got.
    async fn announce_after_handshake(
        &self,
        direction: Direction,
        frame: &Frame,
        method: Option<&str>,
    ) {
        if direction != Direction::AgentToClient
            || !matches!(frame, Frame::Response { .. })
            || method != Some(method::agent::INITIALIZE)
        {
            return;
        }
        if let Some(info) = self.agent_info.lock().await.take() {
            self.outbox.notify_browser(ext::AGENT_INFO, &info);
        }
    }

    /// Passes a frame on, putting a response back into the browser's id space.
    ///
    /// The agent answers the ids the relay minted; the browser is waiting on
    /// its own. Translating here rather than at every call site means the one
    /// place that can drop a frame is the one place that knows why.
    async fn forward(&self, direction: Direction, frame: &Frame) {
        if direction == Direction::AgentToClient
            && let Frame::Response { id, payload } = frame
        {
            let Some(id) = self.ids.lock().await.for_browser(id) else {
                // The browser that asked has gone. Delivering this to the one
                // now attached would answer a question it never asked, and
                // could be read as the answer to a question it did.
                tracing::debug!(%id, "dropped a response owed to a browser that left");
                return;
            };
            let frame = Frame::Response {
                id,
                payload: payload.clone(),
            };
            self.send(direction, frame.to_line());
            return;
        }
        self.send(direction, frame.to_line());
    }

    fn send(&self, direction: Direction, line: String) {
        self.outbox.send_line(direction, line);
    }
}

/// The wire spelling of a direction, matching the TypeScript union.
fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::ClientToAgent => "clientToAgent",
        Direction::AgentToClient => "agentToClient",
    }
}

/// Builds the capability-merging rewrite for `initialize`.
///
/// Only meaningful once the server actually implements the capabilities, so it
/// is called by the fs/terminal interceptor rather than by `PassThrough`.
///
/// The browser declares what *it* can do; the server adds what it does on the
/// browser's behalf. Merging rather than replacing means a browser that opts
/// out of a capability stays opted out.
#[allow(dead_code, reason = "used by the fs/terminal interceptor")]
pub fn merge_client_capabilities(frame: &Frame, fs: bool, terminal: bool) -> Option<Frame> {
    let Frame::Request { id, method, params } = frame else {
        return None;
    };
    if method != method::agent::INITIALIZE {
        return None;
    }

    let mut value: serde_json::Value = params
        .as_deref()
        .and_then(|p| serde_json::from_str(p.get()).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let capabilities = value
        .as_object_mut()?
        .entry("clientCapabilities")
        .or_insert_with(|| serde_json::json!({}));

    if fs {
        let entry = capabilities.as_object_mut()?.entry("fs");
        let fs_caps = entry.or_insert_with(|| serde_json::json!({}));
        if let Some(fs_caps) = fs_caps.as_object_mut() {
            fs_caps.insert("readTextFile".into(), true.into());
            fs_caps.insert("writeTextFile".into(), true.into());
        }
    }
    if terminal {
        capabilities
            .as_object_mut()?
            .insert("terminal".into(), true.into());
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
    use serde_json::json;

    fn initialize(params: serde_json::Value) -> Frame {
        Frame::Request {
            id: mjx_acp_core::RequestId::Number(1),
            method: method::agent::INITIALIZE.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        }
    }

    fn capabilities_of(frame: &Frame) -> serde_json::Value {
        let params: serde_json::Value =
            serde_json::from_str(frame.params().unwrap().get()).unwrap();
        params["clientCapabilities"].clone()
    }

    #[test]
    fn initialize_gains_the_capabilities_the_server_provides() {
        let merged = merge_client_capabilities(
            &initialize(json!({ "protocolVersion": 1, "clientCapabilities": {} })),
            true,
            true,
        )
        .unwrap();

        let caps = capabilities_of(&merged);
        assert_eq!(caps["fs"]["readTextFile"], true);
        assert_eq!(caps["fs"]["writeTextFile"], true);
        assert_eq!(caps["terminal"], true);
    }

    #[test]
    fn the_rest_of_the_request_is_untouched() {
        let merged = merge_client_capabilities(
            &initialize(json!({
                "protocolVersion": 1,
                "clientInfo": { "name": "mjx-acp-viewer", "version": "0.1.0" },
                "clientCapabilities": { "elicitation": { "form": true } }
            })),
            true,
            true,
        )
        .unwrap();

        let params: serde_json::Value =
            serde_json::from_str(merged.params().unwrap().get()).unwrap();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientInfo"]["name"], "mjx-acp-viewer");
        // The browser's own capabilities survive the merge.
        assert_eq!(params["clientCapabilities"]["elicitation"]["form"], true);
    }

    #[test]
    fn a_missing_capabilities_object_is_created() {
        let merged =
            merge_client_capabilities(&initialize(json!({ "protocolVersion": 1 })), true, true)
                .unwrap();
        assert_eq!(capabilities_of(&merged)["terminal"], true);
    }

    #[test]
    fn capabilities_the_server_does_not_provide_are_not_claimed() {
        // Claiming a capability we can't honour would make the agent call a
        // method that then fails, which is worse than not offering it.
        let merged = merge_client_capabilities(
            &initialize(json!({ "protocolVersion": 1, "clientCapabilities": {} })),
            false,
            false,
        )
        .unwrap();
        let caps = capabilities_of(&merged);
        assert!(caps.get("terminal").is_none());
        assert!(caps.get("fs").is_none());
    }

    #[test]
    fn only_initialize_is_rewritten() {
        let prompt = Frame::Request {
            id: mjx_acp_core::RequestId::Number(2),
            method: method::agent::SESSION_PROMPT.into(),
            params: None,
        };
        assert!(merge_client_capabilities(&prompt, true, true).is_none());

        let note = Frame::notification(method::agent::SESSION_CANCEL, &json!({})).unwrap();
        assert!(merge_client_capabilities(&note, true, true).is_none());
    }

    #[test]
    fn direction_names_match_the_typescript_union() {
        assert_eq!(direction_name(Direction::ClientToAgent), "clientToAgent");
        assert_eq!(direction_name(Direction::AgentToClient), "agentToClient");
    }

    fn stderr_line(text: &str) -> Frame {
        Frame::notification(ext::AGENT_STDERR, &ext::AgentStderr { line: text.into() }).unwrap()
    }

    #[tokio::test]
    async fn a_frame_sent_while_nobody_is_attached_is_dropped() {
        // Not queued. An agent left running while a browser is away can produce
        // megabytes of terminal output, and flushing it on reattach would
        // arrive *after* the replay that already rebuilt the thread — out of
        // order, and duplicating what the replay just said.
        let sink = BrowserSink::default();
        assert!(!sink.is_attached());
        sink.send(stderr_line("into the void").to_line());

        let mut browser = sink.attach();
        assert!(sink.is_attached());
        sink.send(stderr_line("after attaching").to_line());

        let line = browser
            .recv()
            .await
            .expect("the attached socket gets frames");
        assert!(line.contains("after attaching"), "{line}");
        assert!(
            browser.try_recv().is_err(),
            "the frame sent while detached was queued rather than dropped"
        );
    }

    #[tokio::test]
    async fn attaching_again_replaces_the_socket_that_was_there() {
        // This is take-over: the second tab wins, and the first one's writer
        // ends because its receiver is dropped.
        let sink = BrowserSink::default();
        let mut first = sink.attach();
        let mut second = sink.attach();

        sink.send(stderr_line("hello").to_line());

        assert!(
            second.recv().await.is_some(),
            "the newest socket should receive"
        );
        assert!(
            first.recv().await.is_none(),
            "the replaced socket's receiver should be closed, ending its writer"
        );
    }

    #[tokio::test]
    async fn detaching_stops_delivery_without_ending_the_outbox() {
        let sink = BrowserSink::default();
        let mut browser = sink.attach();
        sink.detach();
        assert!(!sink.is_attached());

        // Sending after a detach must not panic and must not deliver.
        sink.send(stderr_line("nobody home").to_line());
        assert!(browser.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_socket_that_went_away_is_forgotten() {
        // A connection reports whether a browser is watching it. The writer task
        // drops its receiver when the socket goes, and that alone has to be
        // enough — nothing calls `detach` on the abort path, because by then a
        // replacement socket may already have attached and clearing the slot
        // would disconnect the tab that just took over.
        let sink = BrowserSink::default();
        let browser = sink.attach();
        drop(browser);

        assert!(!sink.is_attached(), "a closed sender is not an attachment");
        sink.send(stderr_line("to a closed socket").to_line());
        assert!(!sink.is_attached(), "a closed sender should clear the slot");
    }
}
