import { describe, expect, test } from "vitest";
import { diffLines, diffStat } from "./diff";

/** Renders a diff the way `git diff` would, for compact assertions. */
function render(oldText: string, newText: string, context?: number): string {
  return diffLines(oldText, newText, context)
    .map((line) => {
      const sign = line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " ";
      return `${sign}${line.text}`;
    })
    .join("\n");
}

describe("diffLines", () => {
  test("identical text produces no changes", () => {
    const lines = diffLines("a\nb\nc", "a\nb\nc");
    expect(lines.every((l) => l.kind === "context")).toBe(true);
    expect(diffStat(lines)).toEqual({ added: 0, removed: 0 });
  });

  test("a replaced line shows as a removal and an addition", () => {
    expect(render("a\nb\nc", "a\nB\nc")).toBe(" a\n-b\n+B\n c");
  });

  test("insertions and deletions are found rather than rewriting everything", () => {
    // A naive diff would report every line after the insertion as changed.
    const lines = diffLines("a\nc", "a\nb\nc");
    expect(diffStat(lines)).toEqual({ added: 1, removed: 0 });

    const removed = diffLines("a\nb\nc", "a\nc");
    expect(diffStat(removed)).toEqual({ added: 0, removed: 1 });
  });

  test("line numbers track each side independently", () => {
    const lines = diffLines("a\nb\nc", "a\nB\nc");
    const removedLine = lines.find((l) => l.kind === "removed");
    const addedLine = lines.find((l) => l.kind === "added");

    expect(removedLine?.oldLine).toBe(2);
    expect(removedLine?.newLine).toBeUndefined();
    expect(addedLine?.newLine).toBe(2);
    expect(addedLine?.oldLine).toBeUndefined();
  });

  test("far-away context is elided into a single marker", () => {
    // An agent changing one line of a long file should not produce a wall of
    // unchanged text.
    const before = Array.from({ length: 40 }, (_, i) => `line${i}`).join("\n");
    const after = before.replace("line20", "CHANGED");
    const lines = diffLines(before, after, 2);

    expect(lines.length).toBeLessThan(15);
    expect(lines.filter((l) => l.text === "⋯")).toHaveLength(2);
    expect(diffStat(lines)).toEqual({ added: 1, removed: 1 });
  });

  test("empty text on either side is handled", () => {
    expect(diffStat(diffLines("", "a\nb"))).toEqual({ added: 2, removed: 0 });
    expect(diffStat(diffLines("a\nb", ""))).toEqual({ added: 0, removed: 2 });
    expect(diffLines("", "")).toEqual([]);
  });

  test("the demo fix renders as the change it is", () => {
    const before = [
      "export function median(xs) {",
      "  const mid = Math.floor(sorted.length / 2);",
      "  return sorted[mid];",
      "}",
    ].join("\n");
    const after = [
      "export function median(xs) {",
      "  const mid = Math.floor(sorted.length / 2);",
      "  return sorted.length % 2 === 0",
      "    ? (sorted[mid - 1] + sorted[mid]) / 2",
      "    : sorted[mid];",
      "}",
    ].join("\n");

    expect(diffStat(diffLines(before, after))).toEqual({ added: 3, removed: 1 });
  });
});
