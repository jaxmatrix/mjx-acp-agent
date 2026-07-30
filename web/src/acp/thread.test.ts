import { describe, expect, test } from "vitest";
import type { ElicitationSchema, SessionNotification } from "@agentclientprotocol/sdk";

import {
  addTerminal,
  appendTerminalOutput,
  appendUserPrompt,
  applyUpdate,
  attachElicitation,
  attachPermission,
  cancelPendingElicitations,
  clearPermission,
  completeElicitation,
  setTerminalExit,
  settleElicitation,
} from "./thread";
import {
  chunkText,
  emptyThread,
  type Elicitation,
  type Entry,
  type SessionConfigOption,
  type Thread,
  type ToolCall,
} from "./types";

/** Wraps an update in the notification the reducer takes. */
function n(update: unknown): SessionNotification {
  return { sessionId: "s1", update } as SessionNotification;
}

/** Applies a sequence of updates to a fresh thread. */
function fold(...updates: unknown[]): Thread {
  return updates.reduce<Thread>((thread, update) => applyUpdate(thread, n(update)), emptyThread());
}

function text(t: string) {
  return { type: "text", text: t };
}

/** A model selector, as an agent would advertise it. */
function modelOption(currentValue: string): SessionConfigOption {
  return {
    id: "model",
    name: "Model",
    category: "model",
    type: "select",
    currentValue,
    options: [
      { value: "sonnet", name: "Sonnet" },
      { value: "opus", name: "Opus" },
    ],
  };
}

/** The single tool call in a thread. */
function onlyToolCall(thread: Thread): ToolCall {
  const entry = thread.entries.find((e): e is Extract<Entry, { type: "toolCall" }> => e.type === "toolCall");
  if (!entry) throw new Error("no tool call in the thread");
  return entry.toolCall;
}

