//! Server-side thread state, folded from the traffic passing through the relay.
//!
//! Two things use it. `_mjx/session/replay` serves it to a browser that has
//! lost its copy, and it is a second implementation of the folding rules that
//! the browser's is checked against (see
//! `crates/mjx-acp-thread/tests/fixture.rs`).
//!
//! **Limitation.** This state lives and dies with the WebSocket, because the
//! agent subprocess does: closing the socket ends the connection and the agent
//! with it. So replay currently helps a browser that lost its *in-memory* copy
//! on a live connection, not one that reloaded the page — a reload starts a
//! new agent and a new session. Surviving a reload needs the agent to outlive
//! the socket, which is a connection-pooling change in the server, not a
//! change here.

use std::collections::HashMap;

use mjx_acp_core::{Frame, RequestId, acp, method};
use mjx_acp_thread::Thread;

/// Every session on one connection.
#[derive(Debug, Default)]
pub struct SessionStore {
    threads: HashMap<String, Thread>,
    /// `session/prompt` requests in flight, so their responses can be
    /// attributed to the right session.
    pending_prompts: HashMap<RequestId, String>,
    /// `session/new` requests in flight, so the id in the response can be
    /// matched with the mode state that comes with it.
    pending_new_sessions: Vec<RequestId>,
}

