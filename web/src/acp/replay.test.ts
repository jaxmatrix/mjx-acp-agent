/**
 * The wire differences between the two thread models.
 *
 * `fixture.test.ts` proves the adapter reproduces a whole real thread. These
 * cover the individual ways the Rust serialization differs from what this side
 * expects, so a failure says which one moved.
 */

import { describe, expect, test } from "vitest";

import { threadFromReplay } from "./replay";
import { appendUserPrompt } from "./thread";
import type { Thread } from "./types";

describe("a replayed thread", () => {
  test("an unknown session is nothing, not an empty thread", () => {
    // The server answers null for a session it has never heard of, and a
    // browser may legitimately ask before one exists. Returning an empty thread
    // would let a reload wipe a conversation the page still had.
    expect(threadFromReplay(null)).toBeNull();
    expect(threadFromReplay(undefined)).toBeNull();
    expect(threadFromReplay("nonsense")).toBeNull();
  });

  test("a tool call arrives flat and comes back nested", () => {
    // Rust tags its enums internally, so the call's own fields sit alongside
    // `type` rather than under a `toolCall` key.
    const thread = threadFromReplay({
      entries: [
        {
          type: "toolCall",
          id: "call_read",
          title: "Read stats.js",
          kind: "read",
          status: "completed",
          content: [{ type: "content", content: { type: "text", text: "..." } }],
        },
      ],
    })!;

    const entry = thread.entries[0];
    expect(entry?.type).toBe("toolCall");
    if (entry?.type !== "toolCall") throw new Error("unreachable");
    expect(entry.id).toBe("call_read");
    expect(entry.toolCall.title).toBe("Read stats.js");
    expect(entry.toolCall.content).toHaveLength(1);
    // Locations were omitted rather than sent empty.
    expect(entry.toolCall.locations).toEqual([]);
  });

  test("collections Rust left out come back empty", () => {
    // `plan`, `availableCommands` and the rest are skipped when empty, but the
    // UI iterates them unconditionally.
    const thread = threadFromReplay({ entries: [], status: "idle" })!;
    expect(thread.plan).toEqual([]);
    expect(thread.availableCommands).toEqual([]);
    expect(thread.terminals).toEqual({});
    expect(thread.stopReason).toBeUndefined();
    expect(thread.usage).toBeUndefined();
    expect(thread.modes).toBeUndefined();
    expect(thread.configOptions).toEqual([]);
  });

  // The whole reason the server folds config options at all: after a reload the
  // browser's `session/new` is answered from a recording, so the model actually
  // in effect can only come from here.
  test("config options survive a replay", () => {
    const thread = threadFromReplay({
      entries: [],
      status: "idle",
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model",
          type: "select",
          currentValue: "opus",
          options: [{ value: "opus", name: "Opus" }],
        },
        { id: "web", name: "Web search", type: "boolean", currentValue: true },
      ],
    })!;

    expect(thread.configOptions).toHaveLength(2);
    expect(thread.configOptions[0]).toMatchObject({ type: "select", currentValue: "opus" });
    expect(thread.configOptions[1]).toMatchObject({ type: "boolean", currentValue: true });
  });

  test("a config option we cannot read costs one control, not the page", () => {
    const thread = threadFromReplay({
      entries: [],
      status: "idle",
      configOptions: [
        { id: "good", name: "Good", type: "boolean", currentValue: false },
        // No `currentValue`, so nothing could say what the control is set to.
        { id: "bad", name: "Bad", type: "select" },
        { name: "nameless" },
        "not an option at all",
      ],
    })!;

    expect(thread.configOptions.map((option) => option.id)).toEqual(["good"]);
  });

  test("entry ids are renumbered so the next prompt cannot collide", () => {
    // Rust numbers from one shared counter, this model from the entry's
    // position, and Rust's counter reaches the position the *next* entry will
    // take. Left alone, the `assistant-2` below and the `user-2` that
    // `appendUserPrompt` is about to mint are one React key for two entries.
    const replayed = threadFromReplay({
      entries: [
        { type: "user", id: "user-1", content: [], isOptimistic: true },
        { type: "assistant", id: "assistant-2", chunks: [] },
      ],
      status: "idle",
    })!;
    expect(replayed.entries.map((entry) => entry.id)).toEqual(["user-0", "assistant-1"]);

    const next: Thread = appendUserPrompt(replayed, "and now this");
    const ids = next.entries.map((entry) => entry.id);
    expect(new Set(ids).size, `duplicate keys: ${ids.join(", ")}`).toBe(ids.length);
  });

  test("a tool call keeps the agent's own id", () => {
    // Every later update about a call is keyed by it, so renumbering one would
    // orphan the card the moment the agent said anything more about it.
    const thread = threadFromReplay({
      entries: [
        { type: "user", id: "user-1", content: [] },
        { type: "toolCall", id: "call_edit", title: "Edit", kind: "edit", status: "in_progress" },
      ],
    })!;
    expect(thread.entries.map((entry) => entry.id)).toEqual(["user-0", "call_edit"]);
  });

  test("a turn still running is still running", () => {
    const thread = threadFromReplay({ entries: [], status: "generating" })!;
    expect(thread.status).toBe("generating");
  });

  test("an entry shaped like nothing we know costs one entry, not the page", () => {
    const thread = threadFromReplay({
      entries: [
        { type: "user", id: "user-1", content: [] },
        { type: "somethingNewer" },
        null,
        { type: "assistant", id: "assistant-3", chunks: [] },
      ],
    })!;
    expect(thread.entries.map((entry) => entry.type)).toEqual(["user", "assistant"]);
    // And the survivors are still numbered by their position in what is left.
    expect(thread.entries.map((entry) => entry.id)).toEqual(["user-0", "assistant-1"]);
  });

  test("a stop reason we do not model is dropped rather than shown", () => {
    expect(threadFromReplay({ entries: [], stopReason: "end_turn" })!.stopReason).toBe("end_turn");
    expect(threadFromReplay({ entries: [], stopReason: "who_knows" })!.stopReason).toBeUndefined();
  });
});
