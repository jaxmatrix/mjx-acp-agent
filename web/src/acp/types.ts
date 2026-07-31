/**
 * The thread model, mirroring `crates/mjx-acp-thread`.
 *
 * Both sides fold the same `session/update` stream into the same shapes, so the
 * server's replay and the browser's live state agree. Ported in spirit from
 * Zed's `acp_thread::AgentThreadEntry`, minus everything that needs an editor.
 */

import type {
  ContentBlock,
  ElicitationSchema,
  PermissionOption,
  PlanEntry,
  ToolCallStatus,
  ToolKind,
} from "@agentclientprotocol/sdk";

import type { McpServer } from "./mcp";

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

/**
 * A structured question the agent put to the user.
 *
 * Mirrors `mjx_acp_thread::Elicitation`, field for field, because this one *is*
 * carried by `_mjx/session/replay` — unlike a permission prompt, which the relay
 * re-asks over the socket instead. A form the user is halfway through is worth
 * bringing back across a reload, and a permission prompt is hidden by its tool
 * call settling where an elicitation, which need not have a tool call, is not.
 */
export interface Elicitation {
  /** Stable identity for the UI. */
  id: string;
  /** The JSON-RPC id the answer has to carry. */
  requestId: string | number;
  message: string;
  /** The tool call this belongs to. A label, not a home. */
  toolCallId?: string;
  mode: ElicitationMode;
  state: ElicitationState;
  /** What the user submitted, once they have. */
  content?: Record<string, ElicitationValue>;
}

/**
 * How the user is being asked.
 *
 * Only the two modes the protocol defines. An unknown one is declined rather
 * than modelled: the spec says a client must not render one as if it understood
 * it.
 */
export type ElicitationMode =
  | { mode: "form"; requestedSchema: ElicitationSchema }
  | { mode: "url"; elicitationId: string; url: string };

/** The three `CreateElicitationResponse` actions, plus the state before them. */
export type ElicitationState = "pending" | "accepted" | "declined" | "cancelled";

/** What a form field can come back as. */
export type ElicitationValue = string | number | boolean | string[];

/**
 * What the user did with a question.
 *
 * The three `CreateElicitationResponse` actions. Declining and cancelling are
 * kept apart because the protocol keeps them apart: declining is an answer,
 * cancelling is refusing to give one.
 */
export type ElicitationAnswer =
  | { action: "accept"; content?: Record<string, ElicitationValue> }
  | { action: "decline" }
  | { action: "cancel" };

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
  | { type: "toolCall"; id: string; toolCall: ToolCall }
  | { type: "elicitation"; id: string; elicitation: Elicitation };

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
  /**
   * Whether this terminal accepts keystrokes.
   *
   * True only for one the server opened for a login. A terminal the *agent*
   * asked for is read-only: it owns that process, and ACP has no notion of a
   * client typing into it. The server refuses a write to one either way; this
   * is so the UI does not offer what would be refused.
   */
  interactive?: boolean;
}

/**
 * How the agent will accept being authenticated, and what this server can do
 * about it. Mirrors `ext::AuthMethodInfo`.
 *
 * Every field is a name or a reason. There is nowhere here to put a credential,
 * which is the point: the server holds them so the browser does not have to.
 */
export interface AuthMethodInfo {
  id: string;
  name: string;
  description?: string;
  kind: string;
  /** The provider that would handle it, if one would. */
  provider?: string;
  /** What the operator must do, when the server cannot get further alone. */
  instructions?: string;
  link?: string;
  secrets?: AuthSecret[];
  declines?: AuthDecline[];
  satisfied: boolean;
}

/** One variable a method needs, and whether the server has it. Never its value. */
export interface AuthSecret {
  name: string;
  label?: string;
  present: boolean;
  optional: boolean;
}

/** Why one provider passed on a method. */
export interface AuthDecline {
  provider: string;
  reason: string;
}

/** Mirrors `ext::AuthState`: what this connection knows about authenticating. */
export interface AuthState {
  required: boolean;
  authenticated: boolean;
  methods: AuthMethodInfo[];
  refusedMethod?: string;
}

/** Mirrors `ext::AuthAttemptResult`. */
export interface AuthAttemptResult {
  authenticated: boolean;
  message: string;
  /** A login terminal to show and type into, if the attempt started one. */
  terminalId?: string;
}

/** Mirrors `ext::AuthProgress`: how an attempt that outlived its request went. */
export interface AuthProgress {
  methodId: string;
  message: string;
  /** Absent while the attempt is still running. */
  authenticated?: boolean;
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

/** One selectable value of a `select` config option. */
export interface SessionConfigSelectOption {
  value: string;
  name: string;
  description?: string | null;
}

/** Selectable values under a heading, for agents that group them. */
export interface SessionConfigSelectGroup {
  group: string;
  name: string;
  options: SessionConfigSelectOption[];
}

/**
 * A per-session setting the agent exposes — the model, the thinking level, and
 * whatever else it chooses to offer.
 *
 * `category` is a UX hint and nothing more. It is optional, the spec reserves
 * the right to add values, and vendors may invent their own with a `_` prefix,
 * so an unfamiliar one must still render.
 */
export type SessionConfigOption = {
  id: string;
  name: string;
  description?: string | null;
  category?: string | null;
} & (
  | {
      type: "select";
      currentValue: string;
      /** Flat or grouped; the wire format has no discriminator for which. */
      options: SessionConfigSelectOption[] | SessionConfigSelectGroup[];
    }
  | { type: "boolean"; currentValue: boolean }
);

/** A select option's values, once it is known which of the two shapes they are. */
export type SelectShape =
  | { grouped: true; groups: SessionConfigSelectGroup[] }
  | { grouped: false; values: SessionConfigSelectOption[] };

/**
 * Tells a grouped list of selectable values from a flat one.
 *
 * The two are an untagged union on the wire — there is no discriminator — so
 * the shape of the first entry is the only thing to go on. Getting this wrong
 * does not throw: it renders a selector with nothing in it, which is why it is
 * here with a test rather than inline in the component.
 */
export function selectShape(
  options: SessionConfigSelectOption[] | SessionConfigSelectGroup[],
): SelectShape {
  const first = options[0];
  return first && "group" in first && Array.isArray(first.options)
    ? { grouped: true, groups: options as SessionConfigSelectGroup[] }
    : { grouped: false, values: options as SessionConfigSelectOption[] };
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
  configOptions: SessionConfigOption[];
}

/** An empty thread. */
export function emptyThread(): Thread {
  return {
    entries: [],
    plan: [],
    status: "idle",
    availableCommands: [],
    configOptions: [],
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
  /** Pass this back as `resume` to rejoin this agent after a reload. */
  connectionId: string;
  /** True when this socket rejoined an agent that was already running. */
  resumed: boolean;
  /**
   * The MCP servers configured for this agent, and whether it got them.
   *
   * Normalized through `mcpServersOf` before it is read: this whole payload is
   * an unchecked cast off the socket. Never carries a credential — only the
   * *names* of the ones a server uses.
   */
  mcpServers?: McpServer[];
}

/**
 * One conversation the agent knows about, from `session/list`.
 *
 * Only `sessionId` and `cwd` are promised. A title and a timestamp are what
 * make a list worth reading, but an agent that keeps neither still lists its
 * sessions, so both are optional here as they are on the wire.
 */
export interface SessionInfo {
  sessionId: string;
  cwd: string;
  title?: string;
  updatedAt?: string;
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
