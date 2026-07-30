//! The thread model: `session/update` folded into something renderable.
//!
//! A port of Zed's `crates/acp_thread/src/acp_thread.rs`
//! (`reference/zed-acp/acp_thread/`), with GPUI, `Project` and `MultiBuffer`
//! removed. What survives is the part that is genuinely hard — deciding when
//! two streamed chunks are the same message, when a tool call is new versus an
//! update, and which fields a partial update may overwrite.
//!
//! It runs on the server so a browser that reloads can be given the whole
//! thread back instead of an empty page, and so there is a second
//! implementation to check the browser's against. Both are held to the same
//! recorded frames by `fixtures/session-updates.jsonl`.
//!
//! Deliberately not ported: the smooth-streaming text buffer (CSS does that
//! better), git checkpoints and rewind, and the editor-backed diff and terminal
//! views.

use std::collections::BTreeMap;

use mjx_acp_core::{RequestId, acp};
use serde::{Deserialize, Serialize};

mod fold;
pub mod paths;

pub use fold::ThreadEvent;

/// One item in the timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Entry {
    /// Something the user said.
    User(UserMessage),
    /// Something the agent said, including its thinking.
    Assistant(AssistantMessage),
    /// A tool the agent used.
    ToolCall(ToolCall),
    /// A structured question the agent asked the user.
    Elicitation(Elicitation),
}

impl Entry {
    /// The tool call this entry holds, if it is one.
    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    /// The elicitation this entry holds, if it is one.
    pub fn as_elicitation(&self) -> Option<&Elicitation> {
        match self {
            Self::Elicitation(asked) => Some(asked),
            _ => None,
        }
    }

    /// The entry's stable id.
    pub fn id(&self) -> &str {
        match self {
            Self::User(message) => &message.id,
            Self::Assistant(message) => &message.id,
            Self::ToolCall(call) => &call.id,
            Self::Elicitation(asked) => &asked.id,
        }
    }
}

/// A user turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    /// Stable identity for the UI.
    pub id: String,
    /// The agent's own id for this message, once it tells us one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_id: Option<String>,
    /// True until the agent echoes the message back.
    ///
    /// The client shows the prompt the moment it is sent rather than waiting
    /// for a round trip; agents that echo `user_message_chunk` would otherwise
    /// make it appear twice.
    pub is_optimistic: bool,
    /// What was said.
    pub content: Vec<acp::ContentBlock>,
}

/// An agent turn, as a run of chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    /// Stable identity for the UI.
    pub id: String,
    /// Prose and thinking, in the order they arrived.
    pub chunks: Vec<AssistantChunk>,
}

/// Whether a chunk is something the agent said or something it thought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChunkKind {
    /// Prose, shown to the user.
    Message,
    /// Reasoning, shown collapsed.
    Thought,
}

/// A run of content of one kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantChunk {
    /// Prose or thinking.
    pub kind: ChunkKind,
    /// The agent's id for the message this belongs to, when it gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Adjacent text blocks are merged; images and resources are kept as they
    /// arrived.
    pub content: Vec<acp::ContentBlock>,
}

impl AssistantChunk {
    /// The chunk's text, with non-text blocks skipped.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// A tool the agent used, and everything the UI shows about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// The agent's id for this call.
    pub id: String,
    /// Human-readable label.
    pub title: String,
    /// What kind of tool it is, for iconography.
    pub kind: acp::ToolKind,
    /// Where it is in its lifecycle.
    pub status: acp::ToolCallStatus,
    /// What it has to show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<acp::ToolCallContent>,
    /// Files it touched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<acp::ToolCallLocation>,
    /// The arguments it was called with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
    /// What it returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<serde_json::Value>,
}