describe("streaming text", () => {
  test("consecutive chunks of the same kind merge into one entry", () => {
    // Agents emit a chunk per token. Without merging, one sentence becomes
    // hundreds of entries and the UI renders hundreds of paragraphs.
    const thread = fold(
      { sessionUpdate: "agent_message_chunk", content: text("Hello ") },
      { sessionUpdate: "agent_message_chunk", content: text("there, ") },
      { sessionUpdate: "agent_message_chunk", content: text("world.") },
    );

    expect(thread.entries).toHaveLength(1);
    const entry = thread.entries[0];
    expect(entry?.type).toBe("assistant");
    if (entry?.type !== "assistant") throw new Error("unreachable");
    expect(entry.chunks).toHaveLength(1);
    expect(chunkText(entry.chunks[0]!)).toBe("Hello there, world.");
  });

  test("thinking and prose stay separate within one entry", () => {
    // They render differently — one collapsed and dimmed, one not — so merging
    // them would lose the distinction entirely.
    const thread = fold(
      { sessionUpdate: "agent_thought_chunk", content: text("Let me check. ") },
      { sessionUpdate: "agent_thought_chunk", content: text("The file is here.") },
      { sessionUpdate: "agent_message_chunk", content: text("I found it.") },
    );

    const entry = thread.entries[0];
    if (entry?.type !== "assistant") throw new Error("expected an assistant entry");
    expect(entry.chunks.map((c) => c.kind)).toEqual(["thought", "message"]);
    expect(chunkText(entry.chunks[0]!)).toBe("Let me check. The file is here.");
    expect(chunkText(entry.chunks[1]!)).toBe("I found it.");
  });

  test("a tool call between chunks starts a new assistant entry", () => {
    const thread = fold(
      { sessionUpdate: "agent_message_chunk", content: text("Reading. ") },
      { sessionUpdate: "tool_call", toolCallId: "t1", title: "Read", kind: "read" },
      { sessionUpdate: "agent_message_chunk", content: text("Done.") },
    );

    expect(thread.entries.map((e) => e.type)).toEqual(["assistant", "toolCall", "assistant"]);
  });

  test("user chunks accumulate into one message", () => {
    const thread = fold(
      { sessionUpdate: "user_message_chunk", content: text("fix ") },
      { sessionUpdate: "user_message_chunk", content: text("the bug") },
    );
    const entry = thread.entries[0];
    if (entry?.type !== "user") throw new Error("expected a user entry");
    expect(entry.content).toHaveLength(2);
  });

  test("differently labelled messages do not merge", () => {
    // Two distinct messages must not be glued into one paragraph.
    const thread = fold(
      { sessionUpdate: "agent_message_chunk", messageId: "m1", content: text("First.") },
      { sessionUpdate: "agent_message_chunk", messageId: "m2", content: text("Second.") },
    );

    const entry = thread.entries[0];
    if (entry?.type !== "assistant") throw new Error("expected an assistant entry");
    expect(entry.chunks).toHaveLength(2);
    expect(chunkText(entry.chunks[0]!)).toBe("First.");
    expect(chunkText(entry.chunks[1]!)).toBe("Second.");
  });

  test("an unlabelled chunk joins a labelled one", () => {
    // Most agents label nothing; an occasional label must not fragment the
    // message.
    const thread = fold(
      { sessionUpdate: "agent_message_chunk", messageId: "m1", content: text("one ") },
      { sessionUpdate: "agent_message_chunk", content: text("two") },
    );

    const entry = thread.entries[0];
    if (entry?.type !== "assistant") throw new Error("expected an assistant entry");
    expect(entry.chunks).toHaveLength(1);
    expect(chunkText(entry.chunks[0]!)).toBe("one two");
    expect(entry.chunks[0]?.id).toBe("m1");
  });

  test("an echoed prompt is absorbed rather than duplicated", () => {
    // The prompt goes on screen the moment it is sent. An agent that echoes
    // `user_message_chunk` back must not make it appear twice.
    let thread = appendUserPrompt(emptyThread(), [{ type: "text", text: "fix the bug" }]);
    thread = applyUpdate(
      thread,
      n({ sessionUpdate: "user_message_chunk", content: text("fix the bug") }),
    );

    expect(thread.entries).toHaveLength(1);
  });

  test("a user chunk that is not an echo is kept", () => {
    let thread = appendUserPrompt(emptyThread(), [{ type: "text", text: "first" }]);
    thread = applyUpdate(
      thread,
      n({ sessionUpdate: "user_message_chunk", content: text("something else entirely") }),
    );

    expect(thread.entries).toHaveLength(2);
  });
});

