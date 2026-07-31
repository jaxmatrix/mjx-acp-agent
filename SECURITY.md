# Security

## There is no authentication

This is deliberate: the project's goal is that `./scripts/demo.sh` works with no
setup. The consequence is that **anyone who can open a WebSocket to the server
can run arbitrary code as the user running it.** An ACP agent's whole job is to
read files, write files, and execute commands.

## What we do about it

- **Loopback only.** The server binds `127.0.0.1:4321`. It will refuse to bind a
  non-loopback address unless you pass `--i-know-this-is-unauthenticated`.
- **Filesystem jail.** `fs/read_text_file` and `fs/write_text_file` are
  canonicalised and rejected outside the workspace roots configured in
  `mjx.toml`. Symlinks are resolved before the check.
- **File enumeration is jailed too.** `GET /api/files` lists the *names* of files
  under the workspace roots, so the composer can offer them for `@`-mentions. It
  resolves the root it was asked for through the same `resolve_within` the
  filesystem jail uses, never follows a symlinked directory out of a root,
  returns at most 500 paths, and never returns a byte of a file's contents.
- **No credential brokering for agents.** The server never handles an agent's API
  keys. Real agents authenticate themselves the same way they do from a terminal
  (`claude-acp` reuses the `claude` CLI's own session, etc.).
- **An MCP credential never reaches the browser.** `[[mcp_servers]]` entries are
  merged into `session/new` by the server rather than sent by the browser, so an
  `env` or `headers` value travels server → agent only. The sidebar is told the
  *names* of the variables and headers a server carries and never their values;
  `ext::McpServerInfo` has nowhere to put one. The injected frame is also the one
  thing deliberately **not** mirrored to the inspector, which renders in the
  browser.
- **`transport = "acp"` keeps a credential from the agent too.** The server spawns
  that MCP server itself and the agent reaches it over ACP, so it is offered as a
  name and never as a command with an environment. It is not downgraded to stdio
  for an agent that cannot do MCP-over-ACP — that would hand over the thing the
  setting protects — but skipped, with the reason in the sidebar.

## What we do *not* do

- Terminals are **not** sandboxed. `terminal/create` runs the command the agent
  asked for, with the server process's privileges and environment. The
  filesystem jail does not apply to it.
- **An MCP server is a subprocess, exactly as a terminal is.** A `[[mcp_servers]]`
  entry with `transport = "acp"` is spawned by this server with its privileges and
  its environment plus whatever `env`/`env_from` adds; every other transport is
  spawned or dialled by the *agent*. Either way the filesystem jail does not apply
  to it, and configuring one is as much a decision as configuring an agent.
- **A credential in `mjx.toml` is plaintext in a file.** Prefer `env_from` and
  `headers_from`, which name an environment variable instead, so the file can be
  committed and the secret stays in the environment of whoever starts the server.
- There is no rate limiting, no session isolation between browser tabs beyond
  one subprocess per connection, and no audit log beyond the in-memory
  inspector.
- **`GET /api/files` is unauthenticated and needs no WebSocket.** Every other way
  to reach the workspace goes through a connection; this one is a plain `GET`.
  Anyone who can reach the port can learn every filename under the workspace
  roots, whether or not an agent is running. It is not filtered by `.gitignore`
  either — a `.env` in a root is listed by name, never by content. Filenames leak
  intent, and this is strictly more than the rest of `/api/*` gives away.
- **A connection id is a bearer capability.** An agent outlives the socket that
  started it so a reload can rejoin it, and the id that does the rejoining is
  the whole authorisation: anyone who has one can read that conversation,
  prompt the agent, and through it reach the workspace. Ids are random v4
  UUIDs, they are never listed by `GET /api/connections`, and an agent nobody
  comes back to is reaped after `[server] resume_ttl_secs` (five minutes by
  default; `0` disables resuming entirely). None of that is authentication —
  it is what keeps the unauthenticated design from being *worse* than it was.

## Before you expose this

Don't, without putting an authenticating reverse proxy in front of it and
running the server as a low-privilege user in a container. If you add auth, the
WebSocket upgrade is the entry point that matters (`crates/mjx-acp-server`) —
and it is where `?resume=` arrives, so whatever you put there has to cover
rejoining an existing agent as well as starting a new one.

## Reporting

This is a demo project. Open an issue.
