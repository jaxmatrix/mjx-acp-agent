/**
 * React binding for however many {@link AgentConnection}s are open.
 *
 * One hook rather than one per connection, because a connection is a plain
 * object rather than a hook: N of them need no N hooks, only somewhere to put
 * them. They live in a ref, and everything React has to redraw is folded into
 * one reducer beside it.
 *
 * The unit on screen is a *tab* — one conversation, on one agent, in one
 * directory. A connection carries as many tabs as the user opens on it, and the
 * viewer carries as many connections as there are agents in play. Both are
 * needed: sessions multiplex over a socket, but a socket is bound to one agent
 * and one directory, so two agents side by side is two sockets.
 */

import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import type { Dispatch, RefObject } from "react";
import type { ContentBlock } from "@agentclientprotocol/sdk";

import { AgentConnection, type ConnectionStatus } from "./acp/agentConnection";
import { noCapabilities, type AgentCapabilities } from "./acp/capabilities";
import { connectionsStore, focusStore, resumeStore, sessionStore } from "./acp/resume";
import type { Terminals } from "./acp/terminals";
import {
  emptyThread,
  type AgentInfo,
  type ElicitationAnswer,
  type InspectorEntry,
  type SessionInfo,
  type Thread,
} from "./acp/types";

/** One conversation open in the viewer. */
export interface Tab {
  agentId: string;
  cwd: string;
  sessionId: string;
}

/** The agent and directory one socket is bound to. */
export interface Connection {
  agentId: string;
  cwd: string;
}

/** The inspector keeps a bounded log; Zed's ACP tools ring-buffer at 2000. */
const MAX_FRAMES = 2000;

const resume = resumeStore();
const remembered = sessionStore();
const openConnections = connectionsStore();
const lastFocused = focusStore();

/**
 * A connection's key.
 *
 * A NUL rather than a colon or a slash: both of the parts are user data — an
 * agent id from the catalog and an absolute path — and any separator either
 * could contain would make two different connections share a key.
 */
function keyOf({ agentId, cwd }: Connection): string {
  return `${agentId}\u0000${cwd}`;
}

/** A tab's identity, for a React key or a lookup. Unique for the same reason. */
export function tabKey(tab: Tab): string {
  return `${keyOf(tab)}\u0000${tab.sessionId}`;
}

function connectionOf(key: string): Connection {
  const [agentId = "", cwd = ""] = key.split("\u0000");
  return { agentId, cwd };
}

/** Everything React draws for one connection. */
interface ConnectionState {
  status: ConnectionStatus;
  agentInfo?: AgentInfo;
  capabilities: AgentCapabilities;
  frames: InspectorEntry[];
  /** The conversations the agent knows about, once they have been asked for. */
  history: SessionInfo[];
  nextCursor?: string;
  replayingSessionId?: string;
  error?: string;
}

function blankConnection(): ConnectionState {
  return {
    status: { state: "connecting" },
    capabilities: noCapabilities(),
    frames: [],
    history: [],
  };
}

export interface State {
  /** Connection key to its state. */
  connections: Record<string, ConnectionState>;
  /** Connection keys, in the order they were opened. */
  order: string[];
  /**
   * Threads, by connection and then by session.
   *
   * Nested rather than keyed by session id alone: session ids are the agent's,
   * and two agents are perfectly entitled to hand out the same one. Flattened,
   * one conversation would overwrite the other.
   */
  threads: Record<string, Record<string, Thread>>;
  /** Terminals, by connection — they belong to the workspace, not the thread. */
  terminals: Record<string, Terminals>;
  /** Which conversations are open on each connection, in order. */
  open: Record<string, string[]>;
  focused?: Tab;
  /**
   * The conversation to focus if it comes back, from before the reload.
   *
   * Held until it does, because it may be the third of five to be replayed and
   * the first one back would otherwise take the screen. Cleared once it
   * arrives, or left unmet if that conversation is gone.
   */
  restoring?: Tab;
}

