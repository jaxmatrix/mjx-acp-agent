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
 * Reads and writes which conversation this tab is looking at.
 *
 * Separate from the connection id because it answers a different question: the
 * connection id is *which agent*, and this is *which of its sessions*. They come
 * apart the moment the user opens one from the history — the relay still answers
 * a repeat `session/new` with the session that connection started with, which is
 * no longer the one on screen.
 */
export interface SessionStore {
  get(agentId: string, cwd: string): string | undefined;
  set(agentId: string, cwd: string, sessionId: string): void;
  clear(agentId: string, cwd: string): void;
}

/** Reads and writes what this tab was last connected to. */
export interface ChoiceStore {
  get(): Choice | undefined;
  set(choice: Choice): void;
  clear(): void;
}

const CHOICE_KEY = "mjx.connection";

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

/** The session to come back to, across a reload. */
export function sessionStore(storage: Storage | undefined = safeSessionStorage()): SessionStore {
  return {
    get: (agentId, cwd) => read(storage, sessionKey(agentId, cwd)),
    set: (agentId, cwd, sessionId) => write(storage, sessionKey(agentId, cwd), sessionId),
    clear: (agentId, cwd) => write(storage, sessionKey(agentId, cwd), undefined),
  };
}

/** The agent and directory to come back to, across a reload. */
export function choiceStore(storage: Storage | undefined = safeSessionStorage()): ChoiceStore {
  return {
    get() {
      const stored = read(storage, CHOICE_KEY);
      if (!stored) return undefined;
      try {
        const parsed: unknown = JSON.parse(stored);
        if (
          typeof parsed === "object" &&
          parsed !== null &&
          typeof (parsed as Choice).agentId === "string" &&
          typeof (parsed as Choice).cwd === "string"
        ) {
          return { agentId: (parsed as Choice).agentId, cwd: (parsed as Choice).cwd };
        }
      } catch {
        // Whatever is in there is not ours. Start from the picker.
      }
      return undefined;
    },
    set: (choice) => write(storage, CHOICE_KEY, JSON.stringify(choice)),
    clear: () => write(storage, CHOICE_KEY, undefined),
  };
}

function resumeKey(agentId: string, cwd: string): string {
  return `mjx.resume.${agentId}.${cwd}`;
}

function sessionKey(agentId: string, cwd: string): string {
  return `mjx.session.${agentId}.${cwd}`;
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
