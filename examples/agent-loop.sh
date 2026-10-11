#!/usr/bin/env bash
# Minimal agent loop: an agent (here a canned edit step - swap it for your
# LLM call) develops an application inside the v8-vm RAM workspace.
#
#   bash examples/agent-loop.sh
#
set -euo pipefail
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)
WS=${WS:-$(dirname "$OUT")/agent-ws.json}

echo "== seed workspace with a broken app =="
printf 'vm.fs.write("/app/app.js", "globalThis.sum = (xs) => xs.reduce((a, b) => a - b, 0);");' \
  | "$OUT" - --dump="$WS" >/dev/null

echo "== turn 1: run tests, observe failure =="
if "$OUT" --preload="$WS" --dump="$WS" "$DIR/agent-driver.js"; then
  echo "unexpected pass"; exit 1
fi

echo "== turn 2: agent edits the app (swap for an LLM call) =="
printf 'vm.fs.write("/app/app.js", "globalThis.sum = (xs) => xs.reduce((a, b) => a + b, 0);");' \
  | "$OUT" - --preload="$WS" --dump="$WS" >/dev/null

echo "== turn 3: rerun tests =="
"$OUT" --preload="$WS" --dump="$WS" "$DIR/agent-driver.js"
