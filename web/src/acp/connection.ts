/**
 * The ACP client, over a WebSocket.
 *
 * The browser is an ordinary ACP client here — the same role a code editor
 * plays — and the server on the other end of the socket is an ordinary agent.
 * Nothing in this file knows the agent is really a subprocess two hops away.
 *
 * The exceptions are the `_mjx/*` notifications, which carry the things ACP has
 * no vocabulary for because they only exist when the client is remote: which
 * agent we got, its stderr, and the terminal and filesystem activity the server
 * handled on our behalf.
 */

/** How to reach the server. */
export interface ConnectOptions {
  agentId: string;
  cwd?: string;
  /**
   * A connection id from a previous socket, to rejoin the agent it started
   * rather than start another. An unknown or expired one is not an error — the
   * server says so on `_mjx/agent/info` and starts a fresh agent.
   */
  resume?: string;
}

/** The URL of the relay endpoint for `agentId`. */
export function websocketUrl({ agentId, cwd, resume }: ConnectOptions): string {
  const url = new URL("/ws", window.location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("agent", agentId);
  if (cwd) url.searchParams.set("cwd", cwd);
  if (resume) url.searchParams.set("resume", resume);
  return url.toString();
}

/** `_mjx/*` method names, mirroring `crates/mjx-acp-core/src/ext.rs`. */
export const ext = {
  agentInfo: "_mjx/agent/info",
  agentStderr: "_mjx/agent/stderr",
  terminalCreated: "_mjx/terminal/created",
  terminalOutput: "_mjx/terminal/output",
  terminalExit: "_mjx/terminal/exit",
  fsWrote: "_mjx/fs/wrote",
  inspectorFrame: "_mjx/inspector/frame",
  sessionReplay: "_mjx/session/replay",
  sessionTurnEnded: "_mjx/session/turn_ended",
  connectionTakenOver: "_mjx/connection/taken_over",
  authRequired: "_mjx/auth/required",
  authState: "_mjx/auth/state",
  authAttempt: "_mjx/auth/attempt",
  authProgress: "_mjx/auth/progress",
  terminalInput: "_mjx/terminal/input",
  terminalResize: "_mjx/terminal/resize",
} as const;

/** Encodes keystrokes the way `_mjx/terminal/input` wants them. */
export function encodeChunk(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

/** Decodes a base64 terminal chunk into the bytes xterm.js wants. */
export function decodeChunk(base64: string): Uint8Array {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}
