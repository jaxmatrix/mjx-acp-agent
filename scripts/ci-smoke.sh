#!/usr/bin/env bash
#
# Every smoke check, against a server this script starts and stops.
#
# This is what CI runs, and running it is how you reproduce a CI failure. It
# assumes `cargo build --workspace` and `npm --prefix web run build` have
# already happened; it does not build, so a red run is never the build's fault.
#
#   scripts/ci-smoke.sh
#
# Logs and failure artefacts land in .ci-smoke/, which CI uploads on failure.

set -euo pipefail
cd "$(dirname "$0")/.."

LOG_DIR="${MJX_SMOKE_LOG_DIR:-.ci-smoke}"
CACHE_DIR=".mjx-cache-ci"
SERVER="target/debug/mjx-acp-server"
PID=""
URL=""
WS=""

fail() {
  echo "ci-smoke: $*" >&2
  exit 1
}

# Reset the workspace the agent works in. The mock agent really edits stats.js
# and really runs `node --test` against it, so on an already-patched copy the
# turn takes its *failure* path — a degenerate diff and a non-zero exit — and
# every assertion downstream describes a turn nobody meant to test.
reset_workspace() {
  rm -rf demo/workspace
  cp -r demo/pristine demo/workspace
}

# The registry cache, seeded from the checked-in snapshot. mjx.ci.toml points
# the fetch at a dead port, so this is the only thing the catalog can read.
seed_registry() {
  rm -rf "$CACHE_DIR"
  mkdir -p "$CACHE_DIR"
  cp fixtures/registry.json "$CACHE_DIR/registry.json"
}

stop_server() {
  [ -n "$PID" ] || return 0
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  PID=""
}

# mjx.ci.toml asks for port 0, so the address is only knowable from the line the
# server logs once it has bound — the same handshake
# crates/mjx-acp-server/tests/relay.rs uses. Polling the log beats sleeping: it
# is faster when the server is quick and it does not lie when the server is slow.
start_server() {
  local tag="$1" log="$LOG_DIR/$1.log"
  : >"$log"

  MJX_LOG=mjx_acp_server=info MJX_MOCK_SPEED=0 \
    "$SERVER" --config mjx.ci.toml --web-dir web/dist >"$log" 2>&1 &
  PID=$!

  URL=""
  for _ in $(seq 1 300); do
    URL=$(sed -n 's|.*listening on \(http://[0-9.]*:[0-9]*\).*|\1|p' "$log" | head -1)
    [ -n "$URL" ] && break
    kill -0 "$PID" 2>/dev/null || fail "the server exited before it bound a port (see $log)"
    sleep 0.1
  done
  [ -n "$URL" ] || fail "the server never reported a bound address (see $log)"

  # Printing the address and answering on it are different claims.
  curl -fsS "$URL/api/agents" >/dev/null || fail "the server bound $URL but does not answer there"

  WS="${URL/http:/ws:}"
  echo "── $tag: $URL"
}

cleanup() {
  local status=$?
  stop_server
  # Agents outlive their socket by design (resume_ttl_secs), which would leave
  # the job holding open processes it has stopped watching.
  pkill -f 'target/debug/mjx-mock-agent' 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    for log in "$LOG_DIR"/*.log; do
      [ -e "$log" ] || continue
      echo
      echo "───── $log (last 200 lines)"
      tail -n 200 "$log"
    done
  fi
}
trap cleanup EXIT

[ -x "$SERVER" ] || fail "$SERVER is missing; run \`cargo build --workspace\` first"
[ -d web/dist ] || fail "web/dist is missing; run \`npm --prefix web run build\` first"

rm -rf "$LOG_DIR"
mkdir -p "$LOG_DIR"
seed_registry

# Phase 1 — the protocol, over Node's ws. First, because if the relay is broken
# the browser failure is only a symptom and this says so in fewer words.
reset_workspace
start_server protocol
node web/scripts/smoke.mjs "$WS" mock
node web/scripts/history-smoke.mjs "$WS"
node web/scripts/resume-smoke.mjs "$WS"
stop_server

# Phase 2 — the same server, driven by a real browser. Its own server and its
# own pristine workspace: the diff and terminal assertions only hold on one.
reset_workspace
start_server browser
MJX_SMOKE_ARTIFACTS="$LOG_DIR" node web/scripts/browser-smoke.mjs "$URL" mock
stop_server

echo
echo "ci-smoke: every check passed"
