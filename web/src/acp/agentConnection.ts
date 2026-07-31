/**
 * Drives one connection: handshake, session, prompts, and the update stream.
 *
 * The whole ACP surface the UI touches lives here, so the React layer only ever
 * sees a `Thread` and a few callbacks.
 */

import * as acp from "@agentclientprotocol/sdk";
import type { ContentBlock } from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

import {
  agentCapabilitiesOf,
  noCapabilities,
  type AgentCapabilities,
} from "./capabilities";
import { decodeChunk, ext, websocketUrl, type ConnectOptions } from "./connection";
import { threadFromReplay } from "./replay";
import {
  addTerminal,
  appendTerminalOutput,
  setTerminalExit,
  type Terminals,
} from "./terminals";
import {
  appendUserPrompt,
  applyUpdate,
  attachElicitation,
  attachPermission,
  cancelPendingElicitations,
  clearPermission,
  completeElicitation,
  settleElicitation,
} from "./thread";
import {
  emptyThread,
  type AgentInfo,
  type ElicitationAnswer,
  type ElicitationMode,
  type ElicitationState,
  type InspectorEntry,
  type SessionConfigOption,
  type SessionInfo,
  type SessionMode,
  type StopReason,
  type Thread,
} from "./types";

/** What the UI subscribes to. */
export interface ConnectionEvents {
  /** A new thread state, for the conversation it belongs to. */
  thread(sessionId: string, next: Thread): void;
  /** New terminal state, for every terminal on this connection. */
  terminals(next: Terminals): void;
  /** Which agent the server connected us to. */
  agentInfo(info: AgentInfo): void;
  /** What the agent said it can do, once the handshake has answered. */
  capabilities(capabilities: AgentCapabilities): void;
  /** Which session is being replayed by `session/load`, if one is. */
  replaying(sessionId?: string): void;
  /** A conversation this connection now has a thread for. */
  sessionOpened(sessionId: string): void;
  /** A conversation the agent no longer has — deleted, or closed. */
  sessionClosed(sessionId: string): void;
  /** A frame for the inspector. */
  frame(entry: Omit<InspectorEntry, "seq" | "at">): void;
  /** Connection lifecycle. */
  status(status: ConnectionStatus): void;
}

/**
 * Which conversation this tab is looking at, across a reload.
 *
 * An interface rather than the storage itself, because nothing under `acp/`
 * should know about `sessionStorage` — and because a session that is never
 * remembered still works, it just comes back to the connection's original
 * conversation.
 */
export interface OpenSessions {
  get(): string | undefined;
  set(sessionId: string): void;
  clear(): void;
}

/**
 * A session to act on: the listing it came from, or a bare id.
 *
 * Taking the whole listing is what keeps a load from being sent with the wrong
 * directory. An agent's history spans every project it has been used in, and
 * ACP refuses a load, resume or fork whose `cwd` is not the session's own — so
 * the directory has to travel with the id rather than be assumed from the
 * connection. A bare string means "the conversation this connection started",
 * which is the only one whose directory is ours to assume.
 */
export type TargetSession = string | { sessionId: string; cwd?: string };

/** Where the connection is in its lifecycle. */
export type ConnectionStatus =
  | { state: "connecting" }
  // No session id: a connection carries however many conversations the viewer
  // has open on it, and naming one of them here would name the wrong one.
  | { state: "ready" }
  | { state: "failed"; message: string }
  /** Another tab attached to this agent, and this socket is being closed. */
  | { state: "takenOver" }
  | { state: "closed" };

