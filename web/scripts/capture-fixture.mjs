#!/usr/bin/env node
/**
 * Records a real turn's `session/update` stream to `fixtures/session-updates.jsonl`.
 *
 * That file is the contract between the two thread models: the Rust one on the
 * server and the TypeScript one in the browser both fold it, and their results
 * are compared. Recording rather than hand-writing it means the fixture is
 * whatever an agent actually sends, including the parts nobody thought to
 * write a case for.
 *
 *   node web/scripts/capture-fixture.mjs [url] [agentId]
 *
 * Re-run it after changing the mock agent's script.
 */

import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { WebSocket } from "ws";
import * as acp from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

const base = process.argv[2] ?? "ws://127.0.0.1:4321";
const agentId = process.argv[3] ?? "mock";
const out = join(dirname(fileURLToPath(import.meta.url)), "../../fixtures/session-updates.jsonl");

const updates = [];
let cwd = ".";

const client = acp
  .client({ name: "mjx-fixture-capture" })
  .onNotification(acp.methods.client.session.update, (ctx) => {
    updates.push(ctx.params.update);
  })
  .onRequest(acp.methods.client.session.requestPermission, (ctx) => ({
    outcome: {
      outcome: "selected",
      optionId: (ctx.params.options.find((o) => o.kind === "allow_once") ?? ctx.params.options[0])
        .optionId,
    },
  }))
  .onNotification(
    "_mjx/agent/info",
    (v) => v,
    (ctx) => {
      cwd = ctx.params.cwd;
    },
  );

const stream = createWebSocketStream(`${base}/ws?agent=${encodeURIComponent(agentId)}`, {
  WebSocket,
});

const stopReason = await client.connectWith(stream, async (agent) => {
  await agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    clientCapabilities: {},
  });
  const session = await agent.request(acp.methods.agent.session.new, {
    cwd,
    mcpServers: [],
  });
  const response = await agent.request(acp.methods.agent.session.prompt, {
    sessionId: session.sessionId,
    prompt: [{ type: "text", text: "fix the median bug in stats.js" }],
  });
  return response.stopReason;
});

// Absolute paths would make the fixture machine-specific, and the models under
// test never look at path contents.
const text = updates
  .map((update) => JSON.stringify(replacePaths(update, cwd)))
  .join("\n");
writeFileSync(out, `${text}\n`);

console.log(`wrote ${updates.length} updates to ${out} (stopReason: ${stopReason})`);

function replacePaths(value, prefix) {
  return JSON.parse(JSON.stringify(value).replaceAll(prefix, "/workspace"));
}
