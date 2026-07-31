/** React binding for {@link AgentConnection}. */

import { useCallback, useEffect, useRef, useState } from "react";
import type { ContentBlock } from "@agentclientprotocol/sdk";

import { noCapabilities, type AgentCapabilities } from "./acp/capabilities";
import { resumeStore, sessionStore } from "./acp/resume";
import type { Terminals } from "./acp/terminals";
import { AgentConnection, type ConnectionStatus } from "./acp/agentConnection";
import {
  emptyThread,
  type AgentInfo,
  type ElicitationAnswer,
  type InspectorEntry,
  type SessionInfo,
  type Thread,
} from "./acp/types";

/** Everything a connected view needs. */
export interface SessionState {
  thread: Thread;
  /** The connection's terminals — see `acp/terminals.ts`. */
  terminals: Terminals;
  status: ConnectionStatus;
  agentInfo?: AgentInfo;
  /** What the agent said it can do; the history UI offers nothing else. */
  capabilities: AgentCapabilities;
  /** The conversations the agent knows about, once they have been asked for. */
  sessions: SessionInfo[];
  /** More sessions to fetch, if the agent paginated its answer. */
  moreSessions: boolean;
  /** The session on screen. */
  sessionId?: string;
  /** Set while `session/load` is streaming a conversation back. */
  replayingSessionId?: string;
  /** Asks the agent for its sessions again. */
  refreshSessions(): void;
  /** Fetches the next page and appends it. */
  moreSessionsPlease(): void;
  /**
   * These take the whole listing rather than an id: a session belongs to the
   * directory it was started in, and an agent's history spans every project it
   * has been used in. Sending this connection's directory for one of those is
   * what ACP refuses.
   */
  loadSession(session: SessionInfo): void;
  resumeSession(session: SessionInfo): void;
  forkSession(session: SessionInfo): void;
  deleteSession(session: SessionInfo): void;
  closeSession(session: SessionInfo): void;
  newSession(): void;
  frames: InspectorEntry[];
  prompt(prompt: ContentBlock[]): void;
  cancel(): void;
  setMode(modeId: string): void;
  setConfigOption(configId: string, value: string | boolean): void;
  answerPermission(toolCallId: string, optionId: string | null): void;
  answerElicitation(requestId: string | number, answer: ElicitationAnswer): void;
  /** Opens a new socket to the same agent, taking it back from another tab. */
  reconnect(): void;
  error?: string;
}

/** The inspector keeps a bounded log; Zed's ACP tools ring-buffer at 2000. */
const MAX_FRAMES = 2000;

const resume = resumeStore();
const sessions = sessionStore();