/// A structured question the agent put to the user.
///
/// The one request kind that is part of the thread. A permission prompt is not:
/// it is a request in flight, and the relay re-asks it after a replay instead
/// (see `mjx_acp_server::sessions`). Two things make an elicitation different.
/// A form the user is halfway through is worth carrying across a reload, and a
/// permission prompt has a tool call whose status masks a stale one — an
/// elicitation need not be tied to any tool call, so nothing else would.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Elicitation {
    /// Stable identity for the UI.
    pub id: String,
    /// The JSON-RPC id the answer has to carry.
    pub request_id: RequestId,
    /// What the agent says it needs.
    pub message: String,
    /// The tool call this belongs to, when the agent names one.
    ///
    /// A label, not a home: the elicitation renders in the timeline either way,
    /// so there is one code path rather than two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// How the user is being asked.
    #[serde(flatten)]
    pub mode: ElicitationMode,
    /// Where the question is in its lifecycle.
    pub state: ElicitationState,
    /// What the user submitted, once they have.
    ///
    /// Kept rather than discarded: an answered form is history worth reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<BTreeMap<String, acp::ElicitationContentValue>>,
}

/// How the user is being asked.
///
/// Only the two modes the protocol defines. An unknown mode is refused outright
/// rather than modelled: the spec says a client must not render one as if it
/// understood it, and refusing is what lets the browser decline instead of
/// leaving the agent waiting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ElicitationMode {
    /// A form built from a JSON Schema.
    Form {
        /// The fields to ask for.
        requested_schema: acp::ElicitationSchema,
    },
    /// A link to visit and come back from.
    Url {
        /// The agent's own id for this exchange, which
        /// `elicitation/complete` names.
        elicitation_id: String,
        /// Where to send the user.
        url: String,
    },
}

/// Where an elicitation is in its lifecycle.
///
/// The four states are the three `CreateElicitationResponse` actions plus the
/// one before any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ElicitationState {
    /// Waiting on the user.
    Pending,
    /// The user answered.
    Accepted,
    /// The user refused to answer.
    Declined,
    /// Nobody will answer: the turn ended, or the socket went away.
    Cancelled,
}

/// Whether the agent is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStatus {
    /// Waiting for a prompt.
    #[default]
    Idle,
    /// Running a turn.
    Generating,
}

/// Token and cost accounting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Tokens used.
    pub used: u64,
    /// Context window size.
    pub size: u64,
    /// Money spent, when the agent reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

/// A monetary amount.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// How much.
    pub amount: f64,
    /// In what.
    pub currency: String,
}

/// The available and current session modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Modes {
    /// Which mode is active.
    pub current_mode_id: String,
    /// What can be switched to.
    pub available_modes: Vec<acp::SessionMode>,
}

/// A whole conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    /// The timeline.
    pub entries: Vec<Entry>,
    /// The agent's current plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<acp::PlanEntry>,
    /// Whether a turn is running.
    pub status: ThreadStatus,
    /// How the last turn ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<acp::StopReason>,
    /// Token accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Slash commands the agent offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_commands: Vec<acp::AvailableCommand>,
    /// Mode state, once the agent reports any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Modes>,
    /// Per-session settings the agent exposes — the model, the thinking level,
    /// and whatever else it chooses to offer.
    ///
    /// Not wrapped the way [`Modes`] is: a mode state splits "which one" from
    /// "which are available" into two fields, whereas a config option carries
    /// its own current value, so there is nothing to pair it with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<acp::SessionConfigOption>,
    /// Counter behind the generated entry ids.
    #[serde(skip)]
    next_id: usize,
}

impl Thread {
    /// An empty thread.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last entry, if any.
    pub fn last(&self) -> Option<&Entry> {
        self.entries.last()
    }

    /// The tool call with this id.
    pub fn tool_call(&self, id: &str) -> Option<&ToolCall> {
        self.entries
            .iter()
            .filter_map(Entry::as_tool_call)
            .find(|call| call.id == id)
    }

    /// Records a prompt the user just sent, before the agent has seen it.
    pub fn push_user_prompt(&mut self, content: Vec<acp::ContentBlock>) {
        let id = self.mint_id("user");
        self.status = ThreadStatus::Generating;
        self.stop_reason = None;
        self.entries.push(Entry::User(UserMessage {
            id,
            protocol_id: None,
            is_optimistic: true,
            content,
        }));
    }

