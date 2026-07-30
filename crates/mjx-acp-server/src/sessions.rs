//! Server-side thread state, folded from the traffic passing through the relay.
//!
//! Two things use it. `_mjx/session/replay` serves it to a browser that has
//! lost its copy, and it is a second implementation of the folding rules that
//! the browser's is checked against (see
//! `crates/mjx-acp-thread/tests/fixture.rs`).
//!
//! This state lives with the *agent*, not with the socket. An agent outlives
//! the browser that started it (see `relay::Connection`), so a page that
//! reloads and comes back with its connection id is given this thread rather
//! than an empty one.
//!
//! **What it does not hold.** Terminal scrollback: a terminal belongs to the
//! workspace rather than to the thread, so a resumed page shows the tool call
//! that started one without the output it produced. Nor does it hold a
//! permission prompt — that is a request in flight, and the relay re-asks it
//! after the replay instead.
//!
//! **The one exception is an elicitation**, which is here despite also being a
//! request in flight. A form the user is halfway through is worth carrying
//! across a reload rather than being thrown away and asked again, and a
//! permission prompt can be re-asked cheaply because its tool call's status
//! hides a stale one — an elicitation need not belong to any tool call, so
//! nothing would. Because this holds it, the relay must *not* also re-ask it;
//! see `relay::Relay::unanswered`.

use std::collections::HashMap;

use mjx_acp_core::{Frame, RequestId, ResponsePayload, acp, method};
use mjx_acp_thread::{ElicitationState, Thread};

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
    /// `session/set_config_option` requests in flight, so the refreshed set in
    /// the response can be attributed to the right session.
    ///
    /// Without this a config change made before a reload would be lost: the
    /// browser's `session/new` is answered from the recording made when the
    /// session started (see `relay::Handshake`), so the thread here is the only
    /// place the current value can come from.
    pending_config_options: HashMap<RequestId, String>,
    /// `elicitation/create` requests in flight, so the browser's answer can be
    /// attributed to the question it settles.
    ///
    /// Keyed by the *agent's* request id, which is what reaches the browser: an
    /// agent-originated request is not rebound by the id bridge, so the answer
    /// comes back carrying the same id.
    pending_elicitations: HashMap<RequestId, String>,
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
        match frame {
            Frame::Request { id, method, .. } => self.observe_client_request(frame, id, method),
            // The one response the browser sends that is thread state: the
            // answer to an elicitation. Everything else it replies to — `fs/*`,
            // `terminal/*` — never reaches here, because the server answers
            // those itself.
            Frame::Response { id, payload } => self.observe_elicitation_answer(id, payload),
            Frame::Notification { .. } => {}
        }
    }

    fn observe_client_request(&mut self, frame: &Frame, id: &RequestId, method: &str) {
        match method {
            method::agent::SESSION_NEW => self.pending_new_sessions.push(id.clone()),
            method::agent::SESSION_SET_CONFIG_OPTION => {
                if let Ok(Some(request)) = frame.params_as::<acp::SetSessionConfigOptionRequest>() {
                    self.pending_config_options
                        .insert(id.clone(), request.session_id.0.to_string());
                }
            }
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

    /// Settles the elicitation a browser's response answers.
    fn observe_elicitation_answer(&mut self, id: &RequestId, payload: &ResponsePayload) {
        let Some(session_id) = self.pending_elicitations.remove(id) else {
            return;
        };
        let Some(thread) = self.threads.get_mut(&session_id) else {
            return;
        };

        let (state, content) = match payload {
            ResponsePayload::Result(result) => {
                match serde_json::from_str::<acp::CreateElicitationResponse>(result.get()) {
                    Ok(response) => read_action(response.action),
                    // A well-formed response we cannot read is not the user
                    // saying yes or no, so it is neither accepted nor declined.
                    Err(_) => (ElicitationState::Cancelled, None),
                }
            }
            // The browser refused the request outright — an unrenderable mode,
            // or a client that has no handler for one at all.
            ResponsePayload::Error(_) => (ElicitationState::Declined, None),
        };

        thread.settle_elicitation(id, state, content);
    }

    /// Folds in a frame on its way from the agent to the browser.
    pub fn observe_from_agent(&mut self, frame: &Frame) {
        match frame {
            Frame::Request { id, method, .. } if method == method::client::ELICITATION_CREATE => {
                let Ok(Some(request)) = frame.params_as::<acp::CreateElicitationRequest>() else {
                    return;
                };
                let session_id = match request.scope() {
                    acp::ElicitationScope::Session(scope) => scope.session_id.0.to_string(),
                    // Request-scoped: tied to a JSON-RPC request rather than to
                    // a session, which is how an agent asks something before any
                    // session exists. There is no thread to put it in, so the
                    // relay's re-ask path carries it instead.
                    _ => return,
                };

                if self
                    .threads
                    .entry(session_id.clone())
                    .or_default()
                    .push_elicitation(id.clone(), &request)
                {
                    self.pending_elicitations.insert(id.clone(), session_id);
                }
            }

            Frame::Notification { method, .. }
                if method == method::client::ELICITATION_COMPLETE =>
            {
                let Ok(Some(notification)) =
                    frame.params_as::<acp::CompleteElicitationNotification>()
                else {
                    return;
                };
                // The notification names the elicitation, not the session, so
                // the thread holding it has to be looked for. The id is the
                // agent's own and unique across the sessions it is running.
                for thread in self.threads.values_mut() {
                    if thread.complete_elicitation(&notification.elicitation_id.0) {
                        break;
                    }
                }
            }

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
                        if let Some(options) = &response.config_options {
                            thread.set_config_options(options);
                        }
                    }
                } else if let Some(session_id) = self.pending_config_options.remove(id)
                    && let Ok(response) =
                        serde_json::from_str::<acp::SetSessionConfigOptionResponse>(result.get())
                    && let Some(thread) = self.threads.get_mut(&session_id)
                {
                    thread.set_config_options(&response.config_options);
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
                self.pending_config_options.remove(id);
            }

            _ => {}
        }
    }
}