export type Action =
  | { type: "connectionOpened"; key: string }
  | { type: "connectionClosed"; key: string }
  | { type: "status"; key: string; status: ConnectionStatus }
  | { type: "agentInfo"; key: string; info: AgentInfo }
  | { type: "capabilities"; key: string; capabilities: AgentCapabilities }
  | { type: "replaying"; key: string; sessionId?: string }
  | { type: "frame"; key: string; entry: InspectorEntry }
  | { type: "error"; key: string; message?: string }
  | { type: "thread"; key: string; sessionId: string; thread: Thread }
  | { type: "terminals"; key: string; terminals: Terminals }
  | { type: "sessionOpened"; key: string; sessionId: string }
  | { type: "sessionClosed"; key: string; sessionId: string }
  | { type: "history"; key: string; sessions: SessionInfo[]; nextCursor?: string; append?: boolean }
  | { type: "focus"; tab: Tab };

function patch(state: State, key: string, change: Partial<ConnectionState>): State {
  const current = state.connections[key];
  if (!current) return state;
  return { ...state, connections: { ...state.connections, [key]: { ...current, ...change } } };
}

/**
 * Exported for its tests. The decisions in here — where the eye goes when a tab
 * closes under it, which conversation a restore settles on — are the kind that
 * are easy to get subtly wrong and impossible to see going wrong.
 */
export function reduce(state: State, action: Action): State {
  switch (action.type) {
    case "connectionOpened":
      if (state.connections[action.key]) return state;
      return {
        ...state,
        connections: { ...state.connections, [action.key]: blankConnection() },
        order: [...state.order, action.key],
        threads: { ...state.threads, [action.key]: {} },
        terminals: { ...state.terminals, [action.key]: {} },
        open: { ...state.open, [action.key]: [] },
      };

    case "connectionClosed": {
      const { [action.key]: _c, ...connections } = state.connections;
      const { [action.key]: _t, ...threads } = state.threads;
      const { [action.key]: _x, ...terminals } = state.terminals;
      const { [action.key]: _o, ...open } = state.open;
      return {
        ...state,
        connections,
        threads,
        terminals,
        open,
        order: state.order.filter((one) => one !== action.key),
        ...(state.focused && keyOf(state.focused) === action.key ? { focused: undefined } : {}),
      };
    }

    case "status":
      return patch(state, action.key, { status: action.status });
    case "agentInfo":
      return patch(state, action.key, { agentInfo: action.info });
    case "capabilities":
      return patch(state, action.key, { capabilities: action.capabilities });
    case "replaying":
      return patch(state, action.key, { replayingSessionId: action.sessionId });
    case "error":
      return patch(state, action.key, { error: action.message });

    case "frame": {
      const current = state.connections[action.key];
      if (!current) return state;
      const frames = [...current.frames, action.entry];
      return patch(state, action.key, {
        frames: frames.length > MAX_FRAMES ? frames.slice(-MAX_FRAMES) : frames,
      });
    }

    case "history":
      return patch(state, action.key, {
        history: action.append
          ? [...(state.connections[action.key]?.history ?? []), ...action.sessions]
          : action.sessions,
        nextCursor: action.nextCursor,
      });

    case "thread": {
      const onConnection = state.threads[action.key];
      if (!onConnection) return state;
      return {
        ...state,
        threads: {
          ...state.threads,
          [action.key]: { ...onConnection, [action.sessionId]: action.thread },
        },
      };
    }

    case "terminals":
      return { ...state, terminals: { ...state.terminals, [action.key]: action.terminals } };

    case "sessionOpened": {
      const on = state.open[action.key];
      if (!on || on.includes(action.sessionId)) return state;
      const tab = { ...connectionOf(action.key), sessionId: action.sessionId };
      const next = {
        ...state,
        open: { ...state.open, [action.key]: [...on, action.sessionId] },
      };
      // Whatever was being read before the reload, if it has just come back.
      // Otherwise the first conversation to appear anywhere — and after that,
      // opening one in the background does not steal the screen.
      const wanted = state.restoring;
      if (wanted && keyOf(wanted) === action.key && wanted.sessionId === action.sessionId) {
        return { ...next, focused: tab, restoring: undefined };
      }
      if (next.focused) return next;
      return { ...next, focused: tab };
    }

    case "sessionClosed": {
      const on = state.open[action.key];
      if (!on) return state;
      const left = on.filter((one) => one !== action.sessionId);
      const onConnection = { ...(state.threads[action.key] ?? {}) };
      delete onConnection[action.sessionId];
      const next = {
        ...state,
        open: { ...state.open, [action.key]: left },
        threads: { ...state.threads, [action.key]: onConnection },
      };

      // A tab closing under the reader has to leave them somewhere. Its
      // neighbour on the same connection first, since that is where they were
      // looking; anywhere else only if the connection has nothing left.
      const wasFocused =
        state.focused &&
        keyOf(state.focused) === action.key &&
        state.focused.sessionId === action.sessionId;
      if (!wasFocused) return next;

      const neighbour = left[Math.min(on.indexOf(action.sessionId), left.length - 1)];
      if (neighbour) {
        return { ...next, focused: { ...connectionOf(action.key), sessionId: neighbour } };
      }
      return { ...next, focused: firstTabOf(next) };
    }

    case "focus":
      return { ...state, focused: action.tab };
  }
}

