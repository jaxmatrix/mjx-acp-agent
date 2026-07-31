#!/usr/bin/env node
/**
 * Drives an agent that will not start until it is authenticated.
 *
 * The vitest suite covers the reader and the panel in isolation, against a
 * fake. This covers what neither can: a real `-32000` off a real socket, a real
 * PTY on the server side, and the keystrokes that finish a login travelling the
 * whole way back.
 *
 *   node web/scripts/auth-smoke.mjs [url] [agentId]
 *
 * The agent must be the mock in `--needs-auth` mode, and the server must have a
 * `kind = "terminal"` auth provider. See the block this prints if it cannot
 * find them.
 *
 * Exits non-zero with a description of what was missing.
 */

import { WebSocket } from "ws";
import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

const base = process.argv[2] ?? "ws://127.0.0.1:4321";
const agentId = process.argv[3] ?? "locked";

const failures = [];
function check(ok, what) {
  if (!ok) failures.push(what);
}

const ext = new Map();
const waiters = new Map();

function recordExt(method, params) {
  const bucket = ext.get(method) ?? [];
  bucket.push(params);
  ext.set(method, bucket);
  waiters.get(method)?.(params);
}

/** Waits for an `_mjx/*` notification matching `accept`. */
function waitForExt(method, accept = () => true, seconds = 30) {
  const already = (ext.get(method) ?? []).find(accept);
  if (already) return Promise.resolve(already);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`no ${method} within ${seconds}s`)),
      seconds * 1000,
    );
    waiters.set(method, (params) => {
      if (!accept(params)) return;
      clearTimeout(timer);
      waiters.delete(method);
      resolve(params);
    });
  });
}

const passthrough = (value) => value;
const app = acp
  .client({ name: "auth-smoke" })
  .onNotification("_mjx/agent/info", passthrough, (ctx) => recordExt("_mjx/agent/info", ctx.params))
  .onNotification("_mjx/auth/required", passthrough, (ctx) =>
    recordExt("_mjx/auth/required", ctx.params),
  )
  .onNotification("_mjx/auth/progress", passthrough, (ctx) =>
    recordExt("_mjx/auth/progress", ctx.params),
  )
  .onNotification("_mjx/terminal/created", passthrough, (ctx) =>
    recordExt("_mjx/terminal/created", ctx.params),
  )
  .onNotification("_mjx/terminal/output", passthrough, (ctx) =>
    recordExt("_mjx/terminal/output", ctx.params),
  );

const url = `${base}/ws?agent=${encodeURIComponent(agentId)}`;
const connection = await app.connect(createWebSocketStream(url, { WebSocket }));

const initialized = await connection.agent.request(acp.methods.agent.initialize, {
  protocolVersion: acp.PROTOCOL_VERSION,
  clientCapabilities: { elicitation: { form: {}, url: {} } },
  clientInfo: { name: "auth-smoke", version: "0.1.0" },
});

const methods = initialized.authMethods ?? [];
check(methods.length > 0, "the agent advertised no authMethods; is it running --needs-auth?");
check(
  methods.some((m) => m.type === "terminal"),
  "no terminal method was offered, so the server did not declare auth.terminal",
);
check(
  methods.some((m) => m.type === "env_var" && (m.vars ?? []).length > 0),
  "no env_var method named any variables",
);

// The refusal, and what replaces the raw error.
let refusal;
try {
  await connection.agent.request(acp.methods.agent.session.new, { cwd: "", mcpServers: [] });
  check(false, "session/new succeeded; this agent was supposed to refuse");
} catch (error) {
  refusal = error;
}

const data = refusal?.data ?? refusal?.error?.data ?? refusal?.cause?.data;
check(
  refusal?.code === -32000 || refusal?.error?.code === -32000,
  `the refusal was not -32000: ${JSON.stringify(refusal)}`,
);
check(Array.isArray(data?.methods) && data.methods.length > 0, "the -32000 carried no auth detail");
check(data?.refusedMethod === "session/new", "the detail did not say what was refused");

const envMethod = (data?.methods ?? []).find((m) => m.kind === "envVar");
check(envMethod?.secrets?.length > 0, "the env method named no variables in the detail");
check(envMethod?.instructions, "the env method carried no instructions to show");
check(
  !JSON.stringify(data).includes("not-a-real"),
  "a credential value reached the browser",
);

await waitForExt("_mjx/auth/required");

// The interactive login, all the way through.
const attempt = await connection.agent.request("_mjx/auth/attempt", {
  methodId: "mock-terminal-login",
});
check(attempt.terminalId, "a terminal login handed back no terminal to show");
check(
  attempt.authenticated === false,
  "the attempt claimed success before the login had run",
);

await waitForExt("_mjx/terminal/created", (p) => p.terminalId === attempt.terminalId);
await waitForExt("_mjx/terminal/output", (p) =>
  Buffer.from(p.chunk, "base64").toString("utf8").includes("Paste your code"),
);

await connection.agent.request("_mjx/terminal/resize", {
  terminalId: attempt.terminalId,
  rows: 40,
  cols: 100,
});
await connection.agent.request("_mjx/terminal/input", {
  terminalId: attempt.terminalId,
  bytes: Buffer.from("a-code\n", "utf8").toString("base64"),
});

const done = await waitForExt("_mjx/auth/progress", (p) => p.authenticated === true);
check(done.methodId === "mock-terminal-login", "the wrong method reported success");

// And the whole point: the session opens now.
const session = await connection.agent.request(acp.methods.agent.session.new, {
  cwd: "",
  mcpServers: [],
});
check(typeof session.sessionId === "string", "the session still would not open");

const state = await connection.agent.request("_mjx/auth/state", {});
check(state.authenticated === true, "the connection did not report itself authenticated");
check(
  state.methods.some((m) => m.id === "mock-terminal-login" && m.satisfied),
  "the method that worked was not marked",
);

console.log(`methods    ${methods.map((m) => m.type ?? "agent").join(", ")}`);
console.log(`refusal    -32000 with ${data?.methods?.length ?? 0} described methods`);
console.log(`terminal   ${attempt.terminalId}`);
console.log(`login      ${done.message}`);
console.log(`session    ${session.sessionId}`);

if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed:`);
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  process.exit(1);
}
console.log("\nall checks passed");
process.exit(0);
