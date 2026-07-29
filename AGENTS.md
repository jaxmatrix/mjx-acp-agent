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
- **Exactly three interceptions**, all in `mjx-acp-server`: rewriting `initialize` to declare the
  capabilities the server provides; answering `fs/*` and `terminal/*` itself because the workspace
  is server-side; and answering a *repeat* `initialize` or `session/new` from what the agent said
  the first time, because both are once-per-agent and an agent now outlives the socket that started
  it — a browser that reloads asks them again, and a second `session/new` reaching the agent is
  precisely the bug that resuming exists to fix. Adding a fourth needs a reason written down.
- **`_mjx/*` is ours, and never reaches an agent.** It carries what ACP has no vocabulary for
  because it only arises when the client is remote. Defined once in `mjx-acp-core::ext`.
- **Two thread models, held together by a fixture.** `crates/mjx-acp-thread` (Rust, server) and
  `web/src/acp/thread.ts` (TypeScript, browser) implement the same folding rules. Change one and you
  change both, then re-run `fixtures/session-updates.jsonl` through each.
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
node web/scripts/capture-fixture.mjs   # re-record fixtures/session-updates.jsonl
```
