# mjx-acp-viewer

A **web transport for the [Agent Client Protocol](https://agentclientprotocol.com)**.

ACP has exactly one transport in practice: newline-delimited JSON-RPC over a
subprocess's stdio. A browser can't spawn a subprocess, so ACP has never had a
web client. This project adds one:

```
  Browser (React + Vite + TS)              Rust server                    Agent subprocess
  ─────────────────────────────           ─────────────────────          ──────────────────
  ACP *client* role                       mjx-acp-server                 claude-acp / kilo /
  @agentclientprotocol/sdk    ◄── WS ──►  relay + client-side   ◄─stdio─► gemini / codex /
  createWebSocketStream                   capability host                mjx-mock-agent
```

One WebSocket = one ACP connection = one agent subprocess. Both hops carry the
same JSON-RPC frames, so the browser is a bog-standard ACP client and the agent
is a bog-standard ACP agent. Neither knows the other is remote.

The server intercepts exactly two things:

1. **`initialize`** on its way to the agent — merges in the client capabilities
   the browser cannot provide (`fs.readTextFile`, `fs.writeTextFile`,
   `terminal`), because the workspace lives on the server.
2. **`fs/*` and `terminal/*`** on their way from the agent — served locally
   against the workspace and answered directly, then mirrored to the browser as
   `_mjx/*` notifications so the UI can render live terminals and diffs.

Everything else passes through untouched.

## Quick start

```bash
./scripts/demo.sh
```

Builds the web app, starts the server on <http://localhost:4321>, and opens it.

Pick **Mock Agent** and ask it anything. It needs no credentials and no network:
it scripts a full turn — a thought block, streaming text, a file read, a plan, a
diff, a permission request, and a live terminal — so every UI surface is
exercised out of the box. It really does read and edit
`demo/workspace/stats.js` and really does run its tests, so the green test run
at the end is genuine. `scripts/demo.sh` puts the bug back before each run.

Then pick a real agent from the same list.

## Agents

Anything in the [ACP registry](https://agentclientprotocol.com/registry) that
ships via `npx` or `uvx` is offered automatically; the picker shows the command
it will run and greys out what it can't start. Verified working through this
transport:

| Agent | How it starts | Setup |
|---|---|---|
| `mock` | the binary in this repo | none — this is the demo agent |
| `claude-acp` | `npx @agentclientprotocol/claude-agent-acp` | none, if the `claude` CLI is already signed in |
| `kilo` | `kilo acp` | whatever `kilo auth` needs |
| `gemini`, `codex-acp`, +30 more | `npx …` from the registry | their own credentials |

Registry agents published only as a binary download are listed but not
installed for you: fetch one and add an `[[agents]]` entry pointing at it.

## Known limitations

- **A page reload starts over.** The agent subprocess lives and dies with the
  WebSocket, so refreshing gets a new agent and a new session. The server does
  keep thread state and will serve it over `_mjx/session/replay`, but surviving
  a reload needs the agent to outlive the socket — a connection-pooling change
  that isn't done.
- **Terminals are display-only.** ACP gives a client no way to type into a
  terminal the agent started, so neither does this.
- **No authentication.** See [SECURITY.md](SECURITY.md).

## Development

```bash
cargo test --workspace          # 148 tests
npm --prefix web test           # 37 tests
npm --prefix web run dev        # hot reload against a `cargo run` server

# End-to-end against a running server, over the browser's own code path:
node web/scripts/smoke.mjs
```

`fixtures/session-updates.jsonl` is a recorded turn that both thread models —
the Rust one in `crates/mjx-acp-thread` and the TypeScript one in
`web/src/acp/thread.ts` — are folded through and compared against, so the two
can't drift apart silently. Re-record it with
`node web/scripts/capture-fixture.mjs` after changing the mock agent.

## Layout

| Path | What |
|---|---|
| `crates/mjx-acp-core` | JSON-RPC frames, ACP v1 method names, request-id↔method correlation |
| `crates/mjx-acp-thread` | Thread/session model — a GPUI-free port of Zed's `acp_thread` |
| `crates/mjx-agent-catalog` | ACP registry fetch + agent command resolution |
| `crates/mjx-workspace` | Filesystem jail and PTY terminal manager |
| `crates/mjx-acp-server` | axum server: static assets, `/api/agents`, `/ws` relay |
| `crates/mjx-mock-agent` | Scripted credential-free ACP agent for the demo and tests |
| `web/` | React + Vite client |
| `reference/zed-acp/` | Unmodified Zed crates, for reference. Not compiled. |

## Security

There is **no authentication**, by design, so it works out of the box. The
server binds to `127.0.0.1` only. Anyone who can reach the port can run
arbitrary commands as you. See [SECURITY.md](SECURITY.md) before exposing it.

## License

GPL-3.0-or-later. This project ports code from [Zed](https://github.com/zed-industries/zed),
which is GPL-3.0-or-later. See [NOTICE](NOTICE) for attribution.
