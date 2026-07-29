/** The shell: pick an agent, then talk to it. */

import { useState } from "react";

import { AgentPicker } from "./components/AgentPicker";
import { Composer } from "./components/Composer";
import { Inspector } from "./components/Inspector";
import { Sidebar } from "./components/Sidebar";
import { ThreadView } from "./components/ThreadView";
import { useSession } from "./useSession";

export function App() {
  const [connection, setConnection] = useState<{ agentId: string; cwd: string }>();

  if (!connection) {
    return <AgentPicker onConnect={(agentId, cwd) => setConnection({ agentId, cwd })} />;
  }

  return (
    <Conversation
      agentId={connection.agentId}
      cwd={connection.cwd}
      onDisconnect={() => setConnection(undefined)}
    />
  );
}

function Conversation({
  agentId,
  cwd,
  onDisconnect,
}: {
  agentId: string;
  cwd: string;
  onDisconnect(): void;
}) {
  const session = useSession(agentId, cwd);
  const [showInspector, setShowInspector] = useState(false);
  const busy = session.thread.status === "generating";

  return (
    <div className={`app ${showInspector ? "app--with-inspector" : ""}`}>
      <header className="topbar">
        <button type="button" className="link-button" onClick={onDisconnect}>
          ← Agents
        </button>
        <StatusPill session={session} />
        <button
          type="button"
          className="link-button"
          onClick={() => setShowInspector((v) => !v)}
          aria-pressed={showInspector}
        >
          {showInspector ? "Hide" : "Show"} protocol ({session.frames.length})
        </button>
      </header>

      {session.error && <p className="callout callout--error">{session.error}</p>}
      {session.status.state === "failed" && (
        <p className="callout callout--error">
          Connection failed: {session.status.message}
        </p>
      )}

      <div className="app__body">
        <main className="app__main">
          <ThreadView thread={session.thread} onPermission={session.answerPermission} />
          <Composer
            busy={busy}
            ready={session.status.state === "ready"}
            commands={session.thread.availableCommands}
            onSend={session.prompt}
            onCancel={session.cancel}
          />
        </main>

        <Sidebar
          thread={session.thread}
          agentInfo={session.agentInfo}
          status={session.status}
          onSetMode={session.setMode}
        />

        {showInspector && <Inspector frames={session.frames} />}
      </div>
    </div>
  );
}

function StatusPill({ session }: { session: ReturnType<typeof useSession> }) {
  const { status, thread } = session;
  if (status.state === "connecting") return <span className="pill">connecting…</span>;
  if (status.state === "failed") return <span className="pill pill--failed">disconnected</span>;
  if (status.state === "closed") return <span className="pill pill--failed">closed</span>;
  return (
    <span className={`pill pill--${thread.status === "generating" ? "in_progress" : "completed"}`}>
      {thread.status === "generating" ? "working" : "ready"}
    </span>
  );
}
