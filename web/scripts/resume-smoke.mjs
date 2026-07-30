#!/usr/bin/env node
/**
 * Drives a reload against a running server, the way the browser does.
 *
 * `smoke.mjs` covers one connection start to finish. This covers what only
 * exists across two: an agent that outlives its socket, a handshake answered
 * from what the agent said the first time, the questions a departed browser
 * never answered, and the thread the server hands back.
 *
 * Needs a server with a mock agent, so it is a manual check rather than part of
 * `npm test`:
 *
 *   node web/scripts/resume-smoke.mjs [url]
 *
 * Exits non-zero with a description of what was missing.
 */

import { WebSocket } from "ws";
import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

globalThis.WebSocket ??= WebSocket;
const base = process.argv[2] ?? "ws://127.0.0.1:4321";

function open(query, { answerPermission = false, answerElicitation = false } = {}) {
  const info = {};
  const notes = [];
  const app = acp
    .client({ name: "resume-smoke" })
    .onNotification(acp.methods.client.session.update, () => {})
    .onRequest(acp.methods.client.session.requestPermission, () => {
      notes.push("permission-asked");
      if (answerPermission) {
        return { outcome: { outcome: "selected", optionId: "allow_once" } };
      }
      return new Promise(() => {}); // never answer: park the turn
    })
    .onRequest(acp.methods.client.elicitation.create, (ctx) => {
      notes.push(`elicited:${ctx.params.mode}`);
      if (answerElicitation) return { action: "accept", content: { branch: "fix/median" } };
      return new Promise(() => {}); // park on the open form
    })
    .onNotification(acp.methods.client.elicitation.complete, () => {});
  const noop = (v) => v;
  app.onNotification("_mjx/agent/info", noop, (ctx) => Object.assign(info, ctx.params));
  app.onNotification("_mjx/session/turn_ended", noop, (ctx) =>
    notes.push(`turn_ended:${ctx.params.stopReason}`),
  );
  app.onNotification("_mjx/connection/taken_over", noop, () => notes.push("taken-over"));
  for (const m of [
    "_mjx/agent/stderr",
    "_mjx/terminal/created",
    "_mjx/terminal/output",
    "_mjx/terminal/exit",
    "_mjx/fs/wrote",
    "_mjx/inspector/frame",
  ]) {
    app.onNotification(m, noop, () => {});
  }
  const connection = app.connect(createWebSocketStream(`${base}/ws?${query}`));
  return { connection, info, notes };
}

async function handshake(c) {
  await c.connection.agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    clientCapabilities: { elicitation: { form: {}, url: {} } },
    clientInfo: { name: "resume-smoke", version: "0" },
  });
  return c.connection.agent.request(acp.methods.agent.session.new, {
    cwd: c.info.cwd,
    mcpServers: [],
  });
}

async function until(what, predicate, ms = 20_000) {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > deadline) {
      console.error(`FAIL: timed out waiting for ${what}`);
      process.exit(1);
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  console.log(`ok   ${what}`);
}

const check = (ok, what) => {
  if (!ok) {
    console.error(`FAIL: ${what}`);
    process.exit(1);
  }
  console.log(`ok   ${what}`);
};

// First tab: start a turn and abandon it while the agent waits on permission.
const first = open("agent=mock");
const s1 = await handshake(first);
check(first.info.resumed === false, "a fresh connection is not resumed");
check(Boolean(first.info.connectionId), "a connection id was issued");

// Pick a model before prompting, then confirm the *server* wrote it down. Only
// its copy survives the reload below, so if it did not record this, nothing did.
const modelOf = (options) => options?.find((o) => o.id === "model")?.currentValue;
await first.connection.agent.request(acp.methods.agent.session.setConfigOption, {
  sessionId: s1.sessionId,
  configId: "model",
  value: "mock-haiku",
});
const afterSet = await first.connection.agent.request("_mjx/session/replay", {
  sessionId: s1.sessionId,
});
check(modelOf(afterSet.configOptions) === "mock-haiku", "the server recorded the chosen model");

first.connection.agent
  .request(acp.methods.agent.session.prompt, {
    sessionId: s1.sessionId,
    prompt: [{ type: "text", text: "fix the median bug" }],
  })
  .catch(() => {});
await until("the agent asked the first tab for permission", () =>
  first.notes.includes("permission-asked"),
);
first.connection.close();

// Second tab: the reload. This one answers everything, so the turn should finish.
const second = open(`agent=mock&resume=${first.info.connectionId}`, {
  answerPermission: true,
  answerElicitation: true,
});
const s2 = await handshake(second);
check(second.info.resumed === true, "the reload rejoined the running agent");
check(s2.sessionId === s1.sessionId, `the same session came back (${s2.sessionId})`);