/// Reads what the user did into the state the thread records.
type Settled = (
    ElicitationState,
    Option<std::collections::BTreeMap<String, acp::ElicitationContentValue>>,
);

fn read_action(action: acp::ElicitationAction) -> Settled {
    match action {
        acp::ElicitationAction::Accept(accepted) => (ElicitationState::Accepted, accepted.content),
        acp::ElicitationAction::Decline => (ElicitationState::Declined, None),
        // Cancelled, or an action from a future protocol version we cannot
        // read. Either way we cannot claim the user accepted or refused.
        _ => (ElicitationState::Cancelled, None),
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

    /// A select option, as an agent would advertise it.
    fn model_option(current: &str) -> serde_json::Value {
        json!({
            "id": "model", "name": "Model", "category": "model",
            "type": "select", "currentValue": current,
            "options": [
                { "value": "sonnet", "name": "Sonnet" },
                { "value": "opus", "name": "Opus" }
            ]
        })
    }

    fn current_model(store: &SessionStore) -> String {
        match &store.thread("s1").unwrap().config_options[0].kind {
            acp::SessionConfigKind::Select(select) => select.current_value.0.to_string(),
            other => panic!("expected a select, got {other:?}"),
        }
    }

    #[test]
    fn a_new_session_records_its_config_options() {
        let mut store = SessionStore::new();
        store.observe_from_client(&request(
            1,
            method::agent::SESSION_NEW,
            json!({ "cwd": "/w", "mcpServers": [] }),
        ));
        store.observe_from_agent(
            &Frame::result(
                RequestId::Number(1),
                &json!({ "sessionId": "s1", "configOptions": [model_option("sonnet")] }),
            )
            .unwrap(),
        );

        assert_eq!(current_model(&store), "sonnet");
    }

    /// The reason this store watches config options at all: the browser's own
    /// `session/new` is answered from a recording after a reload, so a change
    /// made in between is only remembered here.
    #[test]
    fn setting_a_config_option_updates_the_thread() {
        let mut store = started();
        store.observe_from_client(&request(
            9,
            method::agent::SESSION_SET_CONFIG_OPTION,
            json!({ "sessionId": "s1", "configId": "model", "value": "opus" }),
        ));
        store.observe_from_agent(
            &Frame::result(
                RequestId::Number(9),
                &json!({ "configOptions": [model_option("opus")] }),
            )
            .unwrap(),
        );

        assert_eq!(current_model(&store), "opus");
    }

    #[test]
    fn a_rejected_config_change_leaves_nothing_behind() {
        let mut store = started();
        store.observe_from_client(&request(
            9,
            method::agent::SESSION_SET_CONFIG_OPTION,
            json!({ "sessionId": "s1", "configId": "model", "value": "opus" }),
        ));
        store.observe_from_agent(&Frame::error(
            RequestId::Number(9),
            mjx_acp_core::JsonRpcError::invalid_params("no such model"),
        ));

        assert!(store.pending_config_options.is_empty());
        assert!(store.thread("s1").unwrap().config_options.is_empty());
    }

    #[test]
    fn a_config_option_update_is_folded() {
        let mut store = started();
        store.observe_from_agent(&update(
            "s1",
            json!({ "sessionUpdate": "config_option_update",
                    "configOptions": [model_option("opus")] }),
        ));

        assert_eq!(current_model(&store), "opus");
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

    /// The elicitation the tests below ask, as an agent would write it.
    fn ask(scope: serde_json::Value) -> Frame {
        let mut params = serde_json::json!({
            "mode": "form",
            "message": "Which branch?",
            "requestedSchema": {
                "type": "object",
                "properties": { "branch": { "type": "string" } }
            }
        });
        let serde_json::Value::Object(scope) = scope else {
            panic!("a scope is an object");
        };
        for (key, value) in scope {
            params[key] = value;
        }
        request(20, method::client::ELICITATION_CREATE, params)
    }

    fn only_elicitation(store: &SessionStore) -> &mjx_acp_thread::Elicitation {
        store
            .thread("s1")
            .expect("no thread")
            .entries
            .iter()
            .filter_map(mjx_acp_thread::Entry::as_elicitation)
            .next()
            .expect("no elicitation entry")
    }

    #[test]
    fn an_elicitation_becomes_part_of_the_thread() {
        // The point of holding it here: this is what a reloaded browser is
        // given back, so a form it was halfway through comes back with it.
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "sessionId": "s1" })));

        assert_eq!(only_elicitation(&store).message, "Which branch?");
        assert_eq!(
            only_elicitation(&store).state,
            mjx_acp_thread::ElicitationState::Pending
        );
    }

    #[test]
    fn the_browsers_answer_settles_it() {
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "sessionId": "s1" })));
        store.observe_from_client(
            &Frame::result(
                RequestId::Number(20),
                &json!({ "action": "accept", "content": { "branch": "main" } }),
            )
            .unwrap(),
        );

        let asked = only_elicitation(&store);
        assert_eq!(asked.state, mjx_acp_thread::ElicitationState::Accepted);
        assert!(
            asked
                .content
                .as_ref()
                .is_some_and(|c| c.contains_key("branch"))
        );
        assert!(store.pending_elicitations.is_empty());
    }

    #[test]
    fn declining_is_recorded_as_declining() {
        // Not the same as cancelling: the user was asked and said no, which is
        // an answer the agent is entitled to see reflected in the history.
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "sessionId": "s1" })));
        store.observe_from_client(
            &Frame::result(RequestId::Number(20), &json!({ "action": "decline" })).unwrap(),
        );

        assert_eq!(
            only_elicitation(&store).state,
            mjx_acp_thread::ElicitationState::Declined
        );
    }

    #[test]
    fn a_url_elicitation_is_finished_by_the_notification() {
        let mut store = started();
        store.observe_from_agent(&request(
            20,
            method::client::ELICITATION_CREATE,
            json!({
                "mode": "url",
                "sessionId": "s1",
                "elicitationId": "el-1",
                "url": "https://example.test/authorize",
                "message": "Authorize, then come back."
            }),
        ));
        store.observe_from_agent(
            &Frame::notification(
                method::client::ELICITATION_COMPLETE,
                &json!({ "elicitationId": "el-1" }),
            )
            .unwrap(),
        );

        assert_eq!(
            only_elicitation(&store).state,
            mjx_acp_thread::ElicitationState::Accepted
        );
    }

    #[test]
    fn a_request_scoped_elicitation_is_left_to_the_relay() {
        // It is tied to a JSON-RPC request rather than a session — how an agent
        // asks something before any session exists — so there is no thread for
        // it and the relay's re-ask path carries it instead.
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "requestId": 4 })));

        assert!(
            store
                .thread("s1")
                .unwrap()
                .entries
                .iter()
                .all(|entry| mjx_acp_thread::Entry::as_elicitation(entry).is_none())
        );
        assert!(store.pending_elicitations.is_empty());
    }

    #[test]
    fn a_mode_we_cannot_render_is_not_recorded_as_pending() {
        // The browser will decline it, and that response must not be mistaken
        // for the answer to a question we are tracking.
        let mut store = started();
        store.observe_from_agent(&request(
            20,
            method::client::ELICITATION_CREATE,
            json!({ "mode": "_vendor/hologram", "sessionId": "s1", "message": "step in" }),
        ));

        assert!(store.pending_elicitations.is_empty());
    }

    #[test]
    fn a_malformed_elicitation_does_not_panic() {
        let mut store = started();
        store.observe_from_agent(&request(
            20,
            method::client::ELICITATION_CREATE,
            json!({ "nonsense": true }),
        ));

        assert!(store.pending_elicitations.is_empty());
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