/** A live connection to one agent. */
export class AgentConnection {
  #connection?: acp.ClientConnection;
  #agent?: acp.ClientContext;
  /**
   * The conversation this connection started with.
   *
   * Not "the one on screen" — that is the viewer's business, and it may have
   * several of these open at once. This is only the session `session/new`
   * answered with, which is the one whose directory is ours to assume and the
   * one a question with no session of its own is shown against.
   */
  #anchor?: string;
  /**
   * A thread per conversation.
   *
   * The server has held one of these per session all along — `SessionStore` is
   * keyed by session id — and this is the browser catching up. Updates are
   * folded into the thread they name rather than into "the current one", so a
   * conversation the user is not looking at keeps running rather than going
   * quiet until they look back.
   */
  #threads = new Map<string, Thread>();
  /**
   * Every terminal the server has started for this agent.
   *
   * On the connection rather than in a thread, because that is where one
   * belongs: see `acp/terminals.ts`.
   */
  #terminals: Terminals = {};
  #events: ConnectionEvents;
  /** Resolvers for permission prompts the user hasn't answered yet. */
  #pendingPermissions = new Map<
    string,
    { sessionId: string; resolve: (optionId: string | null) => void }
  >();
  /**
   * Resolvers for elicitations the user hasn't answered yet, keyed by request id.
   *
   * The request id and not the thread entry, because that is the one identity
   * the replayed copy of a question and the re-asked one share.
   *
   * The session travels alongside because an elicitation need not have one: the
   * scope is a union, and its other arm is a question asked before any session
   * exists. Only a session-scoped one belongs to a turn, and so only one of
   * those is given up when a turn ends.
   */
  #pendingElicitations = new Map<
    string,
    { sessionId?: string; resolve: (response: acp.CreateElicitationResponse) => void }
  >();
  /** Whether this socket rejoined an agent that was already running. */
  #resumed = false;
  /**
   * Set when another tab took this connection over.
   *
   * The socket closes straight afterwards, and the close handler must not
   * overwrite the explanation with a bare "closed".
   */
  #takenOver = false;
  /** What the agent said it can do, from the `initialize` response. */
  #capabilities: AgentCapabilities = noCapabilities();
  /** Where the session on screen is remembered across a reload. */
  #memory?: OpenSessions;
  /**
   * The directory this connection was opened for.
   *
   * Only a fallback. A session belongs to the directory it was *started* in,
   * which for anything out of `session/list` is that session's own `cwd` — an
   * agent's history spans every project it has been used in, and the protocol
   * refuses a load whose `cwd` is not the session's.
   */
  #cwd = "";

  constructor(events: ConnectionEvents, memory?: OpenSessions) {
    this.#events = events;
    this.#memory = memory;
  }

  /** The conversation this connection started with. */
  get anchor(): string | undefined {
    return this.#anchor;
  }