describe("tool calls", () => {
  test("an update revises the call in place rather than appending", () => {
    const thread = fold(
      {
        sessionUpdate: "tool_call",
        toolCallId: "t1",
        title: "Read stats.js",
        kind: "read",
        status: "pending",
      },
      { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "in_progress" },
      {
        sessionUpdate: "tool_call_update",
        toolCallId: "t1",
        status: "completed",
        content: [{ type: "content", content: text("file contents") }],
      },
    );

    expect(thread.entries).toHaveLength(1);
    const call = onlyToolCall(thread);
    expect(call.status).toBe("completed");
    expect(call.content).toHaveLength(1);
    // Fields the updates never mentioned survive.
    expect(call.title).toBe("Read stats.js");
    expect(call.kind).toBe("read");
  });

  test("a partial update does not blank out earlier fields", () => {
    // A `tool_call_update` carrying only a status must not erase the content
    // that arrived before it.
    const thread = fold(
      {
        sessionUpdate: "tool_call",
        toolCallId: "t1",
        title: "Edit",
        kind: "edit",
        content: [{ type: "diff", path: "/w/a.js", oldText: "a", newText: "b" }],
        locations: [{ path: "/w/a.js", line: 8 }],
      },
      { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed" },
    );

    const call = onlyToolCall(thread);
    expect(call.content).toHaveLength(1);
    expect(call.locations).toHaveLength(1);
  });

  test("an update for an unseen call is kept, not dropped", () => {
    // Happens when a browser reconnects mid-turn. Showing a partially known
    // tool call beats showing nothing.
    const thread = fold({
      sessionUpdate: "tool_call_update",
      toolCallId: "orphan",
      status: "in_progress",
    });

    expect(onlyToolCall(thread).id).toBe("orphan");
  });

  test("re-announcing a call does not duplicate it", () => {
    const thread = fold(
      { sessionUpdate: "tool_call", toolCallId: "t1", title: "First", kind: "read" },
      { sessionUpdate: "tool_call", toolCallId: "t1", title: "Second", kind: "read" },
    );

    expect(thread.entries).toHaveLength(1);
    expect(onlyToolCall(thread).title).toBe("Second");
  });

  test("defaults fill in for a minimal tool call", () => {
    const thread = fold({ sessionUpdate: "tool_call", toolCallId: "t1", title: "X" });
    const call = onlyToolCall(thread);
    expect(call.kind).toBe("other");
    expect(call.status).toBe("pending");
    expect(call.content).toEqual([]);
  });
});

describe("permission prompts", () => {
  test("a prompt attaches to the tool call it belongs to", () => {
    let thread = fold({
      sessionUpdate: "tool_call",
      toolCallId: "t1",
      title: "Run tests",
      kind: "execute",
    });
    thread = attachPermission(
      thread,
      "t1",
      { requestId: "t1", options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }] },
      "Run tests",
    );

    expect(thread.entries).toHaveLength(1);
    expect(onlyToolCall(thread).awaitingPermission?.options).toHaveLength(1);
  });

  test("a prompt arriving before its tool call creates a placeholder", () => {
    // The request and the describing update race; dropping the prompt would
    // hang the agent forever waiting on a button nobody can see.
    const thread = attachPermission(
      emptyThread(),
      "t1",
      { requestId: "t1", options: [] },
      "Run `node --test`",
    );

    const call = onlyToolCall(thread);
    expect(call.title).toBe("Run `node --test`");
    expect(call.awaitingPermission).toBeDefined();
  });

  test("a re-announced tool call keeps its pending prompt", () => {
    // The agent describing the call again is not the user answering it.
    let thread = attachPermission(emptyThread(), "t1", { requestId: "t1", options: [] }, "X");
    thread = applyUpdate(
      thread,
      n({ sessionUpdate: "tool_call", toolCallId: "t1", title: "X", kind: "execute" }),
    );

    expect(onlyToolCall(thread).awaitingPermission).toBeDefined();
  });

  test("a settled status clears a prompt nobody can answer any more", () => {
    let thread = attachPermission(emptyThread(), "t1", { requestId: "t1", options: [] }, "X");
    thread = applyUpdate(
      thread,
      n({ sessionUpdate: "tool_call_update", toolCallId: "t1", status: "failed" }),
    );

    expect(onlyToolCall(thread).awaitingPermission).toBeUndefined();
  });

  test("answering clears the prompt", () => {
    let thread = attachPermission(emptyThread(), "t1", { requestId: "t1", options: [] }, "X");
    thread = clearPermission(thread, "t1");
    expect(onlyToolCall(thread).awaitingPermission).toBeUndefined();
  });
});

const BRANCH_SCHEMA: ElicitationSchema = {
  type: "object",
  properties: { branch: { type: "string", title: "Branch" } },
  required: ["branch"],
};

