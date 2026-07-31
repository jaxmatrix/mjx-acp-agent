/**
 * The MCP servers the server configured, as the sidebar reads them.
 *
 * Mirrors `ext::McpServerInfo` in `crates/mjx-acp-core/src/ext.rs`. The payload
 * arrives as an unchecked cast off the socket, so it is normalized here rather
 * than trusted in a component: a field of the wrong type would otherwise become
 * `undefined.join` at render time, which loses the whole page.
 *
 * There is deliberately no place to put a credential. The server sends the
 * *names* of the environment variables and headers a server carries and never
 * their values — which is the reason it injects `mcpServers` itself instead of
 * asking the browser to send them.
 */

/** One configured MCP server. */
export interface McpServer {
  name: string;
  /** `stdio`, `http`, `sse` or `acp`. Shown as written; not switched on. */
  transport: string;
  /** The command line, or the URL. */
  target: string;
  /** Names only — never values. */
  secrets: string[];
  /** Why the agent did not get it, or undefined when it did. */
  unavailable?: string;
}

/** Reads the MCP list off an `_mjx/agent/info` payload. */
export function mcpServersOf(value: unknown): McpServer[] {
  if (!Array.isArray(value)) return [];
  return value.filter(isRecord).flatMap((entry) => {
    // A server with no name is one nothing can be said about, including which
    // one it is.
    const name = typeof entry.name === "string" ? entry.name : "";
    if (!name) return [];
    return [
      {
        name,
        transport: typeof entry.transport === "string" ? entry.transport : "stdio",
        target: typeof entry.target === "string" ? entry.target : "",
        secrets: Array.isArray(entry.secrets)
          ? entry.secrets.filter((secret): secret is string => typeof secret === "string")
          : [],
        unavailable: typeof entry.unavailable === "string" ? entry.unavailable : undefined,
      },
    ];
  });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
