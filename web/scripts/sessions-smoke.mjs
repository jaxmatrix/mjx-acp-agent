#!/usr/bin/env node
/**
 * Drives several conversations over one socket, the way the viewer now does.
 *
 * The vitest suite covers the browser's fold against an in-process agent; this
 * covers what it cannot: a real server, a real agent subprocess, and the relay
 * in between. Two conversations run at once on one connection, and then a
 * reload takes both of them back.
 *
 *   node web/scripts/sessions-smoke.mjs [url] [agentId]
 *
 * Exits non-zero with a description of what was missing.
 */

import { WebSocket } from "ws";
import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

const base = process.argv[2] ?? "ws://127.0.0.1:4321";
const agentId = process.argv[3] ?? "mock";

const failures = [];
/** `description` says what went *wrong*, so it reads as the failure it becomes. */
function check(condition, description) {
  if (!condition) failures.push(description);
}

/**
 * Opens a socket and returns the pieces a caller needs.
 *
 * `updates` keeps every `session/update` with the session it named, which is
 * the whole point: attribution is what multiplexing costs, and what it has to
 * get right.
 */
function connect(query) {
  const updates = [];
  const ext = [];
  const app = acp
    .client({ name: "mjx-acp-viewer-sessions-smoke" })
    .onNotification(acp.methods.client.session.update, (ctx) => {
      updates.push({ sessionId: ctx.params.sessionId, update: ctx.params.update });
    })
    .onRequest(acp.methods.client.session.requestPermission, (ctx) => {
      const allow =
        ctx.params.options.find((o) => o.kind === "allow_once") ?? ctx.params.options[0];
      return { outcome: { outcome: "selected", optionId: allow.optionId } };
    })
    .onRequest(acp.methods.client.elicitation.create, () => ({
      action: "accept",
      content: { branch: "fix/median", remote: "upstream" },
    }));

  for (const method of ["_mjx/agent/info", "_mjx/session/turn_ended"]) {
    app.onNotification(
      method,
      (v) => v,
      (ctx) => ext.push({ method, params: ctx.params }),
    );
  }

  const stream = createWebSocketStream(`${base}/ws?${query}`, { WebSocket });
  const connection = app.connect(stream);
  return { connection, agent: connection.agent, updates, ext };
}

async function handshake(agent) {
  await agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    clientCapabilities: {
      session: { configOptions: { boolean: {} } },
      elicitation: { form: {}, url: {} },
    },
    clientInfo: { name: "mjx-acp-viewer-sessions-smoke", version: "0.1.0" },
  });
}

function waitFor(predicate, what, timeoutMs = 20_000) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      if (predicate()) return resolve();
      if (Date.now() - started > timeoutMs) return reject(new Error(`timed out waiting for ${what}`));
      setTimeout(tick, 25);
    };
    tick();
  });
}

const said = (updates, sessionId) =>
  updates
    .filter((one) => one.sessionId === sessionId)
    .flatMap((one) => (one.update.content?.type === "text" ? [one.update.content.text] : []))
    .join("");

// ─── Two conversations, one socket ──────────────────────────────────────────

const first = connect(`agent=${encodeURIComponent(agentId)}`);
await handshake(first.agent);

const info = await waitFor(
  () => first.ext.some((one) => one.method === "_mjx/agent/info"),
  "_mjx/agent/info",
).then(() => first.ext.find((one) => one.method === "_mjx/agent/info").params);

const cwd = info.cwd;
const a = (await first.agent.request(acp.methods.agent.session.new, { cwd, mcpServers: [] }))
  .sessionId;
const b = (await first.agent.request(acp.methods.agent.session.new, { cwd, mcpServers: [] }))
  .sessionId;

check(a !== b, "a second session/new was answered with the first session");

