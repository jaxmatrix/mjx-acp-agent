#!/usr/bin/env node
/**
 * Drives the built viewer in a real browser.
 *
 * `smoke.mjs` and its siblings own the protocol: they speak the same ACP the
 * browser speaks, over Node's `ws`. What they cannot see is everything between
 * the DOM and the socket — and that is where every browser-specific bug this
 * project has had actually lived:
 *
 *   - Chrome refuses a WebSocket handshake whose reply omits the subprotocol
 *     the client asked for. Node's `ws` does not care, so the rejected
 *     handshake only reproduced in a browser.
 *   - `new WebSocket(url, undefined)` stringifies its second argument, so the
 *     SDK sends `Sec-WebSocket-Protocol: undefined` and the server has to echo
 *     it back verbatim.
 *   - A flex item with no minimum height collapses to nothing. The thread and
 *     the terminal both render, both pass every protocol assertion, and both
 *     are invisible.
 *
 * None of those are visible to Node or to `tsc`. So this drives the page the
 * way a person does — clicks the agent, types in the composer, answers the
 * prompts — and fails on anything the browser complains about.
 *
 *   node web/scripts/browser-smoke.mjs [url] [agentId]
 *
 * Point it at the server's own static serving, not at `vite dev`: vite proxies
 * `/ws` through its own middleware, which is the layer that hid the
 * subprotocol bug in the first place.
 *
 * Exits non-zero with a description of what was missing.
 */

import { chromium } from "playwright";

// A `ws://` argument is accepted so a caller can pass the same base URL it
// passes the other smoke scripts.
const base = (process.argv[2] ?? "http://127.0.0.1:4321").replace(/^ws/, "http");
const agentId = process.argv[3] ?? "mock";

const TIMEOUT = Number(process.env.MJX_SMOKE_TIMEOUT_MS ?? 60_000);
const HEADED = process.env.MJX_SMOKE_HEADED === "1";
const ARTIFACTS = process.env.MJX_SMOKE_ARTIFACTS;

const failures = [];
/** What the script was doing, so a timeout says where it happened. */
let phase = "startup";

function check(condition, message) {
  if (!condition) failures.push(message);
}

/** Runs a phase, labelling anything that throws inside it. */
async function step(name, body) {
  phase = name;
  return await body();
}

const browser = await chromium
  .launch({ headless: !HEADED })
  .catch((cause) => {
    console.error(`could not launch chromium: ${cause.message}`);
    console.error("run: npx --prefix web playwright install chromium");
    process.exit(1);
  });

// A real viewport, because the layout assertions below are the point.
const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await context.newPage();

// Every collector is wired before the first navigation: an error thrown while
// the bundle evaluates is exactly the kind this is here to catch.
const consoleErrors = [];
const pageErrors = [];
const badResponses = [];
const failedRequests = [];
const sockets = [];

page.on("console", (message) => {
  // React logs key warnings, `act` warnings and prop-type complaints as
  // errors. They are all real defects; none of them fails a vitest run.
  if (message.type() === "error") consoleErrors.push(message.text());
});
page.on("pageerror", (error) => pageErrors.push(error.message));
page.on("requestfailed", (request) => {
  // Navigations aborted by a reload are not defects.
  const reason = request.failure()?.errorText ?? "unknown";
  if (reason !== "net::ERR_ABORTED") failedRequests.push(`${request.url()} — ${reason}`);
});
page.on("response", (response) => {
  if (response.status() >= 400) badResponses.push(`${response.status()} ${response.url()}`);
});
page.on("websocket", (ws) => {
  const record = { url: ws.url(), errors: [], closed: false };
  ws.on("socketerror", (error) => record.errors.push(String(error)));
  ws.on("close", () => {
    record.closed = true;
  });
  sockets.push(record);
});

