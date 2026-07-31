/** The plan, session controls and usage — everything that isn't the timeline. */

import type { ConnectionStatus } from "../acp/agentConnection";
import { mcpServersOf } from "../acp/mcp";
import type { AgentInfo, Thread } from "../acp/types";
import { ConfigOptions } from "./ConfigOptions";

const PLAN_MARK: Record<string, string> = {
  pending: "○",
  in_progress: "◐",
  completed: "●",
};

export function Sidebar({
  thread,
  agentInfo,
  status,
  onSetMode,
  onSetConfigOption,
}: {
  thread: Thread;
  agentInfo?: AgentInfo;
  status: ConnectionStatus;
  onSetMode(modeId: string): void;
  onSetConfigOption(configId: string, value: string | boolean): void;
}) {
  // Normalized here rather than trusted: `_mjx/agent/info` is cast, not parsed.
  const mcpServers = mcpServersOf(agentInfo?.mcpServers);

  return (
    <aside className="sidebar">
      <section className="sidebar__section">
        <h2>Agent</h2>
        {agentInfo ? (
          <>
            <p className="sidebar__agent">{agentInfo.name}</p>
            <p className="dim sidebar__cwd" title={agentInfo.cwd}>
              {agentInfo.cwd}
            </p>
            {/* Showing the command makes it obvious what is actually running,
                which matters for a server that spawns processes on request. */}
            <code className="sidebar__command">{agentInfo.command.join(" ")}</code>
          </>
        ) : (
          <p className="dim">{status.state === "connecting" ? "Connecting…" : "—"}</p>
        )}
      </section>

      {mcpServers.length > 0 && (
        <section className="sidebar__section">
          <h2>MCP servers</h2>
          <ul className="sidebar__mcp">
            {mcpServers.map((server) => (
              <li
                key={server.name}
                // Shown rather than hidden when the agent did not get it: a
                // configured server missing from this list would look like a
                // typo in the config, and the reason is the useful part.
                className={server.unavailable ? "sidebar__mcp-item is-unavailable" : "sidebar__mcp-item"}
              >
                <span className="sidebar__mcp-name">{server.name}</span>
                <span className="sidebar__mcp-transport">{server.transport}</span>
                <code className="sidebar__mcp-target" title={server.target}>
                  {server.target}
                </code>
                {/* Names only. The server never sends the values, so there is
                    nothing here to leak — see `acp/mcp.ts`. */}
                {server.secrets.length > 0 && (
                  <span className="dim">carries {server.secrets.join(", ")}</span>
                )}
                {server.unavailable && <span className="dim">not offered: {server.unavailable}</span>}
              </li>
            ))}
          </ul>
        </section>
      )}

      {thread.modes && thread.modes.availableModes.length > 0 && (
        <section className="sidebar__section">
          <h2>Mode</h2>
          <select
            value={thread.modes.currentModeId}
            onChange={(event) => onSetMode(event.target.value)}
          >
            {thread.modes.availableModes.map((mode) => (
              <option key={mode.id} value={mode.id}>
                {mode.name}
              </option>
            ))}
          </select>
          <p className="dim">
            {thread.modes.availableModes.find((m) => m.id === thread.modes?.currentModeId)
              ?.description ?? ""}
          </p>
        </section>
      )}

      <ConfigOptions options={thread.configOptions} onSetConfigOption={onSetConfigOption} />

      {thread.plan.length > 0 && (
        <section className="sidebar__section">
          <h2>Plan</h2>
          <ol className="plan">
            {thread.plan.map((entry, index) => (
              <li key={index} className={`plan__entry plan__entry--${entry.status}`}>
                <span className="plan__mark" aria-hidden="true">
                  {PLAN_MARK[entry.status] ?? "○"}
                </span>
                {entry.content}
              </li>
            ))}
          </ol>
        </section>
      )}

      {thread.availableCommands.length > 0 && (
        <section className="sidebar__section">
          <h2>Commands</h2>
          <ul className="commands">
            {thread.availableCommands.map((command) => (
              <li key={command.name}>
                <code>
                  /{command.name}
                  {/* A command that takes an argument says so here too, not
                      only in the composer — this is where they are browsed. */}
                  {command.input && ` <${command.input.hint}>`}
                </code>
                <span className="dim"> {command.description}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {thread.usage && (
        <section className="sidebar__section">
          <h2>Usage</h2>
          <div
            className="usage-bar"
            role="meter"
            aria-valuenow={thread.usage.used}
            aria-valuemax={thread.usage.size}
          >
            <div
              className="usage-bar__fill"
              style={{
                width: `${Math.min(100, (thread.usage.used / Math.max(1, thread.usage.size)) * 100)}%`,
              }}
            />
          </div>
          <p className="dim">
            {thread.usage.used.toLocaleString()} / {thread.usage.size.toLocaleString()} tokens
            {thread.usage.cost &&
              ` · ${thread.usage.cost.amount.toFixed(4)} ${thread.usage.cost.currency}`}
          </p>
        </section>
      )}
    </aside>
  );
}