/** Opens a session against `agentId` in `cwd`, and keeps React in step. */
export function useSession(agentId: string | null, cwd: string | null): SessionState {
  const [thread, setThread] = useState<Thread>(emptyThread);
  const [terminals, setTerminals] = useState<Terminals>({});
  const [status, setStatus] = useState<ConnectionStatus>({ state: "connecting" });
  const [agentInfo, setAgentInfo] = useState<AgentInfo>();
  const [frames, setFrames] = useState<InspectorEntry[]>([]);
  const [error, setError] = useState<string>();
  const [capabilities, setCapabilities] = useState<AgentCapabilities>(noCapabilities);
  const [history, setHistory] = useState<SessionInfo[]>([]);
  const [nextCursor, setNextCursor] = useState<string>();
  const [sessionId, setSessionId] = useState<string>();
  const [replayingSessionId, setReplayingSessionId] = useState<string>();
  /** Bumped to reopen the socket; see `reconnect`. */
  const [attempt, setAttempt] = useState(0);

  const session = useRef<AgentConnection>(null);
  const nextSeq = useRef(0);
  /**
   * Which conversation this view is showing.
   *
   * A ref as well as state because the connection's callbacks fire outside
   * React and have to know, synchronously, whether an update belongs to the
   * conversation on screen. The connection itself holds all of them.
   */
  const showing = useRef<string>(undefined);

  useEffect(() => {
    if (!agentId) return;

    // Reset rather than accumulate: switching agents starts a new conversation.
    setThread(emptyThread());
    setTerminals({});
    setFrames([]);
    setError(undefined);
    setAgentInfo(undefined);
    setCapabilities(noCapabilities());
    setHistory([]);
    setNextCursor(undefined);
    setSessionId(undefined);
    setReplayingSessionId(undefined);
    nextSeq.current = 0;
    showing.current = undefined;

    const memory = {
      get: () => sessions.get(agentId, cwd ?? ""),
      set: (id: string) => sessions.set(agentId, cwd ?? "", id),
      clear: () => sessions.clear(agentId, cwd ?? ""),
    };

    const show = (sessionId: string) => {
      showing.current = sessionId;
      setSessionId(sessionId);
      setThread(active.thread(sessionId));
    };

    const active = new AgentConnection({
      // Only the conversation on screen reaches React. The others are still
      // being folded — that is the point of the connection holding them — they
      // just have nowhere to be drawn until this view shows more than one.
      thread: (sessionId, next) => {
        if (sessionId === showing.current) setThread(next);
      },
      terminals: setTerminals,
      capabilities: setCapabilities,
      replaying: setReplayingSessionId,
      sessionOpened: show,
      sessionClosed: (sessionId) => {
        // Leaving the composer pointed at a session the agent has just dropped
        // would send a prompt that fails with no explanation. Somewhere else is
        // anywhere else it has, and a fresh conversation if it has none.
        if (sessionId !== showing.current) return;
        const somewhere = active.sessions[0];
        if (somewhere) show(somewhere);
        else void active.newSession();
      },
      agentInfo: (info) => {
        // Keep the handle to come back with. The server mints a new id when it
        // could not honour the one we sent, so writing back whatever it says
        // is also how a stale id gets replaced.
        resume.set(agentId, cwd ?? "", info.connectionId);
        setAgentInfo(info);
      },
      status: setStatus,
      frame: (entry) => {
        setFrames((current) => {
          const next = [
            ...current,
            { ...entry, seq: (nextSeq.current += 1), at: Date.now() },
          ];
          return next.length > MAX_FRAMES ? next.slice(-MAX_FRAMES) : next;
        });
      },
    }, memory);
    session.current = active;

    active
      .connect({
        agentId,
        cwd: cwd ?? undefined,
        // If this tab was here before, rejoin the agent it left rather than
        // start another. The reset above is not wasted: the replay replaces the
        // thread a moment later, and until it does an empty page is honest.
        resume: resume.get(agentId, cwd ?? ""),
      })
      .catch((cause: unknown) => {
        setError(cause instanceof Error ? cause.message : String(cause));
      });

    return () => {
      active.close();
      session.current = null;
    };
  }, [agentId, cwd, attempt]);

  const prompt = useCallback((blocks: ContentBlock[]) => {
    const on = showing.current;
    if (!on) return;
    session.current?.prompt(on, blocks).catch((cause: unknown) => {
      setError(cause instanceof Error ? cause.message : String(cause));
    });
  }, []);

  const cancel = useCallback(() => {
    if (showing.current) void session.current?.cancel(showing.current);
  }, []);

  const setMode = useCallback((modeId: string) => {
    if (showing.current) void session.current?.setMode(showing.current, modeId);
  }, []);

  const setConfigOption = useCallback((configId: string, value: string | boolean) => {
    if (showing.current) void session.current?.setConfigOption(showing.current, configId, value);
  }, []);

  const answerPermission = useCallback((toolCallId: string, optionId: string | null) => {
    session.current?.answerPermission(toolCallId, optionId);
  }, []);

  const answerElicitation = useCallback(
    (requestId: string | number, answer: ElicitationAnswer) => {
      session.current?.answerElicitation(requestId, answer);
    },
    [],
  );

  const reconnect = useCallback(() => setAttempt((n) => n + 1), []);

  /** Runs one lifecycle call, and refreshes the list it just changed. */
  const lifecycle = useCallback(
    (run: (session: AgentConnection) => Promise<unknown>, listAgain = true) => {
      const active = session.current;
      if (!active) return;
      run(active)
        .then(async () => {
          if (!listAgain) return;
          // The list is the agent's, not ours: a fork adds an entry, a delete
          // removes one, and a prompt can retitle a session. Asking again is
          // the only way to show what it really has.
          const { sessions: listed, nextCursor: cursor } = await active.listSessions();
          setHistory(listed);
          setNextCursor(cursor);
        })
        .catch((cause: unknown) => {
          setError(cause instanceof Error ? cause.message : String(cause));
        });
    },
    [],
  );

  const refreshSessions = useCallback(() => lifecycle(async () => {}), [lifecycle]);

  const moreSessionsPlease = useCallback(() => {
    const active = session.current;
    if (!active || !nextCursor) return;
    active
      .listSessions(nextCursor)
      .then(({ sessions: listed, nextCursor: cursor }) => {
        setHistory((current) => [...current, ...listed]);
        setNextCursor(cursor);
      })
      .catch((cause: unknown) => {
        setError(cause instanceof Error ? cause.message : String(cause));
      });
  }, [nextCursor]);

  const loadSession = useCallback(
    (info: SessionInfo) => lifecycle((s) => s.loadSession(info), false),
    [lifecycle],
  );
  const resumeSession = useCallback(
    (info: SessionInfo) => lifecycle((s) => s.resumeSession(info), false),
    [lifecycle],
  );
  const forkSession = useCallback(
    (info: SessionInfo) => lifecycle((s) => s.forkSession(info)),
    [lifecycle],
  );
  const deleteSession = useCallback(
    (info: SessionInfo) => lifecycle((s) => s.deleteSession(info)),
    [lifecycle],
  );
  const closeSession = useCallback(
    (info: SessionInfo) => lifecycle((s) => s.closeSession(info)),
    [lifecycle],
  );
  const newSession = useCallback(() => lifecycle((s) => s.newSession()), [lifecycle]);

  return {
    thread,
    terminals,
    status,
    agentInfo,
    capabilities,
    sessions: history,
    moreSessions: nextCursor !== undefined,
    sessionId,
    replayingSessionId,
    refreshSessions,
    moreSessionsPlease,
    loadSession,
    resumeSession,
    forkSession,
    deleteSession,
    closeSession,
    newSession,
    frames,
    prompt,
    cancel,
    setMode,
    setConfigOption,
    answerPermission,
    answerElicitation,
    reconnect,
    error,
  };
}
