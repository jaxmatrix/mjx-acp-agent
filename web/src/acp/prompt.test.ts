import { describe, expect, test } from "vitest";

import { mentionMarkdown, promptBlocks } from "./prompt";

describe("what gets sent", () => {
  test("text with no mentions is one block, unchanged", () => {
    // The behaviour before mentions existed, byte for byte.
    expect(promptBlocks("fix the median bug")).toEqual([
      { type: "text", text: "fix the median bug" },
    ]);
  });

  test("a mention becomes a resource_link between the text around it", () => {
    expect(promptBlocks("look at [@stats.js](file:///w/stats.js) please")).toEqual([
      { type: "text", text: "look at " },
      { type: "resource_link", uri: "file:///w/stats.js", name: "stats.js" },
      { type: "text", text: " please" },
    ]);
  });

  test("a mention on its own is one block", () => {
    expect(promptBlocks("[@stats.js](file:///w/stats.js)")).toEqual([
      { type: "resource_link", uri: "file:///w/stats.js", name: "stats.js" },
    ]);
  });

  test("several mentions all survive, in order", () => {
    const blocks = promptBlocks("[@a.js](file:///w/a.js) and [@b.js](file:///w/b.js)");
    expect(blocks.map((block) => (block.type === "resource_link" ? block.uri : block))).toEqual([
      "file:///w/a.js",
      { type: "text", text: " and " },
      "file:///w/b.js",
    ]);
  });

  test("a link whose uri is not a mention stays literal text", () => {
    // Never put a resource_link on the wire that this project cannot read back.
    const text = "see [@docs](notascheme) for more";
    expect(promptBlocks(text)).toEqual([{ type: "text", text }]);
  });

  test("an ordinary markdown link is not a mention", () => {
    const text = "see [the docs](https://example.com/) for more";
    expect(promptBlocks(text)).toEqual([{ type: "text", text }]);
  });

  test("a url mention becomes a fetch link", () => {
    expect(promptBlocks("[@https://example.com/](https://example.com/)")).toEqual([
      { type: "resource_link", uri: "https://example.com/", name: "https://example.com/" },
    ]);
  });

  test("no characters are dropped", () => {
    const text = "  spaces  and\nnewlines  ";
    expect(promptBlocks(text)).toEqual([{ type: "text", text }]);
  });
});

describe("the markdown a completion inserts", () => {
  test("parentheses in a filename are escaped, or the link would not parse", () => {
    // `file:///tmp/a(1).txt` inside `(...)` ends the link at the first `)`.
    const markdown = mentionMarkdown({ variant: "file", absPath: "/tmp/a(1).txt" });
    expect(markdown).toBe("[@a(1).txt](file:///tmp/a%281%29.txt)");

    // And it comes back out as the canonical URI, unescaped, on the wire.
    expect(promptBlocks(markdown)).toEqual([
      { type: "resource_link", uri: "file:///tmp/a(1).txt", name: "a(1).txt" },
    ]);
  });

  test("a directory keeps its trailing slash", () => {
    const markdown = mentionMarkdown({ variant: "directory", absPath: "/w/src" });
    expect(markdown).toBe("[@src](file:///w/src/)");
    expect(promptBlocks(markdown)).toEqual([
      { type: "resource_link", uri: "file:///w/src/", name: "src" },
    ]);
  });

  test("a space in a filename round trips", () => {
    const markdown = mentionMarkdown({ variant: "file", absPath: "/w/my notes.md" });
    expect(promptBlocks(markdown)).toEqual([
      { type: "resource_link", uri: "file:///w/my%20notes.md", name: "my notes.md" },
    ]);
  });
});
