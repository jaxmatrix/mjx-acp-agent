/** The shell: pick an agent, then talk to it — as many at once as you like. */

import { useEffect, useState } from "react";

import { AgentPicker } from "./components/AgentPicker";
import { Composer } from "./components/Composer";
import { Inspector } from "./components/Inspector";
import { SessionHistory } from "./components/SessionHistory";
import { Sidebar } from "./components/Sidebar";
import { TabStrip } from "./components/TabStrip";
import { ThreadView } from "./components/ThreadView";
import type { ConnectionStatus } from "./acp/agentConnection";
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

  // The history is one agent's, so the tabs it can point at are that agent's.
  const onThisConnection = viewer.tabs.filter(
    (tab) => tab.agentId === current.tab.agentId && tab.cwd === current.tab.cwd,
  );

  useEffect(() => {
    if (canList) refreshSessions();
  }, [canList, refreshSessions]);

  return (
    <div
      className={`app ${showInspector ? "app--with-inspector" : ""} ${
        showHistory ? "app--with-history" : ""
      }`}
    >
      <TabStrip
        tabs={viewer.tabs}
        focused={viewer.focused}
        nameOf={viewer.nameOf}
        agentNameOf={viewer.agentNameOf}
        busy={viewer.busy}
        onFocus={viewer.focus}
        onClose={viewer.closeTab}
        onAdd={onAdd}
      />

      <header className="topbar">
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
        <StatusPill status={current.status} />
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
            openSessionIds={onThisConnection.map((tab) => tab.sessionId)}
            onFocusSession={(sessionId) => {
              const open = onThisConnection.find((tab) => tab.sessionId === sessionId);
              if (open) viewer.focus(open);
            }}
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

    </div>
  );
}

/**
 * Whether the socket behind the conversation on screen is up.
 *
 * Only that. Whether an *agent* is working is a per-conversation question now
 * that a connection carries several, and the tab strip answers it for all of
 * them at once rather than for whichever one happens to be on screen.
 */
function StatusPill({ status }: { status: ConnectionStatus }) {
  if (status.state === "connecting") return <span className="pill">connecting…</span>;
  if (status.state === "failed") return <span className="pill pill--failed">disconnected</span>;
  if (status.state === "takenOver") return <span className="pill pill--failed">other tab</span>;
  if (status.state === "closed") return <span className="pill pill--failed">closed</span>;
  return <span className="pill pill--completed">connected</span>;
}