  /** Every conversation open on this connection. */
  get sessions(): string[] {
    return [...this.#threads.keys()];
  }

  /** One conversation's thread, empty if this connection has no such session. */
  thread(sessionId: string): Thread {
    return this.#threads.get(sessionId) ?? emptyThread();
  }

  /** The terminals running on this connection. */
  get terminals(): Terminals {
    return this.#terminals;
  }

  /** What the connected agent supports. */
  get capabilities(): AgentCapabilities {
    return this.#capabilities;
  }

  /**
   * Opens the connection, handshakes, and creates a session.
   *
   * `openStream` exists so the tests can put a real ACP agent on the other end
   * without a socket. The default is the only one the app uses.
   */
  async connect(
    options: ConnectOptions,
    openStream: (options: ConnectOptions) => acp.Stream = (o) =>
      createWebSocketStream(websocketUrl(o)),
  ): Promise<void> {
    this.#events.status({ state: "connecting" });
    this.#cwd = options.cwd ?? "";

    const stream = openStream(options);
    const app = acp
      .client({ name: "mjx-acp-viewer" })
      .onNotification(acp.methods.client.session.update, (ctx) => {
        this.#record(ctx.params, acp.methods.client.session.update);
        // Into the thread this update names. One connection carries several
        // conversations — a fork leaves the original running, and the viewer
        // keeps more than one open deliberately — and each of them is somewhere
        // on screen, so none of this is ours to drop.
        this.#update(ctx.params.sessionId, (thread) => applyUpdate(thread, ctx.params));
      })
      .onRequest(acp.methods.client.session.requestPermission, (ctx) =>
        this.#requestPermission(ctx.params),
      )
      .onRequest(acp.methods.client.elicitation.create, (ctx) =>
        // `ctx.requestId` is the JSON-RPC id, not anything in the params. It is
        // the one identity this question shares with the copy of it the server
        // folded into the thread, which is what lets a reload show one form.
        //
        // A null id means a notification, which this is not — but the SDK types
        // it as possible, and a question we could never answer is one to decline
        // rather than to put in front of someone.
        ctx.requestId == null
          ? { action: "decline" }
          : this.#createElicitation(ctx.params, ctx.requestId),
      )
      .onNotification(acp.methods.client.elicitation.complete, (ctx) => {
        this.#record(ctx.params, acp.methods.client.elicitation.complete);
        this.#completeElicitation(ctx.params.elicitationId);
      });

    this.#registerExtensions(app);

    try {
      const connection = app.connect(stream);
      this.#connection = connection;
      this.#agent = connection.agent;

      connection.closed
        .then(() => {
          // A socket that was taken over has already been explained. Saying
          // "closed" now would replace the reason with the symptom.
          if (!this.#takenOver) this.#events.status({ state: "closed" });
        })
        .catch((error: unknown) => {
          if (this.#takenOver) return;
          this.#events.status({ state: "failed", message: describe(error) });
        });

      // The browser declares only what it can actually do. The server merges in
      // `fs` and `terminal` on the way past, because those live on its side.
      //
      // `configOptions.boolean` is declared because the sidebar renders a
      // toggle for one. Selects need no such opt-in, so this is the only part
      // of the config-option surface that has to be announced.
      //
      // `elicitation` is declared *here* rather than merged in by the server,
      // even though every other client capability the agent gets is the
      // server's. An elicitation needs a human, and the human is on this side
      // of the socket — so it is not a server-provided capability and the
      // relay has no business answering one.
      //
      // Both values are `{}` and not `true`: the schema types these as objects
      // (`ElicitationFormCapabilities`) with a lenient deserializer, so `true`
      // reads as "absent" on the agent's side and silently disables the whole
      // feature.
      const initialized = await connection.agent.request(acp.methods.agent.initialize, {
        protocolVersion: acp.PROTOCOL_VERSION,
        clientCapabilities: {
          session: { configOptions: { boolean: {} } },
          elicitation: { form: {}, url: {} },
        },
        clientInfo: { name: "mjx-acp-viewer", version: "0.1.0" },
      });

      // Which session methods the UI may offer. Read here rather than thrown
      // away, because it varies enormously between agents and a client that
      // guesses calls methods the agent never claimed.
      this.#capabilities = agentCapabilitiesOf(initialized);
      this.#events.capabilities(this.#capabilities);

      // Read before anything writes it: `#openSession` below records the
      // session this connection came back with, which would overwrite the one
      // the tab was actually looking at.
      const remembered = this.#memory?.get();

      // Exactly one, however many conversations this browser is restoring. The
      // relay answers the *first* `session/new` of an attachment from its
      // recording and lets every later one reach the agent — so asking once per
      // remembered conversation would leave a real, empty session behind for
      // each of them on every reload.
      const session = await connection.agent.request(acp.methods.agent.session.new, {
        cwd: options.cwd ?? "",
        mcpServers: [],
      });

      this.#anchor = session.sessionId;
      this.#openSession(session.sessionId);
      this.#applySessionState(session.sessionId, session);

      // On a resumed connection neither of those requests reached the agent:
      // the server answered both from what the agent said the first time, so
      // this is the same session it was already running. What it has been doing
      // since is in the thread the server folded.
      if (this.#resumed) {
        await this.#resumeThread(connection.agent, session.sessionId, remembered);
      }

      this.#events.status({ state: "ready" });
    } catch (error) {
      this.#events.status({ state: "failed", message: describe(error) });
      throw error;
    }
  }

  /**
   * Takes the conversation back from the server after a reload.
   *
   * The thread is **replaced**, not merged. The server's copy is the whole
   * conversation, folded from the same stream this side would have folded, so
   * anything already here is a duplicate of part of it.
   *
   * A failure is survivable: the connection is real and the agent is running,
   * so an empty timeline is a worse page rather than a broken one, and saying
   * so beats refusing to connect.
   */
  async #replay(agent: acp.ClientContext, sessionId: string): Promise<boolean> {
    try {
      const replayed: unknown = await agent.request(ext.sessionReplay, { sessionId });
      const thread = threadFromReplay(replayed);
      if (thread) this.#update(sessionId, () => thread);
      return thread !== null;
    } catch (error) {
      this.#events.frame({
        direction: "agentToClient",
        method: ext.sessionReplay,
        intercepted: true,
        line: `could not replay the thread: ${describe(error)}`,
      });
      return false;
    }
  }

