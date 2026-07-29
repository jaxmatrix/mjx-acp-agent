# reference/

Unmodified copies of the Zed source we port from. **Nothing here is compiled**
— `reference` is in the Cargo workspace's `exclude` list — and nothing here is
shipped. It exists so the port can be checked against the original without
needing `../zed` checked out.

`zed-acp/` is **git-ignored and local-only**, like spec material: 4 MB of
third-party source does not belong in this history. Only this README is
committed. Recreate it with:

```sh
mkdir -p reference/zed-acp
cd ../zed
cp -r crates/acp_thread crates/agent_servers crates/acp_tools crates/agent_ui \
      ../mjx-acp-viewer/reference/zed-acp/
cp crates/project/src/agent_server_store.rs crates/project/src/agent_registry_store.rs \
      ../mjx-acp-viewer/reference/zed-acp/
```

Copied from `zed-industries/zed` at commit `e4ac280d48` (2026-07-28):

| Path | Zed path |
|---|---|
| `zed-acp/acp_thread/` | `crates/acp_thread/` |
| `zed-acp/agent_servers/` | `crates/agent_servers/` |
| `zed-acp/acp_tools/` | `crates/acp_tools/` |
| `zed-acp/agent_ui/` | `crates/agent_ui/` |
| `zed-acp/agent_server_store.rs` | `crates/project/src/agent_server_store.rs` |
| `zed-acp/agent_registry_store.rs` | `crates/project/src/agent_registry_store.rs` |

All GPL-3.0-or-later, Copyright Zed Industries, Inc. See `../NOTICE`.

## The parts that matter

- `acp_thread/src/acp_thread.rs` — `handle_session_update` (~:2544) is the single
  funnel every agent→client streaming update passes through. `upsert_tool_call`
  (~:3185) and `update_tool_call` (~:3110) are the fiddly bits worth copying
  carefully.
- `agent_servers/src/acp.rs` — `AcpConnection::stdio` (~:804) is the only
  transport Zed ever constructs, and `client_capabilities_for_agent` (~:764) is
  the capability set real agents are tested against.
- `acp_tools/src/acp_tools.rs` — request-id↔method correlation (~:69), needed
  because JSON-RPC responses carry no method name.
- `agent_ui/src/conversation_view/thread_view.rs` — the UI surface inventory.
  Big, but it is the definitive list of states an ACP client must render.