/** The first conversation open anywhere, in tab order. */
function firstTabOf(state: State): Tab | undefined {
  for (const key of state.order) {
    const sessionId = state.open[key]?.[0];
    if (sessionId) return { ...connectionOf(key), sessionId };
  }
  return undefined;
}

/** What a view of one conversation needs. */
export interface TabState {
  tab: Tab;
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
  /** Set while `session/load` is streaming a conversation back. */
  replayingSessionId?: string;
  frames: InspectorEntry[];
  error?: string;
  refreshSessions(): void;
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
  prompt(prompt: ContentBlock[]): void;
  cancel(): void;
  setMode(modeId: string): void;
  setConfigOption(configId: string, value: string | boolean): void;
  answerPermission(toolCallId: string, optionId: string | null): void;
  answerElicitation(requestId: string | number, answer: ElicitationAnswer): void;
  /** Opens a new socket to this agent, taking it back from another tab. */
  reconnect(): void;
}

/** The whole viewer. */
export interface Viewer {
  tabs: Tab[];
  focused?: Tab;
  /** The conversation on screen, absent only when nothing is open. */
  current?: TabState;
  focus(tab: Tab): void;
  /** Whether a conversation is mid-turn, for the tab strip. */
  busy(tab: Tab): boolean;
  /**
   * What to call a tab: the conversation's own title where the agent has given
   * one, since several tabs on one agent are otherwise the same word repeated.
   */
  nameOf(tab: Tab): string;
  /** The agent behind a tab, for when more than one is open to confuse it with. */
  agentNameOf(tab: Tab): string;
  /** Opens a connection to an agent, and a first conversation on it. */
  connect(agentId: string, cwd: string): void;
  /** Starts another conversation on a connection already open. */
  newTab(connection: Connection): void;
  /** Closes one conversation, and the connection with it if it was the last. */
  closeTab(tab: Tab): void;
}

export const EMPTY: State = { connections: {}, order: [], threads: {}, terminals: {}, open: {} };

function restored(): State {
  const wanted = lastFocused.get();
  return wanted ? { ...EMPTY, restoring: wanted } : EMPTY;
}

