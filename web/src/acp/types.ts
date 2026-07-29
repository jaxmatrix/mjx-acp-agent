/**
 * The thread model, mirroring `crates/mjx-acp-thread`.
 *
 * Both sides fold the same `session/update` stream into the same shapes, so the
 * server's replay and the browser's live state agree. Ported in spirit from
 * Zed's `acp_thread::AgentThreadEntry`, minus everything that needs an editor.
 */

import type {
  ContentBlock,
  PermissionOption,
  PlanEntry,
  ToolCallStatus,
  ToolKind,
} from "@agentclientprotocol/sdk";

/** How a turn ended. */
export type StopReason =
  | "end_turn"
  | "max_tokens"
  | "max_turn_requests"
  | "refusal"
  | "cancelled";

/** Whether the agent is working. */
export type ThreadStatus = "idle" | "generating";

/**
 * One run of assistant content: prose, or the model thinking aloud.
 *
 * `id` is the agent's `messageId`, when it labels its messages. Two chunks
 * merge only when their ids are compatible — see `canMergeMessageChunks`.
 */
export interface AssistantChunk {
  kind: "message" | "thought";
  id?: string;
  /** Adjacent text blocks are merged; images and resources are kept whole. */
  content: ContentBlock[];
}

/** The text of a chunk, with non-text blocks skipped. */
export function chunkText(chunk: AssistantChunk): string {
  return chunk.content
    .map((block) => (block.type === "text" ? block.text : ""))
    .join("");
}

/** What a tool call has to show. */
export type ToolCallContent =
  | { type: "content"; content: ContentBlock }
  | { type: "diff"; path: string; oldText: string | null; newText: string }
  | { type: "terminal"; terminalId: string };

/** A file or region the tool touched. */
export interface ToolCallLocation {
  path: string;
  line?: number | null;
}

/**
 * A tool call, plus the extra state the UI needs.
 *
 * `awaitingPermission` is not part of the protocol: `session/request_permission`
 * arrives as a separate request, and joining it to its tool call is what lets
 * the prompt render inside the card it belongs to.
 */
export interface ToolCall {
  id: string;
  title: string;
  kind: ToolKind;
  status: ToolCallStatus;
  content: ToolCallContent[];
  locations: ToolCallLocation[];
  rawInput?: unknown;
  rawOutput?: unknown;
  awaitingPermission?: PermissionRequest;
}

/** A pending authorization, waiting on the user. */
export interface PermissionRequest {
  /** JSON-RPC id to answer with. */
  requestId: string | number;
  options: PermissionOption[];
}

/** One item in the timeline. */
export type Entry =
  | {
      type: "user";
      id: string;
      content: ContentBlock[];
      /**
       * True until the agent echoes the message back. The prompt is shown the
       * moment it is sent rather than after a round trip, so an agent that
       * echoes `user_message_chunk` would otherwise show it twice.
       */
      isOptimistic?: boolean;
      /** The agent's own id for this message, once it tells us one. */
      protocolId?: string;
    }
  | { type: "assistant"; id: string; chunks: AssistantChunk[] }
  | { type: "toolCall"; id: string; toolCall: ToolCall };

/** A terminal the server is running for the agent. */
export interface Terminal {
  id: string;
  command: string;
  args: string[];
  cwd: string;
  /** Raw bytes received so far, for xterm.js to replay into a fresh view. */
  output: Uint8Array[];
  truncated: boolean;
  exitCode?: number | null;
  signal?: string | null;
}

/** Token and cost accounting, for the usage bar. */
export interface Usage {
  used: number;
  size: number;
  cost?: { amount: number; currency: string };
}

/** A slash command the agent offers. */
export interface AvailableCommand {
  name: string;
  description: string;
  input?: { hint: string } | null;
}

/** A mode the session can be switched to. */
export interface SessionMode {
  id: string;
  name: string;
  description?: string | null;
}

/** The whole conversation. */
export interface Thread {
  entries: Entry[];
  plan: PlanEntry[];
  status: ThreadStatus;
  stopReason?: StopReason;
  usage?: Usage;
  availableCommands: AvailableCommand[];
  modes?: { currentModeId: string; availableModes: SessionMode[] };
  terminals: Record<string, Terminal>;
}

/** An empty thread. */
export function emptyThread(): Thread {
  return {
    entries: [],
    plan: [],
    status: "idle",
    availableCommands: [],
    terminals: {},
  };
}

/** One frame in the protocol inspector. */
export interface InspectorEntry {
  seq: number;
  at: number;
  direction: "clientToAgent" | "agentToClient";
  /** The method this frame is, or the one it answers. */
  method?: string;
  /** Whether the server answered it instead of us. */
  intercepted: boolean;
  line: string;
}

/** What the server said it connected us to. */
export interface AgentInfo {
  agentId: string;
  name: string;
  command: string[];
  cwd: string;
}

/** An entry in the agent picker, from `GET /api/agents`. */
export interface CatalogEntry {
  id: string;
  name: string;
  description?: string;
  icon?: string;
  availability:
    | { state: "ready" }
    | { state: "missingProgram"; program: string }
    | { state: "needsManualInstall" };
  command?: string[];
  isLocalOverride: boolean;
}

/** A directory an agent can be pointed at, from `GET /api/workspaces`. */
export interface WorkspaceRoot {
  path: string;
  name: string;
}
