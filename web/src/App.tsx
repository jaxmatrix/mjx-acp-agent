/** The shell: pick an agent, then talk to it. */

import { useState } from "react";

import { choiceStore, resumeStore } from "./acp/resume";
import { AgentPicker } from "./components/AgentPicker";
import { Composer } from "./components/Composer";
import { Inspector } from "./components/Inspector";
import { Sidebar } from "./components/Sidebar";
import { ThreadView } from "./components/ThreadView";
import { useSession } from "./useSession";

const choice = choiceStore();
const resume = resumeStore();

export function App() {
  // Restored, not empty: the server keeps the agent alive across a reload, and
  // a page that came back to the picker would leave that conversation running
  // where nobody could see it.
  const [connection, setConnection] = useState(choice.get);

  if (!connection) {
    return (
      <AgentPicker
        onConnect={(agentId, cwd) => {
          choice.set({ agentId, cwd });
          setConnection({ agentId, cwd });
        }}
      />
    );
  }

  return (
    <Conversation
      agentId={connection.agentId}
      cwd={connection.cwd}
      onDisconnect={() => {
        // Leaving on purpose is not a reload. Forget the agent so the next
        // visit starts clean rather than rejoining something abandoned.
        resume.clear(connection.agentId, connection.cwd);
        choice.clear();
        setConnection(undefined);
      }}
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
      {session.status.state === "takenOver" && (
        // The agent is still running; it is just answering to another tab now.
        // Taking it back is the same move that tab made to get it.
        <p className="callout">
          This session was opened in another tab.{" "}
          <button type="button" className="link-button" onClick={session.reconnect}>
            Take it back
          </button>
        </p>
      )}

      <div className="app__body">
        <main className="app__main">
          <ThreadView
            thread={session.thread}
            onPermission={session.answerPermission}
            onElicitation={session.answerElicitation}
          />
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
          onSetConfigOption={session.setConfigOption}
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
  if (status.state === "takenOver") return <span className="pill pill--failed">other tab</span>;
  if (status.state === "closed") return <span className="pill pill--failed">closed</span>;
  return (
    <span className={`pill pill--${thread.status === "generating" ? "in_progress" : "completed"}`}>
      {thread.status === "generating" ? "working" : "ready"}
    </span>
  );
}
