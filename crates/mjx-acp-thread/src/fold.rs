//! Folding `session/update` into the thread.
//!
//! Port of `AcpThread::handle_session_update` and the `upsert_tool_call` /
//! `update_tool_call` pair it delegates to
//! (`reference/zed-acp/acp_thread/src/acp_thread.rs:2544, 3185, 3110`).

use mjx_acp_core::acp;

use crate::{
    AssistantChunk, AssistantMessage, ChunkKind, Cost, Entry, Modes, Thread, ToolCall, Usage,
    can_merge_message_chunks,
};

/// What changed, so a caller can push a targeted update rather than the whole
/// thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadEvent {
    /// An entry was appended.
    EntryAdded(usize),
    /// An entry changed in place.
    EntryUpdated(usize),
    /// The plan changed.
    PlanUpdated,
    /// Slash commands changed.
    CommandsUpdated,
    /// The active mode changed.
    ModeUpdated,
    /// The session config options changed.
    ConfigOptionsUpdated,
    /// Token accounting changed.
    UsageUpdated,
}

impl Thread {
    /// Applies one update, returning what changed.
    ///
    /// An update this doesn't model is ignored rather than rejected: the
    /// protocol grows, and a viewer that fails on an unfamiliar variant is a
    /// viewer that breaks against every newer agent.
    pub fn apply(&mut self, update: &acp::SessionUpdate) -> Vec<ThreadEvent> {
        match update {
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                self.push_user_chunk(chunk.content.clone(), message_id(chunk))
            }
            acp::SessionUpdate::AgentMessageChunk(chunk) => self.push_assistant_chunk(
                ChunkKind::Message,
                chunk.content.clone(),
                message_id(chunk),
            ),
            acp::SessionUpdate::AgentThoughtChunk(chunk) => self.push_assistant_chunk(
                ChunkKind::Thought,
                chunk.content.clone(),
                message_id(chunk),
            ),
            acp::SessionUpdate::ToolCall(call) => self.upsert_tool_call(call),
            acp::SessionUpdate::ToolCallUpdate(update) => self.update_tool_call(update),

            acp::SessionUpdate::Plan(plan) => {
                self.plan = plan.entries.clone();
                vec![ThreadEvent::PlanUpdated]
            }
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                self.available_commands = update.available_commands.clone();
                vec![ThreadEvent::CommandsUpdated]
            }
            acp::SessionUpdate::CurrentModeUpdate(update) => {
                // A mode id names one of the modes `session/new` returned. With
                // no such list there is nothing to select, so this is dropped
                // rather than inventing a mode that isn't offered.
                match &mut self.modes {
                    Some(modes) => {
                        modes.current_mode_id = update.current_mode_id.0.to_string();
                        vec![ThreadEvent::ModeUpdated]
                    }
                    None => vec![],
                }
            }
            acp::SessionUpdate::ConfigOptionUpdate(update) => {
                // Deliberately not the shape of the mode arm above. That one
                // carries an id naming something `session/new` offered, so with
                // no list it is meaningless; this carries the whole set, so it
                // is applied even when nothing was advertised at `session/new`
                // — an agent may start offering options mid-session.
                self.config_options = update.config_options.clone();
                vec![ThreadEvent::ConfigOptionsUpdated]
            }
            acp::SessionUpdate::UsageUpdate(update) => {
                self.usage = Some(Usage {
                    used: update.used,
                    size: update.size,
                    cost: update.cost.as_ref().map(|cost| Cost {
                        amount: cost.amount,
                        currency: cost.currency.clone(),
                    }),
                });
                vec![ThreadEvent::UsageUpdated]
            }
            _ => vec![],
        }
    }

    /// Records the modes reported by `session/new` or `session/load`.
    pub fn set_modes(&mut self, state: &acp::SessionModeState) {
        self.modes = Some(Modes {
            current_mode_id: state.current_mode_id.0.to_string(),
            available_modes: state.available_modes.clone(),
        });
    }

    /// Records the config options reported by `session/new` or `session/load`.
    pub fn set_config_options(&mut self, options: &[acp::SessionConfigOption]) {
        self.config_options = options.to_vec();
    }

    /// Folds in a user chunk echoed back by the agent.
    ///
    /// The client already showed the prompt optimistically, so an echo that
    /// matches it is absorbed rather than appended — otherwise the user's own
    /// message appears twice. Ported from Zed's `handle_session_update`
    /// (`acp_thread.rs:2551`).
    fn push_user_chunk(
        &mut self,
        content: acp::ContentBlock,
        message_id: Option<String>,
    ) -> Vec<ThreadEvent> {
        if let Some(Entry::User(message)) = self.entries.last_mut()
            && message.is_optimistic
            && message.content.iter().any(|block| same_block(block, &content))
            && can_merge_message_chunks(message.protocol_id.as_deref(), message_id.as_deref())
        {
            if message.protocol_id.is_none() {
                message.protocol_id = message_id;
            }
            return vec![];
        }

        // A chunk that isn't an echo extends the current user message, or
        // starts one.
        if let Some(Entry::User(message)) = self.entries.last_mut()
            && !message.is_optimistic
            && can_merge_message_chunks(message.protocol_id.as_deref(), message_id.as_deref())
        {
            message.content.push(content);
            return vec![ThreadEvent::EntryUpdated(self.entries.len() - 1)];
        }

        let id = self.mint_id("user");
        self.entries.push(Entry::User(crate::UserMessage {
            id,
            protocol_id: message_id,
            is_optimistic: false,
            content: vec![content],
        }));
        vec![ThreadEvent::EntryAdded(self.entries.len() - 1)]
    }

    /// Folds in a chunk of agent prose or thinking.
    ///
    /// Port of `push_assistant_content_block_with_message_id`
    /// (`acp_thread.rs:2765`), minus the streaming text buffer.
    fn push_assistant_chunk(
        &mut self,
        kind: ChunkKind,
        content: acp::ContentBlock,
        message_id: Option<String>,
    ) -> Vec<ThreadEvent> {
        let index = self.entries.len().saturating_sub(1);
        if let Some(Entry::Assistant(message)) = self.entries.last_mut() {
            if let Some(chunk) = message.chunks.last_mut()
                && chunk.kind == kind
                && can_merge_message_chunks(chunk.id.as_deref(), message_id.as_deref())
            {
                if chunk.id.is_none() {
                    chunk.id = message_id;
                }
                append_content(&mut chunk.content, content);
                return vec![ThreadEvent::EntryUpdated(index)];
            }

            // Same entry, new run: the kind changed, or the agent started a
            // differently-labelled message.
            message.chunks.push(AssistantChunk {
                kind,
                id: message_id,
                content: vec![content],
            });
            return vec![ThreadEvent::EntryUpdated(index)];
        }

        let id = self.mint_id("assistant");
        self.entries.push(Entry::Assistant(AssistantMessage {
            id,
            chunks: vec![AssistantChunk {
                kind,
                id: message_id,
                content: vec![content],
            }],
        }));
        vec![ThreadEvent::EntryAdded(self.entries.len() - 1)]
    }

    /// Inserts a tool call, or replaces one we have already seen.
    ///
    /// Port of `upsert_tool_call` (`acp_thread.rs:3185`). Upsert rather than
    /// append because agents re-announce calls, and because a permission
    /// request can introduce an id before the call describing it arrives.
    fn upsert_tool_call(&mut self, call: &acp::ToolCall) -> Vec<ThreadEvent> {
        let incoming = ToolCall {
            id: call.tool_call_id.0.to_string(),
            title: call.title.clone(),
            kind: call.kind,
            status: call.status,
            content: call.content.clone(),
            locations: call.locations.clone(),
            raw_input: call.raw_input.clone(),
            raw_output: call.raw_output.clone(),
        };

        match self.index_of_tool_call(&incoming.id) {
            Some(index) => {
                self.entries[index] = Entry::ToolCall(incoming);
                vec![ThreadEvent::EntryUpdated(index)]
            }
            None => {
                self.entries.push(Entry::ToolCall(incoming));
                vec![ThreadEvent::EntryAdded(self.entries.len() - 1)]
            }
        }
    }

    /// Applies a partial update to a tool call.
    ///
    /// Port of `update_tool_call` (`acp_thread.rs:3110`). Only the fields
    /// actually present are touched: an update carrying just a status must not
    /// blank out content that arrived earlier.
    fn update_tool_call(&mut self, update: &acp::ToolCallUpdate) -> Vec<ThreadEvent> {
        let id = update.tool_call_id.0.to_string();

        let Some(index) = self.index_of_tool_call(&id) else {
            // An update for a call we never saw. Keep it: a browser that
            // reconnects mid-turn should still see the tool running, and
            // dropping it would leave a gap that never fills.
            self.entries.push(Entry::ToolCall(ToolCall {
                id,
                title: update.fields.title.clone().unwrap_or_default(),
                kind: update.fields.kind.unwrap_or_default(),
                status: update.fields.status.unwrap_or_default(),
                content: update.fields.content.clone().unwrap_or_default(),
                locations: update.fields.locations.clone().unwrap_or_default(),
                raw_input: update.fields.raw_input.clone(),
                raw_output: update.fields.raw_output.clone(),
            }));
            return vec![ThreadEvent::EntryAdded(self.entries.len() - 1)];
        };

        let Entry::ToolCall(call) = &mut self.entries[index] else {
            unreachable!("index_of_tool_call only returns tool call entries");
        };

        if let Some(title) = &update.fields.title {
            call.title = title.clone();
        }
        if let Some(kind) = update.fields.kind {
            call.kind = kind;
        }
        if let Some(status) = update.fields.status {
            call.status = status;
        }
        if let Some(content) = &update.fields.content {
            call.content = content.clone();
        }
        if let Some(locations) = &update.fields.locations {
            call.locations = locations.clone();
        }
        if update.fields.raw_input.is_some() {
            call.raw_input = update.fields.raw_input.clone();
        }
        if update.fields.raw_output.is_some() {
            call.raw_output = update.fields.raw_output.clone();
        }

        vec![ThreadEvent::EntryUpdated(index)]
    }

    fn index_of_tool_call(&self, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.as_tool_call().is_some_and(|call| call.id == id))
    }
}

