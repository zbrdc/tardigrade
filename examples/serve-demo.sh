#!/usr/bin/env bash
# End-to-end proof: curl -> host bridge -> RAM VM (wasm inside).
set -euo pipefail
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)
PORT=${PORT:-8077}

BRIDGE=${BRIDGE:-$(dirname "$OUT")/serve_bridge}
"$BRIDGE" "$DIR/webapp.js" "$PORT" &
BRIDGE=$!
trap 'kill $BRIDGE 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  curl -s "http://127.0.0.1:$PORT/" >/dev/null 2>&1 && break
  sleep 0.1
done

curl -s "http://127.0.0.1:$PORT/" | grep -q "19+23=42"
curl -s -X POST --data '{"xs":[1,2,3]}' "http://127.0.0.1:$PORT/api/sum" | grep -qx "6"
echo SERVE-OK