  /**
   * Takes back the conversation this tab was looking at before the reload.
   *
   * Usually that is the session the connection started with, and `recorded` —
   * the id the relay answered `session/new` with from its recording — is it.
   * But once a session has been opened from the history they are different
   * conversations, and the relay has no way to know which one was on screen: it
   * answers the handshake the same either way. So the tab remembers, and what
   * it remembers is tried first.
   *
   * A remembered session the server has no thread for is one that has been
   * deleted, or belongs to a connection that has since been reaped. Falling
   * back to the recorded one is better than an empty page with a composer
   * pointed at a session that is gone.
   */
  async #resumeThread(
    agent: acp.ClientContext,
    recorded: string,
    remembered: string | undefined,
  ): Promise<void> {
    if (remembered && remembered !== recorded) {
      this.#openSession(remembered);
      if (await this.#replay(agent, remembered)) return;

      // Gone: deleted, or on a connection that has since been reaped. Drop the
      // thread we opened for it and come back to the one this connection
      // started with, which is still there.
      this.#forget(remembered);
      this.#memory?.set(recorded);
    }
    await this.#replay(agent, recorded);
  }

  /** Lists the conversations the agent knows about. */
  async listSessions(cursor?: string): Promise<{ sessions: SessionInfo[]; nextCursor?: string }> {
    if (!this.#agent) return { sessions: [] };
    const response = await this.#agent.request(acp.methods.agent.session.list, {
      ...(cursor ? { cursor } : {}),
    });
    return {
      sessions: (response.sessions ?? []) as SessionInfo[],
      ...(response.nextCursor ? { nextCursor: response.nextCursor } : {}),
    };
  }

  /**
   * Opens a past conversation, and returns the id it is now under.
   *
   * `session/load` streams the whole history back as `session/update`
   * notifications, and those arrive *during* this call — the response is not
   * the start of the replay, it is the end of it. So the thread is opened
   * *before* the request goes out; anything else would drop the first few
   * updates for naming a session this client did not yet have.
   *
   * A load that fails costs nothing: the thread it opened is dropped again,
   * unless the conversation was already open, in which case it was never ours
   * to drop.
   */
  async loadSession(session: TargetSession): Promise<string> {
    const { sessionId, cwd } = this.#target(session);
    if (!this.#agent) return sessionId;
    const wasOpen = this.#threads.has(sessionId);

    this.#openSession(sessionId);
    this.#update(sessionId, () => emptyThread());
    this.#events.replaying(sessionId);
    try {
      const loaded = await this.#agent.request(acp.methods.agent.session.load, {
        sessionId,
        cwd,
        mcpServers: [],
      });
      this.#applySessionState(sessionId, loaded);
      return sessionId;
    } catch (error) {
      if (!wasOpen) this.#forget(sessionId);
      throw error;
    } finally {
      this.#events.replaying(undefined);
    }
  }

  /**
   * Reactivates a session without replaying it.
   *
   * The difference from a load, and the only reason both exist: the agent picks
   * the conversation back up, but says nothing about what is in it. So the
   * thread comes from the server's fold — which is the same thing a reload
   * does — rather than from the agent.
   */
  async resumeSession(session: TargetSession): Promise<string> {
    const { sessionId, cwd } = this.#target(session);
    if (!this.#agent) return sessionId;
    const resumed = await this.#agent.request(acp.methods.agent.session.resume, {
      sessionId,
      cwd,
    });
    this.#openSession(sessionId);
    this.#update(sessionId, () => emptyThread());
    this.#applySessionState(sessionId, resumed);
    await this.#replay(this.#agent, sessionId);
    return sessionId;
  }

  /**
   * Branches a conversation, and moves to the branch.
   *
   * The fork carries the context of the session it came from, but not its
   * history as updates — the agent replays nothing — so the thread starts
   * empty. The original is untouched and still in the list.
   */
  async forkSession(session: TargetSession): Promise<string | undefined> {
    if (!this.#agent) return undefined;
    const { sessionId, cwd } = this.#target(session);
    const forked = await this.#agent.request(acp.methods.agent.session.fork, {
      sessionId,
      cwd,
    });
    this.#openSession(forked.sessionId);
    this.#update(forked.sessionId, () => emptyThread());
    this.#applySessionState(forked.sessionId, forked);
    return forked.sessionId;
  }

  /** Removes a conversation from the agent for good. */
  async deleteSession(session: TargetSession): Promise<void> {
    if (!this.#agent) return;
    const { sessionId } = this.#target(session);
    await this.#agent.request(acp.methods.agent.session.delete, { sessionId });
    this.#forget(sessionId);
  }

  /** Frees a conversation's resources, leaving it in the list. */
  async closeSession(session: TargetSession): Promise<void> {
    if (!this.#agent) return;
    const { sessionId } = this.#target(session);
    await this.#agent.request(acp.methods.agent.session.close, { sessionId });
    this.#forget(sessionId);
  }

  /**
   * Starts a fresh conversation on the connection we already have.
   *
   * The relay answers the *first* `session/new` of an attachment from its
   * recording; this is a later one, so it reaches the agent and really is new.
   */
  async newSession(): Promise<string | undefined> {
    if (!this.#agent) return undefined;
    const session = await this.#agent.request(acp.methods.agent.session.new, {
      // A new conversation belongs here, in the directory this connection was
      // opened for — unlike a listed one, which brings its own.
      cwd: this.#cwd,
      mcpServers: [],
    });
    this.#openSession(session.sessionId);
    this.#update(session.sessionId, () => emptyThread());
    this.#applySessionState(session.sessionId, session);
    return session.sessionId;
  }

  /**
   * The id and directory to send for a session.
   *
   * A bare id is taken to mean the conversation this connection started, which
   * is the only one whose directory is ours to assume.
   */
  #target(session: TargetSession): { sessionId: string; cwd: string } {
    if (typeof session === "string") return { sessionId: session, cwd: this.#cwd };
    return { sessionId: session.sessionId, cwd: session.cwd || this.#cwd };
  }

  /**
   * Gives a conversation a thread, and says so.
   *
   * Idempotent, because every way of arriving at a session goes through here
   * and an agent may hand back one that is already open — a load of the
   * conversation already on screen, or a resume of one in another tab. Opening
   * it twice would replace a live thread with an empty one.
   */
  #openSession(sessionId: string): void {
    this.#memory?.set(sessionId);
    if (this.#threads.has(sessionId)) return;
    this.#threads.set(sessionId, emptyThread());
    this.#events.sessionOpened(sessionId);
  }

  /**
   * Drops a conversation the agent no longer has.
   *
   * Anything of its still waiting on a human goes with it: the session is gone,
   * so an answer would have nowhere to land, and the agent is no longer
   * listening for one.
   */
  #forget(sessionId: string): void {
    if (!this.#threads.delete(sessionId)) return;
    for (const [key, pending] of this.#pendingElicitations) {
      if (pending.sessionId !== sessionId) continue;
      pending.resolve({ action: "cancel" });
      this.#pendingElicitations.delete(key);
    }
    for (const [key, pending] of this.#pendingPermissions) {
      if (pending.sessionId !== sessionId) continue;
      pending.resolve(null);
      this.#pendingPermissions.delete(key);
    }
    this.#events.sessionClosed(sessionId);
  }

  /**
   * Folds in the modes and config options a session answered with.
   *
   * `session/new`, `session/load`, `session/resume` and `session/fork` all
   * carry the same two, which is why this is not written out four times.
   */
  #applySessionState(
    sessionId: string,
    response: {
      modes?: { currentModeId: string; availableModes: SessionMode[] } | null;
      configOptions?: unknown[] | null;
    },
  ): void {
    if (response.configOptions) {
      const configOptions = response.configOptions as SessionConfigOption[];
      this.#update(sessionId, (thread) => ({ ...thread, configOptions }));
    }
    if (response.modes) {
      const modes = response.modes;
      this.#update(sessionId, (thread) => ({
        ...thread,
        modes: {
          currentModeId: modes.currentModeId,
          availableModes: modes.availableModes,
        },
      }));
    }
  }

  /**
   * Sends a prompt and runs a turn to completion.
   *
   * The blocks, not the text: a prompt carrying an `@`-mention is a run of
   * text and `resource_link` blocks, and the optimistic copy on screen has to
   * be the same blocks the agent will echo back.
   */
  async prompt(sessionId: string, prompt: ContentBlock[]): Promise<void> {
    if (!this.#agent) throw new Error("not connected");

    this.#update(sessionId, (thread) => appendUserPrompt(thread, prompt));
    try {
      const response = await this.#agent.request(acp.methods.agent.session.prompt, {
        sessionId,
        prompt,
      });
      this.#update(sessionId, (thread) =>
        this.#endTurn(sessionId, thread, response.stopReason as StopReason),
      );
    } catch (error) {
      this.#update(sessionId, (thread) => this.#endTurn(sessionId, thread, thread.stopReason));
      throw error;
    }
  }

  /** Asks the agent to abandon one conversation's turn. */
  async cancel(sessionId: string): Promise<void> {
    if (!this.#agent) return;
    await this.#agent.notify(acp.methods.agent.session.cancel, { sessionId });
  }

  /** Switches a session's mode. */
  async setMode(sessionId: string, modeId: string): Promise<void> {
    if (!this.#agent) return;
    await this.#agent.request(acp.methods.agent.session.setMode, { sessionId, modeId });
    this.#update(sessionId, (thread) =>
      thread.modes ? { ...thread, modes: { ...thread.modes, currentModeId: modeId } } : thread,
    );
  }

  /**
   * Switches a session config option — the model, the thinking level, and
   * whatever else the agent offers.
   *
   * Unlike `setMode`, nothing is patched optimistically. The response carries
   * the whole refreshed set, because setting one option can change another's
   * available values, so the agent's answer replaces what we had.
   */
  async setConfigOption(
    sessionId: string,
    configId: string,
    value: string | boolean,
  ): Promise<void> {
    if (!this.#agent) return;
    const response = await this.#agent.request(acp.methods.agent.session.setConfigOption, {
      sessionId,
      configId,
      // A select value goes untyped: the schema reads an absent `type` as a
      // value id, which is what a select is.
      ...(typeof value === "boolean" ? { type: "boolean" as const, value } : { value }),
    });
    const configOptions = (response.configOptions ?? []) as SessionConfigOption[];
    this.#update(sessionId, (thread) => ({ ...thread, configOptions }));
  }

  /**
   * Answers an outstanding permission prompt.
   *
   * The conversation it belongs to comes from the pending entry rather than
   * from the caller: the card the user pressed knows the tool call, and the
   * tool call is what the agent asked about.
   */
  answerPermission(toolCallId: string, optionId: string | null): void {
    const pending = this.#pendingPermissions.get(toolCallId);
    if (!pending) return;
    pending.resolve(optionId);
    this.#pendingPermissions.delete(toolCallId);
    this.#update(pending.sessionId, (thread) => clearPermission(thread, toolCallId));
  }

  /** Answers an outstanding elicitation. */
  answerElicitation(requestId: string | number, answer: ElicitationAnswer): void {
    const key = String(requestId);
    const pending = this.#pendingElicitations.get(key);
    if (!pending) return;
    pending.resolve(toElicitationResponse(answer));
    this.#pendingElicitations.delete(key);
    this.#update(pending.sessionId ?? this.#anchor ?? "", (thread) =>
      settleElicitation(
        thread,
        requestId,
        SETTLED[answer.action],
        answer.action === "accept" ? answer.content : undefined,
      ),
    );
  }

  /** Closes the connection. */
  close(): void {
    // Anything still waiting on a human will never be answered now; reject it
    // so the agent isn't left blocked on a dead socket.
    for (const { resolve } of this.#pendingPermissions.values()) resolve(null);
    this.#pendingPermissions.clear();
    // An elicitation is cancelled rather than declined: nobody refused it, the
    // socket simply went away, and `decline` would tell the agent the user said
    // no to something they were never shown the end of.
    for (const { resolve } of this.#pendingElicitations.values()) resolve({ action: "cancel" });
    this.#pendingElicitations.clear();
    this.#connection?.close();
  }

  /**
   * Blocks the agent until the user answers.
   *
   * ACP models permission as an ordinary request, so simply not resolving is
   * what "waiting for the user" means — the agent's turn is suspended until
   * this promise settles.
   */
  #requestPermission(
    params: acp.RequestPermissionRequest,
  ): Promise<acp.RequestPermissionResponse> {
    const toolCallId = params.toolCall.toolCallId;
    const sessionId = params.sessionId;
    this.#record(params, acp.methods.client.session.requestPermission);

    this.#update(sessionId, (thread) =>
      attachPermission(
        thread,
        toolCallId,
        { requestId: toolCallId, options: params.options },
        params.toolCall.title ?? "Permission required",
      ),
    );

    return new Promise((resolve) => {
      this.#pendingPermissions.set(toolCallId, {
        sessionId,
        resolve: (optionId) => {
          resolve(
            optionId
              ? { outcome: { outcome: "selected", optionId } }
              : { outcome: { outcome: "cancelled" } },
          );
        },
      });
    });
  }

  /**
   * Marks a turn finished, and gives up on what it was still asking.
   *
   * A form left on screen after the turn is over would collect an answer with
   * nowhere to go: the agent has stopped listening. The server does the same to
   * its own copy of the thread, so a reload agrees with what was on screen.
   */
  #endTurn(sessionId: string, thread: Thread, stopReason: StopReason | undefined): Thread {
    for (const [key, pending] of this.#pendingElicitations) {
      // Only this conversation's, and only the ones that have a conversation.
      // Another session's turn is still running, and a request-scoped question
      // belongs to no turn at all — it was asked outside one, so no turn ending
      // is the end of it.
      if (pending.sessionId !== sessionId) continue;
      this.#pendingElicitations.delete(key);
    }
    return { ...cancelPendingElicitations(thread), status: "idle", stopReason };
  }

  /**
   * Blocks the agent until the user answers a structured question.
   *
   * The same shape as `#requestPermission` — an unresolved promise is what
   * "waiting for the user" means — with one difference: a mode this client
   * cannot draw is declined immediately rather than waited on. The spec forbids
   * rendering an unknown mode as if we understood it, and leaving the agent
   * hanging on a form nobody can see would be worse than saying no.
   */
  #createElicitation(
    params: acp.CreateElicitationRequest,
    requestId: string | number,
  ): Promise<acp.CreateElicitationResponse> {
    this.#record(params, acp.methods.client.elicitation.create);

    const mode = toElicitationMode(params);
    if (!mode) return Promise.resolve({ action: "decline" });

    // A question asked outside any session — during auth, or configuration —
    // has no conversation of its own to appear in. It is shown against the one
    // this connection started with, which is the only thread that is certainly
    // there, and it is answerable wherever it is shown.
    const sessionId = sessionOf(params);
    const shownIn = sessionId ?? this.#anchor ?? "";

    this.#update(shownIn, (thread) =>
      attachElicitation(thread, {
        // Numbered by position, the way every other minted entry id is. A
        // replayed copy of this question is renumbered the same way, so the two
        // agree without either having to know about the other.
        id: `elicitation-${thread.entries.length}`,
        requestId,
        message: params.message,
        ...(toolCallOf(params) ? { toolCallId: toolCallOf(params)! } : {}),
        ...mode,
        state: "pending",
      }),
    );

    return new Promise((resolve) => {
      this.#pendingElicitations.set(String(requestId), {
        ...(sessionId ? { sessionId } : {}),
        resolve,
      });
    });
  }

  /**
   * Finishes a URL-mode exchange the agent says is over.
   *
   * The request is still outstanding at this point — URL mode ends with this
   * notification rather than with the user pressing anything — so it is answered
   * here. `accept` is the honest reading: the agent would not be telling us it
   * is finished unless it had what it needed.
   */
  #completeElicitation(elicitationId: string): void {
    // Which request to answer is in the thread, on the entry this notification
    // names — no second index to keep in step with it. The notification names
    // no session either, so every conversation is looked through; the id is the
    // agent's and unique across all of them.
    for (const [sessionId, thread] of this.#threads) {
      const asked = thread.entries.find(
        (entry) =>
          entry.type === "elicitation" &&
          entry.elicitation.mode.mode === "url" &&
          entry.elicitation.mode.elicitationId === elicitationId,
      );
      if (asked?.type !== "elicitation") continue;

      const key = String(asked.elicitation.requestId);
      this.#pendingElicitations.get(key)?.resolve({ action: "accept" });
      this.#pendingElicitations.delete(key);
      this.#update(sessionId, (next) => completeElicitation(next, elicitationId));
      return;
    }
  }

  /**
   * Registers the `_mjx/*` notifications.
   *
   * These carry what ACP has no vocabulary for, because it only arises when the
   * client is remote: the terminal and filesystem work the server did on our
   * behalf, and the frames we would otherwise never see.
   */
  #registerExtensions(app: acp.ClientApp): void {
    const passthrough = <T>(value: unknown): T => value as T;

    app.onNotification(ext.agentInfo, passthrough<AgentInfo>, (ctx) => {
      // Sent immediately after the handshake response, a full round trip
      // before `session/new` resolves, so this is always set by the time
      // `connect` reads it.
      this.#resumed = ctx.params.resumed === true;
      this.#events.agentInfo(ctx.params);
    });

    app.onNotification(
      ext.sessionTurnEnded,
      passthrough<{ sessionId: string; stopReason: string }>,
      (ctx) => {
        // A turn started on a socket that has since gone. ACP ends a turn with
        // the response to `session/prompt`, and that response was owed to the
        // browser that sent it — so without this the thread would sit at
        // "generating" for a turn that finished minutes ago.
        //
        // The conversation it names, and no other: the connection may well
        // have another turn running, and ending that one would leave an agent
        // working against a thread that says it is idle.
        const { sessionId } = ctx.params;
        this.#update(sessionId, (thread) =>
          this.#endTurn(sessionId, thread, ctx.params.stopReason as StopReason),
        );
      },
    );

    app.onNotification(ext.connectionTakenOver, passthrough<unknown>, () => {
      this.#takenOver = true;
      this.#events.status({ state: "takenOver" });
    });

    app.onNotification(ext.agentStderr, passthrough<{ line: string }>, (ctx) => {
      this.#events.frame({
        direction: "agentToClient",
        method: "stderr",
        intercepted: true,
        line: ctx.params.line,
      });
    });

    app.onNotification(
      ext.terminalCreated,
      passthrough<{ terminalId: string; command: string; args: string[]; cwd: string }>,
      (ctx) => {
        const { terminalId, command, args, cwd } = ctx.params;
        this.#updateTerminals((terminals) =>
          addTerminal(terminals, { id: terminalId, command, args, cwd }),
        );
      },
    );

    app.onNotification(
      ext.terminalOutput,
      passthrough<{ terminalId: string; chunk: string; truncated: boolean }>,
      (ctx) => {
        const bytes = decodeChunk(ctx.params.chunk);
        this.#updateTerminals((terminals) =>
          appendTerminalOutput(terminals, ctx.params.terminalId, bytes, ctx.params.truncated),
        );
      },
    );

    app.onNotification(
      ext.terminalExit,
      passthrough<{ terminalId: string; exitCode?: number; signal?: string }>,
      (ctx) => {
        this.#updateTerminals((terminals) =>
          setTerminalExit(terminals, ctx.params.terminalId, ctx.params.exitCode, ctx.params.signal),
        );
      },
    );

    app.onNotification(
      ext.inspectorFrame,
      passthrough<Omit<InspectorEntry, "seq" | "at">>,
      (ctx) => this.#events.frame(ctx.params),
    );
  }

  #update(sessionId: string, mutate: (thread: Thread) => Thread): void {
    const next = mutate(this.#threads.get(sessionId) ?? emptyThread());
    this.#threads.set(sessionId, next);
    this.#events.thread(sessionId, next);
  }

  #updateTerminals(mutate: (terminals: Terminals) => Terminals): void {
    this.#terminals = mutate(this.#terminals);
    this.#events.terminals(this.#terminals);
  }

  #record(params: unknown, method: string): void {
    this.#events.frame({
      direction: "agentToClient",
      method,
      intercepted: false,
      line: JSON.stringify({ jsonrpc: "2.0", method, params }),
    });
  }
}