describe("elicitations", () => {
  /** A pending form, as the reducer takes one. */
  function form(requestId: string | number, id = "elicitation-0"): Elicitation {
    return {
      id,
      requestId,
      message: "Which branch?",
      toolCallId: "t1",
      mode: { mode: "form", requestedSchema: BRANCH_SCHEMA },
      state: "pending",
    };
  }

  function onlyElicitation(thread: Thread): Elicitation {
    const entry = thread.entries.find((e) => e.type === "elicitation");
    if (entry?.type !== "elicitation") throw new Error("no elicitation entry");
    return entry.elicitation;
  }

  test("a question becomes a timeline entry of its own", () => {
    // Its own entry, not a field on a tool call: an elicitation need not belong
    // to one, so a single render path means the timeline rather than the card.
    const thread = attachElicitation(emptyThread(), form(7));

    expect(thread.entries).toHaveLength(1);
    expect(onlyElicitation(thread).state).toBe("pending");
  });

  test("the same question arriving twice is one entry", () => {
    // After a reload it does arrive twice: once inside the replayed thread, and
    // once re-asked over the new socket, because a response has to belong to a
    // request this connection received. The request id is what pairs them.
    let thread = attachElicitation(emptyThread(), form(7, "elicitation-0"));
    thread = attachElicitation(thread, form(7, "elicitation-9"));

    expect(thread.entries).toHaveLength(1);
    // The first id wins, so React sees the same element rather than a new one.
    expect(onlyElicitation(thread).id).toBe("elicitation-0");
  });

  test("two different questions are two entries", () => {
    let thread = attachElicitation(emptyThread(), form(7));
    thread = attachElicitation(thread, form(8, "elicitation-1"));
    expect(thread.entries).toHaveLength(2);
  });

  test("answering settles the entry in place and keeps what was submitted", () => {
    let thread = attachElicitation(emptyThread(), form(7));
    thread = settleElicitation(thread, 7, "accepted", { branch: "main" });

    expect(thread.entries).toHaveLength(1);
    expect(onlyElicitation(thread).state).toBe("accepted");
    expect(onlyElicitation(thread).content).toEqual({ branch: "main" });
  });

  test("a url question is finished by the notification, not by an answer", () => {
    const url: Elicitation = {
      id: "elicitation-0",
      requestId: 3,
      message: "Authorize, then come back.",
      mode: { mode: "url", elicitationId: "el-1", url: "https://example.test/" },
      state: "pending",
    };
    const thread = completeElicitation(attachElicitation(emptyThread(), url), "el-1");
    expect(onlyElicitation(thread).state).toBe("accepted");
  });

  test("a turn ending gives up on what is still open, but not on an answer", () => {
    let thread = attachElicitation(emptyThread(), form(7));
    thread = attachElicitation(thread, form(8, "elicitation-1"));
    thread = settleElicitation(thread, 7, "declined");
    thread = cancelPendingElicitations(thread);

    const states = thread.entries.map((entry) =>
      entry.type === "elicitation" ? entry.elicitation.state : null,
    );
    expect(states).toEqual(["declined", "cancelled"]);
  });

  test("settling something already settled changes nothing at all", () => {
    // Identity, not just value: React re-renders on it, so a no-op that returns
    // a new thread redraws the whole timeline for nothing.
    const thread = settleElicitation(attachElicitation(emptyThread(), form(7)), 7, "accepted");
    expect(settleElicitation(thread, 7, "declined")).toBe(thread);
    expect(cancelPendingElicitations(thread)).toBe(thread);
    expect(completeElicitation(thread, "nobody")).toBe(thread);
  });
});

describe("plan, commands and usage", () => {
  test("a later plan replaces the earlier one wholesale", () => {
    const thread = fold(
      {
        sessionUpdate: "plan",
        entries: [{ content: "Read", priority: "high", status: "in_progress" }],
      },
      {
        sessionUpdate: "plan",
        entries: [
          { content: "Read", priority: "high", status: "completed" },
          { content: "Fix", priority: "high", status: "in_progress" },
        ],
      },
    );

    expect(thread.plan).toHaveLength(2);
    expect(thread.plan[0]?.status).toBe("completed");
  });

  test("available commands and usage are recorded", () => {
    const thread = fold(
      {
        sessionUpdate: "available_commands_update",
        availableCommands: [{ name: "test", description: "Run tests" }],
      },
      { sessionUpdate: "usage_update", used: 4812, size: 200000 },
    );

    expect(thread.availableCommands[0]?.name).toBe("test");
    expect(thread.usage?.used).toBe(4812);
  });

  test("a mode update only applies once modes are known", () => {
    // `current_mode_update` names a mode from the list `session/new` returned;
    // without that list there is nothing to display.
    const withoutModes = fold({ sessionUpdate: "current_mode_update", currentModeId: "ask" });
    expect(withoutModes.modes).toBeUndefined();

    const base: Thread = {
      ...emptyThread(),
      modes: { currentModeId: "code", availableModes: [{ id: "ask", name: "Ask" }] },
    };
    const updated = applyUpdate(base, n({ sessionUpdate: "current_mode_update", currentModeId: "ask" }));
    expect(updated.modes?.currentModeId).toBe("ask");
  });

  // The mirror image of the rule above, and the reason the two arms are not
  // written the same way. Mirrors `a_config_option_update_stands_on_its_own`.
  test("a config option update stands on its own", () => {
    const thread = fold({
      sessionUpdate: "config_option_update",
      configOptions: [modelOption("sonnet")],
    });

    expect(thread.configOptions).toHaveLength(1);
    expect(thread.configOptions[0]?.name).toBe("Model");
  });

  test("a config option update replaces rather than merges", () => {
    const base: Thread = {
      ...emptyThread(),
      configOptions: [
        modelOption("sonnet"),
        { id: "web", name: "Web search", type: "boolean", currentValue: true },
      ],
    };
    const updated = applyUpdate(
      base,
      n({ sessionUpdate: "config_option_update", configOptions: [modelOption("opus")] }),
    );

    // The dropped boolean is not a bug: an agent that stops offering an option
    // says so by leaving it out of the set it sends.
    expect(updated.configOptions).toHaveLength(1);
    expect(updated.configOptions[0]).toMatchObject({ currentValue: "opus" });
  });
});

