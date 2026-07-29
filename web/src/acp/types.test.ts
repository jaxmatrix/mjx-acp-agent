/**
 * The shapes that have no discriminator on the wire.
 *
 * A select option's values are an untagged union of "flat list" and "grouped
 * list". Reading it wrong is silent — the selector renders with nothing in it
 * rather than throwing — so it is worth pinning.
 */

import { describe, expect, test } from "vitest";

import { selectShape } from "./types";

describe("selectShape", () => {
  test("a flat list of values is read as flat", () => {
    const shape = selectShape([
      { value: "sonnet", name: "Sonnet" },
      { value: "opus", name: "Opus" },
    ]);

    expect(shape.grouped).toBe(false);
    if (shape.grouped) throw new Error("unreachable");
    expect(shape.values.map((v) => v.value)).toEqual(["sonnet", "opus"]);
  });

  test("a grouped list is read as grouped", () => {
    const shape = selectShape([
      { group: "fast", name: "Fast", options: [{ value: "haiku", name: "Haiku" }] },
      { group: "capable", name: "Capable", options: [{ value: "opus", name: "Opus" }] },
    ]);

    expect(shape.grouped).toBe(true);
    if (!shape.grouped) throw new Error("unreachable");
    expect(shape.groups.flatMap((g) => g.options.map((o) => o.value))).toEqual(["haiku", "opus"]);
  });

  test("an empty list is flat, not grouped", () => {
    // An agent may offer an option with nothing to pick yet. Reading that as
    // grouped would render an empty `<optgroup>` instead of an empty select.
    expect(selectShape([]).grouped).toBe(false);
  });

  test("a group with no options is still a group", () => {
    // The `options` array is what marks a group, and an empty one is legal.
    const shape = selectShape([{ group: "empty", name: "Empty", options: [] }]);
    expect(shape.grouped).toBe(true);
  });
});
