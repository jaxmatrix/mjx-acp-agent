/**
 * Drives one connection: handshake, session, prompts, and the update stream.
 *
 * The whole ACP surface the UI touches lives here, so the React layer only ever
 * sees a `Thread` and a few callbacks.
 */

import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

import { decodeChunk, ext, websocketUrl, type ConnectOptions } from "./connection";
import {
  addTerminal,
  appendTerminalOutput,
  appendUserPrompt,
  applyUpdate,
  attachPermission,
  clearPermission,
  setTerminalExit,
} from "./thread";
import type { AgentInfo, InspectorEntry, StopReason, Thread } from "./types";

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
      );

    this.#registerExtensions(app);

    try {
      const connection = app.connect(stream);
      this.#connection = connection;
      this.#agent = connection.agent;

      connection.closed
        .then(() => this.#events.status({ state: "closed" }))
        .catch((error: unknown) => {
          this.#events.status({ state: "failed", message: describe(error) });
        });

      // The browser declares only what it can actually do. The server merges in
      // `fs` and `terminal` on the way past, because those live on its side.
      const initialized = await connection.agent.request(acp.methods.agent.initialize, {
        protocolVersion: acp.PROTOCOL_VERSION,
        clientCapabilities: {},
        clientInfo: { name: "mjx-acp-viewer", version: "0.1.0" },
      });

      const session = await connection.agent.request(acp.methods.agent.session.new, {
        cwd: options.cwd ?? "",
        mcpServers: [],
      });

      this.#sessionId = session.sessionId;
      if (session.modes) {
        this.#update((thread) => ({
          ...thread,
          modes: {
            currentModeId: session.modes!.currentModeId,
            availableModes: session.modes!.availableModes,
          },
        }));
      }

      void initialized;
      this.#events.status({ state: "ready", sessionId: session.sessionId });
    } catch (error) {
      this.#events.status({ state: "failed", message: describe(error) });
      throw error;
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
      this.#update((thread) => ({
        ...thread,
        status: "idle",
        stopReason: response.stopReason as StopReason,
      }));
    } catch (error) {
      this.#update((thread) => ({ ...thread, status: "idle" }));
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

  /** Answers an outstanding permission prompt. */
  answerPermission(toolCallId: string, optionId: string | null): void {
    this.#pendingPermissions.get(toolCallId)?.(optionId);
    this.#pendingPermissions.delete(toolCallId);
    this.#update((thread) => clearPermission(thread, toolCallId));
  }

  /** Closes the connection. */
  close(): void {
    // Anything still waiting on a human will never be answered now; reject it
    // so the agent isn't left blocked on a dead socket.
    for (const resolve of this.#pendingPermissions.values()) resolve(null);
    this.#pendingPermissions.clear();
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
   * Registers the `_mjx/*` notifications.
   *
   * These carry what ACP has no vocabulary for, because it only arises when the
   * client is remote: the terminal and filesystem work the server did on our
   * behalf, and the frames we would otherwise never see.
   */
  #registerExtensions(app: acp.ClientApp): void {
    const passthrough = <T>(value: unknown): T => value as T;

    app.onNotification(ext.agentInfo, passthrough<AgentInfo>, (ctx) => {
      this.#events.agentInfo(ctx.params);
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

/** A human-readable message for anything thrown. */
function describe(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return JSON.stringify(error);
}
