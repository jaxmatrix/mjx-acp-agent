/**
 * Folds the `session/update` stream into a renderable thread.
 *
 * A pure reducer, deliberately: it is the piece most likely to be wrong in
 * subtle ways, and the only way to be sure it matches the server's Rust model
 * is to run both over the same recorded frames.
 *
 * Ported from Zed's `AcpThread::handle_session_update`
 * (`reference/zed-acp/acp_thread/src/acp_thread.rs`), which is the single funnel
 * every streaming update passes through there too.
 */

import type { ContentBlock, SessionNotification } from "@agentclientprotocol/sdk";
import type {
  AssistantChunk,
  Elicitation,
  ElicitationState,
  ElicitationValue,
  Entry,
  PermissionRequest,
  SessionConfigOption,
  Thread,
  ToolCall,
  ToolCallContent,
} from "./types";

/** Applies one `session/update` to `thread`, returning a new thread. */
export function applyUpdate(thread: Thread, notification: SessionNotification): Thread {
  const update = notification.update as Record<string, unknown> & {
    sessionUpdate: string;
  };

  const messageId = update.messageId as string | undefined;

  switch (update.sessionUpdate) {
    case "user_message_chunk":
      return appendUserContent(thread, update.content as ContentBlock, messageId);

    case "agent_message_chunk":
      return appendAssistantChunk(thread, "message", update.content as ContentBlock, messageId);

    case "agent_thought_chunk":
      return appendAssistantChunk(thread, "thought", update.content as ContentBlock, messageId);

    case "tool_call":
      return upsertToolCall(thread, toToolCall(update));

    case "tool_call_update":
      return updateToolCall(thread, update);

    case "plan":
      return { ...thread, plan: (update.entries as Thread["plan"]) ?? [] };

    case "available_commands_update":
      return {
        ...thread,
        availableCommands: (update.availableCommands as Thread["availableCommands"]) ?? [],
      };

    case "current_mode_update":
      return thread.modes
        ? {
            ...thread,
            modes: { ...thread.modes, currentModeId: update.currentModeId as string },
          }
        : thread;

    // Deliberately not shaped like the mode arm above. That one carries an id
    // naming something `session/new` offered, so with no list it is meaningless;
    // this carries the whole set, so it applies even when nothing was
    // advertised — an agent may start offering options mid-session.
    case "config_option_update":
      return {
        ...thread,
        configOptions: (update.configOptions as SessionConfigOption[]) ?? [],
      };

    case "usage_update":
      return {
        ...thread,
        usage: {
          used: (update.used as number) ?? 0,
          size: (update.size as number) ?? 0,
          cost: update.cost as Thread["usage"] extends undefined ? never : never,
        },
      };

    default:
      // An update we don't model is not an error: the protocol grows, and a
      // viewer that throws on an unfamiliar variant is a viewer that breaks
      // against every newer agent.
      return thread;
  }
}

/**
 * Whether two chunks belong to the same message.
 *
 * Ported from Zed's `can_merge_message_chunks`
 * (`reference/zed-acp/acp_thread/src/acp_thread.rs:378`) and mirrored by
 * `mjx-acp-thread`. Agents that label their messages must not have two
 * distinct ones glued together; agents that label nothing — most of them —
 * must still get their chunks merged, or every token becomes a paragraph.
 */
export function canMergeMessageChunks(existing?: string, incoming?: string): boolean {
  if (existing != null && incoming != null) return existing === incoming;
  return true;
}

/**
 * Appends a block, merging it into the previous one when both are text.
 *
 * Non-text blocks are kept whole, so an image mid-message survives.
 */
function appendContent(content: ContentBlock[], incoming: ContentBlock): ContentBlock[] {
  const last = content.at(-1);
  if (last?.type === "text" && incoming.type === "text") {
    return [...content.slice(0, -1), { ...last, text: last.text + incoming.text }];
  }
  return [...content, incoming];
}

/**
 * Folds in a user chunk echoed back by the agent.
 *
 * The prompt is already on screen, added optimistically when it was sent, so
 * an echo that matches it is absorbed rather than appended.
 */
