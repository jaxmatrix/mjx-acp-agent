import { describe, expect, test } from "vitest";

import { addTerminal, appendTerminalOutput, setTerminalExit } from "./terminals";

describe("terminals", () => {
  test("output accumulates and exit is recorded", () => {
    let terminals = addTerminal(
      {},
      { id: "t1", command: "node", args: ["--test"], cwd: "/w" },
    );
    terminals = appendTerminalOutput(terminals, "t1", new Uint8Array([1, 2]), false);
    terminals = appendTerminalOutput(terminals, "t1", new Uint8Array([3]), true);
    terminals = setTerminalExit(terminals, "t1", 0, null);

    const terminal = terminals.t1;
    expect(terminal?.output).toHaveLength(2);
    expect(terminal?.truncated).toBe(true);
    expect(terminal?.exitCode).toBe(0);
  });

  test("output for an unknown terminal is dropped without throwing", () => {
    expect(appendTerminalOutput({}, "ghost", new Uint8Array([1]), false)).toEqual({});
  });
});
