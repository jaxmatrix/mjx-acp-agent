/**
 * Drives one connection: handshake, session, prompts, and the update stream.
 *
 * The whole ACP surface the UI touches lives here, so the React layer only ever
 * sees a `Thread` and a few callbacks.
 */

import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

import { decodeChunk, ext, websocketUrl, type ConnectOptions } from "./connection";
import { threadFromReplay } from "./replay";
import {
  addTerminal,
  appendTerminalOutput,
  appendUserPrompt,
  applyUpdate,
  attachElicitation,
  attachPermission,
  cancelPendingElicitations,
  clearPermission,
  completeElicitation,
  setTerminalExit,
  settleElicitation,
} from "./thread";
import type {
  AgentInfo,
  ElicitationAnswer,
  ElicitationMode,
  ElicitationState,
  InspectorEntry,
  SessionConfigOption,
  StopReason,
  Thread,
} from "./types";

/** What the UI subscribes to. */
export interface SessionEvents {
  /** A new thread state. */
  thread(next: Thread): void;
  /** Which agent the server connected us to. */
  agentInfo(info: AgentInfo): void;
  /** A frame for the inspector. */
  frame(entry: Omit<InspectorEntry, "seq" | "at">): void;
  /** Connection lifecycle. */
  status(status: SessionStatus): void;
}

/** Where the session is in its lifecycle. */
export type SessionStatus =
  | { state: "connecting" }
  | { state: "ready"; sessionId: string }
  | { state: "failed"; message: string }
  /** Another tab attached to this agent, and this socket is being closed. */
  | { state: "takenOver" }
  | { state: "closed" };

/** A live session. */
export class Session {
  #connection?: acp.ClientConnection;
  #agent?: acp.ClientContext;
  #sessionId?: string;
  #thread: Thread;
  #events: SessionEvents;
  /** Resolvers for permission prompts the user hasn't answered yet. */
  #pendingPermissions = new Map<string, (optionId: string | null) => void>();
  /**
   * Resolvers for elicitations the user hasn't answered yet, keyed by request id.
   *
   * The request id and not the thread entry, because that is the one identity
   * the replayed copy of a question and the re-asked one share.
   */
  #pendingElicitations = new Map<string, (response: acp.CreateElicitationResponse) => void>();
  /** Whether this socket rejoined an agent that was already running. */
  #resumed = false;
  /**
   * Set when another tab took this connection over.
   *
   * The socket closes straight afterwards, and the close handler must not
   * overwrite the explanation with a bare "closed".
   */
  #takenOver = false;

  constructor(initial: Thread, events: SessionEvents) {
    this.#thread = initial;
    this.#events = events;
  }

  /** The current thread. */
  get thread(): Thread {
    return this.#thread;
  }

  /** Opens the connection, handshakes, and creates a session. */
  async connect(options: ConnectOptions): Promise<void> {
    this.#events.status({ state: "connecting" });

    const stream = createWebSocketStream(websocketUrl(options));
    const app = acp
      .client({ name: "mjx-acp-viewer" })
      .onNotification(acp.methods.client.session.update, (ctx) => {
        this.#record(ctx.params, acp.methods.client.session.update);
        this.#update((thread) => applyUpdate(thread, ctx.params));
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

      const session = await connection.agent.request(acp.methods.agent.session.new, {
        cwd: options.cwd ?? "",
        mcpServers: [],
      });

      this.#sessionId = session.sessionId;
      if (session.configOptions) {
        const configOptions = session.configOptions as SessionConfigOption[];
        this.#update((thread) => ({ ...thread, configOptions }));
      }
      if (session.modes) {
        this.#update((thread) => ({
          ...thread,
          modes: {
            currentModeId: session.modes!.currentModeId,
            availableModes: session.modes!.availableModes,
          },
        }));
      }

