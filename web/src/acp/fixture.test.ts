/**
 * Folds the same recorded turn the Rust model folds.
 *
 * `fixtures/session-updates.jsonl` is a real `session/update` stream. The
 * assertions here deliberately mirror `crates/mjx-acp-thread/tests/fixture.rs`
 * number for number: two implementations of the same folding rules are only
 * worth having if something notices when they disagree.
 *
 * Regenerate with:  node web/scripts/capture-fixture.mjs
 */

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, test } from "vitest";
import type { SessionNotification, SessionUpdate } from "@agentclientprotocol/sdk";

import { threadFromReplay } from "./replay";
import { appendUserPrompt, applyUpdate } from "./thread";
import { chunkText, emptyThread, type Thread } from "./types";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../../../fixtures");
const fixturePath = join(fixtures, "session-updates.jsonl");

const updates: SessionUpdate[] = readFileSync(fixturePath, "utf8")
  .split("\n")
  .filter((line) => line.trim().length > 0)
  .map((line) => JSON.parse(line) as SessionUpdate);

function folded(): Thread {
  // The capture starts after the prompt was sent, so mirror what the client
  // does: the user's message is on screen before the agent says anything.
  let thread = appendUserPrompt(emptyThread(), "fix the median bug in stats.js");
  for (const update of updates) {
    thread = applyUpdate(thread, { sessionId: "s1", update } as SessionNotification);
  }
  return thread;
}

describe("the recorded turn", () => {
  test("every update is present", () => {
    expect(updates).toHaveLength(69);
  });

  test("folds to the same shape the Rust model produces", () => {
    const thread = folded();

    // Mirrors `the_recorded_turn_folds_to_a_stable_shape`.
    expect(thread.entries).toHaveLength(8);
    expect(thread.entries.map((e) => e.type)).toEqual([
      "user",
      "assistant",
      "toolCall",
      "assistant",
      "toolCall",
      "assistant",
      "toolCall",
      "assistant",
    ]);

    const first = thread.entries[1];
    if (first?.type !== "assistant") throw new Error("expected an assistant message");
    expect(first.chunks).toHaveLength(2);
    expect(first.chunks[0]?.kind).toBe("thought");
    expect(first.chunks[1]?.kind).toBe("message");
    expect(chunkText(first.chunks[0]!)).toMatch(/^Let me look at/);
    expect(chunkText(first.chunks[1]!)).toContain("stats.js");

    const calls = thread.entries
      .filter((e) => e.type === "toolCall")
      .map((e) => (e.type === "toolCall" ? e.toolCall : null!));
    expect(calls.map((c) => c.id)).toEqual(["call_read", "call_edit", "call_test"]);
    for (const call of calls) {
      expect(call.status, `${call.id} did not finish`).toBe("completed");
    }

    const edit = calls.find((c) => c.id === "call_edit")!;
    const diff = edit.content[0];
    if (diff?.type !== "diff") throw new Error("the edit tool call has no diff");
    expect(diff.oldText).toContain("return sorted[mid];");
    expect(diff.newText).toContain("/ 2");

    expect(thread.plan).toHaveLength(3);
    expect(thread.plan.every((entry) => entry.status === "completed")).toBe(true);

    expect(thread.usage?.used).toBe(4812);
    expect(thread.usage?.size).toBe(200000);
    expect(thread.availableCommands).toHaveLength(2);

    // The agent moved itself onto a more capable model partway through, and the
    // fold followed it.
    expect(thread.configOptions).toHaveLength(3);
    expect(thread.configOptions[0]).toMatchObject({ id: "model", currentValue: "mock-opus" });
  });

  test("the prompt appears once, not twice", () => {
    // The client adds it optimistically and the agent may echo it back.
    const users = folded().entries.filter((e) => e.type === "user");
    expect(users).toHaveLength(1);
  });

  test("folding is deterministic", () => {
    expect(JSON.stringify(folded())).toBe(JSON.stringify(folded()));
  });

  test("the server's replay rebuilds exactly what this model folds", () => {
    // The other assertions here check that both models fold to the same
    // *shape*. This one checks that the shape survives the wire: a reload gets
    // the thread as Rust serialized it, and anything the adapter mistranslates
    // is a conversation that comes back wrong. `toEqual` rather than
    // `toStrictEqual` on purpose — Rust omits a field where the browser holds
    // `undefined`, and those mean the same thing.
    //
    // Regenerate with:  MJX_UPDATE_FIXTURES=1 cargo test -p mjx-acp-thread
    const replayed: unknown = JSON.parse(
      readFileSync(join(fixtures, "session-thread.json"), "utf8"),
    );
    expect(threadFromReplay(replayed)).toEqual(folded());
  });
});
