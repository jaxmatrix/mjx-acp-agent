import { describe, expect, test } from "vitest";

import { rank, score } from "./rank";
import { applyCompletion, triggerAt } from "./trigger";

/** `text` with `|` marking the caret. */
function at(marked: string) {
  const caret = marked.indexOf("|");
  return triggerAt(marked.replace("|", ""), caret);
}

describe("an @-mention trigger", () => {
  test("opens on @ at the start of the input", () => {
    expect(at("@sta|")).toEqual({ kind: "mention", range: [0, 4], query: "sta" });
  });

  test("opens on @ after a space or a newline", () => {
    expect(at("look at @sta|")).toEqual({ kind: "mention", range: [8, 12], query: "sta" });
    expect(at("look at\n@|")).toEqual({ kind: "mention", range: [8, 9], query: "" });
  });

  test("does not open mid-word", () => {
    // An address is not a mention, and this is the whole reason the character
    // before @ is checked.
    expect(at("mail user@host|")).toBeNull();
  });

  test("closes once a space is typed", () => {
    expect(at("@stats.js and |")).toBeNull();
  });

  test("accepts the characters a path is made of", () => {
    expect(at("@web/src/stats-1.test.js|")).toMatchObject({ query: "web/src/stats-1.test.js" });
  });

  test("does not reopen inside a completed mention", () => {
    // The `(` of the markdown link is not part of a query.
    expect(at("[@stats.js](file:///w/stats.js)|")).toBeNull();
  });
});

describe("a /-command trigger", () => {
  test("opens on a bare slash command", () => {
    expect(at("/te|")).toEqual({
      kind: "command",
      range: [0, 3],
      name: "te",
      argument: null,
    });
  });

  test("survives the space that used to end it", () => {
    // The old check was `!text.includes(" ")`, which is why arguments have
    // never had anywhere to appear.
    expect(at("/test |")).toMatchObject({ name: "test", argument: "" });
    expect(at("/test unit|")).toMatchObject({ name: "test", argument: "unit" });
  });

  test("does not open on a path in the middle of prose", () => {
    expect(at("look in src/stats.js|")).toBeNull();
  });

  test("does not open on a lone slash", () => {
    expect(at("/ |")).toBeNull();
  });

  test("a mention wins over a command on the same line", () => {
    // Both are present; the caret is in the mention.
    expect(at("/test @sta|")).toMatchObject({ kind: "mention", query: "sta" });
  });
});

describe("applying a completion", () => {
  test("replaces the trigger and puts the caret after it", () => {
    const trigger = at("look at @sta|");
    expect(trigger).not.toBeNull();
    const applied = applyCompletion("look at @sta", trigger!.range, "[@stats.js](file:///w/s.js) ");
    expect(applied.text).toBe("look at [@stats.js](file:///w/s.js) ");
    expect(applied.caret).toBe(applied.text.length);
  });

  test("keeps whatever followed the trigger", () => {
    const applied = applyCompletion("@sta please", [0, 4], "[@s](file:///s) ");
    expect(applied.text).toBe("[@s](file:///s)  please");
  });
});

describe("ranking candidates", () => {
  test("a subsequence match is found, a missing character is not", () => {
    expect(score("web/src/stats.js", "wsstats")).not.toBeNull();
    expect(score("web/src/stats.js", "zzz")).toBeNull();
  });

  test("an adjacent run beats scattered letters", () => {
    expect(score("src/stats.js", "stats")!).toBeGreaterThan(score("s-t-a-t-s.js", "stats")!);
  });

  test("a shallow path beats a deep one", () => {
    expect(score("stats.js", "stats")!).toBeGreaterThan(score("a/b/c/d/stats.js", "stats")!);
  });

  test("an empty query keeps the server's order", () => {
    const paths = ["b.js", "a.js"];
    expect(rank(paths, "", (p) => p)).toEqual(paths);
  });

  test("ranking drops what does not match at all", () => {
    expect(rank(["stats.js", "readme.md"], "stats", (p) => p)).toEqual(["stats.js"]);
  });
});
