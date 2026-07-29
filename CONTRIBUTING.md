# Contributing to mjx-acp-viewer

This project is built **deliberately, test-first, and incrementally**. It grows in small,
always-green steps — never a large untested drop, never a shortcut we intend to "fix later".

## The development loop (TDD)

Every change follows **red → green → refactor**:

1. **Red** — write a failing test first. Prefer a *protocol* test: fold a recorded update stream, or
   drive a real agent through a real socket.
2. **Green** — write the minimum code to make it pass.
3. **Refactor** — clean it up with the tests still green.

Before writing code for a non-trivial piece, decide the design first — allocations, failure modes,
and what happens when a peer sends something we have never seen. We prefer the correct design over
the merely-working one.

## Test tiers

1. **Unit** — the pure logic: frame parsing, request↔method correlation, the thread fold, the line
   diff, the filesystem jail. Fast, no I/O, no sockets.
2. **Fixture parity** — `fixtures/session-updates.jsonl` is a *recorded* turn from a live run. Both
   thread models fold it and assert the same numbers
   (`crates/mjx-acp-thread/tests/fixture.rs` and `web/src/acp/fixture.test.ts`). This is what stops
   the Rust and TypeScript implementations drifting apart silently. **If you change the folding
   rules, the same assertion must move on both sides.**
3. **End-to-end** — `crates/mjx-acp-server/tests/relay.rs` starts the real server on a real port,
   connects a real WebSocket, and drives the real mock agent through a real PTY. It is the only
   thing that proves the premise: that ACP survives being carried over a WebSocket without either
   peer being adapted.

Tests must be **hermetic**. The end-to-end tests write their own workspace into a temp dir and seed
the registry cache from `fixtures/registry.json` with the fetch pointed at a dead port — they never
touch the network and never read a file the demo might have edited.

The browser is not optional as a test surface. Node's `ws` is lenient where a browser is strict, and
real bugs have only shown up in Chromium — a rejected handshake, a subprotocol echo, a squashed flex
item. `node web/scripts/smoke.mjs` drives a running server over the browser's exact code path.

## Adding support for a new part of the protocol

1. Add the shape to the mock agent's script (`crates/mjx-mock-agent/src/script.rs`) and assert it
   deserializes as the real schema type — the mock writes wire JSON by hand precisely so the test
   checks our understanding of the protocol, not a serializer against itself.
2. Re-record the fixture: `node web/scripts/capture-fixture.mjs`.
3. Fold it in **both** thread models and update both fixture tests.
4. Render it. An update the server understands and the UI drops is not done.

## Required checks (must be green before every commit)

```sh
cargo fmt --all
cargo build  --workspace
cargo test   --workspace
cargo clippy --workspace --all-targets
npm --prefix web run typecheck
npm --prefix web test
```

## Git & commit conventions

- **Atomic commits** — one self-contained change per commit, so history is easy to roll back and
  cherry-pick. Split unrelated changes.
- **Commit only when green** — a test is committed with or before the code it covers.
- **No `Co-Authored-By` or AI-attribution trailers.** Keep messages plain: imperative subject, body
  explaining *why*. Conventional-commit prefixes are encouraged: `feat(server): …`, `fix(core): …`,
  `chore: …`, `docs: …`, `test: …`, `refactor: …`.
- **Branching:** project-setup commits go directly on `main`. Once feature development begins, create
  a **feature branch** and consolidate via a **pull request**; `main` stays the integration branch.
- **Never stage `reference/`** (it is git-ignored, local-only material). Test inputs belong in
  `fixtures/`, and the demo's source project in `demo/pristine/`.
- **Never commit `demo/workspace/`** — it is generated from `demo/pristine/` by `scripts/demo.sh`,
  and the demo agent really edits it.

## Code style

### Rust

- No `unwrap`/`expect`/`panic` on anything that arrived over a socket or off a disk — those are
  untrusted. Return typed `thiserror` errors; `anyhow` is for the binary and tests.
- A frame we cannot classify is **forwarded**, not dropped. An update we do not model is **ignored**,
  not rejected. The protocol grows.
- Errors an agent will read must distinguish *refused* from *absent*: a `-32002` for a file it can
  see makes it retry forever. See `WorkspaceError::code`.
- Comments explain *why*, especially where the obvious implementation is wrong — the ordering of the
  `initialize` handshake, the subprotocol echo, and the terminal announcement all look arbitrary
  until you read the reason.

### TypeScript

- The reducer in `web/src/acp/thread.ts` is **pure** and never mutates its input: React re-renders on
  identity, so an in-place mutation shows stale output until something unrelated redraws.
- Anything arriving from the server is untrusted input, not a typed value you may assume.
- Keep `web/src/acp/` free of React. The components consume a `Thread`; they do not speak protocol.

### Ported code

When porting from Zed, name the origin in a doc comment with the file and, where it helps, the line
(`reference/zed-acp/acp_thread/src/acp_thread.rs:2544`). Say what you deliberately left behind and
why — the port is only trustworthy if the omissions are visible.
