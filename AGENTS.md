# AGENTS.md

Vendor-neutral entry point for coding agents working in **mjx-acp-viewer**.

Fuller guidance lives in:

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — the development loop, the test tiers, required checks, and
  git/commit conventions.
- [`README.md`](README.md) — what the transport is, how to run it, and the known limitations.
- [`SECURITY.md`](SECURITY.md) — why there is no authentication and what compensates for it.

## What this project is

A **web transport for the [Agent Client Protocol](https://agentclientprotocol.com)**. ACP is only
ever spoken over a subprocess's stdio, and a browser cannot spawn a subprocess. A Rust server accepts
an ACP connection over a WebSocket, starts the requested agent, and relays between them.

```
Browser (React)  ◄── WebSocket ──►  mjx-acp-server  ◄── stdio ──►  agent subprocess
ACP *client* role                   relay + capability host        claude-acp / kilo / mock
```

## The short version

- **The relay is transparent by default.** Both hops carry the same JSON-RPC frames; the browser is
  an ordinary ACP client and the agent an ordinary ACP agent. *Forward it* is the default for
  everything — a relay that only passes what it understands breaks the day either peer speaks a
  newer protocol version.
- **Exactly five interceptions**, all in `mjx-acp-server`, and each one needs its reason written
  down here:
  1. Rewriting `initialize` to declare the capabilities the server provides.
  2. Answering `fs/*` and `terminal/*` itself, because the workspace is server-side.
  3. Answering a *repeat* `initialize` or `session/new` from what the agent said the first time,
     because both are once-per-agent and an agent now outlives the socket that started it — a
     browser that reloads asks them again, and a second `session/new` reaching the agent is
     precisely the bug that resuming exists to fix.
  4. Adding the configured MCP servers to `session/new`, `session/load`, `session/fork` and
     `session/resume`. The browser is the ACP client, so this is nominally its field to fill — but
     the servers are configured in `mjx.toml`, their paths resolve against it, and their credentials
     must not travel to a browser to be sent straight back. Merge, never replace.
  5. Answering `mcp/*` for a server configured `transport = "acp"`, because a browser cannot spawn
     an MCP server any more than it can open a PTY. **Conditional**, unlike (2): with nothing
     configured these are forwarded, so an agent that reaches for MCP-over-ACP uninvited meets a
     client that does not implement it rather than a server pretending to. That is why
     `method::is_mcp_over_acp` is separate from `is_server_provided_capability`.
- **`_mjx/*` is ours, and never reaches an agent.** It carries what ACP has no vocabulary for
  because it only arises when the client is remote. Defined once in `mjx-acp-core::ext`.
- **A connection carries several conversations, and the viewer several connections.**
  `SessionStore` was keyed by session id from the start and the browser has caught
  up: `AgentConnection` holds a thread per session and folds an update into the
  one it names, so a conversation nobody is looking at keeps running. A socket is
  still bound to one agent and one directory, so two agents side by side is two
  sockets — `useSessions` holds a map of them. Two consequences worth knowing:
  a reload sends exactly **one** `session/new` per connection and then one
  `_mjx/session/replay` per conversation, since every later `session/new` reaches
  the agent and would leave an empty session behind; and threads are keyed by
  connection *and* session, because session ids are the agent's and two agents may
  choose the same one.
- **A terminal belongs to the workspace, not to a thread.** `_mjx/terminal/*` carries
  a terminal id and no session id, because a terminal id is already unique across the
  workspace — there is nothing to route one by. Both thread models leave them out; the
  browser holds them on the connection, so a replay does not take the scrollback with
  the thread it replaces.
- **One request kind is thread state: an elicitation.** Everything else the agent
  asks the browser — a permission prompt, `fs/*`, `terminal/*` — is a request in
  flight and nothing more. A structured question and its answer are part of the
  conversation, so both thread models hold them and a replay carries them. A
  pending one is *also* re-asked over the socket, because a browser cannot
  respond to a request the connection it is holding never received; the two are
  matched by JSON-RPC id.
- **The session lifecycle is forwarded, not intercepted.** `session/list`,
  `session/load`, `session/fork`, `session/resume`, `session/delete` and
  `session/close` pass straight through; what the server adds is the *fold*. A
  load replays a whole conversation as `session/update` notifications, arriving
  during the call rather than after it, so both thread models empty that
  session's thread before the request goes out — otherwise the replay doubles
  what was already there. The UI offers only the capabilities the agent named in
  `initialize`, and the browser remembers which of an agent's sessions it has
  open, because the relay's recorded `session/new` cannot know.
- **Two thread models, held together by a fixture.** `crates/mjx-acp-thread` (Rust, server) and
  `web/src/acp/thread.ts` (TypeScript, browser) implement the same folding rules. Change one and you
  change both, then re-run `fixtures/session-updates.jsonl` through each. `MentionUri` is ported
  twice for the same reason and pinned by `fixtures/mention-uris.json` — the two will disagree on
  percent-encoding if nothing is watching.
- **Test-driven & incremental** — write the failing test first; keep every increment green.
- **Atomic commits, no `Co-Authored-By`/AI-attribution trailers.**
- **Do the work thoroughly and correctly — no monkey-patching.** Decide the design before coding.
- **Say what is true.** If a capability is not implemented, the README says so rather than the code
  pretending. See *Known limitations*.

## Layering

Dependencies point **downward only**:

```
mjx-acp-core            frames, method names, request↔method correlation, the _mjx/* vocabulary
  ├─ mjx-acp-thread     the thread model (a GPUI-free port of Zed's acp_thread)
  ├─ mjx-workspace      filesystem jail + PTY terminals
  └─ mjx-agent-catalog  ACP registry + command resolution   (depends on none of the above)
        └─ mjx-acp-server   the relay; the only crate that knows about all of them
mjx-mock-agent          depends on mjx-acp-core only; it is a peer, not a layer
```

`web/` talks to the server over the wire and shares no code with it — only the `_mjx/*` method names
and the thread-model rules, both of which are duplicated deliberately and pinned by the fixture.

## Provenance and licence

This project is **GPL-3.0-or-later** because it ports code from [Zed](https://github.com/zed-industries/zed),
which is GPL-3.0-or-later. Every ported piece names its origin in a doc comment; keep that up when
you port more. See [`NOTICE`](NOTICE).

`reference/zed-acp/` is a local-only copy of the Zed crates we port from — **git-ignored, never
staged**, the same way test material is kept out of history. `reference/README.md` records the commit
it came from.

## Commands

```sh
cargo build  --workspace
cargo test   --workspace
cargo clippy --workspace --all-targets
npm --prefix web test
npm --prefix web run typecheck

./scripts/demo.sh                 # build everything and open the viewer
node web/scripts/smoke.mjs        # drive a running server over the browser's own code path
node web/scripts/history-smoke.mjs     # the same, for session/list and session/load
node web/scripts/sessions-smoke.mjs    # two conversations on one socket, and a reload that keeps both
node web/scripts/capture-fixture.mjs   # re-record fixtures/session-updates.jsonl
```
