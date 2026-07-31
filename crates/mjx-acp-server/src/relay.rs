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
use std::sync::atomic::{AtomicBool, Ordering};

use futures::{SinkExt, StreamExt};
use mjx_acp_core::{
    Direction, Frame, JsonRpcError, MethodCorrelator, ResponsePayload, ext, method,
};

use crate::agent_process::{AgentHandle, AgentProcess};
use crate::config::McpServerConfig;
use crate::id_bridge::IdBridge;
use crate::mcp::{self, Transports};
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
pub struct BrowserSink(Arc<std::sync::Mutex<Attached>>);

/// Whichever socket the sink points at, and which attachment that is.
#[derive(Default)]
struct Attached {
    /// Bumped on every attach. A socket on its way out compares this against
    /// the number it was given, so it can tell "nobody is here now" from "I
    /// have been replaced" — and never clears a sink pointing at its successor.
    generation: u64,
    sender: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl BrowserSink {
    /// Points the sink at a new socket, returning that attachment's number and
    /// the receiver its writer task should drain.
    ///
    /// The socket already attached, if any, is dropped. Its writer keeps
    /// running until it has drained what is already queued, so a notice written
    /// immediately before this call still reaches the browser being displaced.
    pub fn attach(&self) -> (u64, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut slot = self.slot();
        slot.generation += 1;
        slot.sender = Some(tx);
        (slot.generation, rx)
    }

    /// Forgets the socket attached as `generation`, if it is still the one
    /// attached. Frames sent from then on go nowhere.
    ///
    /// A socket that has already been taken over must not clear the sink on its
    /// way out: that would disconnect the tab which just replaced it.
    pub fn detach(&self, generation: u64) {
        let mut slot = self.slot();
        if slot.generation == generation {
            slot.sender = None;
        }
    }

    /// Whether `generation` is still the attachment the sink points at.
    pub fn is_current(&self, generation: u64) -> bool {
        self.slot().generation == generation
    }

    /// Whether a live socket is attached.
    ///
    /// A writer task that has ended has dropped its receiver, so the sender is
    /// closed even though the slot still holds it. Checking that here means a
    /// socket that went away is never reported as attached, without needing the
    /// code that noticed to reach back into the sink.
    pub fn is_attached(&self) -> bool {
        self.slot()
            .sender
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
        let Some(sender) = slot.sender.as_ref() else {
            return;
        };
        if sender.send(line).is_err() {
            // The writer task is gone, so the socket is too. Clear the slot
            // rather than keep reporting ourselves attached to a dead socket.
            slot.sender = None;
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Attached> {
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

    /// As [`Outbox::for_test`], with a socket already attached so what the
    /// browser would receive can be read too.
    #[cfg(test)]
    pub fn for_test_with_browser() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (outbox, to_agent_rx) = Self::for_test();
        let (_generation, to_browser_rx) = outbox.to_browser.attach();
        (outbox, to_agent_rx, to_browser_rx)
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
    /// Announced to each browser once its handshake is answered.
    /// See [`Relay::announce_after_handshake`].
    agent_info: ext::AgentInfo,
    /// MCP servers from `mjx.toml`, added to every session the browser opens.
    /// Configuration rather than a capability, which is why it lives here beside
    /// `agent_info` and not in the interceptor.
    mcp_servers: Vec<McpServerConfig>,
    /// The agent's answers to the once-per-agent handshake.
    handshake: tokio::sync::Mutex<Handshake>,
    /// Questions the agent has put to the browser and is still waiting on.
    ///
    /// A browser that leaves mid-turn takes the answer with it, and
    /// `session/request_permission` parks the turn until *someone* answers.
    /// Asking the next browser is what makes a reload resume a turn that is
    /// still running rather than one that has quietly stalled.
    ///
    /// A pending elicitation is here as well as in the thread: the thread is
    /// what carries its *history* across a reload, and this is what makes it
    /// answerable, since a browser cannot respond to a request the connection it
    /// is holding never received.
    unanswered: tokio::sync::Mutex<Vec<Frame>>,
    /// Whether the browser now attached has already been put the list above.
    ///
    /// The re-ask is triggered by `_mjx/session/replay`, and a browser sends one
    /// of those per conversation it is restoring — so without this a browser
    /// with three open sessions would be asked the same question three times,
    /// each carrying the agent's one id for it. A client that keys its pending
    /// requests by id, as it must to answer them, would keep only the last
    /// handler and orphan the rest.
    ///
    /// Per attachment rather than per relay: the *next* browser has heard none
    /// of it and still has to be asked.
    reasked: AtomicBool,
    /// Thread state, so a browser that reloads can be given the conversation
    /// back instead of an empty page.
    sessions: tokio::sync::Mutex<SessionStore>,
}

/// The agent's answers to the handshake, kept so a browser that reattaches can
/// be given the same ones without the agent seeing either question twice.
///
/// This is the relay's third interception, and the reason is that both of these
/// are once-per-*agent* rather than once-per-socket. A second `initialize` on a
/// live agent is at best redundant and at worst resets it; a second
/// `session/new` is precisely the bug — a reload starting a fresh conversation
/// beside the one still running.
#[derive(Default)]
struct Handshake {
    initialize: Option<Box<serde_json::value::RawValue>>,
    new_session: Option<Box<serde_json::value::RawValue>>,
    /// Which MCP transports the agent declared, read out of `initialize` once.
    /// Kept here because this is where the answer is, and because a browser that
    /// reattached never asks `initialize` again — so nothing else would know.
    transports: Option<Transports>,
    /// Whether the browser now attached should have its first `session/new`
    /// answered from the recording. Cleared once it has been, so a client that
    /// deliberately opens a second session still gets a real one.
    replay_new_session: bool,
}

impl Handshake {
    /// Starts a new attachment. The browser arriving is about to ask both
    /// questions again, and gets the recorded answers if there are any.
    fn reattach(&mut self) {
        self.replay_new_session = self.new_session.is_some();
    }

    /// Remembers what the agent answered, keyed by the method it answers.
    fn record(&mut self, method: &str, result: &serde_json::value::RawValue) {
        let slot = match method {
            method::agent::INITIALIZE => &mut self.initialize,
            method::agent::SESSION_NEW => &mut self.new_session,
            _ => return,
        };
        // The first answer is the one that holds. A later `session/new` is a
        // second session, not a correction of the first.
        slot.get_or_insert_with(|| result.to_owned());
    }

    /// The MCP transports the agent declared, parsed once.
    ///
    /// Empty until `initialize` has been answered — a client that pipelined a
    /// `session/new` behind it gets stdio servers only, which every agent
    /// supports, rather than a guess.
    fn transports(&mut self) -> Transports {
        if let Some(transports) = self.transports {
            return transports;
        }
        let Some(initialize) = &self.initialize else {
            return Transports::default();
        };
        *self
            .transports
            .insert(Transports::from_initialize_result(initialize))
    }

    /// The recorded answer to `method`, if this attachment should be given it.
    fn recorded(&mut self, method: &str) -> Option<Box<serde_json::value::RawValue>> {
        match method {
            method::agent::INITIALIZE => self.initialize.clone(),
            method::agent::SESSION_NEW if self.replay_new_session => {
                self.replay_new_session = false;
                self.new_session.clone()
            }
            _ => None,
        }
    }
}

/// Why an attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detached {
    /// The browser's socket closed. The agent is untouched and can be
    /// reattached to.
    BrowserLeft,
    /// Another socket attached in this one's place.
    TakenOver,
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
    /// The reader of the socket currently attached, so a socket taking over can
    /// end the one it displaces. Stored with its attachment number so a socket
    /// leaving normally does not clear its successor's entry.
    reader: std::sync::Mutex<Option<(u64, tokio::task::AbortHandle)>>,
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
    mcp_servers: Vec<McpServerConfig>,
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
        agent_info,
        mcp_servers,
        handshake: tokio::sync::Mutex::new(Handshake::default()),
        unanswered: tokio::sync::Mutex::new(Vec::new()),
        reasked: AtomicBool::new(false),
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
        reader: std::sync::Mutex::new(None),
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
        // requests from one.
        self.relay.ids.lock().await.reattach();
        self.relay.handshake.lock().await.reattach();
        // The browser arriving has heard none of the outstanding questions,
        // however many times the one leaving was asked them.
        self.relay.reasked.store(false, Ordering::SeqCst);

        // Take over from whoever is here. The second tab always wins: a browser
        // that reloads can open its new socket before the old one's close has
        // been processed, and refusing the second attachment would make an
        // ordinary refresh fail — the exact thing this is all for.
        let evicted = self.take_reader();
        if evicted.is_some() {
            self.relay
                .outbox
                .notify_browser(ext::CONNECTION_TAKEN_OVER, &serde_json::json!({}));
        }

        // The sink is swapped before the displaced reader is ended, so that
        // reader wakes up already knowing it has been replaced. Its writer
        // keeps draining until the queue is empty, which is what gets the
        // notice above onto the wire.
        let (generation, mut to_browser_rx) = self.relay.outbox.to_browser.attach();
        if let Some(evicted) = evicted {
            evicted.abort();
        }

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
        *self.reader_slot() = Some((generation, reader.abort_handle()));

        // A dead agent still ends the socket: a browser with nothing to talk to
        // is better told than left waiting. The converse is no longer true — an
        // agent with no browser now waits for the next one.
        let mut alive = self.agent_alive.subscribe();
        let detached = tokio::select! {
            _ = &mut reader => {
                if self.relay.outbox.to_browser.is_current(generation) {
                    Detached::BrowserLeft
                } else {
                    Detached::TakenOver
                }
            }
            _ = alive.wait_for(|alive| !*alive) => Detached::AgentGone,
        };

        reader.abort();
        // Both of these are no-ops once another socket has taken over, which is
        // the point: its arrival is what ends our writer, and ending that writer
        // ourselves would drop the notice telling this browser why.
        self.relay.outbox.to_browser.detach(generation);
        if detached != Detached::TakenOver {
            browser_writer.abort();
        }
        // One lock, not two: `reader_slot()` is a plain mutex, so testing the
        // slot in an `if let` and clearing it inside would wait on ourselves.
        let mut slot = self.reader_slot();
        if slot
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            *slot = None;
        }
        drop(slot);

        detached
    }

    /// Whether a browser is watching this connection right now.
    pub fn is_attached(&self) -> bool {
        self.relay.outbox.to_browser.is_attached()
    }

    /// Whether the agent has gone. Nothing can be resumed from here.
    pub fn is_finished(&self) -> bool {
        !*self.agent_alive.borrow()
    }

    /// The agent's process id, while it is running.
    pub async fn agent_pid(&self) -> Option<u32> {
        self.handle.lock().await.as_ref().and_then(AgentHandle::pid)
    }

    fn take_reader(&self) -> Option<tokio::task::AbortHandle> {
        self.reader_slot().take().map(|(_, handle)| handle)
    }

    fn reader_slot(&self) -> std::sync::MutexGuard<'_, Option<(u64, tokio::task::AbortHandle)>> {
        self.reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

        // A browser that reattached asks the handshake again, because it is an
        // ordinary ACP client and that is what an ACP client does on a new
        // connection. Answering it here is what makes resuming transparent: the
        // agent never sees either question twice, and no client needs to know
        // it is talking to an agent that was already running.
        if direction == Direction::ClientToAgent && self.answer_from_recording(&frame).await {
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

        // Add the configured MCP servers before anything observes the frame, so
        // the store, the inspector and the interceptor all see the bytes the
        // agent will actually receive. A `session/new` answered from the
        // recording never arrives here, which is right: the agent that is still
        // running was given these servers when the first browser opened it.
        let frame = if direction == Direction::ClientToAgent && !self.mcp_servers.is_empty() {
            let transports = self.handshake.lock().await.transports();
            mcp::merge_mcp_servers(&frame, &self.mcp_servers, &transports).unwrap_or(frame)
        } else {
            frame
        };

        let method = self.correlator.lock().await.observe(direction, &frame);
        let label = frame
            .method()
            .map(str::to_owned)
            .or_else(|| method.map(|m| m.to_string()));

        // Which turn this response ends, read before the store forgets it. Only
        // needed if the response turns out to be undeliverable, but by then the
        // record is gone.
        let ends_turn = if direction == Direction::AgentToClient
            && matches!(frame, Frame::Response { .. })
            && label.as_deref() == Some(method::agent::SESSION_PROMPT)
        {
            let sessions = self.sessions.lock().await;
            frame
                .id()
                .and_then(|id| sessions.session_of_prompt(id))
                .map(str::to_owned)
        } else {
            None
        };

        {
            let mut sessions = self.sessions.lock().await;
            match direction {
                Direction::ClientToAgent => sessions.observe_from_client(&frame),
                Direction::AgentToClient => sessions.observe_from_agent(&frame),
            }
        }

        // A turn that has ended will never answer what it asked. Left on the
        // list, those questions would be put to every browser that ever
        // attaches; a permission prompt is hidden by its tool call settling, but
        // an elicitation need not have a tool call, so nothing would hide it.
        if let Some(session_id) = &ends_turn {
            self.forget_questions(session_id).await;
        }

        // The browser has answered something the agent asked, so stop offering
        // it to the next browser.
        if direction == Direction::ClientToAgent
            && let Frame::Response { id, .. } = &frame
        {
            self.unanswered
                .lock()
                .await
                .retain(|asked| asked.id() != Some(id));
        }

        // Keep the agent's side of the handshake, so the next browser to attach
        // can be given it without the agent being asked again.
        if direction == Direction::AgentToClient
            && let Frame::Response {
                payload: ResponsePayload::Result(result),
                ..
            } = &frame
            && let Some(method) = label.as_deref()
        {
            self.handshake.lock().await.record(method, result);
        }

        let disposition = match direction {
            Direction::ClientToAgent => self.interceptor.on_client_frame(&frame, &self.outbox),
            Direction::AgentToClient => self.interceptor.on_agent_frame(&frame, &self.outbox),
        };

        // A rewritten frame takes the forwarded frame's place and is otherwise
        // treated identically. Giving it a path of its own is what let it
        // silently lose `ends_turn`, and would have let it lose the re-ask list
        // and the announcement next.
        let forwarded = match &disposition {
            Disposition::Forward => Some(&frame),
            Disposition::Rewrite(rewritten) => Some(rewritten),
            Disposition::Intercept => None,
        };

        match forwarded {
            Some(frame) => {
                // Only what really reaches the browser is worth re-asking. The
                // interceptor's `fs/*` and `terminal/*` traffic is answered by
                // the server and never seen there.
                //
                // A pending elicitation goes on the list too, even though the
                // replay also carries it. The thread is what makes the *history*
                // of one survive a reload; the re-ask is what makes it
                // answerable, because a browser cannot respond to a request the
                // connection it is holding never received. The browser matches
                // the two up by request id.
                if direction == Direction::AgentToClient && matches!(frame, Frame::Request { .. }) {
                    self.unanswered.lock().await.push(frame.clone());
                }
                self.forward(direction, frame, ends_turn).await;
                self.announce_after_handshake(direction, frame, label.as_deref())
                    .await;
            }
            None => {
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

        // Only after the replay is on the wire. A permission prompt that
        // arrived first would be folded into the thread, and the replay would
        // then replace that thread wholesale and wipe it.
        if method == ext::SESSION_REPLAY {
            self.reask_unanswered().await;
        }
    }

    /// Drops the questions a finished turn asked and never had answered.
    ///
    /// Two halves, because the two places a pending question lives both have to
    /// let go of it: the re-ask list, so the next browser is not asked; and the
    /// thread, so a replay does not show a live form for a turn that is over.
    async fn forget_questions(&self, session_id: &str) {
        let dropped = {
            let mut unanswered = self.unanswered.lock().await;
            let before = unanswered.len();
            unanswered.retain(|asked| session_of(asked).as_deref() != Some(session_id));
            before - unanswered.len()
        };
        let cancelled = self
            .sessions
            .lock()
            .await
            .cancel_pending_elicitations(session_id);

        if dropped > 0 || cancelled > 0 {
            tracing::debug!(
                session_id,
                dropped,
                cancelled,
                "the turn ended with questions outstanding"
            );
        }
    }

    /// Puts the agent's outstanding questions to the browser now attached.
    ///
    /// They keep the agent's own ids, so the answer correlates without the
    /// browser having to know it is answering something it never heard asked.
    async fn reask_unanswered(&self) {
        // Once per attachment, however many conversations this browser is
        // restoring. See [`Relay::reasked`].
        if self.reasked.swap(true, Ordering::SeqCst) {
            return;
        }
        for frame in self.unanswered.lock().await.iter() {
            tracing::debug!(
                method = frame.method(),
                "re-asking a departed browser's question"
            );
            self.outbox.to_browser(frame);
        }
    }

    /// Answers a handshake this agent has already been through.
    ///
    /// Returns whether it did, in which case the frame goes no further.
    /// A question with no recorded answer — a browser that reloaded during the
    /// millisecond `initialize` was in flight — falls through and is asked for
    /// real, which is the only thing that could still work.
    async fn answer_from_recording(&self, frame: &Frame) -> bool {
        let Frame::Request { id, method, .. } = frame else {
            return false;
        };
        let Some(result) = self.handshake.lock().await.recorded(method) else {
            return false;
        };

        tracing::debug!(method, "answering a repeat handshake from the recording");
        self.outbox.to_browser(&Frame::Response {
            id: id.clone(),
            payload: ResponsePayload::Result(result),
        });
        self.announce(method, true).await;
        true
    }

    /// Sends `_mjx/agent/info` once the `initialize` handshake has completed.
    ///
    /// It cannot go out any earlier. ACP puts `initialize` first for a reason —
    /// nothing about the connection is agreed until it returns — and a
    /// conformant client discards frames that arrive before its response. The
    /// browser would silently never learn which agent it got.
    ///
    /// It goes out once per *attachment*, not once per agent: every browser
    /// that attaches needs to be told what it is talking to, and the one that
    /// resumed needs to be told that it resumed.
    async fn announce_after_handshake(
        &self,
        direction: Direction,
        frame: &Frame,
        method: Option<&str>,
    ) {
        if direction != Direction::AgentToClient || !matches!(frame, Frame::Response { .. }) {
            return;
        }
        if let Some(method) = method {
            self.announce(method, false).await;
        }
    }

    /// Announces the agent, if `method` is the handshake that had to come first.
    ///
    /// The MCP list is built here rather than once at startup because whether a
    /// server was *offered* depends on what this agent declared, and because a
    /// reattaching browser is announced to from `answer_from_recording` — where
    /// no interceptor runs and nothing else would fill it in.
    async fn announce(&self, method: &str, resumed: bool) {
        if method != method::agent::INITIALIZE {
            return;
        }
        let transports = self.handshake.lock().await.transports();
        let info = ext::AgentInfo {
            resumed,
            mcp_servers: self
                .mcp_servers
                .iter()
                .map(|server| ext::McpServerInfo {
                    name: server.name.clone(),
                    transport: server.transport().to_string(),
                    target: server.target(),
                    secrets: server.secret_names(),
                    unavailable: mcp::skip_reason(server, &transports),
                })
                .collect(),
            ..self.agent_info.clone()
        };
        self.outbox.notify_browser(ext::AGENT_INFO, &info);
    }

    /// Passes a frame on, putting a response back into the browser's id space.
    ///
    /// The agent answers the ids the relay minted; the browser is waiting on
    /// its own. Translating here rather than at every call site means the one
    /// place that can drop a frame is the one place that knows why.
    async fn forward(&self, direction: Direction, frame: &Frame, ends_turn: Option<String>) {
        if direction == Direction::AgentToClient
            && let Frame::Response { id, payload } = frame
        {
            let Some(id) = self.ids.lock().await.for_browser(id) else {
                // The browser that asked has gone. Delivering this to the one
                // now attached would answer a question it never asked, and
                // could be read as the answer to a question it did.
                tracing::debug!(%id, "dropped a response owed to a browser that left");
                if let Some(session_id) = ends_turn {
                    // Except that this one carried news. A prompt response is
                    // the only thing in ACP that says a turn is over, so a
                    // browser that inherited the turn has to hear it some other
                    // way or it waits for an end that has already happened.
                    self.outbox.notify_browser(
                        ext::SESSION_TURN_ENDED,
                        &ext::SessionTurnEnded {
                            session_id,
                            stop_reason: stop_reason_of(payload),
                        },
                    );
                }
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

/// Why a turn ended, as ACP spells it.
///
/// A failed prompt still ends the turn — the same reading `SessionStore` takes
/// — so an error response reports `cancelled` rather than nothing at all.
fn stop_reason_of(payload: &ResponsePayload) -> String {
    let ResponsePayload::Result(result) = payload else {
        return "cancelled".into();
    };
    serde_json::from_str::<serde_json::Value>(result.get())
        .ok()
        .and_then(|value| value.get("stopReason")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "cancelled".into())
}

/// The session a frame's params name, if they name one.
///
/// Read from the JSON rather than from a typed request, because this has to work
/// for every method a turn can ask about — including ones we do not model.
/// A frame that names no session belongs to no turn, which is the honest answer
/// for a request-scoped elicitation.
fn session_of(frame: &Frame) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(frame.params()?.get()).ok()?;
    value.get("sessionId")?.as_str().map(str::to_owned)
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

    /// An interceptor that replaces every response the agent sends, so the
    /// [`Disposition::Rewrite`] path can be exercised without a real one.
    struct RewriteResponses;

    impl Interceptor for RewriteResponses {
        fn on_agent_frame(&self, frame: &Frame, _outbox: &Outbox) -> Disposition {
            let Frame::Response { id, .. } = frame else {
                return Disposition::Forward;
            };
            Disposition::Rewrite(Frame::error(
                id.clone(),
                JsonRpcError::internal("rewritten by the test"),
            ))
        }
    }

    /// A relay with no agent behind it, for exercising `handle` directly.
    fn relay_for_test<I: Interceptor>(
        interceptor: I,
    ) -> (
        Relay<I>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (outbox, to_agent, to_browser) = Outbox::for_test_with_browser();
        let relay = Relay {
            interceptor: Arc::new(interceptor),
            correlator: tokio::sync::Mutex::new(MethodCorrelator::new()),
            ids: tokio::sync::Mutex::new(IdBridge::new()),
            outbox,
            agent_info: ext::AgentInfo {
                agent_id: "test".into(),
                name: "Test".into(),
                command: Vec::new(),
                cwd: String::new(),
                connection_id: "c1".into(),
                resumed: false,
                mcp_servers: Vec::new(),
            },
            mcp_servers: Vec::new(),
            handshake: tokio::sync::Mutex::new(Handshake::default()),
            unanswered: tokio::sync::Mutex::new(Vec::new()),
            reasked: AtomicBool::new(false),
            sessions: tokio::sync::Mutex::new(SessionStore::new()),
        };
        (relay, to_agent, to_browser)
    }

    /// The `_mjx/*` notifications sent to the browser, in order.
    fn notifications(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> Vec<(String, String)> {
        let mut seen = Vec::new();
        while let Ok(line) = rx.try_recv() {
            if let Ok(Frame::Notification { method, .. }) = Frame::parse(&line) {
                seen.push((method, line));
            }
        }
        seen
    }

    #[tokio::test]
    async fn a_rewritten_prompt_response_still_says_the_turn_is_over() {
        // A response that is rewritten reaches the browser by the same path as
        // one that is forwarded, so it must carry the same news. Before the two
        // paths were merged the rewrite dropped `ends_turn`, and a browser that
        // inherited the turn would have shown "generating" for ever.
        let (relay, _to_agent, mut to_browser) = relay_for_test(RewriteResponses);

        let prompt = Frame::Request {
            id: mjx_acp_core::RequestId::Number(1),
            method: method::agent::SESSION_PROMPT.into(),
            params: Some(
                serde_json::value::to_raw_value(&json!({ "sessionId": "s1", "prompt": [] }))
                    .unwrap(),
            ),
        };
        relay
            .handle(Direction::ClientToAgent, prompt.to_line())
            .await;

        // The browser that asked goes away, exactly as a reload does.
        relay.ids.lock().await.reattach();

        let answer = Frame::Response {
            id: mjx_acp_core::RequestId::Number(1),
            payload: ResponsePayload::Result(
                serde_json::value::to_raw_value(&json!({ "stopReason": "end_turn" })).unwrap(),
            ),
        };
        relay
            .handle(Direction::AgentToClient, answer.to_line())
            .await;

        let ended = notifications(&mut to_browser)
            .into_iter()
            .find(|(method, _)| method == ext::SESSION_TURN_ENDED)
            .expect("a rewritten prompt response must still end the turn");
        assert!(ended.1.contains("\"sessionId\":\"s1\""));
    }

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
    fn the_agents_mcp_transports_are_read_from_the_handshake_it_recorded() {
        let mut handshake = Handshake::default();
        // Before `initialize` is answered there is nothing to read, and the
        // answer must be the conservative one rather than a guess.
        assert_eq!(handshake.transports(), Transports::default());

        let result = serde_json::value::to_raw_value(&json!({
            "protocolVersion": 1,
            "agentCapabilities": { "mcpCapabilities": { "http": true, "sse": false } },
        }))
        .unwrap();
        handshake.record(method::agent::INITIALIZE, &result);

        let transports = handshake.transports();
        assert!(transports.http);
        assert!(!transports.sse);
        // Read once and kept: a browser that reattaches never asks `initialize`
        // again, so the recording is the only copy there will ever be.
        assert_eq!(handshake.transports(), transports);
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
                "clientCapabilities": { "elicitation": { "form": {} } }
            })),
            true,
            true,
        )
        .unwrap();

        let params: serde_json::Value =
            serde_json::from_str(merged.params().unwrap().get()).unwrap();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientInfo"]["name"], "mjx-acp-viewer");
        // The browser's own capabilities survive the merge. `elicitation` is a
        // real one it declares, spelled the way the schema wants it: an object,
        // because a bare `true` reads as absent on the agent's side.
        assert!(params["clientCapabilities"]["elicitation"]["form"].is_object());
    }

    fn elicitation(params: serde_json::Value) -> Frame {
        Frame::Request {
            id: mjx_acp_core::RequestId::Number(9),
            method: method::client::ELICITATION_CREATE.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        }
    }

    #[test]
    fn a_question_is_matched_to_its_turn_by_the_session_it_names() {
        let permission = Frame::Request {
            id: mjx_acp_core::RequestId::Number(3),
            method: method::client::SESSION_REQUEST_PERMISSION.into(),
            params: Some(
                serde_json::value::to_raw_value(&json!({ "sessionId": "s1", "options": [] }))
                    .unwrap(),
            ),
        };
        assert_eq!(session_of(&permission).as_deref(), Some("s1"));

        // A request-scoped elicitation belongs to no turn, so no turn ending
        // should drop it.
        assert!(
            session_of(&elicitation(json!({
                "mode": "url",
                "requestId": 4,
                "elicitationId": "el-1",
                "url": "https://example.test/",
                "message": "authorize"
            })))
            .is_none()
        );

        let no_params = Frame::Request {
            id: mjx_acp_core::RequestId::Number(5),
            method: method::client::SESSION_REQUEST_PERMISSION.into(),
            params: None,
        };
        assert!(session_of(&no_params).is_none());
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

        let (_, mut browser) = sink.attach();
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
        let (_, mut first) = sink.attach();
        let (_, mut second) = sink.attach();

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
        let (generation, mut browser) = sink.attach();
        sink.detach(generation);
        assert!(!sink.is_attached());

        // Sending after a detach must not panic and must not deliver.
        sink.send(stderr_line("nobody home").to_line());
        assert!(browser.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_socket_that_went_away_is_forgotten() {
        // A connection reports whether a browser is watching it, and a socket
        // whose writer has ended is not one.
        let sink = BrowserSink::default();
        let (_, browser) = sink.attach();
        drop(browser);

        assert!(!sink.is_attached(), "a closed sender is not an attachment");
        sink.send(stderr_line("to a closed socket").to_line());
        assert!(!sink.is_attached(), "a closed sender should clear the slot");
    }

    #[tokio::test]
    async fn a_displaced_socket_cannot_detach_its_successor() {
        // The ordering that makes take-over safe. The first socket wakes up
        // *after* the second has attached, and calls `detach` with its own
        // number on the way out; if that cleared the sink, the tab that just
        // took over would go silent.
        let sink = BrowserSink::default();
        let (first, _first_rx) = sink.attach();
        let (second, mut second_rx) = sink.attach();

        sink.detach(first);

        assert!(sink.is_current(second));
        sink.send(stderr_line("still connected").to_line());
        assert!(
            second_rx.recv().await.is_some(),
            "the successor was cut off"
        );
    }

    fn result(json: serde_json::Value) -> Box<serde_json::value::RawValue> {
        serde_json::value::to_raw_value(&json).unwrap()
    }

    #[test]
    fn a_reattached_browser_is_given_the_handshake_the_agent_already_answered() {
        let mut handshake = Handshake::default();
        handshake.record(
            method::agent::INITIALIZE,
            &result(json!({"protocolVersion": 1})),
        );
        handshake.record(
            method::agent::SESSION_NEW,
            &result(json!({"sessionId": "s1"})),
        );

        handshake.reattach();

        assert!(handshake.recorded(method::agent::INITIALIZE).is_some());
        let session = handshake.recorded(method::agent::SESSION_NEW).unwrap();
        assert!(session.get().contains("s1"));
    }

    #[test]
    fn a_fresh_connection_has_nothing_to_answer_from() {
        let mut handshake = Handshake::default();
        handshake.reattach();
        assert!(handshake.recorded(method::agent::INITIALIZE).is_none());
        assert!(handshake.recorded(method::agent::SESSION_NEW).is_none());
    }

    #[test]
    fn only_the_first_session_new_of_an_attachment_is_replayed() {
        // A reload asks for the session it already had. A client that then
        // deliberately opens a second one must reach the agent, or resuming
        // would have quietly capped every client at one session forever.
        let mut handshake = Handshake::default();
        handshake.record(
            method::agent::SESSION_NEW,
            &result(json!({"sessionId": "s1"})),
        );
        handshake.reattach();

        assert!(handshake.recorded(method::agent::SESSION_NEW).is_some());
        assert!(
            handshake.recorded(method::agent::SESSION_NEW).is_none(),
            "the second session/new must reach the agent"
        );
    }

    #[test]
    fn the_first_answer_is_the_one_kept() {
        // `record` sees every session/new response, including ones for extra
        // sessions. The conversation to come back to is the first.
        let mut handshake = Handshake::default();
        handshake.record(
            method::agent::SESSION_NEW,
            &result(json!({"sessionId": "s1"})),
        );
        handshake.record(
            method::agent::SESSION_NEW,
            &result(json!({"sessionId": "s2"})),
        );
        handshake.reattach();

        let session = handshake.recorded(method::agent::SESSION_NEW).unwrap();
        assert!(session.get().contains("s1"), "{}", session.get());
    }

    #[test]
    fn other_methods_are_not_recorded() {
        let mut handshake = Handshake::default();
        handshake.record(
            method::agent::SESSION_PROMPT,
            &result(json!({"stopReason": "end_turn"})),
        );
        handshake.reattach();
        assert!(handshake.recorded(method::agent::SESSION_PROMPT).is_none());
    }
}
