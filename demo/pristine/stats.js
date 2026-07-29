// A few summary statistics, with one bug in it on purpose.

export function mean(xs) {
  if (xs.length === 0) return NaN;
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

export function median(xs) {
  if (xs.length === 0) return NaN;
  const sorted = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  // BUG: for even-length input this returns the upper of the two middle
  // values instead of their average.
  return sorted[mid];
}

export function stddev(xs) {
  if (xs.length < 2) return NaN;
  const m = mean(xs);
  const variance = xs.reduce((a, x) => a + (x - m) ** 2, 0) / (xs.length - 1);
  return Math.sqrt(variance);
}
