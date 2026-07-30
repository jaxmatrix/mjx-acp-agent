/**
 * Ranking mention candidates.
 *
 * The server narrows by substring and caps the answer; this decides what to
 * show first. A subsequence match rather than a substring one, so `wsstats`
 * finds `web/src/stats.js` — that is the point of a fuzzy picker.
 */

/**
 * How well `candidate` matches `query`, or null when it does not.
 *
 * Higher is better. Nothing about the absolute value is meaningful; only the
 * order is.
 */
export function score(candidate: string, query: string): number | null {
  if (query.length === 0) return 0;

  const haystack = candidate.toLowerCase();
  const needle = query.toLowerCase();

  let total = 0;
  let at = 0;
  let previous = -2;
  for (const char of needle) {
    const found = haystack.indexOf(char, at);
    if (found < 0) return null;
    // A run of adjacent characters is a much better match than the same
    // characters scattered across a path.
    if (found === previous + 1) total += 8;
    // Matching at a path or word boundary usually means the user typed the
    // start of a name rather than a letter from the middle of one.
    if (found === 0 || "/\\-_. ".includes(haystack[found - 1] ?? "")) total += 4;
    previous = found;
    at = found + 1;
  }

  // Prefer what is close to the surface, and short. A deep path that happens
  // to contain the letters is rarely what was meant.
  total -= (haystack.match(/[/\\]/g)?.length ?? 0) * 2;
  total -= Math.floor(haystack.length / 16);
  return total;
}

/** Sorts by score, keeping the server's order among equals. */
export function rank<T>(items: T[], query: string, key: (item: T) => string): T[] {
  return items
    .map((item, index) => ({ item, index, score: score(key(item), query) }))
    .filter((scored): scored is { item: T; index: number; score: number } => scored.score !== null)
    .sort((a, b) => b.score - a.score || a.index - b.index)
    .map((scored) => scored.item);
}
