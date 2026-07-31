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
//! request in flight. A permission prompt is one click and leaves nothing worth
//! reading behind; a question the agent asked, and what the user answered, is
//! part of the conversation. So this holds the whole lifecycle, and a reload
//! shows an accepted form as accepted rather than losing it.
//!
//! **More than one session per connection.** An agent that supports
//! `session/list` lets the browser open a conversation from before this
//! connection existed, so this store holds a thread per session and a load
//! *resets* the one it names — a load replays the whole conversation, and
//! folding that on top of what was there would double it.
//!
//! Which of those sessions the browser is looking at is the browser's own
//! business, not this store's. The relay still answers a repeat `session/new`
//! from the recording it made when the connection started (see
//! `relay::Handshake`), because that is what makes a reload transparent to an
//! ordinary ACP client; a browser that has since loaded a different session
//! remembers that itself and asks `_mjx/session/replay` for it by name.
//!
//! It is *also* re-asked over the socket (`relay::Relay::unanswered`), and that
//! is not redundant: a browser cannot answer a request the connection it is
//! holding never received, so the replay alone would put an unanswerable form on
//! screen. The two are matched up by request id on the browser's side.

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
    /// `session/new` and `session/fork` requests in flight, so the id in the
    /// response can be matched with the mode state that comes with it. Both
    /// answer with a session id this store has not seen before.
    pending_session_creations: Vec<RequestId>,
    /// `session/load` and `session/resume` requests in flight, keyed to the
    /// session they name, so the modes and config options in the response land
    /// on a session that already exists.
    pending_session_states: HashMap<RequestId, String>,
    /// `session/delete` and `session/close` requests in flight.
    ///
    /// Kept until the response, because a refused delete must not lose the
    /// history: the session still exists on the agent, and a browser that
    /// reloads is still owed it.
    pending_forgets: HashMap<RequestId, String>,
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

    /// Gives up on every question a session is still waiting on an answer to.
    ///
    /// Called when a turn ends: whatever it asked and never got an answer to
    /// will not be answered now, and a replayed thread showing a live form for
    /// a finished turn would invite an answer with nowhere to go.
    ///
    /// Returns how many were still pending.
    pub fn cancel_pending_elicitations(&mut self, session_id: &str) -> usize {
        self.pending_elicitations
            .retain(|_, pending| pending != session_id);
        self.threads
            .get_mut(session_id)
            .map(Thread::cancel_pending_elicitations)
            .unwrap_or(0)
    }

    /// Drops everything held for a session the agent no longer has.
    ///
    /// The thread goes with it: `_mjx/session/replay` for a deleted session
    /// should answer "nothing", not hand back a conversation that has been
    /// removed from the agent it belonged to.
    fn forget(&mut self, session_id: &str) {
        self.threads.remove(session_id);
        self.pending_prompts
            .retain(|_, pending| pending != session_id);
        self.pending_config_options
            .retain(|_, pending| pending != session_id);
        self.pending_elicitations
            .retain(|_, pending| pending != session_id);
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
            method::agent::SESSION_NEW | method::agent::SESSION_FORK => {
                self.pending_session_creations.push(id.clone());
            }

            // A load streams the whole conversation back as `session/update`
            // notifications, so the thread it lands in has to start empty:
            // folding a replay on top of what was already here would show every
            // message twice, and three times after the next load. The browser
            // does the same to its own copy for the same reason.
            method::agent::SESSION_LOAD => {
                let Ok(Some(request)) = frame.params_as::<acp::LoadSessionRequest>() else {
                    return;
                };
                let session_id = request.session_id.0.to_string();
                self.threads.insert(session_id.clone(), Thread::default());
                // The replayed conversation is the whole conversation, and it
                // does not contain the question the old one was still asking.
                self.pending_elicitations
                    .retain(|_, pending| pending != &session_id);
                self.pending_session_states.insert(id.clone(), session_id);
            }

            // A resume reactivates a session *without* replaying it, which is
            // the only thing that distinguishes it from a load — so the thread
            // it has is the thread it keeps.
            method::agent::SESSION_RESUME => {
                if let Ok(Some(request)) = frame.params_as::<acp::ResumeSessionRequest>() {
                    self.pending_session_states
                        .insert(id.clone(), request.session_id.0.to_string());
                }
            }

            method::agent::SESSION_DELETE => {
                if let Ok(Some(request)) = frame.params_as::<acp::DeleteSessionRequest>() {
                    self.pending_forgets
                        .insert(id.clone(), request.session_id.0.to_string());
                }
            }

            method::agent::SESSION_CLOSE => {
                if let Ok(Some(request)) = frame.params_as::<acp::CloseSessionRequest>() {
                    self.pending_forgets
                        .insert(id.clone(), request.session_id.0.to_string());
                }
            }
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
                } else if let Some(index) =
                    self.pending_session_creations.iter().position(|p| p == id)
                {
                    self.pending_session_creations.remove(index);
                    // A fork answers the same shape as a new session — an id
                    // this store has not seen, with the modes and options it
                    // starts life with — so one reader covers both. What it
                    // forked *from* keeps its own thread: the fork is a second
                    // conversation, not a move.
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
                } else if let Some(session_id) = self.pending_session_states.remove(id) {
                    // `session/resume` answers the same shape as `session/load`,
                    // so this reads both.
                    if let Ok(response) =
                        serde_json::from_str::<acp::LoadSessionResponse>(result.get())
                        && let Some(thread) = self.threads.get_mut(&session_id)
                    {
                        if let Some(modes) = &response.modes {
                            thread.set_modes(modes);
                        }
                        if let Some(options) = &response.config_options {
                            thread.set_config_options(options);
                        }
                    }
                } else if let Some(session_id) = self.pending_forgets.remove(id) {
                    // The agent says the session is gone, or has been freed.
                    // Either way there is nothing here left to replay.
                    self.forget(&session_id);
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
                self.pending_session_creations
                    .retain(|pending| pending != id);
                self.pending_config_options.remove(id);
                self.pending_session_states.remove(id);
                // A refused delete is a session that still exists. Forgetting
                // its thread here would lose a history the agent still has.
                self.pending_forgets.remove(id);
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
    fn a_finished_turn_gives_up_on_what_it_asked() {
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "sessionId": "s1" })));

        assert_eq!(store.cancel_pending_elicitations("s1"), 1);
        assert_eq!(
            only_elicitation(&store).state,
            mjx_acp_thread::ElicitationState::Cancelled
        );
        assert!(store.pending_elicitations.is_empty());
    }

    #[test]
    fn giving_up_does_not_undo_an_answer_already_given() {
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "sessionId": "s1" })));
        store.observe_from_client(
            &Frame::result(RequestId::Number(20), &json!({ "action": "decline" })).unwrap(),
        );

        assert_eq!(store.cancel_pending_elicitations("s1"), 0);
        assert_eq!(
            only_elicitation(&store).state,
            mjx_acp_thread::ElicitationState::Declined
        );
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

    /// A session/load request for `session_id`, as the browser sends it.
    fn load(id: i64, session_id: &str) -> Frame {
        request(
            id,
            method::agent::SESSION_LOAD,
            json!({ "sessionId": session_id, "cwd": "/w", "mcpServers": [] }),
        )
    }

    #[test]
    fn a_loaded_session_starts_its_thread_from_empty() {
        // `session/load` streams the whole conversation back. Folding that on
        // top of what was already here would show every message twice, and
        // three times after the next load.
        let mut store = started();
        store.observe_from_agent(&update(
            "s1",
            json!({ "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hi" } }),
        ));
        assert_eq!(store.thread("s1").unwrap().entries.len(), 2);

        store.observe_from_client(&load(30, "s1"));
        assert_eq!(store.thread("s1").unwrap().entries.len(), 0);

        store.observe_from_agent(&update(
            "s1",
            json!({ "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hi" } }),
        ));
        assert_eq!(store.thread("s1").unwrap().entries.len(), 1);
    }

    #[test]
    fn a_load_answers_with_the_modes_and_options_in_effect() {
        // The same reason `session/set_config_option` is watched: after a reload
        // the browser's own handshake is answered from a recording, so this
        // thread is the only place the current model can come from.
        let mut store = started();
        store.observe_from_client(&load(30, "s1"));
        store.observe_from_agent(
            &Frame::result(
                RequestId::Number(30),
                &json!({
                    "modes": { "currentModeId": "ask", "availableModes": [{ "id": "ask", "name": "Ask" }] },
                    "configOptions": [model_option("opus")]
                }),
            )
            .unwrap(),
        );

        assert_eq!(current_model(&store), "opus");
        assert_eq!(
            store
                .thread("s1")
                .unwrap()
                .modes
                .as_ref()
                .unwrap()
                .current_mode_id,
            "ask"
        );
    }

    #[test]
    fn a_resumed_session_keeps_the_conversation_it_had() {
        // The whole difference between resuming and loading: nothing is
        // replayed, so nothing may be thrown away either.
        let mut store = started();
        store.observe_from_client(&request(
            31,
            method::agent::SESSION_RESUME,
            json!({ "sessionId": "s1", "cwd": "/w" }),
        ));

        assert_eq!(store.thread("s1").unwrap().entries.len(), 1);

        store.observe_from_agent(
            &Frame::result(
                RequestId::Number(31),
                &json!({ "configOptions": [model_option("opus")] }),
            )
            .unwrap(),
        );
        assert_eq!(current_model(&store), "opus");
    }

    #[test]
    fn a_fork_opens_a_thread_of_its_own() {
        let mut store = started();
        store.observe_from_client(&request(
            32,
            method::agent::SESSION_FORK,
            json!({ "sessionId": "s1", "cwd": "/w" }),
        ));
        store.observe_from_agent(
            &Frame::result(
                RequestId::Number(32),
                &json!({ "sessionId": "s2", "configOptions": [model_option("opus")] }),
            )
            .unwrap(),
        );

        // The fork is a second conversation, and the one it came from is
        // untouched: its history stays where the browser can still replay it.
        assert_eq!(store.len(), 2);
        assert_eq!(store.thread("s1").unwrap().entries.len(), 1);
        assert!(store.thread("s2").unwrap().entries.is_empty());
    }

    #[test]
    fn deleting_or_closing_a_session_forgets_its_thread() {
        for method in [method::agent::SESSION_DELETE, method::agent::SESSION_CLOSE] {
            let mut store = started();
            store.observe_from_client(&request(33, method, json!({ "sessionId": "s1" })));
            // Not yet: the agent has not said it went through.
            assert!(store.thread("s1").is_some(), "{method} forgot too early");

            store.observe_from_agent(&Frame::result(RequestId::Number(33), &json!({})).unwrap());
            assert!(store.thread("s1").is_none(), "{method} did not forget");
        }
    }

    #[test]
    fn a_refused_delete_leaves_the_conversation_alone() {
        // The agent said no, so the session still exists — and a browser that
        // reloads is still owed its history.
        let mut store = started();
        store.observe_from_client(&request(
            33,
            method::agent::SESSION_DELETE,
            json!({ "sessionId": "s1" }),
        ));
        store.observe_from_agent(&Frame::error(
            RequestId::Number(33),
            mjx_acp_core::JsonRpcError::invalid_params("no such session"),
        ));

        assert!(store.thread("s1").is_some());
    }

    #[test]
    fn loading_gives_up_on_the_questions_the_old_thread_was_asking() {
        // The replayed conversation is the whole conversation. A form left
        // pending from before it would be answered into a thread that no longer
        // holds the question.
        let mut store = started();
        store.observe_from_agent(&ask(serde_json::json!({ "sessionId": "s1" })));
        store.observe_from_client(&load(30, "s1"));

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
