# mjx-acp-viewer

**A web transport for the [Agent Client Protocol](https://agentclientprotocol.com).**

ACP standardises how a coding agent talks to the editor driving it — streaming
messages, tool calls, diffs, terminals, permission prompts. In practice it has
exactly one transport: newline-delimited JSON-RPC over a subprocess's stdio.
[Zed](https://github.com/zed-industries/zed), the reference implementation,
constructs no other. A browser cannot spawn a subprocess, so ACP has never had a
web client.

This adds one.

```
  Browser (React + Vite + TS)              Rust server                    Agent subprocess
  ─────────────────────────────           ─────────────────────          ──────────────────
  ACP *client* role                       mjx-acp-server                 claude-acp / kilo /
  @agentclientprotocol/sdk    ◄── WS ──►  relay + client-side   ◄─stdio─► gemini / codex /
  createWebSocketStream                   capability host                mjx-mock-agent
```

One WebSocket = one ACP connection = one agent subprocess. Both hops carry the
same JSON-RPC frames, so the browser is an ordinary ACP client and the agent an
ordinary ACP agent. Neither knows the other is remote.

---

## Quick start

```bash
./scripts/demo.sh
```

Builds both halves, starts the server on <http://localhost:4321>, and opens it.
Needs no credentials and no network.

Pick **Mock Agent** and ask it anything. It scripts a full turn — a thought
block, streaming text, a file read, a plan, a diff, a permission request, a live
terminal, and a form to fill in — so every UI surface is exercised out of the
box. It also starts with one conversation already behind it, so **History** has
something to open before you have had a second one. It genuinely
reads and rewrites `demo/workspace/stats.js` and genuinely runs `node --test`
against it, so the green test run at the end is real, not staged.

Then pick a real agent from the same list.

## How it works

The server is a **relay with exactly two interception points**. Everything else
passes through untouched — including frames it cannot classify, because a relay
that only forwards what it understands breaks the day either peer speaks a newer
protocol version.

**1. `initialize`, on its way to the agent.** The browser declares what it can
do, which is very little: it has no filesystem and cannot start a process. The
server merges in `fs.readTextFile`, `fs.writeTextFile` and `terminal` before
forwarding, because those live on its side. Without this the agent would never
call them at all.

**2. `fs/*` and `terminal/*`, on their way from the agent.** Never forwarded.
The server answers them against the workspace and replies directly. Files are
read and written through a jail that canonicalises paths first, so `..` and
symlinks are resolved before the containment check. Terminals get a real PTY,
because test runners and build tools change their output when stdout is not a
TTY, and the point is to show what a terminal would show.

That leaves the UI with a blind spot — it never sees the traffic the server
answered — so every outcome is mirrored back over an `_mjx/*` extension
notification. ACP routes any `_`-prefixed method to its extension mechanism, so
these coexist with the protocol rather than colliding with it.

| Method | Direction | Carries |
|---|---|---|
| `_mjx/agent/info` | → browser | which agent started, its command line and cwd, and the id to resume it with |
| `_mjx/agent/stderr` | → browser | the agent's diagnostics, so a crash is visible |
| `_mjx/terminal/created` | → browser | a terminal exists, before any of its output |
| `_mjx/terminal/output` | → browser | incremental PTY bytes, base64 |
| `_mjx/terminal/exit` | → browser | exit code or signal |
| `_mjx/fs/wrote` | → browser | a file changed, with before and after |
| `_mjx/inspector/frame` | → browser | a frame the browser never saw, for the inspector |
| `_mjx/session/replay` | → server | give me the thread state you folded |
| `_mjx/session/turn_ended` | → browser | a turn started on an earlier socket has finished |
| `_mjx/connection/taken_over` | → browser | another tab attached; this socket is closing |

`_mjx/*` is between the browser and this server only. An agent never receives
one.

### Reloading, and agents that outlive the socket

Closing a tab is not quitting an editor, so the agent does not die with the
socket. It keeps running, along with the thread the server folded and any
terminals it started, and the browser is given an id on `_mjx/agent/info` to
come back with as `?resume=`. On the way back the server answers `initialize`
and `session/new` from what the agent said the first time — so the reload gets
the session it already had rather than a second one beside it — and the browser
replaces its thread from `_mjx/session/replay`.

Three consequences worth knowing:

- **A turn that was running keeps running.** A question the agent asked and the
  departed browser never answered is put to the browser that replaces it, which
  is what stops a reload during a permission prompt parking the agent forever.
- **A second tab takes over rather than being refused.** On a reload the new
  socket can arrive before the old one's close has been processed, so refusing
  would make an ordinary refresh fail. The displaced tab is told, and offers to
  take it back.
- **An open form comes back twice, and lands once.** A pending
  `elicitation/create` is both carried in the replayed thread and re-asked over
  the new socket. Neither alone works: the thread is what makes the question and
  its answer part of the conversation, and the re-ask is what makes it
  answerable, since a browser cannot respond to a request the connection it is
  holding never received. The browser matches them by JSON-RPC id.
- **Anything the handshake announced is announced as of when it started.**
  Session modes and config options — the model selector among them — arrive on
  the `session/new` response, and after a reload that response is a recording.
  So the server folds them into the thread as well, and the sidebar reads the
  model actually in effect from the replay rather than from the handshake.

An agent nobody comes back to is reaped after `[server] resume_ttl_secs`, five
minutes by default; `0` turns the whole thing off. `GET /api/connections` shows
what is currently pooled — without the ids, since a connection id is the
capability to talk to a running agent and nothing authenticates that endpoint.

### Session history

An agent that keeps its conversations can offer them back. `claude-acp` and
`kilo` advertise `loadSession` and `sessionCapabilities: { list, delete, fork,
resume, close }`; most of the registry advertises none of it. So the history
drawer is built from what the connected agent said in `initialize` and nothing
else — every button is one capability, and an agent that lists but cannot fork
gets no Fork.

The relay forwards all six untouched. There is no fourth interception here: what
the server adds is the *fold*, because it keeps a thread per session and a
`session/load` replays a whole conversation back as `session/update`
notifications. Those arrive **during** the call, before its response, so both
sides empty that session's thread before the request goes out. Otherwise the
replay lands on top of what was already there — every message twice, and three
times after the next load.

Two smaller consequences:

- **The tab remembers which conversation it is looking at.** A reload still has
  its `session/new` answered from the recording made when the connection
  started, which is what makes resuming transparent to an ordinary ACP client —
  but that is no longer the session on screen once one has been opened from the
  history, and the relay has no way to know. A remembered session the server has
  no thread for falls back to the recorded one.
- **`session/update` is filtered by session id in the browser.** A fork leaves
  the original running, so more than one conversation can be live on a
  connection, and folding all of them into the thread on screen would put one
  conversation's messages in another.

A session belongs to the directory it was started in, and an agent's history
spans every project it has been used in — so a load, resume or fork carries
*that session's* `cwd`, not this connection's. Sending the wrong one is refused
with `-32002`, which reads as if the conversation were missing when it is only
somewhere else. Opening one from another directory works; the drawer marks it,
because the workspace the server reads and writes through is still this
connection's, so files outside it will be refused.

`session/delete` removes a conversation; `session/close` only frees what it is
holding and leaves it listed. `session/resume` picks one back up *without*
replaying it, so the thread comes from the server's fold rather than the agent.

### Two thread models, pinned together

Both sides fold the `session/update` stream into a renderable thread: the server
in `crates/mjx-acp-thread` (a GPUI-free port of Zed's `acp_thread`) and the
browser in `web/src/acp/thread.ts`. The rules are subtle — when two streamed
chunks are the same message, when a tool call is new rather than an update,
which fields a partial update may overwrite — so both are folded through the
same recorded turn, `fixtures/session-updates.jsonl`, and asserted against the
same numbers. Two implementations of the same rules are only worth having if
something notices when they disagree.

Two entry kinds cannot be covered that way, because they arrive as *requests*
rather than as updates and so can never appear in a recorded notification
stream. For elicitations the Rust model writes
`fixtures/session-elicitations.json` instead and the browser reads it back
through its replay adapter, which pins the serialization the same way.

## Agents

Anything in the [ACP registry](https://agentclientprotocol.com/registry) that
ships via `npx` or `uvx` is offered automatically — 39 agents at time of
writing. The picker shows the command it will run and explains what it can't
start rather than hiding it.

| Agent | How it starts | Setup |
|---|---|---|
| `mock` | the binary in this repo | none — this is the demo agent |
| `claude-acp` | `npx @agentclientprotocol/claude-agent-acp` | none, if the `claude` CLI is already signed in |
| `kilo` | `kilo acp` | whatever `kilo auth` needs |
| `gemini`, `codex-acp`, +30 more | `npx …` / `uvx …` from the registry | their own credentials |

Agents published only as a binary download are listed but not installed for you:
fetch one and point an `[[agents]]` entry at it.

## Configuration

Everything in `mjx.toml` has a working default; the file is optional and a
partial file is fine. Paths resolve relative to the file, not the working
directory.

```toml
[server]
bind = "127.0.0.1:4321"      # loopback by design — see SECURITY.md

[workspace]
roots = ["demo/workspace"]   # the fs jail, and the cwd choices in the picker

[registry]
url = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json"
cache_dir = ".mjx-cache"     # the catalog still works offline from this

[[agents]]                   # shadows the registry entry with the same id
id = "kilo"
name = "Kilo"
command = "kilo"
args = ["acp"]
```

Configured agents lead the picker, in the order you wrote them.

## Development

```bash
cargo test   --workspace          # 183 tests
cargo clippy --workspace --all-targets
npm --prefix web test             # 60 tests
npm --prefix web run typecheck

npm --prefix web run dev          # hot reload against a `cargo run` server
```

Three test tiers, described in [CONTRIBUTING.md](CONTRIBUTING.md):

- **Unit** — frame parsing, request↔method correlation, the thread fold, the
  line diff, the filesystem jail.
- **Fixture parity** — both thread models folded through the same recorded turn.
- **End-to-end** — `crates/mjx-acp-server/tests/relay.rs` starts the real server
  on a real port, connects a real WebSocket, and drives the real mock agent
  through a real PTY.

Against a running server, over the browser's own code path:

```bash
node web/scripts/smoke.mjs              # drive a full turn and check every surface
node web/scripts/capture-fixture.mjs    # re-record fixtures/session-updates.jsonl
```

The browser is not optional as a test surface. Node's `ws` is lenient where a
browser is strict, and real bugs have only ever shown up in Chromium.

## Layout

| Path | What |
|---|---|
| `crates/mjx-acp-core` | JSON-RPC frames, ACP v1 method names, request↔method correlation, the `_mjx/*` vocabulary |
| `crates/mjx-acp-thread` | The thread model — a GPUI-free port of Zed's `acp_thread` |
| `crates/mjx-agent-catalog` | ACP registry fetch and agent command resolution |
| `crates/mjx-workspace` | Filesystem jail and PTY terminal manager |
| `crates/mjx-acp-server` | The relay: static assets, `/api/*`, `/ws`, and the pool of running agents |
| `crates/mjx-mock-agent` | Scripted credential-free agent, for the demo and the tests |
| `web/` | The browser client; `web/src/acp/` is protocol-only and React-free |
| `demo/pristine/` | The demo's source project, copied to the ignored `demo/workspace/` |
| `fixtures/` | The recorded turn, the elicitation shapes, and a registry snapshot |
| `reference/` | Where the local-only Zed copy goes. Git-ignored. |

## Known limitations

- **A reload keeps the conversation, but not the terminal scrollback.** The
  agent outlives the socket, so refreshing rejoins the same agent and the same
  session, and a turn that was running carries on. What a terminal printed
  before the reload is gone, though: that lives in the workspace rather than in
  the thread the server folds.
- **A dropped connection is not reconnected for you.** A new socket can rejoin
  a running agent, but nothing retries automatically after a network blip —
  reload the page.
- **Terminals are display-only.** ACP gives a client no way to type into a
  terminal the agent started, so neither does this.
- **Binary-only registry agents are not installed for you.** Roughly fifteen
  publish no `npx`/`uvx` distribution; they are listed with an explanation.
- **A form's half-filled answer does not survive a reload.** The question and
  what was finally answered do — they are part of the thread — but text typed
  into a form and not yet sent is browser-local and goes with the page.
- **Session history is only as good as the agent's.** Everything in the drawer
  comes from `session/list`; an agent that does not keep conversations has none,
  and the drawer is not offered at all. `additionalDirectories` on a load or a
  fork is not sent, and pagination is a "load more" rather than infinite scroll.
- **No MCP passthrough and no `@`-mentions** yet.
- **No authentication.** See below.

## Security

There is **no authentication**, deliberately, so the demo works with no setup.
The consequence is blunt: anyone who can reach the port can read your files and
run commands as you. The server binds `127.0.0.1` only and refuses a non-loopback
address without an explicit flag; the filesystem is jailed to the configured
roots. Terminals are **not** sandboxed. Read [SECURITY.md](SECURITY.md) before
exposing it to anything.

## License

**GPL-3.0-or-later.** This project ports code from
[Zed](https://github.com/zed-industries/zed), which is GPL-3.0-or-later, so the
copyleft carries over. Every ported piece names its origin in a doc comment; see
[NOTICE](NOTICE) for the full attribution.

The Agent Client Protocol SDKs it depends on are Apache-2.0.