      // On a resumed connection neither of those requests reached the agent:
      // the server answered both from what the agent said the first time, so
      // this is the same session it was already running. What it has been doing
      // since is in the thread the server folded.
      if (this.#resumed) await this.#replay(connection.agent, session.sessionId);

      void initialized;
      this.#events.status({ state: "ready", sessionId: session.sessionId });
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
  async #replay(agent: acp.ClientContext, sessionId: string): Promise<void> {
    try {
      const replayed: unknown = await agent.request(ext.sessionReplay, { sessionId });
      const thread = threadFromReplay(replayed);
      if (thread) this.#update(() => thread);
    } catch (error) {
      this.#events.frame({
        direction: "agentToClient",
        method: ext.sessionReplay,
        intercepted: true,
        line: `could not replay the thread: ${describe(error)}`,
      });
    }
  }

  /** Sends a prompt and runs a turn to completion. */
  async prompt(text: string): Promise<void> {
    if (!this.#agent || !this.#sessionId) throw new Error("not connected");

    this.#update((thread) => appendUserPrompt(thread, text));
    try {
      const response = await this.#agent.request(acp.methods.agent.session.prompt, {
        sessionId: this.#sessionId,
        prompt: [{ type: "text", text }],
      });
      this.#update((thread) => this.#endTurn(thread, response.stopReason as StopReason));
    } catch (error) {
      this.#update((thread) => this.#endTurn(thread, thread.stopReason));
      throw error;
    }
  }

  /** Asks the agent to abandon the current turn. */
  async cancel(): Promise<void> {
    if (!this.#agent || !this.#sessionId) return;
    await this.#agent.notify(acp.methods.agent.session.cancel, {
      sessionId: this.#sessionId,
    });
  }

  /** Switches the session mode. */
  async setMode(modeId: string): Promise<void> {
    if (!this.#agent || !this.#sessionId) return;
    await this.#agent.request(acp.methods.agent.session.setMode, {
      sessionId: this.#sessionId,
      modeId,
    });
    this.#update((thread) =>
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
  async setConfigOption(configId: string, value: string | boolean): Promise<void> {
    if (!this.#agent || !this.#sessionId) return;
    const response = await this.#agent.request(acp.methods.agent.session.setConfigOption, {
      sessionId: this.#sessionId,
      configId,
      // A select value goes untyped: the schema reads an absent `type` as a
      // value id, which is what a select is.
      ...(typeof value === "boolean" ? { type: "boolean" as const, value } : { value }),
    });
    const configOptions = (response.configOptions ?? []) as SessionConfigOption[];
    this.#update((thread) => ({ ...thread, configOptions }));
  }

  /** Answers an outstanding permission prompt. */
  answerPermission(toolCallId: string, optionId: string | null): void {
    this.#pendingPermissions.get(toolCallId)?.(optionId);
    this.#pendingPermissions.delete(toolCallId);
    this.#update((thread) => clearPermission(thread, toolCallId));
  }

  /** Answers an outstanding elicitation. */
  answerElicitation(requestId: string | number, answer: ElicitationAnswer): void {
    const key = String(requestId);
    this.#pendingElicitations.get(key)?.(toElicitationResponse(answer));
    this.#pendingElicitations.delete(key);
    this.#update((thread) =>
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
    for (const resolve of this.#pendingPermissions.values()) resolve(null);
    this.#pendingPermissions.clear();
    // An elicitation is cancelled rather than declined: nobody refused it, the
    // socket simply went away, and `decline` would tell the agent the user said
    // no to something they were never shown the end of.
    for (const resolve of this.#pendingElicitations.values()) resolve({ action: "cancel" });
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
    this.#record(params, acp.methods.client.session.requestPermission);

    this.#update((thread) =>
      attachPermission(
        thread,
        toolCallId,
        { requestId: toolCallId, options: params.options },
        params.toolCall.title ?? "Permission required",
      ),
    );

    return new Promise((resolve) => {
      this.#pendingPermissions.set(toolCallId, (optionId) => {
        resolve(
          optionId
            ? { outcome: { outcome: "selected", optionId } }
            : { outcome: { outcome: "cancelled" } },
        );
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
  #endTurn(thread: Thread, stopReason: StopReason | undefined): Thread {
    this.#pendingElicitations.clear();
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

    this.#update((thread) =>
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
      this.#pendingElicitations.set(String(requestId), resolve);
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
    // names — no second index to keep in step with it.
    const asked = this.#thread.entries.find(
      (entry) =>
        entry.type === "elicitation" &&
        entry.elicitation.mode.mode === "url" &&
        entry.elicitation.mode.elicitationId === elicitationId,
    );
    if (asked?.type === "elicitation") {
      const key = String(asked.elicitation.requestId);
      this.#pendingElicitations.get(key)?.({ action: "accept" });
      this.#pendingElicitations.delete(key);
    }
    this.#update((thread) => completeElicitation(thread, elicitationId));
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
        this.#update((thread) => this.#endTurn(thread, ctx.params.stopReason as StopReason));
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
        this.#update((thread) => addTerminal(thread, { id: terminalId, command, args, cwd }));
      },
    );

    app.onNotification(
      ext.terminalOutput,
      passthrough<{ terminalId: string; chunk: string; truncated: boolean }>,
      (ctx) => {
        const bytes = decodeChunk(ctx.params.chunk);
        this.#update((thread) =>
          appendTerminalOutput(thread, ctx.params.terminalId, bytes, ctx.params.truncated),
        );
      },
    );

    app.onNotification(
      ext.terminalExit,
      passthrough<{ terminalId: string; exitCode?: number; signal?: string }>,
      (ctx) => {
        this.#update((thread) =>
          setTerminalExit(thread, ctx.params.terminalId, ctx.params.exitCode, ctx.params.signal),
        );
      },
    );

    app.onNotification(
      ext.inspectorFrame,
      passthrough<Omit<InspectorEntry, "seq" | "at">>,
      (ctx) => this.#events.frame(ctx.params),
    );
  }

  #update(mutate: (thread: Thread) => Thread): void {
    this.#thread = mutate(this.#thread);
    this.#events.thread(this.#thread);
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