/** The protocol's action, as the thread spells the state it leaves behind. */
const SETTLED: Record<ElicitationAnswer["action"], ElicitationState> = {
  accept: "accepted",
  decline: "declined",
  cancel: "cancelled",
};

function toElicitationResponse(answer: ElicitationAnswer): acp.CreateElicitationResponse {
  return answer.action === "accept" && answer.content
    ? { action: "accept", content: answer.content }
    : { action: answer.action };
}

/**
 * The tool call an elicitation names, if it names one.
 *
 * Only a session-scoped elicitation can: `toolCallId` lives on that scope, and
 * the scope is flattened into the request, so it is absent rather than null on
 * anything else.
 */
function toolCallOf(params: acp.CreateElicitationRequest): string | undefined {
  const scoped = params as { toolCallId?: string | null };
  return typeof scoped.toolCallId === "string" ? scoped.toolCallId : undefined;
}

/**
 * The conversation an elicitation belongs to, if it belongs to one.
 *
 * The scope is a union with two arms, flattened onto the request the way
 * `toolCallId` above is: a session-scoped question carries `sessionId`, and a
 * request-scoped one carries only `requestId` — that is how an agent asks
 * something during auth or configuration, before any session exists. The server
 * makes exactly this distinction when it folds one into a thread.
 */
function sessionOf(params: acp.CreateElicitationRequest): string | undefined {
  const scoped = params as { sessionId?: string | null };
  return typeof scoped.sessionId === "string" ? scoped.sessionId : undefined;
}

/**
 * The mode, or null if this client cannot draw it.
 *
 * Narrowed with the SDK's own guards rather than by reading `mode`: the union is
 * flattened on the wire, and the guards are generated from the schema.
 */
function toElicitationMode(params: acp.CreateElicitationRequest): { mode: ElicitationMode } | null {
  if (acp.CreateElicitationRequest.isForm(params)) {
    return { mode: { mode: "form", requestedSchema: params.requestedSchema } };
  }
  if (acp.CreateElicitationRequest.isUrl(params)) {
    return { mode: { mode: "url", elicitationId: params.elicitationId, url: params.url } };
  }
  return null;
}

/** A human-readable message for anything thrown. */
function describe(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return JSON.stringify(error);
}
