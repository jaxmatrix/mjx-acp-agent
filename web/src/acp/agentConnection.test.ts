/**
 * Drives {@link AgentConnection} against a real ACP agent, in process.
 *
 * The agent here is the SDK's own agent app rather than a hand-written double,
 * so every frame is parsed by the generated schemas on the way past — a request
 * this client builds wrongly fails here rather than against a real agent. What
 * it stands in for is the *server*, which for everything in this file is a wire
 * that forwards: the session lifecycle is not intercepted.
 */

import * as acp from "@agentclientprotocol/sdk";
import { describe, expect, test, vi } from "vitest";

import { AgentConnection, type ConnectionEvents, type OpenSessions } from "./agentConnection";
import type { Terminals } from "./terminals";
import { emptyThread, type Thread } from "./types";

/** A conversation the fake agent remembers, as the updates that built it. */
const YESTERDAY = [
  { sessionUpdate: "user_message_chunk", content: { type: "text", text: "rename avg" } },
  { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "renamed it" } },
] as const;

/** What the test agent was asked to do, so a test can assert on it. */
interface AgentLog {
  methods: string[];
  loaded: string[];
  replayed: string[];
  /** Every `session/prompt` the agent received, blocks and all. */
  prompts: acp.ContentBlock[][];
}

/**
 * An ACP agent that lists, loads and forks, wired straight to `AgentConnection`.
 *
 * A pair of object streams rather than a socket: the SDK reads and writes
 * messages, not bytes, so this is the same code path with the transport taken
 * out.
 */
