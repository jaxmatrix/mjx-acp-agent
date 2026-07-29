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

use mjx_acp_core::acp;
use serde::{Deserialize, Serialize};

mod fold;

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
}

impl Entry {
    /// The tool call this entry holds, if it is one.
    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    /// The entry's stable id.
    pub fn id(&self) -> &str {
        match self {
            Self::User(message) => &message.id,
            Self::Assistant(message) => &message.id,
            Self::ToolCall(call) => &call.id,
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
}