function appendUserContent(
  thread: Thread,
  content: ContentBlock,
  messageId?: string,
): Thread {
  const last = thread.entries.at(-1);

  if (last?.type === "user") {
    const isEcho =
      last.isOptimistic === true &&
      last.content.some((block) => sameBlock(block, content)) &&
      canMergeMessageChunks(last.protocolId, messageId);

    if (isEcho) {
      // Adopt the agent's id for the message, but change nothing else.
      if (last.protocolId == null && messageId != null) {
        const entries = thread.entries.slice(0, -1);
        entries.push({ ...last, protocolId: messageId });
        return { ...thread, entries };
      }
      return thread;
    }

    if (!last.isOptimistic && canMergeMessageChunks(last.protocolId, messageId)) {
      const entries = thread.entries.slice(0, -1);
      entries.push({ ...last, content: [...last.content, content] });
      return { ...thread, entries };
    }
  }

  return {
    ...thread,
    entries: [
      ...thread.entries,
      {
        type: "user",
        id: `user-${thread.entries.length}`,
        content: [content],
        isOptimistic: false,
        protocolId: messageId,
      },
    ],
  };
}

/**
 * Whether two blocks are the same block, for spotting an echoed prompt.
 *
 * Compared by what identifies each kind, not by serialisation. An agent is
 * free to add a `mimeType`, a `title` or a `size` when it echoes a link back —
 * `resource_link` requires only `name` and `uri` and carries seven optional
 * fields — and a prompt shown twice is worse than one shown with the agent's
 * extra fields. Mirrored by `same_block` in `crates/mjx-acp-thread/src/fold.rs`.
 */
function sameBlock(a: ContentBlock, b: ContentBlock): boolean {
  if (a.type !== b.type) return false;
  switch (a.type) {
    case "text":
      return a.text === (b as typeof a).text;
    case "resource_link":
      // What it points at is what it is.
      return a.uri === (b as typeof a).uri;
    case "image":
    case "audio":
      return a.data === (b as typeof a).data && a.mimeType === (b as typeof a).mimeType;
    case "resource":
      return a.resource.uri === (b as typeof a).resource.uri;
    default:
      return JSON.stringify(a) === JSON.stringify(b);
  }
}

/**
 * Appends a streamed chunk, merging into the previous run when it is the same
 * kind and the message ids are compatible.
 *
 * Agents emit a chunk per token, so without merging a single sentence would
 * become hundreds of entries. Prose and thinking stay separate, because they
 * render differently.
 */
function appendAssistantChunk(
  thread: Thread,
  kind: AssistantChunk["kind"],
  content: ContentBlock,
  messageId?: string,
): Thread {
  const last = thread.entries.at(-1);

  if (last?.type === "assistant") {
    const chunks = [...last.chunks];
    const lastChunk = chunks.at(-1);

    if (lastChunk && lastChunk.kind === kind && canMergeMessageChunks(lastChunk.id, messageId)) {
      chunks[chunks.length - 1] = {
        kind,
        id: lastChunk.id ?? messageId,
        content: appendContent(lastChunk.content, content),
      };
    } else {
      chunks.push({ kind, id: messageId, content: [content] });
    }

    const entries = thread.entries.slice(0, -1);
    entries.push({ ...last, chunks });
    return { ...thread, entries };
  }

  return {
    ...thread,
    entries: [
      ...thread.entries,
      {
        type: "assistant",
        id: `assistant-${thread.entries.length}`,
        chunks: [{ kind, id: messageId, content: [content] }],
      },
    ],
  };
}

function toToolCall(update: Record<string, unknown>): ToolCall {
  return {
    id: update.toolCallId as string,
    title: (update.title as string) ?? "",
    kind: (update.kind as ToolCall["kind"]) ?? "other",
    status: (update.status as ToolCall["status"]) ?? "pending",
    content: (update.content as ToolCallContent[]) ?? [],
    locations: (update.locations as ToolCall["locations"]) ?? [],
    rawInput: update.rawInput,
    rawOutput: update.rawOutput,
  };
}

