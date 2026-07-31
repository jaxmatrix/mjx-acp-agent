/** The shell: pick an agent, then talk to it — as many at once as you like. */

import { useEffect, useState } from "react";

import { AgentPicker } from "./components/AgentPicker";
import { Composer } from "./components/Composer";
import { Inspector } from "./components/Inspector";
import { SessionHistory } from "./components/SessionHistory";
import { Sidebar } from "./components/Sidebar";
import { ThreadView } from "./components/ThreadView";
import { useSessions, type TabState, type Viewer } from "./useSessions";

export function App() {
  const viewer = useSessions();
  const [picking, setPicking] = useState(false);

  // Nothing open — a first visit, or everything closed. The picker is the whole
  // page then, and a way to add an agent beside the ones already open when not.
  //
  // What is open is restored rather than empty: the server keeps agents alive
  // across a reload, and a page that came back to the picker would leave those
  // conversations running where nobody could see them.
  if (viewer.tabs.length === 0 || picking) {
    return (
      <AgentPicker
        onConnect={(agentId, cwd) => {
          viewer.connect(agentId, cwd);
          setPicking(false);
        }}
        {...(viewer.tabs.length > 0 ? { onCancel: () => setPicking(false) } : {})}
      />
    );
  }

  // Connections are open but no conversation has come back yet.
  if (!viewer.current) return <p className="callout">Connecting…</p>;

  return <Conversation viewer={viewer} current={viewer.current} onAdd={() => setPicking(true)} />;
}

function Conversation({
  viewer,
  current,
  onAdd,
}: {
  viewer: Viewer;
  current: TabState;
  onAdd(): void;
}) {
  const [showInspector, setShowInspector] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  const busy = current.thread.status === "generating";
  const { capabilities, refreshSessions } = current;

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
        <button type="button" className="link-button" onClick={onAdd}>
          + Agent
        </button>
        {canList && (
          <button
            type="button"
            className="link-button"
            onClick={() => setShowHistory((v) => !v)}
            aria-pressed={showHistory}
          >
            History ({current.sessions.length})
          </button>
        )}
        <StatusPill current={current} />
        <button
          type="button"
          className="link-button"
          onClick={() => setShowInspector((v) => !v)}
          aria-pressed={showInspector}
        >
          {showInspector ? "Hide" : "Show"} protocol ({current.frames.length})
        </button>
      </header>

      {current.error && <p className="callout callout--error">{current.error}</p>}
      {current.status.state === "failed" && (
        <p className="callout callout--error">Connection failed: {current.status.message}</p>
      )}
      {current.status.state === "takenOver" && (
        // The agent is still running; it is just answering to another tab now.
        // Taking it back is the same move that tab made to get it, and every
        // conversation open on this connection comes back with it.
        //
        // Named, because another agent may be open beside this one and only
        // this one has been taken.
        <p className="callout">
          {current.agentInfo?.name ?? current.tab.agentId} was opened in another tab.{" "}
          <button type="button" className="link-button" onClick={current.reconnect}>
            Take it back
          </button>
        </p>
      )}

      {current.replayingSessionId && <p className="callout">Replaying the conversation…</p>}

      <div className="app__body">
        {showHistory && (
          <SessionHistory
            sessions={current.sessions}
            currentSessionId={current.tab.sessionId}
            workspaceCwd={current.tab.cwd}
            capabilities={current.capabilities}
            replayingSessionId={current.replayingSessionId}
            moreSessions={current.moreSessions}
            onLoad={current.loadSession}
            onResume={current.resumeSession}
            onFork={current.forkSession}
            onDelete={current.deleteSession}
            onClose={current.closeSession}
            onNew={current.newSession}
            onRefresh={current.refreshSessions}
            onMore={current.moreSessionsPlease}
          />
        )}

        <main className="app__main">
          <ThreadView
            thread={current.thread}
            terminals={current.terminals}
            onPermission={current.answerPermission}
            onElicitation={current.answerElicitation}
          />
          <Composer
            busy={busy}
            ready={current.status.state === "ready"}
            commands={current.thread.availableCommands}
            cwd={current.agentInfo?.cwd ?? current.tab.cwd}
            onSend={current.prompt}
            onCancel={current.cancel}
          />
        </main>

        <Sidebar
          thread={current.thread}
          agentInfo={current.agentInfo}
          status={current.status}
          onSetMode={current.setMode}
          onSetConfigOption={current.setConfigOption}
        />

        {showInspector && <Inspector frames={current.frames} />}
      </div>

      {/* Every conversation is live; this is the one place to switch between
          them until the tab strip lands. */}
      {viewer.tabs.length > 1 && (
        <nav className="tabs-fallback">
          {viewer.tabs.map((tab) => (
            <button
              key={`${tab.agentId} ${tab.cwd} ${tab.sessionId}`}
              type="button"
              className="link-button"
              aria-pressed={tab.sessionId === current.tab.sessionId}
              onClick={() => viewer.focus(tab)}
            >
              {tab.agentId}
              {viewer.busy(tab) ? " •" : ""}
            </button>
          ))}
        </nav>
      )}
    </div>
  );
}

/**
 * Whether this connection is reachable, and whether its agent is working.
 *
 * Two questions rather than one, now that a connection carries several
 * conversations — the socket is the connection's and the turn is one
 * conversation's. They still share a pill until the tab strip has somewhere to
 * show the second per tab.
 */
function StatusPill({ current }: { current: TabState }) {
  const { status, thread } = current;
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