    /// Records a question the agent has put to the user.
    ///
    /// Returns whether it could be modelled. A mode this client cannot render
    /// is refused here rather than stored, so the caller knows to decline it.
    pub fn push_elicitation(
        &mut self,
        request_id: RequestId,
        request: &acp::CreateElicitationRequest,
    ) -> bool {
        let Some(mode) = read_mode(&request.mode) else {
            return false;
        };

        let id = self.mint_id("elicitation");
        self.entries.push(Entry::Elicitation(Elicitation {
            id,
            request_id,
            message: request.message.clone(),
            tool_call_id: match request.scope() {
                acp::ElicitationScope::Session(scope) => {
                    scope.tool_call_id.as_ref().map(|id| id.0.to_string())
                }
                _ => None,
            },
            mode,
            state: ElicitationState::Pending,
            content: None,
        }));
        true
    }

    /// Records how an elicitation ended, keyed by the request it answers.
    ///
    /// The entry is settled in place rather than removed: the answer is history
    /// worth reading, and the browser renumbers a replayed thread by position,
    /// so dropping an entry would shift every id after it.
    ///
    /// Returns whether there was a pending one to settle.
    pub fn settle_elicitation(
        &mut self,
        request_id: &RequestId,
        state: ElicitationState,
        content: Option<BTreeMap<String, acp::ElicitationContentValue>>,
    ) -> bool {
        let Some(asked) = self
            .pending_elicitations()
            .find(|asked| &asked.request_id == request_id)
        else {
            return false;
        };
        asked.state = state;
        asked.content = content;
        true
    }

    /// Records that a URL-mode elicitation is finished.
    ///
    /// URL mode does not end with a response — the agent sends an
    /// `elicitation/complete` notification, which names the elicitation rather
    /// than the request that carried it.
    ///
    /// Returns whether there was a pending one with that id.
    pub fn complete_elicitation(&mut self, elicitation_id: &str) -> bool {
        let Some(asked) = self.pending_elicitations().find(|asked| {
            matches!(&asked.mode, ElicitationMode::Url { elicitation_id: id, .. } if id == elicitation_id)
        }) else {
            return false;
        };
        asked.state = ElicitationState::Accepted;
        true
    }

    /// Gives up on every question still waiting on the user.
    ///
    /// Called when a turn is abandoned. Without it a replayed thread would show
    /// a live form for a turn that ended, and the answer would have nowhere to
    /// go. Returns how many were still waiting.
    pub fn cancel_pending_elicitations(&mut self) -> usize {
        let mut cancelled = 0;
        for asked in self.pending_elicitations() {
            asked.state = ElicitationState::Cancelled;
            cancelled += 1;
        }
        cancelled
    }

    /// Every question still waiting on the user, mutably.
    fn pending_elicitations(&mut self) -> impl Iterator<Item = &mut Elicitation> {
        self.entries
            .iter_mut()
            .filter_map(|entry| match entry {
                Entry::Elicitation(asked) => Some(asked),
                _ => None,
            })
            .filter(|asked| asked.state == ElicitationState::Pending)
    }

    /// Records how a turn ended.
    pub fn finish_turn(&mut self, stop_reason: acp::StopReason) {
        self.status = ThreadStatus::Idle;
        self.stop_reason = Some(stop_reason);
    }

    pub(crate) fn mint_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }
}

/// Reads the protocol's elicitation mode into the two we can render.
///
/// `None` for anything else. The schema keeps the raw payload of an unknown
/// mode so a proxy can forward it, but this is not a proxy — it is the model
/// behind a form, and a mode we cannot draw is one we must decline.
fn read_mode(mode: &acp::ElicitationMode) -> Option<ElicitationMode> {
    match mode {
        acp::ElicitationMode::Form(form) => Some(ElicitationMode::Form {
            requested_schema: form.requested_schema.clone(),
        }),
        acp::ElicitationMode::Url(url) => Some(ElicitationMode::Url {
            elicitation_id: url.elicitation_id.0.to_string(),
            url: url.url.clone(),
        }),
        _ => None,
    }
}

