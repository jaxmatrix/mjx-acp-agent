/**
 * Drives {@link Session} against a real ACP agent, in process.
 *
 * The agent here is the SDK's own agent app rather than a hand-written double,
 * so every frame is parsed by the generated schemas on the way past — a request
 * this client builds wrongly fails here rather than against a real agent. What
 * it stands in for is the *server*, which for everything in this file is a wire
 * that forwards: the session lifecycle is not intercepted.
 */

import * as acp from "@agentclientprotocol/sdk";
import { describe, expect, test } from "vitest";

import { Session, type SessionEvents, type SessionMemory } from "./session";
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
}

/**
 * An ACP agent that lists, loads and forks, wired straight to `Session`.
 *
 * A pair of object streams rather than a socket: the SDK reads and writes
 * messages, not bytes, so this is the same code path with the transport taken
 * out.
 */
function connectedAgent(
  capabilities: unknown,
  resumed = false,
  threads: Set<string> = new Set(["yesterday", "fresh"]),
): { stream: acp.Stream; log: AgentLog } {
  const log: AgentLog = { methods: [], loaded: [], replayed: [] };
  const toAgent = new TransformStream<acp.AnyMessage, acp.AnyMessage>();
  const toClient = new TransformStream<acp.AnyMessage, acp.AnyMessage>();

  const app = acp
    .agent({ name: "fake-agent" })
    .onRequest(acp.methods.agent.initialize, async (ctx) => {
      log.methods.push("initialize");
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
    .onRequest(acp.methods.agent.session.new, () => {
      log.methods.push("session/new");
      return { sessionId: "fresh" };
    })
    .onRequest(acp.methods.agent.session.list, () => {
      log.methods.push("session/list");
      return {
        sessions: [
          { sessionId: "yesterday", cwd: "/w", title: "Rename the helper" },
          { sessionId: "fresh", cwd: "/w" },
        ],
      };
    })
    .onRequest(acp.methods.agent.session.load, async (ctx) => {
      log.methods.push("session/load");
      log.loaded.push(ctx.params.sessionId);
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
    .onRequest(acp.methods.agent.session.fork, () => {
      log.methods.push("session/fork");
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
  return { stream: { readable: toClient.readable, writable: toAgent.writable }, log };
}

/** Everything `Session` reports, collected. */
function watcher(): { events: SessionEvents; thread: () => Thread; replaying: () => string[] } {
  let thread = emptyThread();
  const replaying: string[] = [];
  return {
    thread: () => thread,
    replaying: () => replaying,
    events: {
      thread: (next) => (thread = next),
      agentInfo: () => {},
      capabilities: () => {},
      replaying: (sessionId) => replaying.push(sessionId ?? "—"),
      sessionChanged: () => {},
      frame: () => {},
      status: () => {},
    },
  };
}

/** A memory that starts holding `sessionId`, as a reloaded tab's would. */
function memory(sessionId?: string): SessionMemory & { value: () => string | undefined } {
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

async function connected(capabilities: unknown = LIFECYCLE, held?: string, threads?: Set<string>) {
  const seen = watcher();
  const held_ = memory(held);
  // A tab that remembers a session is a tab that reloaded, so the connection it
  // gets back is a resumed one.
  const agent = connectedAgent(capabilities, held !== undefined, threads);
  const session = new Session(emptyThread(), seen.events, held_);
  await session.connect({ agentId: "fake", cwd: "/w" }, () => agent.stream);
  return { session, seen, memory: held_, log: agent.log };
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
    expect(sessions.map((s) => s.sessionId)).toEqual(["yesterday", "fresh"]);
    expect(sessions[0]?.title).toBe("Rename the helper");
  });

  test("a load rebuilds the thread from the replay alone", async () => {
    const { session, seen } = await connected();

    // Something on screen first, so an empty thread afterwards would be an
    // accident rather than the point.
    await session.prompt("hello").catch(() => {});
    await session.loadSession("yesterday", "/w");

    // Exactly the replayed conversation: the prompt that was on screen belonged
    // to the session we left.
    expect(seen.thread().entries).toHaveLength(2);
    expect(seen.thread().entries[0]?.type).toBe("user");
    expect(session.sessionId).toBe("yesterday");
    expect(seen.replaying()).toEqual(["yesterday", "—"]);
  });

  test("a second load is not a longer conversation", async () => {
    const { session, seen } = await connected();
    await session.loadSession("yesterday", "/w");
    const first = seen.thread().entries.length;
    await session.loadSession("yesterday", "/w");
    expect(seen.thread().entries).toHaveLength(first);
  });

  test("updates for a session this tab has left are not folded in", async () => {
    // A fork leaves the original running, and its updates keep arriving. Folded
    // into the thread on screen they would be one conversation's messages
    // appearing in another.
    const { session, seen } = await connected();
    await session.loadSession("yesterday", "/w");
    const before = seen.thread().entries.length;

    await session.forkSession("yesterday", "/w");
    expect(session.sessionId).toBe("forked");
    expect(seen.thread().entries).toHaveLength(0);

    // The old session is still being talked about; none of it lands here.
    await session.loadSession("yesterday", "/w");
    expect(seen.thread().entries).toHaveLength(before);
  });

  test("remembers the conversation on screen, so a reload comes back to it", async () => {
    const { session, memory: held } = await connected();
    expect(held.value()).toBe("fresh");
    await session.loadSession("yesterday", "/w");
    expect(held.value()).toBe("yesterday");
  });

  test("a reload takes back the session it remembers, not the connection's first", async () => {
    // The relay answers a repeat `session/new` with the session the connection
    // started with. After the user has opened one from the history, that is no
    // longer the conversation on screen — and only the tab knows which is.
    const { session, log, seen } = await connected(LIFECYCLE, "yesterday");
    expect(session.sessionId).toBe("yesterday");
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
    const { session, memory: held, log } = await connected(
      LIFECYCLE,
      "yesterday",
      new Set(["fresh"]),
    );

    expect(session.sessionId).toBe("fresh");
    expect(held.value()).toBe("fresh");
    expect(log.replayed).toEqual(["yesterday", "fresh"]);
  });

  test("deleting the session on screen leaves for a new one", async () => {
    // Otherwise the composer is pointed at a session the agent has forgotten,
    // and the next prompt fails with nothing to explain it.
    const { session, memory: held } = await connected();
    await session.deleteSession("fresh", "/w");

    expect(session.sessionId).toBe("fresh"); // the fake agent hands out one id
    expect(held.value()).toBe("fresh");
  });
});