/**
 * Inserts a tool call, or updates it in place if we've seen the id before.
 *
 * Upsert rather than append because a `tool_call` can legitimately arrive for
 * an id already introduced by a `session/request_permission`, and because
 * agents re-announce calls.
 */
function upsertToolCall(thread: Thread, toolCall: ToolCall): Thread {
  const index = thread.entries.findIndex(
    (entry) => entry.type === "toolCall" && entry.toolCall.id === toolCall.id,
  );
  if (index === -1) {
    return {
      ...thread,
      entries: [...thread.entries, { type: "toolCall", id: toolCall.id, toolCall }],
    };
  }
  const entries = [...thread.entries];
  const existing = entries[index] as Extract<Entry, { type: "toolCall" }>;
  entries[index] = {
    ...existing,
    // Keep a pending permission prompt: the re-announcement is the agent
    // describing the call, not the user answering it.
    toolCall: { ...toolCall, awaitingPermission: existing.toolCall.awaitingPermission },
  };
  return { ...thread, entries };
}

/**
 * Applies a partial update to an existing tool call.
 *
 * Only the fields actually present are touched — a `tool_call_update` carrying
 * just a status must not blank out the content that arrived earlier.
 */
function updateToolCall(thread: Thread, update: Record<string, unknown>): Thread {
  const id = update.toolCallId as string;
  const index = thread.entries.findIndex(
    (entry) => entry.type === "toolCall" && entry.toolCall.id === id,
  );

  // An update for a call we never saw: keep it rather than drop it, so a
  // reconnect mid-turn still shows the tool running.
  if (index === -1) return upsertToolCall(thread, toToolCall(update));

  const entries = [...thread.entries];
  const existing = entries[index] as Extract<Entry, { type: "toolCall" }>;
  const merged: ToolCall = { ...existing.toolCall };

  if (update.title != null) merged.title = update.title as string;
  if (update.kind != null) merged.kind = update.kind as ToolCall["kind"];
  if (update.status != null) merged.status = update.status as ToolCall["status"];
  if (update.content != null) merged.content = update.content as ToolCallContent[];
  if (update.locations != null) merged.locations = update.locations as ToolCall["locations"];
  if (update.rawInput !== undefined) merged.rawInput = update.rawInput;
  if (update.rawOutput !== undefined) merged.rawOutput = update.rawOutput;

  // Any settled status resolves an outstanding prompt: the agent has moved on,
  // so a prompt still on screen would be answering a question nobody is asking.
  if (merged.status !== "pending" && merged.status !== "in_progress") {
    merged.awaitingPermission = undefined;
  }

  entries[index] = { ...existing, toolCall: merged };
  return { ...thread, entries };
}

/** Attaches a permission prompt to the tool call it belongs to. */
export function attachPermission(
  thread: Thread,
  toolCallId: string,
  request: PermissionRequest,
  fallbackTitle: string,
): Thread {
  const index = thread.entries.findIndex(
    (entry) => entry.type === "toolCall" && entry.toolCall.id === toolCallId,
  );

  // The permission request can arrive before the `tool_call` that describes
  // it, so create a placeholder rather than dropping the prompt on the floor.
  if (index === -1) {
    return {
      ...thread,
      entries: [
        ...thread.entries,
        {
          type: "toolCall",
          id: toolCallId,
          toolCall: {
            id: toolCallId,
            title: fallbackTitle,
            kind: "other",
            status: "pending",
            content: [],
            locations: [],
            awaitingPermission: request,
          },
        },
      ],
    };
  }

  const entries = [...thread.entries];
  const existing = entries[index] as Extract<Entry, { type: "toolCall" }>;
  entries[index] = {
    ...existing,
    toolCall: { ...existing.toolCall, awaitingPermission: request },
  };
  return { ...thread, entries };
}

