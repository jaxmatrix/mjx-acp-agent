import { test } from "node:test";
import assert from "node:assert/strict";
import { mean, median, stddev } from "./stats.js";

test("mean", () => {
  assert.equal(mean([1, 2, 3, 4]), 2.5);
  assert.ok(Number.isNaN(mean([])));
});

test("median, odd length", () => {
  assert.equal(median([3, 1, 2]), 2);
});

test("median, even length", () => {
  // Fails until the off-by-one in median() is fixed.
  assert.equal(median([1, 2, 3, 4]), 2.5);
});

test("stddev", () => {
  assert.ok(Math.abs(stddev([2, 4, 4, 4, 5, 5, 7, 9]) - 2.13809) < 1e-4);
});
