/** Choose an agent and a working directory, then connect. */

import { useEffect, useState } from "react";
import type { CatalogEntry, WorkspaceRoot } from "../acp/types";

export function AgentPicker({
  onConnect,
}: {
  onConnect(agentId: string, cwd: string): void;
}) {
  const [agents, setAgents] = useState<CatalogEntry[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceRoot[]>([]);
  const [cwd, setCwd] = useState<string>();
  const [error, setError] = useState<string>();
  const [showAll, setShowAll] = useState(false);

  useEffect(() => {
    Promise.all([
      fetch("/api/agents").then((r) => r.json() as Promise<CatalogEntry[]>),
      fetch("/api/workspaces").then((r) => r.json() as Promise<WorkspaceRoot[]>),
    ])
      .then(([catalog, roots]) => {
        setAgents(catalog);
        setWorkspaces(roots);
        setCwd(roots[0]?.path);
      })
      .catch((cause: unknown) => {
        setError(cause instanceof Error ? cause.message : String(cause));
      });
  }, []);

  const ready = agents.filter((agent) => agent.availability.state === "ready");
  const rest = agents.filter((agent) => agent.availability.state !== "ready");
  const shown = showAll ? [...ready, ...rest] : ready;

  return (
    <main className="picker">
      <header className="picker__intro">
        <h1>mjx-acp-viewer</h1>
        <p className="dim">
          A web transport for the Agent Client Protocol. Pick an agent — the server starts it and
          relays the protocol to this page.
        </p>
      </header>

      {error && <p className="callout callout--error">Could not reach the server: {error}</p>}

      {workspaces.length > 1 && (
        <label className="picker__cwd">
          Working directory
          <select value={cwd} onChange={(event) => setCwd(event.target.value)}>
            {workspaces.map((root) => (
              <option key={root.path} value={root.path}>
                {root.name} — {root.path}
              </option>
            ))}
          </select>
        </label>
      )}

      <ul className="picker__list">
        {shown.map((agent) => {
          const available = agent.availability.state === "ready";
          return (
            <li key={agent.id}>
              <button
                type="button"
                className="agent-card"
                disabled={!available || !cwd}
                onClick={() => cwd && onConnect(agent.id, cwd)}
              >
                <span className="agent-card__name">
                  {agent.name}
                  {agent.isLocalOverride && (
                    <span className="pill" title="Configured in mjx.toml">
                      local
                    </span>
                  )}
                </span>
                {agent.description && <span className="dim">{agent.description}</span>}
                {/* The command is shown up front: this server spawns processes
                    on request, so what will run should never be a surprise. */}
                {agent.command && <code className="agent-card__command">{agent.command.join(" ")}</code>}
                {!available && <span className="agent-card__why">{whyNot(agent)}</span>}
              </button>
            </li>
          );
        })}
      </ul>

      {rest.length > 0 && (
        <button type="button" className="link-button" onClick={() => setShowAll((v) => !v)}>
          {showAll ? "Hide" : `Show ${rest.length} unavailable`} agents
        </button>
      )}

      {agents.length === 0 && !error && <p className="dim">Loading agents…</p>}
    </main>
  );
}

/** Why an agent can't be started, in words the reader can act on. */
function whyNot(agent: CatalogEntry): string {
  switch (agent.availability.state) {
    case "missingProgram":
      return `${agent.availability.program} is not on PATH`;
    case "needsManualInstall":
      return "Published only as a binary download — install it and add an [[agents]] entry";
    default:
      return "";
  }
}