/** Clears a permission prompt once it has been answered. */
export function clearPermission(thread: Thread, toolCallId: string): Thread {
  const index = thread.entries.findIndex(
    (entry) => entry.type === "toolCall" && entry.toolCall.id === toolCallId,
  );
  if (index === -1) return thread;

  const entries = [...thread.entries];
  const existing = entries[index] as Extract<Entry, { type: "toolCall" }>;
  entries[index] = {
    ...existing,
    toolCall: { ...existing.toolCall, awaitingPermission: undefined },
  };
  return { ...thread, entries };
}

/**
 * Adds a question the agent has put to the user, or refreshes one already here.
 *
 * The refresh is the point. After a reload the same question arrives twice — once
 * inside the replayed thread, and once re-asked over the new socket, because a
 * response has to belong to a request *this* connection received. Matching on the
 * request id is what turns those two into one form on screen.
 *
 * The entry keeps its existing id, so React sees the same element rather than a
 * replacement.
 */
export function attachElicitation(thread: Thread, elicitation: Elicitation): Thread {
  const index = thread.entries.findIndex(
    (entry) => entry.type === "elicitation" && entry.elicitation.requestId === elicitation.requestId,
  );
  if (index === -1) {
    return {
      ...thread,
      entries: [...thread.entries, { type: "elicitation", id: elicitation.id, elicitation }],
    };
  }

  const entries = [...thread.entries];
  const existing = entries[index] as Extract<Entry, { type: "elicitation" }>;
  entries[index] = {
    ...existing,
    elicitation: { ...elicitation, id: existing.id },
  };
  return { ...thread, entries };
}

/**
 * Records how a question ended, found by the request it answers.
 *
 * Revised in place rather than removed, matching the Rust model: the answer is
 * history worth reading, and every entry id after a removed one would shift.
 */
export function settleElicitation(
  thread: Thread,
  requestId: string | number,
  state: ElicitationState,
  content?: Record<string, ElicitationValue>,
): Thread {
  return mapElicitations(thread, (asked) =>
    asked.state === "pending" && asked.requestId === requestId
      ? { ...asked, state, ...(content ? { content } : {}) }
      : asked,
  );
}

/**
 * Records that a URL-mode question is finished.
 *
 * URL mode does not end with a response — the agent sends an
 * `elicitation/complete` naming the elicitation rather than the request.
 */
export function completeElicitation(thread: Thread, elicitationId: string): Thread {
  return mapElicitations(thread, (asked) =>
    asked.state === "pending" &&
    asked.mode.mode === "url" &&
    asked.mode.elicitationId === elicitationId
      ? { ...asked, state: "accepted" }
      : asked,
  );
}

/**
 * Gives up on every question still waiting on the user.
 *
 * A turn that has ended will not read an answer, so a form left on screen would
 * invite one that goes nowhere. The server does the same to its own copy.
 */
export function cancelPendingElicitations(thread: Thread): Thread {
  return mapElicitations(thread, (asked) =>
    asked.state === "pending" ? { ...asked, state: "cancelled" } : asked,
  );
}

/** Rewrites every elicitation entry, leaving the thread alone if none changed. */
function mapElicitations(
  thread: Thread,
  revise: (elicitation: Elicitation) => Elicitation,
): Thread {
  let changed = false;
  const entries = thread.entries.map((entry) => {
    if (entry.type !== "elicitation") return entry;
    const revised = revise(entry.elicitation);
    if (revised === entry.elicitation) return entry;
    changed = true;
    return { ...entry, elicitation: revised };
  });
  // Identity matters: React re-renders on it, so a no-op must stay a no-op.
  return changed ? { ...thread, entries } : thread;
}

/**
 * Records a user prompt optimistically, before the agent acknowledges it.
 *
 * Takes the blocks rather than the text, because a prompt carrying a mention
 * is several blocks and the optimistic echo has to match what was actually
 * sent — see `appendUserContent`, which absorbs the agent's echo of it.
 */
export function appendUserPrompt(thread: Thread, content: ContentBlock[]): Thread {
  return {
    ...thread,
    status: "generating",
    stopReason: undefined,
    entries: [
      ...thread.entries,
      {
        type: "user",
        id: `user-${thread.entries.length}`,
        content,
        isOptimistic: true,
      },
    ],
  };
}
