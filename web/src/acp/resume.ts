/**
 * What a tab remembers across a reload.
 *
 * The server keeps an agent alive for a while after its socket goes, and hands
 * out an id to come back with. Somewhere has to hold that id across the reload
 * — along with which agent and directory it was, or the page comes back to the
 * picker and the conversation the server kept is invisible.
 *
 * `sessionStorage`, because it survives a reload but not a new tab, which is
 * exactly the lifetime of a connection id. `localStorage` would share the id
 * with every tab in the window, and since a second socket takes the connection
 * over, that would have tabs fighting over one agent.
 */

/** The agent and directory a tab was connected to. */
export interface Choice {
  agentId: string;
  cwd: string;
}

/** Reads and writes the id a reload comes back with. */
export interface ResumeStore {
  get(agentId: string, cwd: string): string | undefined;
  set(agentId: string, cwd: string, connectionId: string): void;
  clear(agentId: string, cwd: string): void;
}

/**
 * Reads and writes which of an agent's conversations this tab has open.
 *
 * Separate from the connection id because it answers a different question: the
 * connection id is *which agent*, and this is *which of its sessions*. They come
 * apart the moment the user opens one from the history — the relay still answers
 * a repeat `session/new` with the session that connection started with, which is
 * no longer the only one being looked at.
 *
 * A list rather than one id, because a viewer that can hold several
 * conversations has to bring all of them back, not the last one touched.
 */
export interface SessionStore {
  get(agentId: string, cwd: string): string[];
  set(agentId: string, cwd: string, sessionIds: string[]): void;
  clear(agentId: string, cwd: string): void;
}

/**
 * Reads and writes the agents this tab has open, in order.
 *
 * A list rather than one, because the viewer holds a connection per agent and
 * directory in play — two agents on one workspace is two sockets, since a socket
 * is bound to one of each. Which *conversations* are open on each of them is a
 * different question, answered by {@link SessionStore}.
 */
export interface ConnectionsStore {
  get(): Choice[];
  set(connections: Choice[]): void;
}

/**
 * Reads and writes which conversation this tab was last looking at.
 *
 * Its own key rather than a field on either list above: the lists say what is
 * open, and this says where the eye was. A focus naming something that is no
 * longer open is simply ignored.
 */
export interface FocusStore {
  get(): Tab | undefined;
  set(tab: Tab): void;
}

/** One conversation open in the viewer. */
export interface Tab extends Choice {
  sessionId: string;
}

const CONNECTIONS_KEY = "mjx.connections";
const FOCUS_KEY = "mjx.focus";

/**
 * A store over `storage`, defaulting to this tab's `sessionStorage`.
 *
 * Injectable because it has to be testable, and the tests here run under Node
 * with no `window` at all.
 */
export function resumeStore(storage: Storage | undefined = safeSessionStorage()): ResumeStore {
  return {
    get: (agentId, cwd) => read(storage, resumeKey(agentId, cwd)),
    set: (agentId, cwd, connectionId) => write(storage, resumeKey(agentId, cwd), connectionId),
    clear: (agentId, cwd) => write(storage, resumeKey(agentId, cwd), undefined),
  };
}

/** The sessions to come back to, across a reload. */
export function sessionStore(storage: Storage | undefined = safeSessionStorage()): SessionStore {
  return {
    get(agentId, cwd) {
      const stored = read(storage, sessionKey(agentId, cwd));
      if (!stored) return [];
      try {
        const parsed: unknown = JSON.parse(stored);
        // Anything under our key that is not a list of ids is not ours, and a
        // conversation restored from a shape we misread would be the wrong one.
        return Array.isArray(parsed) && parsed.every((id) => typeof id === "string")
          ? (parsed as string[])
          : [];
      } catch {
        return [];
      }
    },
    set: (agentId, cwd, sessionIds) =>
      write(
        storage,
        sessionKey(agentId, cwd),
        sessionIds.length > 0 ? JSON.stringify(sessionIds) : undefined,
      ),
    clear: (agentId, cwd) => write(storage, sessionKey(agentId, cwd), undefined),
  };
}

/** The agents and directories to come back to, across a reload. */
export function connectionsStore(
  storage: Storage | undefined = safeSessionStorage(),
): ConnectionsStore {
  return {
    get() {
      const parsed = parse(read(storage, CONNECTIONS_KEY));
      // Whatever is in there is not ours. Start from the picker.
      if (!Array.isArray(parsed)) return [];
      return parsed.filter(isChoice).map(({ agentId, cwd }) => ({ agentId, cwd }));
    },
    set: (connections) =>
      write(
        storage,
        CONNECTIONS_KEY,
        connections.length > 0 ? JSON.stringify(connections) : undefined,
      ),
  };
}

/** The conversation to come back to, across a reload. */
export function focusStore(storage: Storage | undefined = safeSessionStorage()): FocusStore {
  return {
    get() {
      const parsed = parse(read(storage, FOCUS_KEY));
      if (!isChoice(parsed) || typeof (parsed as Tab).sessionId !== "string") return undefined;
      const tab = parsed as Tab;
      return { agentId: tab.agentId, cwd: tab.cwd, sessionId: tab.sessionId };
    },
    set: (tab) => write(storage, FOCUS_KEY, JSON.stringify(tab)),
  };
}

function parse(stored: string | undefined): unknown {
  if (!stored) return undefined;
  try {
    return JSON.parse(stored);
  } catch {
    return undefined;
  }
}

function isChoice(value: unknown): value is Choice {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as Choice).agentId === "string" &&
    typeof (value as Choice).cwd === "string"
  );
}

function resumeKey(agentId: string, cwd: string): string {
  return `mjx.resume.${agentId}.${cwd}`;
}

function sessionKey(agentId: string, cwd: string): string {
  return `mjx.sessions.${agentId}.${cwd}`;
}

/**
 * Storage throws rather than returns when it is disabled, which both private
 * browsing and some corporate policies do. Losing the ability to resume is a
 * worse page, not a broken one, so every access degrades to "nothing stored".
 */
function read(storage: Storage | undefined, key: string): string | undefined {
  try {
    return storage?.getItem(key) ?? undefined;
  } catch {
    return undefined;
  }
}

function write(storage: Storage | undefined, key: string, value: string | undefined): void {
  try {
    if (value === undefined) storage?.removeItem(key);
    else storage?.setItem(key, value);
  } catch {
    // Nothing to do about it, and nothing worth telling the user.
  }
}

function safeSessionStorage(): Storage | undefined {
  try {
    return globalThis.sessionStorage;
  } catch {
    return undefined;
  }
}
