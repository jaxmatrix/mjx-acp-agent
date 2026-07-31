//! An MCP server this process holds open on an agent's behalf.
//!
//! ACP's `unstable_mcp_over_acp` lets a client say "there is a server called
//! `private`, ask me for it" instead of handing the agent a command to run. The
//! agent then calls `mcp/connect` and sends every MCP message through
//! `mcp/message`, and the *client* owns the connection. That is what this crate
//! is: the owning end.
//!
//! Two things follow, and both are the point of the feature. The command and its
//! environment never leave this process, so a credential in `mjx.toml` is never
//! handed to the agent; and an agent that could not spawn the server itself —
//! sandboxed, or on another machine — can still use it.
//!
//! **This crate knows nothing about ACP.** MCP is JSON-RPC 2.0, so it speaks
//! [`Frame`] to the child and leaves the ACP envelope
//! (`{connectionId, method, params}`) to the caller. Keeping the translation out
//! of here is what makes the transport testable over a pipe, with no subprocess
//! and no protocol at all above it.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use mjx_acp_core::{Frame, JsonRpcError, RequestId, ResponsePayload};
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// How long the child gets to exit after its stdin closes, before it is killed.
/// The same grace period the agent subprocess gets.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// What the MCP server answered.
pub type Reply = Result<Box<RawValue>, JsonRpcError>;

/// Something the MCP server said that nobody asked it for.
///
/// The caller forwards these on to the agent, which is the MCP *client* here and
/// so is the one that has to deal with them.
#[derive(Debug)]
pub enum FromServer {
    /// A one-way message, `notifications/tools/list_changed` and the like.
    Notification {
        method: String,
        params: Option<Box<RawValue>>,
    },
    /// A call expecting an answer — sampling, or a roots listing. The `id` is
    /// the server's own and means nothing to anyone else; pass it back to
    /// [`McpServer::answer`] with whatever the agent replied.
    Request {
        id: RequestId,
        method: String,
        params: Option<Box<RawValue>>,
    },
}

/// A running MCP server, and the plumbing to talk to it.
pub struct McpServer {
    /// Display name, for logs and for the error a caller sees.
    name: String,
    /// Lines bound for the server's input.
    to_server: mpsc::UnboundedSender<String>,
    /// Requests we have sent and are waiting on, by the id we minted.
    ///
    /// A `std::sync::Mutex` and not tokio's: nothing here is held across an
    /// await, and the reader task takes it from a synchronous context.
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Reply>>>,
    next_id: AtomicI64,
    /// The subprocess, when there is one. Absent for a server driven over a
    /// pipe, which is how the tests drive it.
    child: Mutex<Option<Child>>,
}