// Both at once. Not awaited in turn: the point is that they overlap.
// The agent is a scripted mock and says the same thing whatever it is asked,
// so the prompts carry the markers: what tells the two conversations apart is
// what the *user* said, which is the part the server folds per session.
const MARKER_A = "marker-alpha";
const MARKER_B = "marker-beta";
const turnA = first.agent.request(acp.methods.agent.session.prompt, {
  sessionId: a,
  prompt: [{ type: "text", text: `${MARKER_A} fix the median bug` }],
});
const turnB = first.agent.request(acp.methods.agent.session.prompt, {
  sessionId: b,
  prompt: [{ type: "text", text: `${MARKER_B} rename the helper` }],
});

const [endedA, endedB] = await Promise.all([turnA, turnB]);
check(endedA.stopReason === "end_turn", `session A ended with ${endedA.stopReason}`);
check(endedB.stopReason === "end_turn", `session B ended with ${endedB.stopReason}`);

const forA = first.updates.filter((one) => one.sessionId === a);
const forB = first.updates.filter((one) => one.sessionId === b);
check(forA.length > 0, "session A was told nothing");
check(forB.length > 0, "session B was told nothing");
check(
  first.updates.every((one) => one.sessionId === a || one.sessionId === b),
  "an update named a session nobody opened",
);
check(said(forA, a).length > 0, "session A's updates carried no text");
check(said(forB, b).length > 0, "session B's updates carried no text");

// The threads the server folded, which is what a reload is given back.
const threadA = await first.agent.request("_mjx/session/replay", { sessionId: a });
const threadB = await first.agent.request("_mjx/session/replay", { sessionId: b });
check(threadA !== null && threadB !== null, "the server folded no thread for one of them");
check(
  JSON.stringify(threadA) !== JSON.stringify(threadB),
  "both conversations folded to the same thread",
);
check(JSON.stringify(threadA).includes(MARKER_A), "session A's prompt is not in its own thread");
check(JSON.stringify(threadB).includes(MARKER_B), "session B's prompt is not in its own thread");
check(
  !JSON.stringify(threadB).includes(MARKER_A),
  "session A's prompt turned up in session B's thread",
);
check(
  !JSON.stringify(threadA).includes(MARKER_B),
  "session B's prompt turned up in session A's thread",
);

first.connection.close();

// ─── A reload takes both of them back ───────────────────────────────────────

const second = connect(
  `agent=${encodeURIComponent(agentId)}&resume=${encodeURIComponent(info.connectionId)}`,
);
await handshake(second.agent);
const rejoined = await waitFor(
  () => second.ext.some((one) => one.method === "_mjx/agent/info"),
  "_mjx/agent/info on the reload",
).then(() => second.ext.find((one) => one.method === "_mjx/agent/info").params);

check(rejoined.resumed === true, "the reload started a new agent instead of rejoining");

// Exactly one `session/new`, however many conversations are coming back. The
// relay answers the first of an attachment from its recording and lets every
// later one reach the agent, so one per conversation would leave a real, empty
// session behind for each of them on every reload.
const anchor = (
  await second.agent.request(acp.methods.agent.session.new, { cwd, mcpServers: [] })
).sessionId;
check(anchor === a, `the recording answered with ${anchor}, not the connection's first session`);

const backA = await second.agent.request("_mjx/session/replay", { sessionId: a });
const backB = await second.agent.request("_mjx/session/replay", { sessionId: b });
check(backA !== null, "session A did not come back");
check(backB !== null, "session B did not come back");
check(
  JSON.stringify(backA) === JSON.stringify(threadA),
  "session A came back as a different conversation",
);
check(
  JSON.stringify(backB) === JSON.stringify(threadB),
  "session B came back as a different conversation",
);

// And the agent really does still have both, not just the server's fold.
const listed = await second.agent.request(acp.methods.agent.session.list, {});
const ids = (listed.sessions ?? []).map((one) => one.sessionId);
check(ids.includes(a) && ids.includes(b), `the agent lists ${ids.join(", ")}`);

second.connection.close();

console.log(`agent      ${info.name} (${info.agentId})`);
console.log(`sessions   ${a}, ${b}`);
console.log(`updates    ${forA.length} for A, ${forB.length} for B`);

if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed:`);
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  process.exit(1);
}
console.log("\nall session checks passed");
