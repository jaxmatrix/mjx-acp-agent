#!/usr/bin/env node
/**
 * Drives a running server the way the browser does.
 *
 * This exercises the exact code path the web client uses — the SDK's
 * `createWebSocketStream` and `acp.client()` — against a real server and a real
 * agent subprocess. The vitest suite covers the reducer in isolation; this
 * covers everything the reducer can't: the handshake, the transport, and the
 * `_mjx/*` extensions.
 *
 *   node web/scripts/smoke.mjs [url] [agentId]
 *
 * Exits non-zero with a description of what was missing.
 */

import { WebSocket } from "ws";
import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

const base = process.argv[2] ?? "ws://127.0.0.1:4321";
const agentId = process.argv[3] ?? "mock";

const seen = {
  updates: [],
  ext: new Map(),
  permissionAsked: false,
};

/** Resolvers for `waitForExt`, keyed by method. */
const waiters = new Map();

function recordExt(method, params) {
  const bucket = seen.ext.get(method) ?? [];
  bucket.push(params);
  seen.ext.set(method, bucket);
  waiters.get(method)?.(params);
  waiters.delete(method);
}

/**
 * Waits for an `_mjx/*` notification.
 *
 * These are not responses, so they are not ordered against the request that
 * provoked them: `_mjx/agent/info` is sent immediately after the `initialize`
 * response and lands a moment later. The browser doesn't care — it re-renders
 * whenever state changes — but a script has to wait.
 */
function waitForExt(method, timeoutMs = 15_000) {
  const already = seen.ext.get(method);
  if (already?.length) return Promise.resolve(already[0]);

  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timed out waiting for ${method}`)),
      timeoutMs,
    );
    waiters.set(method, (params) => {
      clearTimeout(timer);
      resolve(params);
    });
  });
}

const client = acp
  .client({ name: "mjx-acp-viewer-smoke" })
  .onNotification(acp.methods.client.session.update, (ctx) => {
    seen.updates.push(ctx.params.update);
  })
  .onRequest(acp.methods.client.session.requestPermission, (ctx) => {
    seen.permissionAsked = true;
    const allow =
      ctx.params.options.find((o) => o.kind === "allow_once") ?? ctx.params.options[0];
    return { outcome: { outcome: "selected", optionId: allow.optionId } };
  });

// The `_mjx/*` namespace: everything the server does on the browser's behalf.
for (const method of [
  "_mjx/agent/info",
  "_mjx/agent/stderr",
  "_mjx/terminal/created",
  "_mjx/terminal/output",
  "_mjx/terminal/exit",
  "_mjx/fs/wrote",
  "_mjx/inspector/frame",
]) {
  client.onNotification(
    method,
    (v) => v,
    (ctx) => recordExt(method, ctx.params),
  );
}

const url = `${base}/ws?agent=${encodeURIComponent(agentId)}`;
const stream = createWebSocketStream(url, { WebSocket });

const failures = [];
function check(condition, description) {
  if (!condition) failures.push(description);
}

const result = await client.connectWith(stream, async (agent) => {
  const initialized = await agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    // Declare nothing: the server must add fs and terminal on our behalf.
    clientCapabilities: {},
  });

  // Sent right after the initialize response, never before it: a conformant
  // ACP client discards anything that precedes the handshake.
  const info = await waitForExt("_mjx/agent/info");

  const session = await agent.request(acp.methods.agent.session.new, {
    cwd: info.cwd,
    mcpServers: [],
  });

  const response = await agent.request(acp.methods.agent.session.prompt, {
    sessionId: session.sessionId,
    prompt: [{ type: "text", text: "fix the median bug in stats.js" }],
  });

  return { initialized, info, response };
});

const kinds = seen.updates.map((u) => u.sessionUpdate);
for (const required of [
  "agent_thought_chunk",
  "agent_message_chunk",
  "tool_call",
  "tool_call_update",
  "plan",
]) {
  check(kinds.includes(required), `no ${required} arrived`);
}

check(result.response.stopReason === "end_turn", `stopReason was ${result.response.stopReason}`);
check(seen.permissionAsked, "the agent never asked for permission");

// A diff with real before/after text proves the server read the file for us.
const diff = seen.updates
  .flatMap((u) => u.content ?? [])
  .find((c) => c.type === "diff");
check(diff != null, "no diff reached the client");
check(
  diff?.oldText?.includes("export function median"),
  "the diff was built from a fallback, so fs/read_text_file never worked",
);

// Terminal output proves the PTY ran and streamed.
const terminalChunks = seen.ext.get("_mjx/terminal/output") ?? [];
check(terminalChunks.length > 0, "no terminal output was streamed");
const terminalText = Buffer.concat(
  terminalChunks.map((c) => Buffer.from(c.chunk, "base64")),
).toString("utf8");
check(terminalText.length > 0, "terminal chunks decoded to nothing");

check((seen.ext.get("_mjx/fs/wrote") ?? []).length > 0, "no file write was mirrored");
check(
  (seen.ext.get("_mjx/inspector/frame") ?? []).length > 0,
  "the inspector was told nothing about intercepted traffic",
);

console.log(`agent      ${result.info.name} (${result.info.agentId})`);
console.log(`command    ${result.info.command.join(" ")}`);
console.log(`cwd        ${result.info.cwd}`);
console.log(`updates    ${seen.updates.length} (${new Set(kinds).size} kinds)`);
console.log(`terminal   ${terminalChunks.length} chunks, ${terminalText.length} bytes`);
console.log(`intercept  ${(seen.ext.get("_mjx/inspector/frame") ?? []).length} frames`);
console.log(`stopReason ${result.response.stopReason}`);

if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed:`);
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  process.exit(1);
}
console.log("\nall checks passed");