/// Whether two blocks are the same block, for spotting an echoed prompt.
///
/// Compared by what identifies each kind, not by `PartialEq` over every field.
/// An agent is free to add a `mime_type`, a `title` or a `size` when it echoes
/// a link back — `ResourceLink` requires only `name` and `uri` and carries
/// seven optional fields — and a prompt shown twice is worse than one shown
/// with the agent's extra fields. Mirrored by `sameBlock` in
/// `web/src/acp/thread.ts`.
fn same_block(a: &acp::ContentBlock, b: &acp::ContentBlock) -> bool {
    use acp::ContentBlock as Block;
    match (a, b) {
        (Block::Text(a), Block::Text(b)) => a.text == b.text,
        // What it points at is what it is.
        (Block::ResourceLink(a), Block::ResourceLink(b)) => a.uri == b.uri,
        (Block::Image(a), Block::Image(b)) => a.data == b.data && a.mime_type == b.mime_type,
        (Block::Audio(a), Block::Audio(b)) => a.data == b.data && a.mime_type == b.mime_type,
        (Block::Resource(a), Block::Resource(b)) => match (resource_uri(a), resource_uri(b)) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        },
        _ => a == b,
    }
}

fn resource_uri(resource: &acp::EmbeddedResource) -> Option<&str> {
    match &resource.resource {
        acp::EmbeddedResourceResource::TextResourceContents(text) => Some(&text.uri),
        acp::EmbeddedResourceResource::BlobResourceContents(blob) => Some(&blob.uri),
        // The schema is `non_exhaustive`; a kind we have never seen has no uri
        // we can compare, so it is only ever equal to itself field for field.
        _ => None,
    }
}

