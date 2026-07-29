/**
 * A line diff, for rendering `ToolCallContent` of type `diff`.
 *
 * Written out rather than pulled in: a merge view from an editor library is a
 * large dependency for something that only ever renders a read-only unified
 * diff, and the whole algorithm is the twenty lines below.
 */

/** One line of a rendered diff. */
export interface DiffLine {
  kind: "context" | "added" | "removed";
  /** Line number in the old text, if the line exists there. */
  oldLine?: number;
  /** Line number in the new text, if the line exists there. */
  newLine?: number;
  text: string;
}

/**
 * A unified diff of `oldText` and `newText`, with `context` lines of
 * unchanged text kept around each change.
 */
export function diffLines(oldText: string, newText: string, context = 3): DiffLine[] {
  const before = oldText.length ? oldText.split("\n") : [];
  const after = newText.length ? newText.split("\n") : [];
  return trimToContext(walk(before, after, lcs(before, after)), context);
}

/**
 * Longest-common-subsequence table.
 *
 * O(n·m) in both time and memory, which is fine for a file an agent just
 * rewrote and would not be for a repository-wide diff.
 */
function lcs(before: string[], after: string[]): number[][] {
  const table: number[][] = Array.from({ length: before.length + 1 }, () =>
    new Array<number>(after.length + 1).fill(0),
  );

  for (let i = before.length - 1; i >= 0; i -= 1) {
    for (let j = after.length - 1; j >= 0; j -= 1) {
      table[i]![j] =
        before[i] === after[j]
          ? table[i + 1]![j + 1]! + 1
          : Math.max(table[i + 1]![j]!, table[i]![j + 1]!);
    }
  }
  return table;
}

/** Walks the table into a line-by-line diff. */
function walk(before: string[], after: string[], table: number[][]): DiffLine[] {
  const lines: DiffLine[] = [];
  let i = 0;
  let j = 0;

  while (i < before.length && j < after.length) {
    if (before[i] === after[j]) {
      lines.push({ kind: "context", oldLine: i + 1, newLine: j + 1, text: before[i]! });
      i += 1;
      j += 1;
    } else if (table[i + 1]![j]! >= table[i]![j + 1]!) {
      lines.push({ kind: "removed", oldLine: i + 1, text: before[i]! });
      i += 1;
    } else {
      lines.push({ kind: "added", newLine: j + 1, text: after[j]! });
      j += 1;
    }
  }
  while (i < before.length) {
    lines.push({ kind: "removed", oldLine: i + 1, text: before[i]! });
    i += 1;
  }
  while (j < after.length) {
    lines.push({ kind: "added", newLine: j + 1, text: after[j]! });
    j += 1;
  }
  return lines;
}

/**
 * Drops unchanged lines more than `context` away from any change.
 *
 * An agent rewriting one line of a 500-line file produces a diff that is 499
 * lines of noise without this.
 */
function trimToContext(lines: DiffLine[], context: number): DiffLine[] {
  const near = new Set<number>();
  lines.forEach((line, index) => {
    if (line.kind === "context") return;
    for (let i = index - context; i <= index + context; i += 1) near.add(i);
  });

  const kept: DiffLine[] = [];
  let skipping = false;
  lines.forEach((line, index) => {
    if (near.has(index)) {
      kept.push(line);
      skipping = false;
    } else if (!skipping) {
      // One marker per elided run, rather than one per dropped line.
      kept.push({ kind: "context", text: "⋯" });
      skipping = true;
    }
  });
  return kept;
}

/** How many lines a diff adds and removes. */
export function diffStat(lines: DiffLine[]): { added: number; removed: number } {
  return {
    added: lines.filter((l) => l.kind === "added").length,
    removed: lines.filter((l) => l.kind === "removed").length,
  };
}
