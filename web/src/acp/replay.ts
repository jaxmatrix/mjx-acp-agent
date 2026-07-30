/**
 * Rebuilds a thread from what `_mjx/session/replay` returns.
 *
 * The server folds the same `session/update` stream into the same model, but it
 * is the Rust one, and it serializes the way Rust does: a tool call entry is
 * flat rather than nested, empty collections are left out entirely, and
 * terminals are not part of the model at all. So a replay is not a `Thread` —
 * it is something a `Thread` can be built from, which is what this does.
 *
 * Everything here treats the payload as untrusted input rather than as a typed
 * value. It arrives over a socket, and a shape we did not expect should cost us
 * one entry rather than the whole page.
 */

import type { ContentBlock, PlanEntry } from "@agentclientprotocol/sdk";

import {
  emptyThread,
  type AssistantChunk,
  type AvailableCommand,
  type Entry,
  type SessionConfigOption,
  type SessionConfigSelectOption,
  type SessionMode,
  type StopReason,
  type Thread,
  type ToolCall,
  type ToolCallContent,
  type ToolCallLocation,
  type Usage,
} from "./types";

const STOP_REASONS: readonly string[] = [
  "end_turn",
  "max_tokens",
  "max_turn_requests",
  "refusal",
  "cancelled",
];

/**
 * Rebuilds a browser `Thread` from a replay payload.
 *
 * Returns null when the server has no thread for that session, which is a
 * normal answer rather than a failure: a browser may ask before one exists.
 */
export function threadFromReplay(payload: unknown): Thread | null {
  if (!isRecord(payload)) return null;

  const empty = emptyThread();
  return {
    entries: asArray(payload.entries)
      .map(toEntry)
      .filter((entry): entry is Entry => entry !== null)
      .map(renumber),
    plan: asArray(payload.plan) as PlanEntry[],
    status: payload.status === "generating" ? "generating" : "idle",
    stopReason:
      typeof payload.stopReason === "string" && STOP_REASONS.includes(payload.stopReason)
        ? (payload.stopReason as StopReason)
        : undefined,
    usage: toUsage(payload.usage),
    availableCommands: asArray(payload.availableCommands) as AvailableCommand[],
    modes: toModes(payload.modes),
    configOptions: asArray(payload.configOptions)
      .map(toConfigOption)
      .filter((option): option is SessionConfigOption => option !== null),
    // Not recoverable. The scrollback of a terminal the agent ran lives in the
    // workspace, not in the thread, so a resumed page shows the tool call
    // without the output it produced.
    terminals: empty.terminals,
  };
}

/**
 * Gives an entry the id this model would have minted for it.
 *
 * The two models number entries differently — Rust from one shared counter,
 * the browser from the entry's position — and the difference is not cosmetic.
 * Rust's counter can reach the position of the *next* entry, so a replayed
 * `assistant-2` in a two-entry thread collides with the `user-2` the next
 * prompt is about to mint, and React would see one key for two entries.
 * Renumbering makes a replayed thread indistinguishable from a folded one.
 *
 * Tool calls keep the agent's own id, because that is what every later update
 * about them is keyed by.
 */
function renumber(entry: Entry, index: number): Entry {
  return entry.type === "toolCall" ? entry : { ...entry, id: `${entry.type}-${index}` };
}

function toEntry(raw: unknown): Entry | null {
  if (!isRecord(raw)) return null;

  switch (raw.type) {
    case "user":
      return {
        type: "user",
        id: "",
        content: asArray(raw.content) as ContentBlock[],
        isOptimistic: raw.isOptimistic === true,
        ...(typeof raw.protocolId === "string" ? { protocolId: raw.protocolId } : {}),
      };

    case "assistant":
      return {
        type: "assistant",
        id: "",
        chunks: asArray(raw.chunks)
          .map(toChunk)
          .filter((chunk): chunk is AssistantChunk => chunk !== null),
      };

    // Flat on the wire, because Rust tags its enums internally. The browser
    // nests the call inside the entry, so it has to be lifted back out.
    case "toolCall": {
      const toolCall = toToolCall(raw);
      return toolCall && { type: "toolCall", id: toolCall.id, toolCall };
    }

    default:
      return null;
  }
}

function toChunk(raw: unknown): AssistantChunk | null {
  if (!isRecord(raw)) return null;
  return {
    kind: raw.kind === "thought" ? "thought" : "message",
    ...(typeof raw.id === "string" ? { id: raw.id } : {}),
    content: asArray(raw.content) as ContentBlock[],
  };
}

function toToolCall(raw: Record<string, unknown>): ToolCall | null {
  if (typeof raw.id !== "string") return null;
  return {
    id: raw.id,
    title: typeof raw.title === "string" ? raw.title : raw.id,
    kind: (raw.kind ?? "other") as ToolCall["kind"],
    status: (raw.status ?? "pending") as ToolCall["status"],
    content: asArray(raw.content) as ToolCallContent[],
    locations: asArray(raw.locations) as ToolCallLocation[],
    ...("rawInput" in raw ? { rawInput: raw.rawInput } : {}),
    ...("rawOutput" in raw ? { rawOutput: raw.rawOutput } : {}),
    // Deliberately not carried: a permission prompt is not part of the thread
    // the server folds. One still outstanding is re-asked after the replay,
    // which is what puts it back on the card it belongs to.
  };
}

function toUsage(raw: unknown): Usage | undefined {
  if (!isRecord(raw) || typeof raw.used !== "number" || typeof raw.size !== "number") {
    return undefined;
  }
  const cost = isRecord(raw.cost) ? raw.cost : undefined;
  return {
    used: raw.used,
    size: raw.size,
    ...(cost && typeof cost.amount === "number" && typeof cost.currency === "string"
      ? { cost: { amount: cost.amount, currency: cost.currency } }
      : {}),
  };
}

function toModes(raw: unknown): Thread["modes"] {
  if (!isRecord(raw) || typeof raw.currentModeId !== "string") return undefined;
  return {
    currentModeId: raw.currentModeId,
    availableModes: asArray(raw.availableModes) as SessionMode[],
  };
}

/**
 * Rebuilds one config option, or drops it.
 *
 * Dropping one option is the right cost for a shape we don't recognise: the
 * rest of the selectors still work, whereas trusting it would put a control on
 * screen that cannot say what it is currently set to.
 */
function toConfigOption(raw: unknown): SessionConfigOption | null {
  if (!isRecord(raw) || typeof raw.id !== "string" || typeof raw.name !== "string") return null;

  const common = {
    id: raw.id,
    name: raw.name,
    ...(typeof raw.description === "string" ? { description: raw.description } : {}),
    ...(typeof raw.category === "string" ? { category: raw.category } : {}),
  };

  if (raw.type === "boolean" && typeof raw.currentValue === "boolean") {
    return { ...common, type: "boolean", currentValue: raw.currentValue };
  }
  if (raw.type === "select" && typeof raw.currentValue === "string") {
    return {
      ...common,
      type: "select",
      currentValue: raw.currentValue,
      // Left as the agent sent it. Flat and grouped are told apart at render
      // time, where the choice of control is actually made.
      options: asArray(raw.options) as SessionConfigSelectOption[],
    };
  }
  return null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** An absent collection and an empty one mean the same thing: Rust omits both. */
function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}