function connectedAgent(
  capabilities: unknown,
  resumed = false,
  threads: Set<string> = new Set(["yesterday", "fresh"]),
  /** Sessions whose turn parks on an unanswered question instead of ending. */
  parks: Set<string> = new Set(),
): { stream: acp.Stream; log: AgentLog; push: Push } {
  const log: AgentLog = { methods: [], loaded: [], replayed: [], prompts: [] };
  let minted = 0;
  /** The agent's handle on the client, for anything it starts itself. */
  let client: acp.AgentContext | undefined;
  /** Where each session lives, as an agent's history really does span projects. */
  const homes: Record<string, string> = {
    yesterday: "/w",
    fresh: "/w",
    forked: "/w",
    elsewhere: "/other",
  };
  const mustMatchCwd = (sessionId: string, cwd: string | undefined) => {
    const home = homes[sessionId];
    if (home && cwd !== home) {
      // -32002, the way a real agent refuses it: the session exists, but not
      // in the directory that was asked for.
      throw new acp.RequestError(-32002, `${sessionId} belongs to ${home}, not ${cwd}`);
    }
  };
  const toAgent = new TransformStream<acp.AnyMessage, acp.AnyMessage>();
  const toClient = new TransformStream<acp.AnyMessage, acp.AnyMessage>();

  const app = acp
    .agent({ name: "fake-agent" })
    .onRequest(acp.methods.agent.initialize, async (ctx) => {
      log.methods.push("initialize");
      client = ctx.client;
      // The server's announcement, which is what tells the browser it rejoined
      // an agent that was already running. Sent before the response, the way
      // the relay sends it.
      await ctx.client.notify("_mjx/agent/info" as never, {
        agentId: "fake",
        name: "Fake",
        command: [],
        cwd: "/w",
        connectionId: "c1",
        resumed,
      } as never);
      return { protocolVersion: acp.PROTOCOL_VERSION, agentCapabilities: capabilities } as never;
    })
    .onRequest(acp.methods.agent.session.prompt, async (ctx) => {
      log.methods.push("session/prompt");
      log.prompts.push(ctx.params.prompt);
      // A turn that stops on a question nobody has answered yet. The request
      // is awaited, so this handler never returns and the turn stays open —
      // which is what parking really is.
      if (parks.has(ctx.params.sessionId)) {
        await ctx.client.request(acp.methods.client.elicitation.create, {
          sessionId: ctx.params.sessionId,
          message: "which branch?",
          mode: "form",
          requestedSchema: {
            type: "object",
            properties: { branch: { type: "string" } },
          },
        } as never);
      }
      // The server's announcement, not the agent's: a terminal the relay
      // started on the agent's behalf. Sent during the turn, where one is.
      await ctx.client.notify("_mjx/terminal/created" as never, {
        terminalId: `term-${log.prompts.length}`,
        command: "cargo",
        args: ["test"],
        cwd: "/w",
      } as never);
      return { stopReason: "end_turn" as const };
    })
    .onRequest(acp.methods.agent.session.new, () => {
      log.methods.push("session/new");
      // The first is the conversation this connection is anchored to, and the
      // one a reload is answered with from the relay's recording. Later ones
      // really are new, so they need ids of their own.
      minted += 1;
      return { sessionId: minted === 1 ? "fresh" : `fresh-${minted}` };
    })
    .onRequest(acp.methods.agent.session.list, () => {
      log.methods.push("session/list");
      return {
        sessions: [
          { sessionId: "yesterday", cwd: "/w", title: "Rename the helper" },
          { sessionId: "elsewhere", cwd: "/other", title: "Another project" },
          { sessionId: "fresh", cwd: "/w" },
        ],
      };
    })
    .onRequest(acp.methods.agent.session.load, async (ctx) => {
      log.methods.push("session/load");
      log.loaded.push(`${ctx.params.sessionId}@${ctx.params.cwd}`);
      // ACP's rule, and the one a client gets wrong: a session belongs to the
      // directory it was started in, and a load naming any other is refused.
      mustMatchCwd(ctx.params.sessionId, ctx.params.cwd);
      // Streamed *before* the response, which is the ordering the client has to
      // survive: the response is the end of the replay, not the start.
      for (const update of YESTERDAY) {
        await ctx.client.notify(acp.methods.client.session.update, {
          sessionId: ctx.params.sessionId,
          update,
        } as never);
      }
      return { configOptions: [] };
    })
    .onRequest(acp.methods.agent.session.fork, (ctx) => {
      log.methods.push("session/fork");
      mustMatchCwd(ctx.params.sessionId, ctx.params.cwd);
      return { sessionId: "forked" };
    })
    .onRequest(acp.methods.agent.session.delete, () => {
      log.methods.push("session/delete");
      return {};
    });

  // The one thing here that is the *server* rather than the agent: the thread
  // it folded, which is how a reload gets its conversation back without asking
  // the agent to replay anything.
  const asIs = (params: unknown) => params as { sessionId: string };
  app.onRequest("_mjx/session/replay" as never, asIs as never, ((ctx: {
    params: { sessionId: string };
  }) => {
    log.replayed.push(ctx.params.sessionId);
    if (!threads.has(ctx.params.sessionId)) return null;
    return {
      entries: [{ type: "user", content: [{ type: "text", text: "rename avg" }] }],
      status: "idle",
    };
  }) as never);

  app.connect({ readable: toAgent.readable, writable: toClient.writable });
  const push: Push = {
    update: (sessionId, update) =>
      client!.notify(acp.methods.client.session.update, { sessionId, update } as never),
    turnEnded: (sessionId, stopReason) =>
      client!.notify("_mjx/session/turn_ended" as never, { sessionId, stopReason } as never),
  };
  return { stream: { readable: toClient.readable, writable: toAgent.writable }, log, push };
}

/** What a test can make the agent — or the server — say, after connecting. */
interface Push {
  update(sessionId: string, update: unknown): Promise<void>;
  turnEnded(sessionId: string, stopReason: string): Promise<void>;
}

/** Everything `AgentConnection` reports, collected. */
function watcher(): {
  events: ConnectionEvents;
  /** One conversation's thread, or the last one opened if none is named. */
  thread: (sessionId?: string) => Thread;
  terminals: () => Terminals;
  replaying: () => string[];
  /** Every conversation opened, in order, and every one dropped. */
  opened: () => string[];
  closed: () => string[];
} {
  const threads = new Map<string, Thread>();
  let terminals: Terminals = {};
  const replaying: string[] = [];
  const opened: string[] = [];
  const closed: string[] = [];
  return {
    thread: (sessionId) =>
      threads.get(sessionId ?? opened[opened.length - 1] ?? "") ?? emptyThread(),
    terminals: () => terminals,
    replaying: () => replaying,
    opened: () => opened,
    closed: () => closed,
    events: {
      thread: (sessionId, next) => threads.set(sessionId, next),
      terminals: (next) => (terminals = next),
      agentInfo: () => {},
      capabilities: () => {},
      replaying: (sessionId) => replaying.push(sessionId ?? "—"),
      sessionOpened: (sessionId) => opened.push(sessionId),
      sessionClosed: (sessionId) => closed.push(sessionId),
      frame: () => {},
      status: () => {},
    },
  };
}