/// Whether two chunks belong to the same message.
///
/// Ported verbatim from Zed's `can_merge_message_chunks`
/// (`reference/zed-acp/acp_thread/src/acp_thread.rs:378`). Agents that label
/// their messages must not have two distinct ones glued together; agents that
/// don't label anything must still get their chunks merged, or every token
/// would become its own paragraph.
pub fn can_merge_message_chunks(existing: Option<&str>, incoming: Option<&str>) -> bool {
    match (existing, incoming) {
        (Some(existing), Some(incoming)) => existing == incoming,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_merge_unless_two_different_ids_say_otherwise() {
        // Both labelled and equal: same message.
        assert!(can_merge_message_chunks(Some("m1"), Some("m1")));
        // Both labelled and different: two messages.
        assert!(!can_merge_message_chunks(Some("m1"), Some("m2")));
        // Either unlabelled: nothing contradicts merging, and most agents
        // never label at all.
        assert!(can_merge_message_chunks(None, Some("m1")));
        assert!(can_merge_message_chunks(Some("m1"), None));
        assert!(can_merge_message_chunks(None, None));
    }

    #[test]
    fn an_optimistic_prompt_starts_a_turn() {
        let mut thread = Thread::new();
        thread.push_user_prompt(vec![acp::ContentBlock::from("hello")]);

        assert_eq!(thread.status, ThreadStatus::Generating);
        assert!(thread.stop_reason.is_none());
        let Some(Entry::User(message)) = thread.last() else {
            panic!("expected a user message");
        };
        assert!(message.is_optimistic);
    }

    #[test]
    fn entry_ids_are_unique() {
        let mut thread = Thread::new();
        thread.push_user_prompt(vec![]);
        thread.push_user_prompt(vec![]);
        assert_ne!(thread.entries[0].id(), thread.entries[1].id());
    }

    /// Elicitation requests are built from wire JSON rather than with the
    /// schema's builders, so the tests check our reading of the protocol rather
    /// than a serializer against itself.
    fn elicitation(params: serde_json::Value) -> acp::CreateElicitationRequest {
        serde_json::from_value(params).expect("the fixture is not a valid elicitation")
    }

    fn form(tool_call_id: Option<&str>) -> acp::CreateElicitationRequest {
        elicitation(serde_json::json!({
            "mode": "form",
            "sessionId": "s1",
            "toolCallId": tool_call_id,
            "message": "Which branch should I push to?",
            "requestedSchema": {
                "type": "object",
                "properties": { "branch": { "type": "string", "title": "Branch" } },
                "required": ["branch"]
            }
        }))
    }

    fn url() -> acp::CreateElicitationRequest {
        elicitation(serde_json::json!({
            "mode": "url",
            "sessionId": "s1",
            "elicitationId": "el-1",
            "url": "https://example.test/authorize",
            "message": "Authorize, then come back."
        }))
    }

    fn only_elicitation(thread: &Thread) -> &Elicitation {
        thread
            .entries
            .iter()
            .filter_map(Entry::as_elicitation)
            .next()
            .expect("expected an elicitation entry")
    }

    #[test]
    fn an_elicitation_becomes_a_timeline_entry() {
        let mut thread = Thread::new();
        assert!(thread.push_elicitation(RequestId::Number(7), &form(Some("call_edit"))));

        let asked = only_elicitation(&thread);
        assert_eq!(asked.request_id, RequestId::Number(7));
        assert_eq!(asked.message, "Which branch should I push to?");
        assert_eq!(asked.tool_call_id.as_deref(), Some("call_edit"));
        assert_eq!(asked.state, ElicitationState::Pending);
        assert!(matches!(asked.mode, ElicitationMode::Form { .. }));
    }

    #[test]
    fn a_mode_we_cannot_render_is_refused_rather_than_stored() {
        // The protocol reserves unknown modes for future variants, and says a
        // client must not render one as if it understood it. Refusing here is
        // what lets the browser answer `decline` instead of hanging.
        let mut thread = Thread::new();
        let exotic = elicitation(serde_json::json!({
            "mode": "_vendor/hologram",
            "sessionId": "s1",
            "message": "step into the booth"
        }));

        assert!(!thread.push_elicitation(RequestId::Number(1), &exotic));
        assert!(thread.entries.is_empty());
    }

    #[test]
    fn answering_settles_the_entry_in_place() {
        // Settling rather than removing is deliberate: the answer is history the
        // user should still be able to read, and a stable index keeps the ids
        // the browser renumbers a replay by from shifting underneath it.
        let mut thread = Thread::new();
        thread.push_elicitation(RequestId::Number(7), &form(None));
        let before = thread.entries.len();

        let content = BTreeMap::from([("branch".to_string(), "main".into())]);
        assert!(thread.settle_elicitation(
            &RequestId::Number(7),
            ElicitationState::Accepted,
            Some(content),
        ));

        assert_eq!(thread.entries.len(), before);
        let asked = only_elicitation(&thread);
        assert_eq!(asked.state, ElicitationState::Accepted);
        assert_eq!(
            asked.content.as_ref().and_then(|c| c.get("branch")),
            Some(&acp::ElicitationContentValue::String("main".into()))
        );
    }

    #[test]
    fn a_url_elicitation_is_settled_by_its_own_id() {
        // URL mode does not end with a response: the agent says it is done with
        // an `elicitation/complete` notification, which names the elicitation
        // rather than the request.
        let mut thread = Thread::new();
        thread.push_elicitation(RequestId::Number(3), &url());

        assert!(thread.complete_elicitation("el-1"));
        assert_eq!(only_elicitation(&thread).state, ElicitationState::Accepted);
    }

    #[test]
    fn settling_something_we_never_asked_changes_nothing() {
        let mut thread = Thread::new();
        thread.push_elicitation(RequestId::Number(7), &form(None));

        assert!(!thread.settle_elicitation(
            &RequestId::Number(99),
            ElicitationState::Declined,
            None
        ));
        assert!(!thread.complete_elicitation("nobody"));
        assert_eq!(only_elicitation(&thread).state, ElicitationState::Pending);
    }

    #[test]
    fn cancelling_only_touches_the_ones_still_waiting() {
        // An abandoned turn must not leave a live form in a replayed thread,
        // but an answer already given is not undone by the turn ending.
        let mut thread = Thread::new();
        thread.push_elicitation(RequestId::Number(1), &form(None));
        thread.push_elicitation(RequestId::Number(2), &form(None));
        thread.settle_elicitation(&RequestId::Number(1), ElicitationState::Accepted, None);

        assert_eq!(thread.cancel_pending_elicitations(), 1);

        let states: Vec<_> = thread
            .entries
            .iter()
            .filter_map(Entry::as_elicitation)
            .map(|asked| asked.state)
            .collect();
        assert_eq!(
            states,
            vec![ElicitationState::Accepted, ElicitationState::Cancelled]
        );
    }

    #[test]
    fn an_elicitation_survives_the_wire() {
        // This is what `_mjx/session/replay` carries, so the browser has to be
        // able to read back everything it needs to re-render the form.
        let mut thread = Thread::new();
        thread.push_elicitation(RequestId::String("a".into()), &form(Some("call_edit")));

        let json = serde_json::to_value(&thread).unwrap();
        assert_eq!(json["entries"][0]["type"], "elicitation");
        assert_eq!(json["entries"][0]["requestId"], "a");
        assert_eq!(json["entries"][0]["mode"], "form");
        assert_eq!(json["entries"][0]["state"], "pending");
        assert_eq!(json["entries"][0]["toolCallId"], "call_edit");
        assert!(json["entries"][0]["requestedSchema"]["properties"]["branch"].is_object());

        let back: Thread = serde_json::from_value(json).unwrap();
        assert_eq!(
            only_elicitation(&back).request_id,
            RequestId::String("a".into())
        );
    }
}
