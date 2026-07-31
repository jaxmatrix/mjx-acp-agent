/**
 * The terminals the server started for the agent, and their scrollback.
 *
 * Keyed by terminal id, and held by the *connection* rather than by any one
 * thread. Two reasons, and they point the same way:
 *
 * A terminal belongs to the workspace, not to the conversation. The server says
 * so — `SessionStore` keeps no terminal state, and the Rust `Thread` has no
 * field for one — and the protocol agrees: `_mjx/terminal/created`, `/output`
 * and `/exit` carry a terminal id and no session id, because a terminal id is
 * already unique across the whole workspace. There is nothing to route by.
 *
 * And a thread is replaced wholesale by a replay, which is why keeping
 * scrollback in one lost it: opening a past conversation and coming back left
 * the tool call on screen with its output gone.
 *
 * Rendering is unaffected. A terminal only ever reaches the screen through the
 * tool call that names it, and a tool call lives in exactly one thread.
 */

import type { Terminal } from "./types";

/** Every terminal on one connection. */
export type Terminals = Record<string, Terminal>;

/** Registers a terminal the server started for the agent. */
export function addTerminal(
  terminals: Terminals,
  terminal: Omit<Terminal, "output" | "truncated">,
): Terminals {
  return { ...terminals, [terminal.id]: { ...terminal, output: [], truncated: false } };
}

/** Appends streamed bytes to a terminal. */
export function appendTerminalOutput(
  terminals: Terminals,
  terminalId: string,
  chunk: Uint8Array,
  truncated: boolean,
): Terminals {
  const terminal = terminals[terminalId];
  if (!terminal) return terminals;
  return {
    ...terminals,
    [terminalId]: { ...terminal, output: [...terminal.output, chunk], truncated },
  };
}

/** Records a terminal's exit. */
export function setTerminalExit(
  terminals: Terminals,
  terminalId: string,
  exitCode: number | null | undefined,
  signal: string | null | undefined,
): Terminals {
  const terminal = terminals[terminalId];
  if (!terminal) return terminals;
  return { ...terminals, [terminalId]: { ...terminal, exitCode, signal } };
}