try {
  await step("load", async () => {
    await page.goto(base, { waitUntil: "domcontentloaded", timeout: TIMEOUT });
    // `web/dist` is git-ignored, so a job that forgot to build it gets an
    // empty shell and a pile of confusing downstream timeouts instead.
    await page
      .waitForFunction(() => (document.querySelector("#root")?.childElementCount ?? 0) > 0, null, {
        timeout: 10_000,
      })
      .catch(() => {
        throw new Error("#root never rendered; was `npm --prefix web run build` run?");
      });
  });

  await step("picker", async () => {
    const card = page.locator(`[data-testid="agent-card"][data-agent-id="${agentId}"]`);
    await card.waitFor({ state: "visible", timeout: TIMEOUT });
    // The seeded registry contributes agents that cannot start. Their being
    // offered — behind a disclosure, with a reason — is the picker working.
    const unavailable = page.getByRole("button", { name: /Show \d+ unavailable agents/ });
    check(await unavailable.isVisible(), "no unavailable agents were offered; did the registry load?");
    await card.click();
  });

  await step("connect", async () => {
    // The composer being enabled is the honest readiness signal: it is what
    // gates a user from typing, and it waits on the session, not the socket.
    await page
      .locator('[data-testid="composer-input"]:not([disabled])')
      .waitFor({ timeout: TIMEOUT });
    const state = await page.locator('[data-testid="status"]').getAttribute("data-state");
    check(state === "ready", `the status pill read "${state}", not "ready"`);
  });

  await step("layout", async () => {
    // The squashed-flex-item class of bug: every protocol assertion passes and
    // there is nothing on screen.
    const thread = await page.locator('[data-testid="thread"]').boundingBox();
    check((thread?.height ?? 0) >= 200, `the thread is ${thread?.height ?? 0}px tall`);
    check((thread?.width ?? 0) >= 300, `the thread is ${thread?.width ?? 0}px wide`);

    const composer = await page.locator('[data-testid="composer-input"]').boundingBox();
    check(
      composer != null && composer.y + composer.height <= 900,
      "the composer is pushed below the fold",
    );

    const sidebar = await page.locator(".sidebar").boundingBox();
    check((sidebar?.width ?? 0) > 0, "the sidebar has no width");

    // The shell sizes itself to the viewport; only the thread scrolls.
    const pageScrolls = await page.evaluate(
      () => document.scrollingElement.scrollHeight > window.innerHeight + 2,
    );
    check(!pageScrolls, "the page scrolls; something inside it is overflowing the shell");
  });

  await step("mention", async () => {
    const input = page.locator('[data-testid="composer-input"]');
    await input.click();
    await input.pressSequentially("fix the median bug in @stats", { delay: 20 });

    // Exercises the debounced /api/files lookup behind the @ trigger.
    const menu = page.locator("#composer-suggestions");
    await menu.waitFor({ state: "visible", timeout: TIMEOUT });
    check(
      await menu.getByText("stats.js", { exact: false }).first().isVisible(),
      "the completion menu offered no stats.js",
    );

    // The menu gets first refusal on Enter. Accepting a completion must not
    // also send the message — a rule with no Node equivalent, since it lives
    // in a keydown handler and a caret restore.
    await input.press("Enter");
    await menu.waitFor({ state: "hidden", timeout: TIMEOUT });
    check(
      (await page.locator('[data-testid="user-message"]').count()) === 0,
      "accepting a completion sent the message",
    );

    await input.press("Enter");
    const sent = page.locator('[data-testid="user-message"]');
    await sent.first().waitFor({ timeout: TIMEOUT });
    check((await sent.count()) === 1, `${await sent.count()} user messages, expected 1`);
    check(
      (await sent.first().locator(".mention").count()) > 0,
      "the sent message carried no mention chip",
    );
  });

  await step("permission", async () => {
    const allow = page.locator('[data-testid="permission-option"][data-kind="allow_once"]');
    await allow.waitFor({ timeout: TIMEOUT });
    await allow.click();
  });

  await step("elicitation:form", async () => {
    const form = page.locator('[data-testid="elicitation-form"]');
    await form.waitFor({ timeout: TIMEOUT });

    // `remote` is required and has no default, so the form must refuse to
    // submit until it is answered.
    const send = page.locator('[data-testid="elicitation-send"]');
    check(await send.isDisabled(), "the form offered to send with a required field still blank");
    check(
      (await form.locator("#elicit-branch").inputValue()) === "fix/median",
      "the string field did not take its default",
    );

    await form.locator("#elicit-remote").selectOption("upstream");
    await send.waitFor({ timeout: TIMEOUT });
    check(await send.isEnabled(), "the form still refused to send once every required field was set");
    await send.click();
  });

  await step("elicitation:url", async () => {
    // The second elicitation is url mode. Nothing here is clicked: the agent
    // ends it itself with `elicitation/complete`, and following the link would
    // open the live spec site and make this check depend on the network.
    //
    // Which also means the link is not reliably on screen to assert against —
    // under MJX_MOCK_SPEED=0 the completion can arrive before this line runs.
    // So the assertion is the one that holds either way: it arrived, and it
    // did not stay pending.
    await page
      .waitForFunction(() => document.querySelectorAll(".elicitation").length >= 2, null, {
        timeout: TIMEOUT,
      })
      .catch(async () => {
        const rendered = await page.locator(".elicitation").count();
        throw new Error(`${rendered} elicitations rendered, expected the form and the url`);
      });
  });

  await step("terminal", async () => {
    // xterm renders to the DOM, so empty rows mean the emulator was handed a
    // zero-sized parent and `fit()` threw — the failure a protocol check
    // cannot see, because the bytes arrived perfectly well.
    const rows = page.locator('[data-testid="terminal"] .xterm-rows');
    await rows.waitFor({ timeout: TIMEOUT });
    await page
      .waitForFunction(
        () => (document.querySelector('[data-testid="terminal"] .xterm-rows')?.innerText ?? "").trim()
          .length > 0,
        null,
        { timeout: TIMEOUT },
      )
      .catch(() => {
        throw new Error("the terminal rendered no text; check its parent's height");
      });
  });

  await step("turn", async () => {
    await page
      .locator('[aria-label="Working"]')
      .waitFor({ state: "detached", timeout: TIMEOUT })
      .catch(() => {
        throw new Error("the turn never ended");
      });

    const toolCalls = page.locator('[data-testid="tool-call"]');
    const total = await toolCalls.count();
    check(total >= 3, `${total} tool calls rendered, expected at least 3`);
    check(
      (await page.locator('[data-testid="tool-call"][data-status="completed"]').count()) === total,
      "a tool call finished in a state other than completed",
    );

    check(
      (await page.locator('[data-testid="assistant-message"]').count()) >= 2,
      "the agent's prose never rendered",
    );

    // Finished tool calls collapse, so the diff has to be opened — which is
    // also the only exercise the collapse toggle gets. The header toggles, so
    // only the closed ones are clicked; clicking an open one would hide the
    // thing this is about to look for.
    for (let i = 0; i < total; i += 1) {
      const header = toolCalls.nth(i).locator(".tool-call__header");
      if (!(await header.isEnabled())) continue;
      if ((await header.getAttribute("aria-expanded")) === "false") await header.click();
    }

    // A removed line carrying the file's original text proves the server read
    // the file on the browser's behalf: the browser has no filesystem to read
    // it from, so a diff built without `fs/read_text_file` has nothing on its
    // left-hand side at all.
    const diff = page.locator(".diff").first();
    await diff.waitFor({ timeout: TIMEOUT });
    const removed = await diff.locator(".diff__line--removed").allInnerTexts();
    check(removed.length > 0, "the diff removed nothing, so it had no original to diff");
    check(
      removed.some((line) => line.includes("return sorted[mid];")),
      `the buggy line from stats.js is not among the removed lines: ${JSON.stringify(removed)}`,
    );

    // Both exchanges are over: nothing is still waiting on someone who has
    // stopped looking.
    check(
      !(await page.locator(".elicitation").allInnerTexts()).some((text) =>
        text.includes("Waiting for you"),
      ),
      "an elicitation was left pending after the turn ended",
    );

    const planEntries = page.locator(".plan__entry");
    check((await planEntries.count()) === 3, "the plan did not render its three entries");
    check(
      (await page.locator(".plan__entry--completed").count()) === 3,
      "the plan finished with entries still open",
    );
  });

  await step("inspector", async () => {
    const toggle = page.locator('[data-testid="inspector-toggle"]');
    const label = await toggle.innerText();
    const frames = Number(label.match(/\((\d+)\)/)?.[1] ?? 0);
    check(frames > 0, "the inspector was told about no intercepted traffic");
    await toggle.click();
    check(
      (await page.locator(".inspector__row").count()) > 0,
      "the inspector opened with no rows",
    );
  });

  await step("reload", async () => {
    // The server keeps the agent alive across a reload, so the page must come
    // back to the conversation rather than to the picker.
    await page.reload({ waitUntil: "domcontentloaded", timeout: TIMEOUT });
    await page.locator('[data-testid="thread"]').waitFor({ timeout: TIMEOUT });
    check(
      (await page.locator('[data-testid="agent-card"]').count()) === 0,
      "a reload dropped back to the agent picker",
    );
    await page
      .locator('[data-testid="composer-input"]:not([disabled])')
      .waitFor({ timeout: TIMEOUT });
    check(
      (await page.locator('[data-testid="user-message"]').count()) >= 1,
      "the conversation did not survive the reload",
    );
  });

  console.log(`url        ${base}`);
  console.log(`agent      ${agentId}`);
  console.log(`sockets    ${sockets.length}`);
  console.log(`tool calls ${await page.locator('[data-testid="tool-call"]').count()}`);
  console.log(`console    ${consoleErrors.length} errors, ${pageErrors.length} exceptions`);
} catch (cause) {
  failures.push(`${phase}: ${cause.message}`);
  if (ARTIFACTS) {
    await page
      .screenshot({ path: `${ARTIFACTS}/browser-smoke-failure.png`, fullPage: true })
      .catch(() => {});
    await import("node:fs/promises")
      .then(async (fs) =>
        fs.writeFile(`${ARTIFACTS}/browser-smoke-failure.html`, await page.content()),
      )
      .catch(() => {});
  }
} finally {
  await browser.close();
}

// Reported whatever happened above, and deliberately not as a step: when a
// wait times out, the exception the page threw a moment earlier is usually the
// reason, and reporting only the timeout hides it.
for (const text of consoleErrors) check(false, `console error: ${text}`);
for (const text of pageErrors) check(false, `uncaught exception: ${text}`);
for (const text of badResponses) check(false, `HTTP error: ${text}`);
for (const text of failedRequests) check(false, `request failed: ${text}`);

check(sockets.length >= 1, "the page never opened a WebSocket");
for (const socket of sockets) {
  // The guard for the rejected handshake and the `Sec-WebSocket-Protocol:
  // undefined` echo: both of those show up here as a socket that opened at the
  // right url and then errored, and nowhere else.
  check(
    socket.url.includes(`/ws?agent=${agentId}`),
    `a WebSocket opened at an unexpected url: ${socket.url}`,
  );
  for (const error of socket.errors) check(false, `WebSocket error: ${error}`);
}

if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed:`);
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  process.exit(1);
}
console.log("\nall checks passed");
