import { describe, expect, test } from "vitest";

import { choiceStore, resumeStore } from "./resume";

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

describe("the remembered choice", () => {
  test("brings the tab back to the agent it was on", () => {
    // Without this the reload lands on the picker, and the conversation the
    // server kept alive is there but invisible.
    const store = choiceStore(fakeStorage());
    store.set({ agentId: "mock", cwd: "/w" });
    expect(store.get()).toEqual({ agentId: "mock", cwd: "/w" });
    store.clear();
    expect(store.get()).toBeUndefined();
  });

  test("something else's data under our key sends us to the picker", () => {
    const storage = fakeStorage();
    storage.setItem("mjx.connection", "not json");
    expect(choiceStore(storage).get()).toBeUndefined();

    storage.setItem("mjx.connection", JSON.stringify({ agentId: 7 }));
    expect(choiceStore(storage).get()).toBeUndefined();
  });
});
