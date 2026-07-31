/**
 * The viewer's bookkeeping: which conversations are open, and which one is
 * being looked at.
 *
 * The reducer rather than the hook, because everything worth getting right here
 * is a pure decision about state — where the eye goes when a tab closes under
 * it, which conversation a reload settles on — and none of it needs React to be
 * wrong in an interesting way.
 */

import { describe, expect, test } from "vitest";

import { EMPTY, reduce, type Action, type State } from "./useSessions";
import { emptyThread } from "./acp/types";

const MOCK = `mock\u0000/w`;
const GEMINI = `gemini\u0000/w`;

function run(state: State, ...actions: Action[]): State {
  return actions.reduce(reduce, state);
}

/** A connection with `sessions` open on it. */
function opened(key: string, ...sessions: string[]): Action[] {
  return [
    { type: "connectionOpened", key },
    ...sessions.map((sessionId): Action => ({ type: "sessionOpened", key, sessionId })),
  ];
}

/** The tabs, the way the hook derives them for the strip. */
function tabsOf(state: State): string[] {
  return state.order.flatMap((key) => (state.open[key] ?? []).map((id) => `${key}/${id}`));
}

describe("what is open", () => {
  test("tabs are every conversation, grouped by the connection carrying it", () => {
    const state = run(EMPTY, ...opened(MOCK, "a", "b"), ...opened(GEMINI, "x"));
    expect(tabsOf(state)).toEqual([`${MOCK}/a`, `${MOCK}/b`, `${GEMINI}/x`]);
  });

  test("two agents may hand out the same session id without colliding", () => {
    // Session ids are the agent's, and nothing stops two agents choosing the
    // same one. Threads keyed by id alone would have one conversation quietly
    // overwriting the other.
    const mine = { ...emptyThread(), status: "generating" as const };
    const theirs = emptyThread();
    const state = run(
      EMPTY,
      ...opened(MOCK, "s1"),
      ...opened(GEMINI, "s1"),
      { type: "thread", key: MOCK, sessionId: "s1", thread: mine },
      { type: "thread", key: GEMINI, sessionId: "s1", thread: theirs },
    );

    expect(state.threads[MOCK]?.s1?.status).toBe("generating");
    expect(state.threads[GEMINI]?.s1?.status).toBe("idle");
    expect(tabsOf(state)).toHaveLength(2);
  });

  test("the first conversation to arrive is the one to look at", () => {
    const state = run(EMPTY, ...opened(MOCK, "a", "b"));
    expect(state.focused).toEqual({ agentId: "mock", cwd: "/w", sessionId: "a" });
  });

  test("one opening in the background does not steal the screen", () => {
    // A fork, or a second agent finishing its handshake, while something is
    // being read. Moving the page out from under the reader is never right.
    const state = run(EMPTY, ...opened(MOCK, "a"), ...opened(GEMINI, "x"));
    expect(state.focused?.agentId).toBe("mock");
  });
});

