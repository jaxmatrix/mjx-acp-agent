/** The shell: pick an agent, then talk to it. */

import { useEffect, useState } from "react";

import { choiceStore, resumeStore } from "./acp/resume";
import { AgentPicker } from "./components/AgentPicker";
import { Composer } from "./components/Composer";
import { Inspector } from "./components/Inspector";
import { SessionHistory } from "./components/SessionHistory";
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
  const [showHistory, setShowHistory] = useState(false);
  const busy = session.thread.status === "generating";
  const { capabilities, refreshSessions } = session;

  // Only agents that advertise `session/list` have a history to show, and that
  // is not known until the handshake has answered.
  const canList = capabilities.session.list;

  useEffect(() => {
    if (canList) refreshSessions();
  }, [canList, refreshSessions]);

  return (
    <div
      className={`app ${showInspector ? "app--with-inspector" : ""} ${
        showHistory ? "app--with-history" : ""
      }`}
    >
      <header className="topbar">
        <button type="button" className="link-button" onClick={onDisconnect}>
          ← Agents
        </button>
        {canList && (
          <button
            type="button"
            className="link-button"
            onClick={() => setShowHistory((v) => !v)}
            aria-pressed={showHistory}
          >
            History ({session.sessions.length})
          </button>
        )}
        <StatusPill session={session} />
        <button
          type="button"
          className="link-button"
          data-testid="inspector-toggle"
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

      {session.replayingSessionId && (
        <p className="callout">Replaying the conversation…</p>
      )}

      <div className="app__body">
        {showHistory && (
          <SessionHistory
            sessions={session.sessions}
            currentSessionId={session.sessionId}
            workspaceCwd={cwd}
            capabilities={session.capabilities}
            replayingSessionId={session.replayingSessionId}
            moreSessions={session.moreSessions}
            onLoad={session.loadSession}
            onResume={session.resumeSession}
            onFork={session.forkSession}
            onDelete={session.deleteSession}
            onClose={session.closeSession}
            onNew={session.newSession}
            onRefresh={session.refreshSessions}
            onMore={session.moreSessionsPlease}
          />
        )}

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
            cwd={session.agentInfo?.cwd ?? cwd}
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

/** What the pill says, and the class that colours it, per connection state. */
const STATUS: Record<string, { label: string; modifier?: string }> = {
  connecting: { label: "connecting…" },
  failed: { label: "disconnected", modifier: "failed" },
  takenOver: { label: "other tab", modifier: "failed" },
  closed: { label: "closed", modifier: "failed" },
};

function StatusPill({ session }: { session: ReturnType<typeof useSession> }) {
  const { status, thread } = session;
  const working = thread.status === "generating";
  const { label, modifier } = STATUS[status.state] ?? {
    label: working ? "working" : "ready",
    modifier: working ? "in_progress" : "completed",
  };

  // One element rather than five returns, so what is on screen can be read
  // from outside — by a browser check, or by anyone reading the DOM.
  return (
    <span
      className={modifier ? `pill pill--${modifier}` : "pill"}
      data-testid="status"
      data-state={status.state}
      data-busy={working}
    >
      {label}
    </span>
  );
}