describe("robustness", () => {
  test("an unknown update kind is ignored rather than thrown on", () => {
    // The protocol grows. A viewer that throws on an unfamiliar variant breaks
    // against every newer agent.
    const thread = fold(
      { sessionUpdate: "agent_message_chunk", content: text("hi") },
      { sessionUpdate: "something_from_the_future", mystery: 42 },
      { sessionUpdate: "agent_message_chunk", content: text(" there") },
    );

    expect(thread.entries).toHaveLength(1);
    const entry = thread.entries[0];
    if (entry?.type !== "assistant") throw new Error("expected an assistant entry");
    expect(chunkText(entry.chunks[0]!)).toBe("hi there");
  });

  test("non-text content does not corrupt the merged text", () => {
    const thread = fold(
      { sessionUpdate: "agent_message_chunk", content: text("see ") },
      {
        sessionUpdate: "agent_message_chunk",
        content: { type: "image", data: "AAAA", mimeType: "image/png" },
      },
      { sessionUpdate: "agent_message_chunk", content: text("this") },
    );

    const entry = thread.entries[0];
    if (entry?.type !== "assistant") throw new Error("expected an assistant entry");
    expect(entry.chunks).toHaveLength(1);
    expect(entry.chunks[0]?.content).toHaveLength(3);
    expect(chunkText(entry.chunks[0]!)).toBe("see this");
  });

  test("the reducer never mutates its input", () => {
    // React re-renders on identity, so an in-place mutation would show stale
    // output until something unrelated triggered a redraw.
    const before = fold({ sessionUpdate: "agent_message_chunk", content: text("a") });
    const snapshot = JSON.stringify(before);
    const after = applyUpdate(before, n({ sessionUpdate: "agent_message_chunk", content: text("b") }));

    expect(JSON.stringify(before)).toBe(snapshot);
    expect(after).not.toBe(before);
  });
});

describe("terminals", () => {
  test("output accumulates and exit is recorded", () => {
    let thread = addTerminal(emptyThread(), {
      id: "t1",
      command: "node",
      args: ["--test"],
      cwd: "/w",
    });
    thread = appendTerminalOutput(thread, "t1", new Uint8Array([1, 2]), false);
    thread = appendTerminalOutput(thread, "t1", new Uint8Array([3]), true);
    thread = setTerminalExit(thread, "t1", 0, null);

    const terminal = thread.terminals.t1;
    expect(terminal?.output).toHaveLength(2);
    expect(terminal?.truncated).toBe(true);
    expect(terminal?.exitCode).toBe(0);
  });

  test("output for an unknown terminal is dropped without throwing", () => {
    const thread = appendTerminalOutput(emptyThread(), "ghost", new Uint8Array([1]), false);
    expect(thread.terminals).toEqual({});
  });
});