export function useSessions(): Viewer {
  const [state, dispatch] = useReducer(reduce, undefined, restored);
  const connections = useRef(new Map<string, AgentConnection>());
  const seq = useRef(0);
  /** Which connections should be live. Kept in a ref so the effect below is
   *  the only thing that opens and closes sockets. */
  const wanted = useRef<Connection[]>(openConnections.get());

  /** Opens one socket and wires its events into the reducer. */
  const open = useCallback((connection: Connection) => {
    const key = keyOf(connection);
    if (connections.current.has(key)) return;
    const { agentId, cwd } = connection;

    dispatch({ type: "connectionOpened", key });

    const memory = {
      get: () => remembered.get(agentId, cwd),
      add: (id: string) => {
        const held = remembered.get(agentId, cwd);
        if (!held.includes(id)) remembered.set(agentId, cwd, [...held, id]);
      },
      remove: (id: string) =>
        remembered.set(
          agentId,
          cwd,
          remembered.get(agentId, cwd).filter((held) => held !== id),
        ),
    };

    const live = new AgentConnection(
      {
        thread: (sessionId, thread) => dispatch({ type: "thread", key, sessionId, thread }),
        terminals: (terminals) => dispatch({ type: "terminals", key, terminals }),
        capabilities: (capabilities) => dispatch({ type: "capabilities", key, capabilities }),
        replaying: (sessionId) => dispatch({ type: "replaying", key, sessionId }),
        sessionOpened: (sessionId) => dispatch({ type: "sessionOpened", key, sessionId }),
        sessionClosed: (sessionId) => dispatch({ type: "sessionClosed", key, sessionId }),
        agentInfo: (info) => {
          // Keep the handle to come back with. The server mints a new id when
          // it could not honour the one we sent, so writing back whatever it
          // says is also how a stale id gets replaced.
          resume.set(agentId, cwd, info.connectionId);
          dispatch({ type: "agentInfo", key, info });
        },
        status: (status) => dispatch({ type: "status", key, status }),
        frame: (entry) =>
          dispatch({ type: "frame", key, entry: { ...entry, seq: (seq.current += 1), at: Date.now() } }),
      },
      memory,
    );
    connections.current.set(key, live);

    live
      .connect({
        agentId,
        cwd: cwd || undefined,
        // If this tab was here before, rejoin the agent it left rather than
        // start another.
        resume: resume.get(agentId, cwd),
      })
      .catch((cause: unknown) => {
        dispatch({ type: "error", key, message: describe(cause) });
      });
  }, []);

  const close = useCallback((key: string) => {
    connections.current.get(key)?.close();
    connections.current.delete(key);
    dispatch({ type: "connectionClosed", key });
  }, []);

  // Restores whatever this browser tab had open, once. The connections are the
  // seed; which conversations come back on each of them is the connection's own
  // business, out of the same memory it wrote.
  useEffect(() => {
    for (const connection of wanted.current) open(connection);
    return () => {
      for (const live of connections.current.values()) live.close();
      connections.current.clear();
    };
    // Deliberately once: reopening on every render would take the agent over
    // from itself.
  }, [open]);

  const tabs = useMemo(
    () =>
      state.order.flatMap((key) =>
        (state.open[key] ?? []).map((sessionId) => ({ ...connectionOf(key), sessionId })),
      ),
    [state.order, state.open],
  );

  // Persist what is open, so a reload comes back to it. Derived from what is
  // really live rather than from what was asked for, so a connection that
  // failed to open is not waiting to fail again on the next reload.
  useEffect(() => {
    openConnections.set(state.order.map(connectionOf));
  }, [state.order]);

  useEffect(() => {
    if (state.focused) lastFocused.set(state.focused);
  }, [state.focused]);

  const connectTo = useCallback(
    (agentId: string, cwd: string) => {
      const connection = { agentId, cwd };
      wanted.current = [...wanted.current.filter((one) => keyOf(one) !== keyOf(connection)), connection];
      open(connection);
    },
    [open],
  );

  const newTab = useCallback((connection: Connection) => {
    void connections.current.get(keyOf(connection))?.newSession();
  }, []);

  const closeTab = useCallback(
    (tab: Tab) => {
      const key = keyOf(tab);
      const live = connections.current.get(key);
      if (!live) return;
      // The conversation stays on the agent — this closes a view of it, not the
      // thing itself. `session/close` would free its resources, which is a
      // different and more destructive thing to mean by a close button.
      live.dropSession(tab.sessionId);
      const left = (state.open[key] ?? []).filter((one) => one !== tab.sessionId);
      if (left.length === 0) {
        wanted.current = wanted.current.filter((one) => keyOf(one) !== key);
        close(key);
      }
    },
    [close, state.open],
  );

  const focus = useCallback((tab: Tab) => dispatch({ type: "focus", tab }), []);

  const busy = useCallback(
    (tab: Tab) => state.threads[keyOf(tab)]?.[tab.sessionId]?.status === "generating",
    [state.threads],
  );

  const agentNameOf = useCallback(
    (tab: Tab) => state.connections[keyOf(tab)]?.agentInfo?.name ?? tab.agentId,
    [state.connections],
  );

  const nameOf = useCallback(
    (tab: Tab) => {
      // The agent's own title for the conversation, out of the history it
      // listed. Absent until that has been asked for, and for a conversation
      // too new to have been titled — the agent's name is the honest fallback.
      const titled = state.connections[keyOf(tab)]?.history.find(
        (one) => one.sessionId === tab.sessionId,
      )?.title;
      return titled || agentNameOf(tab);
    },
    [state.connections, agentNameOf],
  );

  const current = useCurrentTab(state, connections, dispatch, open);

  return {
    tabs,
    ...(state.focused ? { focused: state.focused } : {}),
    ...(current ? { current } : {}),
    focus,
    busy,
    nameOf,
    agentNameOf,
    connect: connectTo,
    newTab,
    closeTab,
  };
}

