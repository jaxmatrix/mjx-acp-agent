import { describe, expect, test } from "vitest";

import { connectionsStore, focusStore, resumeStore, sessionStore } from "./resume";

/** Enough of `Storage` to stand in for one, since there is no DOM here. */
function fakeStorage(): Storage {
  const items = new Map<string, string>();
  return {
    get length() {
      return items.size;
    },
    clear: () => items.clear(),
    getItem: (k) => items.get(k) ?? null,
    key: (i) => [...items.keys()][i] ?? null,
    removeItem: (k) => void items.delete(k),
    setItem: (k, v) => void items.set(k, v),
  };
}

describe("the resume store", () => {
  test("gives back the id it was given", () => {
    const store = resumeStore(fakeStorage());
    store.set("mock", "/w", "c1");
    expect(store.get("mock", "/w")).toBe("c1");
    store.clear("mock", "/w");
    expect(store.get("mock", "/w")).toBeUndefined();
  });

  test("an id belongs to one agent in one directory", () => {
    // The server refuses a resume for a different agent or directory, so
    // offering one would only ever cost a pointless round trip.
    const store = resumeStore(fakeStorage());
    store.set("mock", "/w", "c1");
    expect(store.get("gemini", "/w")).toBeUndefined();
    expect(store.get("mock", "/elsewhere")).toBeUndefined();
  });

  test("storage that refuses to work costs resuming, not the app", () => {
    // Private browsing and some corporate policies make these throw rather
    // than return.
    const hostile: Storage = {
      ...fakeStorage(),
      getItem: () => {
        throw new Error("nope");
      },
      setItem: () => {
        throw new Error("nope");
      },
      removeItem: () => {
        throw new Error("nope");
      },
    };
    const store = resumeStore(hostile);

    expect(() => store.set("mock", "/w", "c1")).not.toThrow();
    expect(store.get("mock", "/w")).toBeUndefined();
    expect(() => store.clear("mock", "/w")).not.toThrow();
  });

  test("no storage at all is not an error", () => {
    const store = resumeStore(undefined);
    expect(() => store.set("mock", "/w", "c1")).not.toThrow();
    expect(store.get("mock", "/w")).toBeUndefined();
  });
});

describe("the remembered connections", () => {
  test("bring the tab back to the agents it was on", () => {
    // Without this the reload lands on the picker, and the conversations the
    // server kept alive are there but invisible.
    const store = connectionsStore(fakeStorage());
    store.set([
      { agentId: "mock", cwd: "/w" },
      { agentId: "gemini", cwd: "/w" },
    ]);
    expect(store.get()).toEqual([
      { agentId: "mock", cwd: "/w" },
      { agentId: "gemini", cwd: "/w" },
    ]);
    store.set([]);
    expect(store.get()).toEqual([]);
  });

  test("something else's data under our key sends us to the picker", () => {
    const storage = fakeStorage();
    storage.setItem("mjx.connections", "not json");
    expect(connectionsStore(storage).get()).toEqual([]);

    // One bad entry costs that entry. The others name real agents, and
    // dropping them would close a conversation that is still running.
    storage.setItem(
      "mjx.connections",
      JSON.stringify([{ agentId: 7 }, { agentId: "mock", cwd: "/w" }]),
    );
    expect(connectionsStore(storage).get()).toEqual([{ agentId: "mock", cwd: "/w" }]);
  });
});

describe("the remembered focus", () => {
  test("is the one conversation of many to come back to", () => {
    const store = focusStore(fakeStorage());
    store.set({ agentId: "mock", cwd: "/w", sessionId: "s2" });
    expect(store.get()).toEqual({ agentId: "mock", cwd: "/w", sessionId: "s2" });
  });

  test("a focus without a session is no focus at all", () => {
    const storage = fakeStorage();
    storage.setItem("mjx.focus", JSON.stringify({ agentId: "mock", cwd: "/w" }));
    expect(focusStore(storage).get()).toBeUndefined();
  });
});

describe("the remembered sessions", () => {
  test("are the conversations open, not the one the connection started", () => {
    // They come apart as soon as one is opened from the history: the relay
    // still answers a repeat `session/new` with the connection's original
    // session, so this is the only record of what else is being looked at.
    const store = sessionStore(fakeStorage());
    store.set("mock", "/w", ["s2", "s3"]);
    expect(store.get("mock", "/w")).toEqual(["s2", "s3"]);
    store.clear("mock", "/w");
    expect(store.get("mock", "/w")).toEqual([]);
  });

  test("keep their order, so the tabs come back where they were", () => {
    const store = sessionStore(fakeStorage());
    store.set("mock", "/w", ["c", "a", "b"]);
    expect(store.get("mock", "/w")).toEqual(["c", "a", "b"]);
  });

  test("are kept apart from the connection id", () => {
    const storage = fakeStorage();
    resumeStore(storage).set("mock", "/w", "c1");
    sessionStore(storage).set("mock", "/w", ["s2"]);

    expect(resumeStore(storage).get("mock", "/w")).toBe("c1");
    expect(sessionStore(storage).get("mock", "/w")).toEqual(["s2"]);
  });

  test("something else's data under our key costs the tabs, not the app", () => {
    // Restoring a conversation out of a shape we misread would open the wrong
    // one, so anything unrecognised means starting with none.
    const storage = fakeStorage();
    storage.setItem("mjx.sessions.mock./w", "not json");
    expect(sessionStore(storage).get("mock", "/w")).toEqual([]);

    storage.setItem("mjx.sessions.mock./w", JSON.stringify(["ok", 7]));
    expect(sessionStore(storage).get("mock", "/w")).toEqual([]);
  });
});
