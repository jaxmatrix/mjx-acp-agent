/**
 * The conversations the agent has, and what may be done with them.
 *
 * Every button here is gated on a capability the agent advertised in
 * `initialize`. That is not defensiveness: `claude-acp` and `kilo` offer the
 * whole lifecycle, most of the registry offers none of it, and a control that
 * calls a method the agent never claimed fails on its side with nothing to
 * explain it.
 */

import type { AgentCapabilities } from "../acp/capabilities";
import type { SessionInfo } from "../acp/types";

export function SessionHistory({
  sessions,
  currentSessionId,
  capabilities,
  replayingSessionId,
  moreSessions,
  onLoad,
  onResume,
  onFork,
  onDelete,
  onClose,
  onNew,
  onRefresh,
  onMore,
}: {
  sessions: SessionInfo[];
  currentSessionId?: string;
  capabilities: AgentCapabilities;
  replayingSessionId?: string;
  moreSessions: boolean;
  onLoad(sessionId: string): void;
  onResume(sessionId: string): void;
  onFork(sessionId: string): void;
  onDelete(sessionId: string): void;
  onClose(sessionId: string): void;
  onNew(): void;
  onRefresh(): void;
  onMore(): void;
}) {
  const busy = replayingSessionId !== undefined;

  return (
    <section className="history">
      <header className="history__header">
        <h2>History</h2>
        <div className="history__actions">
          <button type="button" className="link-button" onClick={onNew}>
            New
          </button>
          <button type="button" className="link-button" onClick={onRefresh}>
            Refresh
          </button>
        </div>
      </header>

      <ol className="history__list">
        {sessions.map((session) => {
          const current = session.sessionId === currentSessionId;
          return (
            <li
              key={session.sessionId}
              className={`history__entry ${current ? "is-current" : ""}`}
            >
              <p className="history__title" title={session.sessionId}>
                {session.title ?? session.sessionId}
              </p>
              <p className="dim history__meta">
                {current && <span className="pill pill--completed">open</span>}{" "}
                {session.updatedAt ? ago(session.updatedAt) : "—"}
              </p>
              <p className="dim history__cwd" title={session.cwd}>
                {session.cwd}
              </p>

              <div className="history__buttons">
                {/* Loading the conversation already on screen would throw it
                    away and rebuild it, which is work for no change. */}
                {capabilities.loadSession && !current && (
                  <button
                    type="button"
                    className="link-button"
                    disabled={busy}
                    onClick={() => onLoad(session.sessionId)}
                  >
                    {replayingSessionId === session.sessionId ? "Opening…" : "Open"}
                  </button>
                )}
                {capabilities.session.resume && !current && (
                  <button
                    type="button"
                    className="link-button"
                    disabled={busy}
                    title="Pick the conversation back up without replaying it"
                    onClick={() => onResume(session.sessionId)}
                  >
                    Resume
                  </button>
                )}
                {capabilities.session.fork && (
                  <button
                    type="button"
                    className="link-button"
                    disabled={busy}
                    title="Branch this conversation into a new one"
                    onClick={() => onFork(session.sessionId)}
                  >
                    Fork
                  </button>
                )}
                {capabilities.session.close && (
                  <button
                    type="button"
                    className="link-button"
                    disabled={busy}
                    title="Free what this conversation is holding, and keep it"
                    onClick={() => onClose(session.sessionId)}
                  >
                    Close
                  </button>
                )}
                {capabilities.session.delete && (
                  <button
                    type="button"
                    className="link-button link-button--danger"
                    disabled={busy}
                    onClick={() => {
                      // Irreversible, and one row away from Open. Asking is
                      // cheap next to losing a conversation.
                      if (confirm(`Delete “${session.title ?? session.sessionId}” for good?`)) {
                        onDelete(session.sessionId);
                      }
                    }}
                  >
                    Delete
                  </button>
                )}
              </div>
            </li>
          );
        })}

        {sessions.length === 0 && (
          <li className="dim history__empty">No conversations yet.</li>
        )}
      </ol>

      {moreSessions && (
        <button type="button" className="link-button history__more" onClick={onMore}>
          Load more
        </button>
      )}
    </section>
  );
}

/**
 * How long ago, in the coarsest unit that still says something.
 *
 * The timestamp is the agent's, and agents are not always careful with them —
 * an unparseable one shows as itself rather than as "NaN minutes ago".
 */
export function ago(timestamp: string, now: number = Date.now()): string {
  const at = Date.parse(timestamp);
  if (Number.isNaN(at)) return timestamp;

  const seconds = Math.max(0, Math.round((now - at) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(at).toLocaleDateString();
}