/// Appends a block, merging it into the previous one when both are text.
///
/// Agents emit a chunk per token; without this a sentence becomes hundreds of
/// blocks. Non-text blocks are kept whole, so an image in the middle of a
/// message survives.
fn append_content(content: &mut Vec<acp::ContentBlock>, incoming: acp::ContentBlock) {
    if let (Some(acp::ContentBlock::Text(existing)), acp::ContentBlock::Text(incoming)) =
        (content.last_mut(), &incoming)
    {
        existing.text.push_str(&incoming.text);
        return;
    }
    content.push(incoming);
}

/// The `messageId` of a content chunk, when the agent labelled it.
fn message_id(chunk: &acp::ContentChunk) -> Option<String> {
    chunk.message_id.as_ref().map(|id| id.0.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThreadStatus;

    /// Folds a sequence of updates, given as the JSON an agent would send.
    fn fold(updates: &[serde_json::Value]) -> Thread {
        let mut thread = Thread::new();
        for update in updates {
            let update: acp::SessionUpdate = serde_json::from_value(update.clone())
                .unwrap_or_else(|e| panic!("invalid update: {e}\n{update:#}"));
            thread.apply(&update);
        }
        thread
    }

    fn text_chunk(kind: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "sessionUpdate": kind,
            "content": { "type": "text", "text": text }
        })
    }

    fn assistant_chunks(thread: &Thread) -> &[AssistantChunk] {
        match thread.entries.first() {
            Some(Entry::Assistant(message)) => &message.chunks,
            other => panic!("expected an assistant message, got {other:?}"),
        }
    }

    #[test]
    fn consecutive_chunks_of_one_kind_merge() {
        let thread = fold(&[
            text_chunk("agent_message_chunk", "Hello "),
            text_chunk("agent_message_chunk", "there, "),
            text_chunk("agent_message_chunk", "world."),
        ]);

        assert_eq!(thread.entries.len(), 1);
        let chunks = assistant_chunks(&thread);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text(), "Hello there, world.");
    }

    #[test]
    fn thinking_and_prose_stay_separate() {
        let thread = fold(&[
            text_chunk("agent_thought_chunk", "Let me check. "),
            text_chunk("agent_thought_chunk", "It's here."),
            text_chunk("agent_message_chunk", "Found it."),
        ]);

        let chunks = assistant_chunks(&thread);
        assert_eq!(
            chunks.iter().map(|c| c.kind).collect::<Vec<_>>(),
            [ChunkKind::Thought, ChunkKind::Message]
        );
        assert_eq!(chunks[0].text(), "Let me check. It's here.");
    }

    #[test]
    fn differently_labelled_messages_do_not_merge() {
        // Two distinct messages must not be glued into one paragraph.
        let thread = fold(&[
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "m1",
                "content": { "type": "text", "text": "First." }
            }),
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "m2",
                "content": { "type": "text", "text": "Second." }
            }),
        ]);

        let chunks = assistant_chunks(&thread);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text(), "First.");
        assert_eq!(chunks[1].text(), "Second.");
    }

    #[test]
    fn an_unlabelled_chunk_joins_a_labelled_one() {
        // Most agents label nothing; an occasional label must not fragment the
        // message.
        let thread = fold(&[
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk", "messageId": "m1",
                "content": { "type": "text", "text": "one " }
            }),
            text_chunk("agent_message_chunk", "two"),
        ]);

        let chunks = assistant_chunks(&thread);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text(), "one two");
        assert_eq!(chunks[0].id.as_deref(), Some("m1"));
    }

    #[test]
    fn non_text_content_survives_between_text() {
        let thread = fold(&[
            text_chunk("agent_message_chunk", "see "),
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "image", "data": "AAAA", "mimeType": "image/png" }
            }),
            text_chunk("agent_message_chunk", "this"),
        ]);

        let chunks = assistant_chunks(&thread);
        assert_eq!(chunks.len(), 1, "an image must not split the message");
        assert_eq!(chunks[0].content.len(), 3, "the image must not be dropped");
        assert_eq!(chunks[0].text(), "see this");
    }

    #[test]
    fn an_echoed_prompt_is_absorbed_rather_than_duplicated() {
        // The client shows the prompt immediately; agents that echo it back
        // must not make it appear twice.
        let mut thread = Thread::new();
        thread.push_user_prompt(vec![acp::ContentBlock::from("fix the bug")]);

        let echo: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": "fix the bug" }
        }))
        .unwrap();
        let events = thread.apply(&echo);

        assert_eq!(thread.entries.len(), 1, "the prompt was duplicated");
        assert!(events.is_empty(), "an absorbed echo changes nothing");
    }

    #[test]
    fn an_echoed_resource_link_the_agent_annotated_is_still_absorbed() {
        // A ResourceLink needs only a name and a uri, and carries seven
        // optional fields an agent is free to fill in on the way back. What it
        // points at is what identifies it; comparing every field would show
        // the user's own prompt twice.
        let mut thread = Thread::new();
        thread.push_user_prompt(
            serde_json::from_value(serde_json::json!([
                { "type": "resource_link", "uri": "file:///w/stats.js", "name": "stats.js" }
            ]))
            .unwrap(),
        );

        let echo: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": {
                "type": "resource_link",
                "uri": "file:///w/stats.js",
                "name": "stats.js",
                "mimeType": "text/javascript",
                "size": 420
            }
        }))
        .unwrap();
        let events = thread.apply(&echo);

        assert_eq!(thread.entries.len(), 1, "the prompt was duplicated");
        assert!(events.is_empty(), "an absorbed echo changes nothing");
    }

    #[test]
    fn a_link_to_somewhere_else_is_not_an_echo() {
        let mut thread = Thread::new();
        thread.push_user_prompt(
            serde_json::from_value(serde_json::json!([
                { "type": "resource_link", "uri": "file:///w/stats.js", "name": "stats.js" }
            ]))
            .unwrap(),
        );

        let other: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": {
                "type": "resource_link", "uri": "file:///w/other.js", "name": "stats.js"
            }
        }))
        .unwrap();
        thread.apply(&other);

        assert_eq!(thread.entries.len(), 2);
    }

    #[test]
    fn a_user_chunk_that_is_not_an_echo_is_kept() {
        let mut thread = Thread::new();
        thread.push_user_prompt(vec![acp::ContentBlock::from("first")]);

        let other: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": "something else entirely" }
        }))
        .unwrap();
        thread.apply(&other);

        assert_eq!(thread.entries.len(), 2);
    }

    #[test]
    fn tool_call_updates_revise_in_place() {
        let thread = fold(&[
            serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "t1",
                "title": "Read stats.js", "kind": "read", "status": "pending",
                "locations": [{ "path": "/w/stats.js" }]
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }]
            }),
        ]);

        assert_eq!(thread.entries.len(), 1);
        let call = thread.tool_call("t1").unwrap();
        assert_eq!(call.status, acp::ToolCallStatus::Completed);
        assert_eq!(call.content.len(), 1);
        // Fields the update never mentioned survive.
        assert_eq!(call.title, "Read stats.js");
        assert_eq!(call.locations.len(), 1);
    }

    #[test]
    fn a_partial_update_does_not_blank_earlier_fields() {
        let thread = fold(&[
            serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "t1",
                "title": "Edit", "kind": "edit",
                "content": [{ "type": "diff", "path": "/w/a.js", "oldText": "a", "newText": "b" }]
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"
            }),
        ]);

        let call = thread.tool_call("t1").unwrap();
        assert_eq!(call.content.len(), 1, "the diff was lost");
        assert_eq!(call.kind, acp::ToolKind::Edit);
    }

    #[test]
    fn an_update_for_an_unseen_call_is_kept() {
        let thread = fold(&[serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "orphan", "status": "in_progress"
        })]);

        assert_eq!(thread.tool_call("orphan").unwrap().status, acp::ToolCallStatus::InProgress);
    }

    #[test]
    fn re_announcing_a_call_does_not_duplicate_it() {
        let thread = fold(&[
            serde_json::json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "First" }),
            serde_json::json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Second" }),
        ]);

        assert_eq!(thread.entries.len(), 1);
        assert_eq!(thread.tool_call("t1").unwrap().title, "Second");
    }

    #[test]
    fn a_tool_call_between_chunks_starts_a_new_assistant_entry() {
        let thread = fold(&[
            text_chunk("agent_message_chunk", "Reading. "),
            serde_json::json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read" }),
            text_chunk("agent_message_chunk", "Done."),
        ]);

        assert_eq!(thread.entries.len(), 3);
        assert!(matches!(thread.entries[2], Entry::Assistant(_)));
    }

    #[test]
    fn plan_commands_and_usage_are_recorded() {
        let thread = fold(&[
            serde_json::json!({
                "sessionUpdate": "plan",
                "entries": [{ "content": "Read", "priority": "high", "status": "completed" }]
            }),
            serde_json::json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{ "name": "test", "description": "Run tests" }]
            }),
            serde_json::json!({
                "sessionUpdate": "usage_update", "used": 4812, "size": 200000,
                "cost": { "amount": 0.0143, "currency": "USD" }
            }),
        ]);

        assert_eq!(thread.plan.len(), 1);
        assert_eq!(thread.available_commands[0].name, "test");
        let usage = thread.usage.unwrap();
        assert_eq!(usage.used, 4812);
        assert_eq!(usage.cost.unwrap().currency, "USD");
    }

    #[test]
    fn a_mode_update_needs_modes_to_have_been_announced() {
        let update: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "current_mode_update", "currentModeId": "ask"
        }))
        .unwrap();

        let mut without = Thread::new();
        assert!(without.apply(&update).is_empty());
        assert!(without.modes.is_none());

        let mut with = Thread::new();
        with.set_modes(
            &serde_json::from_value(serde_json::json!({
                "currentModeId": "code",
                "availableModes": [{ "id": "ask", "name": "Ask" }]
            }))
            .unwrap(),
        );
        assert_eq!(with.apply(&update), [ThreadEvent::ModeUpdated]);
        assert_eq!(with.modes.unwrap().current_mode_id, "ask");
    }

    /// The mirror image of the mode rule above, and the reason the two arms are
    /// not written the same way: this update carries the whole set, so there is
    /// nothing for it to refer back to.
    #[test]
    fn a_config_option_update_stands_on_its_own() {
        let update: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [{
                "id": "model", "name": "Model", "category": "model",
                "type": "select", "currentValue": "sonnet",
                "options": [
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "opus", "name": "Opus" }
                ]
            }]
        }))
        .unwrap();

        let mut thread = Thread::new();
        assert_eq!(thread.apply(&update), [ThreadEvent::ConfigOptionsUpdated]);
        assert_eq!(thread.config_options.len(), 1);
        assert_eq!(thread.config_options[0].name, "Model");
    }

    #[test]
    fn a_config_option_update_replaces_rather_than_merges() {
        let mut thread = Thread::new();
        thread.set_config_options(&[
            serde_json::from_value(serde_json::json!({
                "id": "model", "name": "Model", "type": "select",
                "currentValue": "sonnet", "options": [{ "value": "sonnet", "name": "Sonnet" }]
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "id": "web", "name": "Web search", "type": "boolean", "currentValue": true
            }))
            .unwrap(),
        ]);

        let update: acp::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [{
                "id": "model", "name": "Model", "type": "select",
                "currentValue": "opus", "options": [{ "value": "opus", "name": "Opus" }]
            }]
        }))
        .unwrap();

        thread.apply(&update);
        // The dropped boolean is not a bug: an agent that stops offering an
        // option says so by leaving it out of the set it sends.
        assert_eq!(thread.config_options.len(), 1);
        assert!(matches!(
            &thread.config_options[0].kind,
            acp::SessionConfigKind::Select(select) if select.current_value.0.as_ref() == "opus"
        ));
    }

    /// Categories are UX only and the spec reserves the right to add more, so
    /// an unfamiliar one has to survive rather than cost us the option.
    #[test]
    fn an_unknown_category_survives() {
        let option: acp::SessionConfigOption = serde_json::from_value(serde_json::json!({
            "id": "vibe", "name": "Vibe", "category": "_vendor_vibe",
            "type": "boolean", "currentValue": false
        }))
        .unwrap();

        let mut thread = Thread::new();
        thread.set_config_options(std::slice::from_ref(&option));
        let round_tripped: acp::SessionConfigOption =
            serde_json::from_str(&serde_json::to_string(&thread.config_options[0]).unwrap())
                .unwrap();
        assert_eq!(round_tripped.category, option.category);
    }

    #[test]
    fn finishing_a_turn_records_how_it_ended() {
        let mut thread = Thread::new();
        thread.push_user_prompt(vec![]);
        assert_eq!(thread.status, ThreadStatus::Generating);

        thread.finish_turn(acp::StopReason::Cancelled);
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.stop_reason, Some(acp::StopReason::Cancelled));
    }

    #[test]
    fn events_point_at_the_entry_that_changed() {
        let mut thread = Thread::new();
        let first: acp::SessionUpdate =
            serde_json::from_value(text_chunk("agent_message_chunk", "a")).unwrap();

        assert_eq!(thread.apply(&first), [ThreadEvent::EntryAdded(0)]);
        assert_eq!(thread.apply(&first), [ThreadEvent::EntryUpdated(0)]);
    }
}