/** A memory that starts holding `sessionId`, as a reloaded tab's would. */
function memory(sessionId?: string): OpenSessions & { value: () => string | undefined } {
  let held = sessionId;
  return {
    get: () => held,
    set: (id) => (held = id),
    clear: () => (held = undefined),
    value: () => held,
  };
}

const LIFECYCLE = {
  loadSession: true,
  sessionCapabilities: { list: {}, delete: {}, fork: {}, resume: {}, close: {} },
};

async function connected(
  capabilities: unknown = LIFECYCLE,
  held?: string,
  threads?: Set<string>,
  parks?: Set<string>,
) {
  const seen = watcher();
  const held_ = memory(held);
  // A tab that remembers a session is a tab that reloaded, so the connection it
  // gets back is a resumed one.
  const agent = connectedAgent(capabilities, held !== undefined, threads, parks);
  const session = new AgentConnection(seen.events, held_);
  await session.connect({ agentId: "fake", cwd: "/w" }, () => agent.stream);
  /**
   * The conversation a single-pane view would be showing: the last one opened
   * that is still open. A view that has one dropped under it moves to another,
   * which is why the closed ones come out.
   */
  const on = () => {
    const live = seen.opened().filter((id) => !seen.closed().includes(id));
    return live[live.length - 1] ?? "";
  };
  return {
    session,
    seen,
    on,
    memory: held_,
    log: agent.log,
    push: agent.push,
    prompt: (blocks: acp.ContentBlock[]) => session.prompt(on(), blocks),
  };
}

