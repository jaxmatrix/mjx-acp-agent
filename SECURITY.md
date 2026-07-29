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
- **No credential brokering.** The server never handles API keys. Real agents
  authenticate themselves the same way they do from a terminal (`claude-acp`
  reuses the `claude` CLI's own session, etc.).

## What we do *not* do

- Terminals are **not** sandboxed. `terminal/create` runs the command the agent
  asked for, with the server process's privileges and environment. The
  filesystem jail does not apply to it.
- There is no rate limiting, no session isolation between browser tabs beyond
  one subprocess per socket, and no audit log beyond the in-memory inspector.

## Before you expose this

Don't, without putting an authenticating reverse proxy in front of it and
running the server as a low-privilege user in a container. If you add auth,
the WebSocket upgrade is the only entry point that matters
(`crates/mjx-acp-server`).

## Reporting

This is a demo project. Open an issue.
