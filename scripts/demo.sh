#!/usr/bin/env bash
# Build everything and start the viewer. Needs no credentials: the default
# `mock` agent is built from this repo.
set -euo pipefail

cd "$(dirname "$0")/.."

URL="http://localhost:4321"

# The demo agent really does edit `demo/workspace/stats.js`, so start from a
# fresh copy of the pristine sources and put the bug back.
echo "==> resetting demo/workspace"
rm -rf demo/workspace
cp -r demo/pristine demo/workspace

echo "==> building the web app"
(cd web && npm install --silent && npm run build --silent)

echo "==> building the server, the mock agent and friends"
cargo build --workspace

echo "==> starting mjx-acp-server on $URL"
echo "    default agent: mock (no credentials needed)"
echo "    workspace:     demo/workspace"
echo

# Open the browser once the port is actually accepting connections, so we don't
# race the server's startup.
(
  for _ in $(seq 1 100); do
    if (exec 3<>/dev/tcp/127.0.0.1/4321) 2>/dev/null; then
      exec 3<&- 3>&-
      for opener in xdg-open open; do
        if command -v "$opener" >/dev/null 2>&1; then
          "$opener" "$URL" >/dev/null 2>&1 || true
          break
        fi
      done
      exit 0
    fi
    sleep 0.1
  done
) &

exec cargo run --bin mjx-acp-server -- --config mjx.toml