const replayed = await second.connection.agent.request("_mjx/session/replay", {
  sessionId: s2.sessionId,
});
check(replayed?.status === "generating", "the turn is still running");
check(replayed.entries?.length > 1, `the conversation survived (${replayed.entries?.length} entries)`);
check(replayed.entries[0].type === "user", "the prompt is the first entry");
check(
  replayed.entries.some((e) => e.type === "toolCall"),
  "tool calls survived",
);

// The point of folding config options server-side, stated as plainly as it can
// be: the reload's own `session/new` is answered from the recording made when
// the session started, so it still says `mock-sonnet`. Only the replay knows
// the agent has since moved to `mock-opus`. A client-side-only implementation
// passes every other check here and shows the wrong model.
check(
  modelOf(s2.configOptions) === "mock-sonnet",
  "the replayed handshake is the original one, as it should be",
);
check(
  modelOf(replayed.configOptions) === "mock-opus",
  `the reload sees the model actually in effect (got ${modelOf(replayed.configOptions)})`,
);

await until("the unanswered question was re-asked", () => second.notes.includes("permission-asked"));
await until("the inherited turn ran to completion", () =>
  second.notes.some((n) => n === "turn_ended:end_turn"),
);

const finished = await second.connection.agent.request("_mjx/session/replay", {
  sessionId: s2.sessionId,
});
check(finished.status === "idle", "the thread is idle again");
check(finished.stopReason === "end_turn", "with the reason the turn ended for");

// A reload with a *form* open, which is the other half of this. A permission
// prompt is re-asked and nothing else; an elicitation is re-asked *and* carried
// in the thread, and the two have to land as one question rather than two.
// Permission is answered so the turn gets as far as the form, then parks there.
const formTab = open("agent=mock", { answerPermission: true });
const s3 = await handshake(formTab);
formTab.connection.agent
  .request(acp.methods.agent.session.prompt, {
    sessionId: s3.sessionId,
    prompt: [{ type: "text", text: "fix the median bug" }],
  })
  .catch(() => {});
await until("the agent put a form to the first tab", () =>
  formTab.notes.includes("elicited:form"),
);
formTab.connection.close();

const formReload = open(`agent=mock&resume=${formTab.info.connectionId}`, {
  answerPermission: true,
  answerElicitation: true,
});
await handshake(formReload);
const withForm = await formReload.connection.agent.request("_mjx/session/replay", {
  sessionId: s3.sessionId,
});
const carried = withForm.entries.filter((e) => e.type === "elicitation");
check(carried.length === 1, `the replay carried ${carried.length} elicitations, expected 1`);
check(carried[0].state === "pending", `the carried form is ${carried[0].state}, expected pending`);
check(carried[0].mode === "form", "the carried form kept its mode");
check(Boolean(carried[0].requestedSchema?.properties), "the carried form kept its schema");
// The request id is what pairs the carried copy with the re-asked one. Without
// it the browser would draw two forms for one question.
check(carried[0].requestId !== undefined, "the carried form carries the id to answer with");
await until("the open form was re-asked so it can be answered", () =>
  formReload.notes.includes("elicited:form"),
);
await until("answering the re-asked form let the turn finish", () =>
  formReload.notes.some((n) => n === "turn_ended:end_turn"),
);
const answered = await formReload.connection.agent.request("_mjx/session/replay", {
  sessionId: s3.sessionId,
});
const settled = answered.entries.filter((e) => e.type === "elicitation");
check(
  settled.every((e) => e.state !== "pending"),
  `no form was left pending once the turn ended (${settled.map((e) => e.state).join(",")})`,
);
check(
  settled.some((e) => e.state === "accepted" && e.content?.branch === "fix/median"),
  "what the user typed survived into the thread",
);
formReload.connection.close();

// A third tab takes it over, and the second is told why.
const third = open(`agent=mock&resume=${first.info.connectionId}`);
await handshake(third);
await until("the displaced tab was told why", () => second.notes.includes("taken-over"));

// An unknown id starts fresh rather than failing.
const stale = open("agent=mock&resume=00000000-0000-0000-0000-000000000000");
await handshake(stale);
check(stale.info.resumed === false, "a stale id starts a fresh agent instead of erroring");
check(stale.info.connectionId !== "00000000-0000-0000-0000-000000000000", "a new id replaced it");

console.log("\nall resume checks passed");
process.exit(0);