describe("a session that can reach its agent's history", () => {
  test("takes the lifecycle out of the handshake", async () => {
    const { session } = await connected();
    expect(session.capabilities.session.list).toBe(true);
    expect(session.capabilities.loadSession).toBe(true);

    const bare = await connected({ loadSession: false });
    expect(bare.session.capabilities.session.list).toBe(false);
  });

  test("lists what the agent has", async () => {
    const { session } = await connected();
    const { sessions } = await session.listSessions();
    expect(sessions.map((s) => s.sessionId)).toEqual(["yesterday", "elsewhere", "fresh"]);
    expect(sessions[0]?.title).toBe("Rename the helper");
  });

  test("a load rebuilds the thread from the replay alone", async () => {
    const { session, seen, log, on, prompt } = await connected();

    // Something on screen first, so an empty thread afterwards would be an
    // accident rather than the point.
    await prompt([{ type: "text", text: "hello" }]).catch(() => {});
    await session.loadSession({ sessionId: "yesterday", cwd: "/w" });

    // Exactly the replayed conversation: the prompt that was on screen belonged
    // to the session we left.
    expect(seen.thread().entries).toHaveLength(2);
    expect(seen.thread().entries[0]?.type).toBe("user");
    expect(on()).toBe("yesterday");
    expect(seen.replaying()).toEqual(["yesterday", "—"]);
    expect(log.loaded).toEqual(["yesterday@/w"]);
  });

  test("a terminal's scrollback outlives the conversation it was started in", async () => {
    // Why terminals are held by the connection rather than by a thread. A load
    // replaces the thread wholesale — it is rebuilt from the server's fold,
    // which has no terminals in it — so scrollback kept in one was lost the
    // moment a past conversation was opened.
    const { session, seen, prompt } = await connected();
    await prompt([{ type: "text", text: "run the tests" }]).catch(() => {});
    expect(Object.keys(seen.terminals())).toEqual(["term-1"]);

    await session.loadSession({ sessionId: "yesterday", cwd: "/w" });

    expect(seen.thread().entries).toHaveLength(2);
    expect(seen.terminals()["term-1"]?.command).toBe("cargo");
  });

  test("a second load is not a longer conversation", async () => {
    const { session, seen } = await connected();
    await session.loadSession({ sessionId: "yesterday", cwd: "/w" });
    const first = seen.thread().entries.length;
    await session.loadSession({ sessionId: "yesterday", cwd: "/w" });
    expect(seen.thread().entries).toHaveLength(first);
  });

  test("each conversation's updates land in that conversation's thread", async () => {
    // A fork leaves the original running, and its updates keep arriving. They
    // belong in the thread they name — putting them in whichever conversation
    // happens to be on screen would show one conversation's messages in
    // another, and dropping them would let a conversation nobody is looking at
    // fall silently out of date.
    const { session, seen } = await connected();
    await session.loadSession({ sessionId: "yesterday", cwd: "/w" });
    const before = seen.thread("yesterday").entries.length;
    expect(before).toBeGreaterThan(0);

    await session.forkSession({ sessionId: "yesterday", cwd: "/w" });
    expect(seen.opened()).toContain("forked");
    expect(seen.thread("forked").entries).toHaveLength(0);
    // Untouched by the fork, and still there to go back to.
    expect(seen.thread("yesterday").entries).toHaveLength(before);

    // Both conversations are open at once, each with a thread of its own.
    expect(session.sessions).toEqual(expect.arrayContaining(["yesterday", "forked"]));
  });

  test("opens a conversation from another directory with its own cwd", async () => {
    // The bug this guards: an agent's history spans every project it has been
    // used in, and sending *this* connection's directory for one of them is
    // refused — `resource_not_found`, which reads as if the conversation were
    // missing when it is only somewhere else.
    const { session, log, on } = await connected();
    const { sessions } = await session.listSessions();
    const elsewhere = sessions.find((s) => s.sessionId === "elsewhere");

    await session.loadSession(elsewhere!);

    expect(log.loaded).toEqual(["elsewhere@/other"]);
    expect(on()).toBe("elsewhere");
  });

  test("a bare id still means the conversation this connection started", async () => {
    const { session, log } = await connected();
    await session.loadSession("yesterday");
    expect(log.loaded).toEqual(["yesterday@/w"]);
  });

  test("remembers the conversation on screen, so a reload comes back to it", async () => {
    const { session, memory: held } = await connected();
    expect(held.value()).toBe("fresh");
    await session.loadSession({ sessionId: "yesterday", cwd: "/w" });
    expect(held.value()).toBe("yesterday");
  });

  test("a reload takes back the session it remembers, not the connection's first", async () => {
    // The relay answers a repeat `session/new` with the session the connection
    // started with. After the user has opened one from the history, that is no
    // longer the conversation on screen — and only the tab knows which is.
    const { log, seen, on } = await connected(LIFECYCLE, "yesterday");
    expect(on()).toBe("yesterday");
    // From the server's fold, not from the agent: nothing was asked to replay,
    // because the conversation never stopped.
    expect(log.replayed).toEqual(["yesterday"]);
    expect(log.loaded).toEqual([]);
    expect(seen.thread().entries).toHaveLength(1);
  });

  test("a remembered session the server has lost falls back to the recorded one", async () => {
    // Deleted, or on a connection that has since been reaped. An empty page
    // with a composer pointed at a session that is gone is worse than the
    // conversation this connection actually started with.
    const { memory: held, log, on } = await connected(LIFECYCLE, "yesterday", new Set(["fresh"]));

    expect(on()).toBe("fresh");
    expect(held.value()).toBe("fresh");
    expect(log.replayed).toEqual(["yesterday", "fresh"]);
  });

  test("deleting a conversation drops its thread and says so", async () => {
    // Whoever is showing it has to hear: a composer left pointed at a session
    // the agent has forgotten sends a prompt that fails with nothing to
    // explain it. Where to go instead is the viewer's decision, not this
    // object's — it may have the conversation on screen, or not.
    const { session, seen } = await connected();
    await session.deleteSession({ sessionId: "fresh", cwd: "/w" });

    expect(seen.closed()).toEqual(["fresh"]);
    expect(session.sessions).not.toContain("fresh");
  });
});