impl McpServer {
    /// Starts `command` in `cwd` and begins talking to it.
    ///
    /// The child inherits this process's environment plus `env`, the same way an
    /// agent subprocess does, so a server that finds its own credentials in the
    /// environment keeps working.
    ///
    /// Returns the server and the stream of everything it says unprompted.
    pub fn spawn(
        name: &str,
        command: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<FromServer>)> {
        let mut process = Command::new(command);
        process
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Not the terminal's process group: a Ctrl-C aimed at the server
            // must not take an MCP server down mid-tool-call.
            .kill_on_drop(true);
        for (key, value) in env {
            process.env(key, value);
        }

        let mut child = process.spawn().with_context(|| {
            format!(
                "could not start the MCP server `{name}`: {}",
                command.display()
            )
        })?;

        let stdin = child.stdin.take().context("child stdin was not piped")?;
        let stdout = child.stdout.take().context("child stdout was not piped")?;
        let stderr = child.stderr.take().context("child stderr was not piped")?;

        // Diagnostics, so a server that fails to start says why in our log
        // rather than vanishing.
        let label = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(server = %label, "{line}");
            }
        });

        let (server, events) = Self::over_streams(name, stdout, stdin);
        *server.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        Ok((server, events))
    }

    /// The same, over any pair of streams. A subprocess is one way to get those;
    /// a test is another.
    pub fn over_streams(
        name: &str,
        from_server: impl AsyncRead + Unpin + Send + 'static,
        mut to_server: impl AsyncWrite + Unpin + Send + 'static,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<FromServer>) {
        let (outbox_tx, mut outbox) = mpsc::unbounded_channel::<String>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<FromServer>();

        let server = Arc::new(Self {
            name: name.to_string(),
            to_server: outbox_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            child: Mutex::new(None),
        });

        tokio::spawn(async move {
            while let Some(line) = outbox.recv().await {
                if to_server
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = to_server.flush().await;
            }
            // Dropping the writer closes the child's stdin, which is how a
            // well-behaved MCP server is asked to stop.
        });

        let reader = server.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(from_server).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                reader.on_line(&line, &events_tx);
            }
            // The server has gone. Nothing else will answer what is in flight,
            // and a caller left awaiting forever is worse than an error.
            reader.fail_everything_pending();
            tracing::debug!(server = %reader.name, "the MCP server closed its output");
        });

        (server, events_rx)
    }

    /// Routes one line from the server.
    fn on_line(&self, line: &str, events: &mpsc::UnboundedSender<FromServer>) {
        let frame = match Frame::parse(line) {
            Ok(frame) => frame,
            Err(err) => {
                // Dropped, not forwarded: unlike the ACP relay, there is no peer
                // downstream that might understand it better than we do.
                tracing::warn!(server = %self.name, %err, "unparseable line from an MCP server");
                return;
            }
        };

        match frame {
            Frame::Response { id, payload } => {
                let Some(waiting) = self.take_pending(&id) else {
                    tracing::warn!(
                        server = %self.name,
                        "an MCP server answered something we never asked"
                    );
                    return;
                };
                let _ = waiting.send(match payload {
                    ResponsePayload::Result(result) => Ok(result),
                    ResponsePayload::Error(error) => Err(error),
                });
            }
            Frame::Notification { method, params } => {
                let _ = events.send(FromServer::Notification { method, params });
            }
            Frame::Request { id, method, params } => {
                let _ = events.send(FromServer::Request { id, method, params });
            }
        }
    }

    /// Calls an MCP method and waits for the answer.
    ///
    /// The id is ours. The agent's ACP request id lives in another id space
    /// entirely and the two must not be confused: an MCP server has no business
    /// receiving an id minted by a browser three hops away, and two agents
    /// sharing one server would collide the moment they did.
    pub async fn request(&self, method: &str, params: Option<Box<RawValue>>) -> Reply {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), tx);

        let frame = Frame::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        if self.to_server.send(frame.to_line()).is_err() {
            self.take_pending(&id);
            return Err(self.gone("is not running"));
        }

        rx.await
            .unwrap_or_else(|_| Err(self.gone("stopped before answering")))
    }

    /// Sends a one-way MCP message. Nothing comes back, by definition.
    pub fn notify(&self, method: &str, params: Option<Box<RawValue>>) {
        let frame = Frame::Notification {
            method: method.to_string(),
            params,
        };
        let _ = self.to_server.send(frame.to_line());
    }

    /// Answers a request the *server* made, with whatever the agent said.
    pub fn answer(&self, id: RequestId, reply: Reply) {
        let frame = match reply {
            Ok(result) => Frame::Response {
                id,
                payload: ResponsePayload::Result(result),
            },
            Err(error) => Frame::error(id, error),
        };
        let _ = self.to_server.send(frame.to_line());
    }

    /// The server's configured name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ends the server: closes its input, gives it a moment, then kills it.
    pub async fn shutdown(&self) {
        let child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.fail_everything_pending();

        let Some(mut child) = child else {
            return;
        };
        // Closing stdin is the polite signal, and the writer task holds the only
        // handle to it — so drop that first by closing the channel it drains.
        drop(child.stdin.take());
        if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
            .await
            .is_err()
        {
            tracing::debug!(server = %self.name, "killing an MCP server that would not exit");
            let _ = child.kill().await;
        }
    }

    fn gone(&self, what: &str) -> JsonRpcError {
        JsonRpcError::internal(format!("the MCP server `{}` {what}", self.name))
    }

    fn take_pending(&self, id: &RequestId) -> Option<oneshot::Sender<Reply>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id)
    }

    /// Tells everyone waiting that no answer is coming.
    fn fail_everything_pending(&self) {
        let waiting: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        for tx in waiting {
            let _ = tx.send(Err(self.gone("stopped")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A server driven over a pipe: one end for the test to read and write, the
    /// other handed to [`McpServer`].
    struct Fixture {
        server: Arc<McpServer>,
        events: mpsc::UnboundedReceiver<FromServer>,
        /// What the server was sent, one JSON-RPC line at a time.
        sent: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        /// Our end of what the server says.
        replies: tokio::io::DuplexStream,
    }

    fn fixture() -> Fixture {
        let (server_reads, replies) = tokio::io::duplex(64 * 1024);
        let (sent, server_writes) = tokio::io::duplex(64 * 1024);
        let (server, events) = McpServer::over_streams("fake", server_reads, server_writes);
        Fixture {
            server,
            events,
            sent: BufReader::new(sent).lines(),
            replies,
        }
    }

    impl Fixture {
        /// The next line the server was sent, as a frame.
        async fn next_sent(&mut self) -> Frame {
            let line = tokio::time::timeout(Duration::from_secs(5), self.sent.next_line())
                .await
                .expect("the server was sent nothing within 5s")
                .unwrap()
                .expect("the stream closed");
            Frame::parse(&line).expect("we write valid JSON-RPC")
        }

        /// Says something back, as the server would.
        async fn say(&mut self, line: &str) {
            self.replies
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }
    }

    fn params(value: serde_json::Value) -> Option<Box<RawValue>> {
        Some(serde_json::value::to_raw_value(&value).unwrap())
    }

    #[tokio::test]
    async fn a_call_gets_the_result_the_server_returned() {
        let mut f = fixture();
        let server = f.server.clone();
        let call = tokio::spawn(async move {
            server
                .request("tools/list", params(json!({ "cursor": null })))
                .await
        });

        let sent = f.next_sent().await;
        assert_eq!(sent.method(), Some("tools/list"));
        // Our own id, not anyone else's: this id space is between us and the
        // server and nothing upstream may leak into it.
        let id = sent.id().unwrap().clone();
        assert!(matches!(id, RequestId::Number(1)), "{id}");

        f.say(
            &Frame::result(id, &json!({ "tools": [{ "name": "stat" }] }))
                .unwrap()
                .to_line(),
        )
        .await;

        let result = call.await.unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_str(result.get()).unwrap();
        assert_eq!(value["tools"][0]["name"], "stat");
    }

    #[tokio::test]
    async fn an_mcp_error_comes_back_as_an_error_and_not_as_a_result() {
        let mut f = fixture();
        let server = f.server.clone();
        let call = tokio::spawn(async move { server.request("tools/call", None).await });

        let id = f.next_sent().await.id().unwrap().clone();
        f.say(
            &Frame::error(
                id,
                JsonRpcError {
                    code: -32602,
                    message: "no such tool".into(),
                    data: None,
                },
            )
            .to_line(),
        )
        .await;

        let error = call.await.unwrap().unwrap_err();
        assert_eq!(error.code, -32602);
        assert_eq!(error.message, "no such tool");
    }

    #[tokio::test]
    async fn two_calls_in_flight_are_answered_out_of_order() {
        // MCP is not request-response over a queue; a server may answer the
        // second call first, and matching by id is the only thing that keeps the
        // two apart.
        let mut f = fixture();
        let first = {
            let server = f.server.clone();
            tokio::spawn(async move { server.request("one", None).await })
        };
        let first_id = f.next_sent().await.id().unwrap().clone();
        let second = {
            let server = f.server.clone();
            tokio::spawn(async move { server.request("two", None).await })
        };
        let second_id = f.next_sent().await.id().unwrap().clone();
        assert_ne!(first_id, second_id);

        f.say(
            &Frame::result(second_id, &json!("second"))
                .unwrap()
                .to_line(),
        )
        .await;
        f.say(&Frame::result(first_id, &json!("first")).unwrap().to_line())
            .await;

        assert_eq!(first.await.unwrap().unwrap().get(), r#""first""#);
        assert_eq!(second.await.unwrap().unwrap().get(), r#""second""#);
    }

    #[tokio::test]
    async fn what_the_server_says_unprompted_is_reported() {
        let mut f = fixture();

        f.say(
            &Frame::notification("notifications/tools/list_changed", &json!({}))
                .unwrap()
                .to_line(),
        )
        .await;
        let event = f.events.recv().await.unwrap();
        let FromServer::Notification { method, .. } = event else {
            panic!("expected a notification, got {event:?}");
        };
        assert_eq!(method, "notifications/tools/list_changed");

        // A request from the server has to be answerable, which is what makes
        // sampling and roots work at all.
        f.say(
            &Frame::Request {
                id: RequestId::String("srv-1".into()),
                method: "roots/list".into(),
                params: None,
            }
            .to_line(),
        )
        .await;
        let event = f.events.recv().await.unwrap();
        let FromServer::Request { id, method, .. } = event else {
            panic!("expected a request, got {event:?}");
        };
        assert_eq!(method, "roots/list");

        f.server.answer(
            id,
            Ok(serde_json::value::to_raw_value(&json!({ "roots": [] })).unwrap()),
        );
        let answered = f.next_sent().await;
        // Answered with the server's own id, untouched: it is the only id the
        // server can correlate.
        assert_eq!(answered.id(), Some(&RequestId::String("srv-1".into())));
    }

    #[tokio::test]
    async fn a_server_that_dies_fails_what_it_never_answered() {
        // A caller awaiting a server that has gone would otherwise hang, and a
        // tool call that hangs is worse than one that fails: the agent is left
        // holding a turn open with nothing to wait for.
        let mut f = fixture();
        let server = f.server.clone();
        let call = tokio::spawn(async move { server.request("tools/list", None).await });
        f.next_sent().await;

        drop(f.replies);

        let error = call.await.unwrap().unwrap_err();
        assert!(error.message.contains("fake"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_rpc_is_dropped_rather_than_fatal() {
        // MCP servers print things. A stray line on stdout must not take the
        // connection down with it.
        let mut f = fixture();
        f.say("this is not JSON").await;
        f.say("{\"not\": \"a frame\"}").await;

        let server = f.server.clone();
        let call = tokio::spawn(async move { server.request("ping", None).await });
        let id = f.next_sent().await.id().unwrap().clone();
        f.say(&Frame::result(id, &json!({})).unwrap().to_line())
            .await;
        assert!(
            call.await.unwrap().is_ok(),
            "the connection survived the noise"
        );
    }
}