/** The focused tab, wired to the connection it belongs to. */
function useCurrentTab(
  state: State,
  connections: RefObject<Map<string, AgentConnection>>,
  dispatch: Dispatch<Action>,
  reopen: (connection: Connection) => void,
): TabState | undefined {
  const tab = state.focused;
  const key = tab ? keyOf(tab) : "";
  const sessionId = tab?.sessionId ?? "";
  const live = connections.current.get(key);

  /**
   * Runs one lifecycle call, moves to the conversation it opened, and refreshes
   * the list it just changed.
   *
   * Moving is the point of the call. Opening, forking or resuming from the
   * history is someone asking to be taken somewhere — unlike a conversation
   * that opens on its own, which must not pull the page out from under them.
   */
  const lifecycle = useCallback(
    (run: (connection: AgentConnection) => Promise<string | undefined | void>, listAgain = true) => {
      if (!live) return;
      run(live)
        .then(async (opened) => {
          if (typeof opened === "string") {
            dispatch({ type: "focus", tab: { ...connectionOf(key), sessionId: opened } });
          }
          if (!listAgain) return;
          // The list is the agent's, not ours: a fork adds an entry, a delete
          // removes one, and a prompt can retitle a session. Asking again is
          // the only way to show what it really has.
          const { sessions, nextCursor } = await live.listSessions();
          dispatch({ type: "history", key, sessions, nextCursor });
        })
        .catch((cause: unknown) => {
          dispatch({ type: "error", key, message: describe(cause) });
        });
    },
    [live, key, dispatch],
  );

  const connection = state.connections[key];

  const moreSessionsPlease = useCallback(() => {
    const cursor = connection?.nextCursor;
    if (!live || !cursor) return;
    live
      .listSessions(cursor)
      .then(({ sessions, nextCursor }) =>
        dispatch({ type: "history", key, sessions, nextCursor, append: true }),
      )
      .catch((cause: unknown) => dispatch({ type: "error", key, message: describe(cause) }));
  }, [live, key, connection?.nextCursor, dispatch]);

  const prompt = useCallback(
    (blocks: ContentBlock[]) => {
      live?.prompt(sessionId, blocks).catch((cause: unknown) => {
        dispatch({ type: "error", key, message: describe(cause) });
      });
    },
    [live, key, sessionId, dispatch],
  );

  const reconnect = useCallback(() => {
    // The same move the other browser tab made to take it. Everything open on
    // this connection comes back with it, out of the same memory a reload uses
    // — taking it back must not cost the conversations that were on it.
    if (!tab) return;
    live?.close();
    connections.current.delete(key);
    dispatch({ type: "connectionClosed", key });
    reopen(connectionOf(key));
  }, [live, key, tab, dispatch, connections, reopen]);

  if (!tab || !connection || !live) return undefined;

  return {
    tab,
    thread: state.threads[key]?.[sessionId] ?? emptyThread(),
    terminals: state.terminals[key] ?? {},
    status: connection.status,
    ...(connection.agentInfo ? { agentInfo: connection.agentInfo } : {}),
    capabilities: connection.capabilities,
    sessions: connection.history,
    moreSessions: connection.nextCursor !== undefined,
    ...(connection.replayingSessionId
      ? { replayingSessionId: connection.replayingSessionId }
      : {}),
    frames: connection.frames,
    ...(connection.error ? { error: connection.error } : {}),
    refreshSessions: () => lifecycle(async () => {}),
    moreSessionsPlease,
    loadSession: (info) => lifecycle((c) => c.loadSession(info), false),
    resumeSession: (info) => lifecycle((c) => c.resumeSession(info), false),
    forkSession: (info) => lifecycle((c) => c.forkSession(info)),
    deleteSession: (info) => lifecycle((c) => c.deleteSession(info)),
    closeSession: (info) => lifecycle((c) => c.closeSession(info)),
    newSession: () => lifecycle((c) => c.newSession()),
    prompt,
    cancel: () => void live.cancel(sessionId),
    setMode: (modeId) => void live.setMode(sessionId, modeId),
    setConfigOption: (configId, value) => void live.setConfigOption(sessionId, configId, value),
    answerPermission: (toolCallId, optionId) => live.answerPermission(toolCallId, optionId),
    answerElicitation: (requestId, answer) => live.answerElicitation(requestId, answer),
    reconnect,
  };
}

function describe(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