describe("where the eye goes when a tab closes", () => {
  test("to its neighbour on the same connection", () => {
    // Where they were looking. Jumping to another agent because a tab closed
    // would lose the thread of what they were doing.
    const state = run(
      EMPTY,
      ...opened(MOCK, "a", "b", "c"),
      ...opened(GEMINI, "x"),
      { type: "focus", tab: { agentId: "mock", cwd: "/w", sessionId: "b" } },
      { type: "sessionClosed", key: MOCK, sessionId: "b" },
    );
    expect(state.focused).toEqual({ agentId: "mock", cwd: "/w", sessionId: "c" });
  });

  test("to the one before it when it was the last on its connection", () => {
    const state = run(
      EMPTY,
      ...opened(MOCK, "a", "b"),
      { type: "focus", tab: { agentId: "mock", cwd: "/w", sessionId: "b" } },
      { type: "sessionClosed", key: MOCK, sessionId: "b" },
    );
    expect(state.focused).toEqual({ agentId: "mock", cwd: "/w", sessionId: "a" });
  });

  test("to another connection only when this one has nothing left", () => {
    const state = run(
      EMPTY,
      ...opened(MOCK, "a"),
      ...opened(GEMINI, "x"),
      { type: "sessionClosed", key: MOCK, sessionId: "a" },
    );
    expect(state.focused).toEqual({ agentId: "gemini", cwd: "/w", sessionId: "x" });
  });

  test("nowhere, when that was the last conversation anywhere", () => {
    const state = run(EMPTY, ...opened(MOCK, "a"), {
      type: "sessionClosed",
      key: MOCK,
      sessionId: "a",
    });
    expect(state.focused).toBeUndefined();
    expect(tabsOf(state)).toEqual([]);
  });

  test("a tab closing somewhere else leaves the eye where it was", () => {
    const state = run(
      EMPTY,
      ...opened(MOCK, "a", "b"),
      { type: "sessionClosed", key: MOCK, sessionId: "b" },
    );
    expect(state.focused).toEqual({ agentId: "mock", cwd: "/w", sessionId: "a" });
  });

  test("its thread goes with it, so a session id reused later starts clean", () => {
    const state = run(
      EMPTY,
      ...opened(MOCK, "a", "b"),
      { type: "thread", key: MOCK, sessionId: "b", thread: emptyThread() },
      { type: "sessionClosed", key: MOCK, sessionId: "b" },
    );
    expect(state.threads[MOCK]).not.toHaveProperty("b");
  });
});

describe("coming back from a reload", () => {
  test("settles on the conversation that was being read, not the first back", () => {
    // The replays land in whatever order the server answers them, and the one
    // that was on screen may well be the third of five. Taking the first would
    // put the reader somewhere they never were.
    const wanted = { agentId: "mock", cwd: "/w", sessionId: "c" };
    const state = run({ ...EMPTY, restoring: wanted }, ...opened(MOCK, "a", "b", "c"));

    expect(state.focused).toEqual(wanted);
    expect(state.restoring).toBeUndefined();
  });

  test("settles on the first one back if the one being read is gone", () => {
    // Deleted while the page was away, or on a connection since reaped. There
    // is nothing to go back to, and the tabs that survived are better than the
    // picker.
    const state = run(
      { ...EMPTY, restoring: { agentId: "mock", cwd: "/w", sessionId: "gone" } },
      ...opened(MOCK, "a", "b"),
    );
    expect(state.focused).toEqual({ agentId: "mock", cwd: "/w", sessionId: "a" });
  });

  test("a remembered focus on another agent still finds it", () => {
    const wanted = { agentId: "gemini", cwd: "/w", sessionId: "x" };
    const state = run({ ...EMPTY, restoring: wanted }, ...opened(MOCK, "a"), ...opened(GEMINI, "x"));
    expect(state.focused).toEqual(wanted);
  });
});

describe("a connection going away", () => {
  test("takes its tabs, threads and terminals with it", () => {
    const state = run(
      EMPTY,
      ...opened(MOCK, "a"),
      ...opened(GEMINI, "x"),
      { type: "connectionClosed", key: MOCK },
    );

    expect(state.order).toEqual([GEMINI]);
    expect(state.threads).not.toHaveProperty(MOCK);
    expect(state.terminals).not.toHaveProperty(MOCK);
    expect(tabsOf(state)).toEqual([`${GEMINI}/x`]);
  });

  test("frames are the connection's, and are bounded", () => {
    // The inspector's job is showing one socket, so the log is not sharded by
    // conversation — but it is a live agent's output and cannot grow forever.
    const state = run(EMPTY, ...opened(MOCK, "a"));
    const many = Array.from({ length: 2100 }, (_, seq): Action => ({
      type: "frame",
      key: MOCK,
      entry: { seq, at: 0, direction: "agentToClient", line: "{}", intercepted: false },
    }));
    const full = run(state, ...many);

    expect(full.connections[MOCK]?.frames).toHaveLength(2000);
    expect(full.connections[MOCK]?.frames[0]?.seq).toBe(100);
  });
});
