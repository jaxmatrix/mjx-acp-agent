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
  Entry,
  PermissionRequest,
  Terminal,
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

/** Structural equality, for spotting an echoed prompt. */
function sameBlock(a: ContentBlock, b: ContentBlock): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
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

/** Records a user prompt optimistically, before the agent acknowledges it. */
export function appendUserPrompt(thread: Thread, text: string): Thread {
  return {
    ...thread,
    status: "generating",
    stopReason: undefined,
    entries: [
      ...thread.entries,
      {
        type: "user",
        id: `user-${thread.entries.length}`,
        content: [{ type: "text", text }],
        isOptimistic: true,
      },
    ],
  };
}

/** Registers a terminal the server started for the agent. */
export function addTerminal(thread: Thread, terminal: Omit<Terminal, "output" | "truncated">): Thread {
  return {
    ...thread,
    terminals: {
      ...thread.terminals,
      [terminal.id]: { ...terminal, output: [], truncated: false },
    },
  };
}

/** Appends streamed bytes to a terminal. */
export function appendTerminalOutput(
  thread: Thread,
  terminalId: string,
  chunk: Uint8Array,
  truncated: boolean,
): Thread {
  const terminal = thread.terminals[terminalId];
  if (!terminal) return thread;
  return {
    ...thread,
    terminals: {
      ...thread.terminals,
      [terminalId]: { ...terminal, output: [...terminal.output, chunk], truncated },
    },
  };
}

/** Records a terminal's exit. */
export function setTerminalExit(
  thread: Thread,
  terminalId: string,
  exitCode: number | null | undefined,
  signal: string | null | undefined,
): Thread {
  const terminal = thread.terminals[terminalId];
  if (!terminal) return thread;
  return {
    ...thread,
    terminals: { ...thread.terminals, [terminalId]: { ...terminal, exitCode, signal } },
  };
}