impl SessionStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The thread for a session, if we have one.
    pub fn thread(&self, session_id: &str) -> Option<&Thread> {
        self.threads.get(session_id)
    }

    /// How many sessions are being tracked.
    pub fn len(&self) -> usize {
        self.threads.len()
    }

    /// The session a `session/prompt` still in flight belongs to.
    ///
    /// A peek, not a take: the relay needs to know which turn a response ends
    /// *before* [`SessionStore::observe_from_agent`] consumes the record, so it
    /// can tell a browser that inherited the turn that it is over.
    pub fn session_of_prompt(&self, id: &RequestId) -> Option<&str> {
        self.pending_prompts.get(id).map(String::as_str)
    }

    /// Whether nothing is being tracked.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// Folds in a frame on its way from the browser to the agent.
    pub fn observe_from_client(&mut self, frame: &Frame) {
        let Frame::Request { id, method, .. } = frame else {
            return;
        };

        match method.as_str() {
            method::agent::SESSION_NEW => self.pending_new_sessions.push(id.clone()),
            method::agent::SESSION_PROMPT => {
                let Ok(Some(request)) = frame.params_as::<acp::PromptRequest>() else {
                    return;
                };
                let session_id = request.session_id.0.to_string();
                // Record the prompt the way the browser does, optimistically,
                // so a reload during the turn still shows what was asked.
                self.threads
                    .entry(session_id.clone())
                    .or_default()
                    .push_user_prompt(request.prompt);
                self.pending_prompts.insert(id.clone(), session_id);
            }
            _ => {}
        }
    }

    /// Folds in a frame on its way from the agent to the browser.
    pub fn observe_from_agent(&mut self, frame: &Frame) {
        match frame {
            Frame::Notification { method, .. } if method == method::client::SESSION_UPDATE => {
                let Ok(Some(notification)) = frame.params_as::<acp::SessionNotification>() else {
                    return;
                };
                self.threads
                    .entry(notification.session_id.0.to_string())
                    .or_default()
                    .apply(&notification.update);
            }

            Frame::Response {
                id,
                payload: mjx_acp_core::ResponsePayload::Result(result),
            } => {
                if let Some(session_id) = self.pending_prompts.remove(id) {
                    if let Ok(response) = serde_json::from_str::<acp::PromptResponse>(result.get())
                        && let Some(thread) = self.threads.get_mut(&session_id)
                    {
                        thread.finish_turn(response.stop_reason);
                    }
                } else if let Some(index) = self.pending_new_sessions.iter().position(|p| p == id) {
                    self.pending_new_sessions.remove(index);
                    if let Ok(response) =
                        serde_json::from_str::<acp::NewSessionResponse>(result.get())
                    {
                        let thread = self
                            .threads
                            .entry(response.session_id.0.to_string())
                            .or_default();
                        if let Some(modes) = &response.modes {
                            thread.set_modes(modes);
                        }
                    }
                }
            }

            // A failed prompt still ends the turn; forgetting the request would
            // leave the thread stuck showing "generating" forever.
            Frame::Response {
                id,
                payload: mjx_acp_core::ResponsePayload::Error(_),
            } => {
                if let Some(session_id) = self.pending_prompts.remove(id)
                    && let Some(thread) = self.threads.get_mut(&session_id)
                {
                    thread.finish_turn(acp::StopReason::Cancelled);
                }
                self.pending_new_sessions.retain(|pending| pending != id);
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjx_acp_thread::ThreadStatus;
    use serde_json::json;

    fn request(id: i64, method: &str, params: serde_json::Value) -> Frame {
        Frame::Request {
            id: RequestId::Number(id),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        }
    }

    fn update(session_id: &str, update: serde_json::Value) -> Frame {
        Frame::notification(
            method::client::SESSION_UPDATE,
            &json!({ "sessionId": session_id, "update": update }),
        )
        .unwrap()
    }

    /// A store that has been through a `session/new` and a prompt.
    fn started() -> SessionStore {
        let mut store = SessionStore::new();
        store.observe_from_client(&request(
            1,
            method::agent::SESSION_NEW,
            json!({ "cwd": "/w", "mcpServers": [] }),
        ));
        store.observe_from_agent(
            &Frame::result(
                RequestId::Number(1),
                &json!({
                    "sessionId": "s1",
                    "modes": {
                        "currentModeId": "code",
                        "availableModes": [{ "id": "ask", "name": "Ask" }]
                    }
                }),
            )
            .unwrap(),
        );
        store.observe_from_client(&request(
            2,
            method::agent::SESSION_PROMPT,
            json!({ "sessionId": "s1", "prompt": [{ "type": "text", "text": "hello" }] }),
        ));
        store
    }

    #[test]
    fn a_new_session_records_its_modes() {
        let store = started();
        let modes = store.thread("s1").unwrap().modes.as_ref().unwrap();
        assert_eq!(modes.current_mode_id, "code");
        assert_eq!(modes.available_modes.len(), 1);
    }

    #[test]
    fn a_prompt_is_recorded_before_the_agent_answers() {
        // A reload mid-turn should still show what was asked.
        let store = started();
        let thread = store.thread("s1").unwrap();
        assert_eq!(thread.entries.len(), 1);
        assert_eq!(thread.status, ThreadStatus::Generating);
    }

    #[test]
    fn updates_are_folded_into_the_right_session() {
        let mut store = started();
        store.observe_from_agent(&update(
            "s1",
            json!({ "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hi" } }),
        ));
        store.observe_from_agent(&update(
            "s2",
            json!({ "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "other" } }),
        ));

        assert_eq!(store.len(), 2);
        assert_eq!(store.thread("s1").unwrap().entries.len(), 2);
        assert_eq!(store.thread("s2").unwrap().entries.len(), 1);
    }

    #[test]
    fn the_prompt_response_ends_the_turn() {
        let mut store = started();
        store.observe_from_agent(
            &Frame::result(RequestId::Number(2), &json!({ "stopReason": "end_turn" })).unwrap(),
        );

        let thread = store.thread("s1").unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.stop_reason, Some(acp::StopReason::EndTurn));
    }

    #[test]
    fn a_failed_prompt_also_ends_the_turn() {
        // Otherwise the thread shows "generating" forever.
        let mut store = started();
        store.observe_from_agent(&Frame::error(
            RequestId::Number(2),
            mjx_acp_core::JsonRpcError::internal("boom"),
        ));

        assert_eq!(store.thread("s1").unwrap().status, ThreadStatus::Idle);
    }

    #[test]
    fn concurrent_prompts_are_attributed_to_their_own_sessions() {
        // Request ids are per-connection, not per-session, so the mapping has
        // to be by id rather than by "the last prompt we saw".
        let mut store = SessionStore::new();
        store.observe_from_client(&request(
            10,
            method::agent::SESSION_PROMPT,
            json!({ "sessionId": "a", "prompt": [] }),
        ));
        store.observe_from_client(&request(
            11,
            method::agent::SESSION_PROMPT,
            json!({ "sessionId": "b", "prompt": [] }),
        ));

        store.observe_from_agent(
            &Frame::result(RequestId::Number(11), &json!({ "stopReason": "end_turn" })).unwrap(),
        );

        assert_eq!(store.thread("b").unwrap().status, ThreadStatus::Idle);
        assert_eq!(
            store.thread("a").unwrap().status,
            ThreadStatus::Generating,
            "the wrong session's turn was ended"
        );
    }

    #[test]
    fn unrelated_traffic_is_ignored() {
        let mut store = SessionStore::new();
        store.observe_from_client(&request(1, "initialize", json!({ "protocolVersion": 1 })));
        store.observe_from_agent(
            &Frame::notification("_mjx/agent/stderr", &json!({ "line": "x" })).unwrap(),
        );
        assert!(store.is_empty());
    }

    #[test]
    fn a_malformed_update_does_not_panic() {
        // Frames come off a socket; nothing guarantees they are well-formed.
        let mut store = SessionStore::new();
        store.observe_from_agent(
            &Frame::notification(method::client::SESSION_UPDATE, &json!({ "nonsense": true }))
                .unwrap(),
        );
        assert!(store.is_empty());
    }
}
