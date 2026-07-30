#!/usr/bin/env node
/**
 * Drives the session history against a running server, the way the browser does.
 *
 * The Rust tests cover the same ground from the other side of the socket. This
 * covers it from *this* side: the SDK client the app really uses, over a real
 * WebSocket, through the relay that forwards the lifecycle untouched.
 *
 * Needs a server with a mock agent, so it is a manual check rather than part of
 * `npm test`:
 *
 *   node web/scripts/history-smoke.mjs [url]
 *
 * Exits non-zero with a description of what was missing.
 */

import { WebSocket } from "ws";
import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

globalThis.WebSocket ??= WebSocket;
const base = process.argv[2] ?? "ws://127.0.0.1:4321";

const check = (ok, what) => {
  if (!ok) {
    console.error(`FAIL: ${what}`);
    process.exit(1);
  }
  console.log(`ok   ${what}`);
};

/** One tab, with the updates it saw recorded per session. */
function open(query) {
  const info = {};
  const updates = [];
  const app = acp
    .client({ name: "history-smoke" })
    .onNotification(acp.methods.client.session.update, (ctx) =>
      updates.push(ctx.params.sessionId),
    )
    .onRequest(acp.methods.client.session.requestPermission, () => ({
      outcome: { outcome: "selected", optionId: "allow_once" },
    }))
    .onRequest(acp.methods.client.elicitation.create, () => ({ action: "decline" }))
    .onNotification(acp.methods.client.elicitation.complete, () => {});
  const noop = (v) => v;
  app.onNotification("_mjx/agent/info", noop, (ctx) => Object.assign(info, ctx.params));
  for (const m of [
    "_mjx/agent/stderr",
    "_mjx/terminal/created",
    "_mjx/terminal/output",
    "_mjx/terminal/exit",
    "_mjx/fs/wrote",
    "_mjx/inspector/frame",
    "_mjx/session/turn_ended",
    "_mjx/connection/taken_over",
  ]) {
    app.onNotification(m, noop, () => {});
  }
  const connection = app.connect(createWebSocketStream(`${base}/ws?${query}`));
  return { connection, info, updates };
}

async function handshake(tab) {
  const initialized = await tab.connection.agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    clientCapabilities: { elicitation: { form: {}, url: {} } },
    clientInfo: { name: "history-smoke", version: "0" },
  });
  const session = await tab.connection.agent.request(acp.methods.agent.session.new, {
    cwd: tab.info.cwd,
    mcpServers: [],
  });
  return { initialized, session };
}

const tab = open("agent=mock");
const { initialized, session } = await handshake(tab);
const agent = tab.connection.agent;

// What the UI reads to decide which buttons exist at all.
const caps = initialized.agentCapabilities ?? {};
check(caps.loadSession === true, "the agent advertises session/load");
for (const offered of ["list", "delete", "fork", "resume", "close"]) {
  check(
    typeof caps.sessionCapabilities?.[offered] === "object" &&
      caps.sessionCapabilities[offered] !== null,
    `the agent advertises session/${offered}`,
  );
}

const listed = await agent.request(acp.methods.agent.session.list, { cwd: tab.info.cwd });
check(Array.isArray(listed.sessions), "session/list came back with a list");
const past = listed.sessions.find((s) => s.sessionId !== session.sessionId);
check(Boolean(past), "a conversation from before this connection is listed");
check(Boolean(past.title), `the listed conversation has a title (${past.title})`);
check(Boolean(past.updatedAt), "and a timestamp to sort it by");

// The replay arrives as ordinary `session/update` notifications, during the
// call rather than after it.
const before = tab.updates.length;
await agent.request(acp.methods.agent.session.load, {
  sessionId: past.sessionId,
  cwd: tab.info.cwd,
  mcpServers: [],
});
const replayed = tab.updates.slice(before);
check(replayed.length > 0, `the load replayed ${replayed.length} updates`);
check(
  replayed.every((id) => id === past.sessionId),
  "every replayed update named the session that was loaded",
);

const thread = await agent.request("_mjx/session/replay", { sessionId: past.sessionId });
check(thread?.entries?.length > 0, `the server folded the replay (${thread.entries.length} entries)`);
const entries = thread.entries.length;

// Twice, because folding a replay onto an existing thread is the failure this
// design exists to avoid, and it is invisible until the second time.
await agent.request(acp.methods.agent.session.load, {
  sessionId: past.sessionId,
  cwd: tab.info.cwd,
  mcpServers: [],
});
const again = await agent.request("_mjx/session/replay", { sessionId: past.sessionId });
check(again.entries.length === entries, `a second load left ${again.entries.length} entries, not ${entries * 2}`);

// A session from another project: it belongs to the directory it was started
// in, and the request has to say so. Sending this connection's directory is
// refused, which is what a client gets wrong.
// Unfiltered, because the drawer is: an agent's history spans every project it
// has been used in, and hiding the rest would hide the case this guards.
const everywhere = await agent.request(acp.methods.agent.session.list, {});
const other = everywhere.sessions.find((s) => s.cwd !== tab.info.cwd);
check(Boolean(other), "the agent's history spans more than this directory");
let refused;
try {
  await agent.request(acp.methods.agent.session.load, {
    sessionId: other.sessionId,
    cwd: tab.info.cwd,
    mcpServers: [],
  });
} catch (error) {
  refused = error;
}
check(refused !== undefined, "loading it with the wrong directory is refused");
check(
  String(refused?.code ?? refused).includes("-32002"),
  `refused as not-found (${refused?.code ?? refused})`,
);

const beforeOther = tab.updates.length;
await agent.request(acp.methods.agent.session.load, {
  sessionId: other.sessionId,
  cwd: other.cwd,
  mcpServers: [],
});
check(tab.updates.length > beforeOther, "and it loads with its own directory");

// A fork is a second conversation, and the one it came from is untouched.
const forked = await agent.request(acp.methods.agent.session.fork, {
  sessionId: past.sessionId,
  cwd: tab.info.cwd,
});
check(forked.sessionId !== past.sessionId, "the fork is a new session");
const withFork = await agent.request(acp.methods.agent.session.list, { cwd: tab.info.cwd });
check(
  withFork.sessions.some((s) => s.sessionId === forked.sessionId),
  "the fork is listed",
);
check(
  withFork.sessions.some((s) => s.sessionId === past.sessionId),
  "and the conversation it came from still is",
);

// Closing frees a session; deleting is what forgets it.
await agent.request(acp.methods.agent.session.close, { sessionId: forked.sessionId });
const afterClose = await agent.request(acp.methods.agent.session.list, { cwd: tab.info.cwd });
check(
  afterClose.sessions.some((s) => s.sessionId === forked.sessionId),
  "a closed session is still listed",
);

await agent.request(acp.methods.agent.session.delete, { sessionId: forked.sessionId });
const afterDelete = await agent.request(acp.methods.agent.session.list, { cwd: tab.info.cwd });
check(
  !afterDelete.sessions.some((s) => s.sessionId === forked.sessionId),
  "a deleted session is gone from the list",
);
const gone = await agent.request("_mjx/session/replay", { sessionId: forked.sessionId });
check(gone === null, "and the server let go of its thread");

tab.connection.close();
console.log("\nall good");