describe("a connection carrying more than one conversation", () => {
  const text = (t: string) => ({
    sessionUpdate: "agent_message_chunk",
    content: { type: "text", text: t },
  });

  test("interleaved updates each land in their own thread", async () => {
    // The whole point. Two conversations are live on one socket, and the agent
    // talks about them in whatever order it likes; neither may be dropped and
    // neither may end up in the other.
    const { session, seen, push } = await connected();
    const first = session.anchor!;
    const second = (await session.newSession())!;
    expect(second).not.toBe(first);

    await push.update(first, text("a1"));
    await push.update(second, text("b1"));
    await push.update(first, text("a2"));

    const said = (sessionId: string) =>
      seen
        .thread(sessionId)
        .entries.flatMap((entry) => (entry.type === "assistant" ? entry.chunks : []))
        .flatMap((chunk) => chunk.content)
        .map((block) => (block.type === "text" ? block.text : ""))
        .join("");

    expect(said(first)).toBe("a1a2");
    expect(said(second)).toBe("b1");
  });

  test("one conversation's turn ending leaves the other one working", async () => {
    // `_mjx/session/turn_ended` is how a turn started on a socket that has
    // since gone is closed out. It names its session, and ignoring that would
    // tell the viewer an agent had stopped when it is still working.
    // Both turns park, so neither ends on its own and the only thing that can
    // end one is the notification under test.
    const { session, seen, push } = await connected(LIFECYCLE, undefined, undefined, new Set([
      "fresh",
      "fresh-2",
    ]));
    const first = session.anchor!;
    const second = (await session.newSession())!;

    void session.prompt(first, [{ type: "text", text: "keep going" }]);
    void session.prompt(second, [{ type: "text", text: "you too" }]);
    await vi.waitFor(() => {
      expect(seen.thread(first).status).toBe("generating");
      expect(seen.thread(second).status).toBe("generating");
    });

    await push.turnEnded(second, "end_turn");

    expect(seen.thread(second).status).toBe("idle");
    expect(seen.thread(first).status).toBe("generating");
  });

  test("a question parked in one conversation survives another's turn ending", async () => {
    // Ending a turn gives up on the questions that turn asked, because nobody
    // is listening for the answers any more. Giving up on *every* pending
    // question would leave a form on screen in another conversation whose
    // agent is still waiting on it.
    const { session, seen, push } = await connected(LIFECYCLE, undefined, undefined, new Set([
      "fresh-2",
    ]));
    const first = session.anchor!;
    const second = (await session.newSession())!;
    expect(second).toBe("fresh-2");

    // Parks: the agent asks, and nothing answers.
    void session.prompt(second, [{ type: "text", text: "rename it" }]);
    await vi.waitFor(() => {
      expect(seen.thread(second).entries.some((e) => e.type === "elicitation")).toBe(true);
    });

    await push.turnEnded(first, "end_turn");

    const asked = seen
      .thread(second)
      .entries.find((entry) => entry.type === "elicitation");
    expect(asked?.type === "elicitation" && asked.elicitation.state).toBe("pending");

    // And it is still answerable: the resolver was not thrown away with the
    // other conversation's turn.
    const requestId =
      asked?.type === "elicitation" ? asked.elicitation.requestId : undefined;
    session.answerElicitation(requestId!, { action: "accept", content: { branch: "fix" } });
    const settled = seen.thread(second).entries.find((entry) => entry.type === "elicitation");
    expect(settled?.type === "elicitation" && settled.elicitation.state).toBe("accepted");
  });
});

describe("a prompt that carries more than text", () => {
  test("every block reaches the agent, in order", async () => {
    // The SDK validates the request against the real schema on the way past,
    // so this is the whole seam: composer to wire.
    const { log, prompt: send } = await connected();
    const prompt: acp.ContentBlock[] = [
      { type: "text", text: "fix the median bug in " },
      { type: "resource_link", uri: "file:///w/stats.js", name: "stats.js" },
      { type: "text", text: " please" },
    ];

    await send(prompt);

    expect(log.prompts).toHaveLength(1);
    expect(log.prompts[0]).toEqual(prompt);
  });

  test("the optimistic echo carries the same blocks that were sent", async () => {
    // Not the text of them: the agent echoes each block back and the fold
    // matches them one at a time, so a lossy optimistic copy shows the prompt
    // twice.
    const { seen, prompt: send } = await connected();
    const prompt: acp.ContentBlock[] = [
      { type: "text", text: "look at " },
      { type: "resource_link", uri: "file:///w/stats.js", name: "stats.js" },
    ];

    await send(prompt);

    const entries = seen.thread().entries;
    const user = entries.find((entry) => entry.type === "user");
    expect(user?.content).toEqual(prompt);
  });
});
