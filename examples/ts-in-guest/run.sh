#!/usr/bin/env bash
# Host side of the in-guest TypeScript check: embed a ts_rust.wasm build
# and a project into one RAM-fs snapshot, then run the whole compiler
# inside a v8vm guest (tsc-guest.js). The guest's exit code is tsc's own.
#
# usage: OUT=<v8vm> TSC_RUST_WASM=<ts_rust.wasm> bash run.sh <project-dir> [workspace.json]
#
# The project directory needs a tsconfig.json; tsc runs as `-p .` with /app
# as its working directory, so outDir and include are relative to it. Paths
# with a newline in them are not supported (the find listing is
# newline-delimited).
set -euo pipefail
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)
WASM=${TSC_RUST_WASM:?set TSC_RUST_WASM to a ts_rust.wasm build}
PROJ=${1:?usage: run.sh <project-dir> [workspace.json]}
WS=${2:-$(dirname "$OUT")/ts-guest-ws.json}
[ -f "$WASM" ] || { echo "run.sh: no wasm at $WASM" >&2; exit 1; }
[ -f "$PROJ/tsconfig.json" ] || { echo "run.sh: $PROJ needs a tsconfig.json" >&2; exit 1; }

# The guest cannot read host files, so the wasm and every project file
# enter the RAM filesystem through one version-2 snapshot, all as base64
# entries: the base64 alphabet is JSON-safe, so content and file names
# never splice into program text. Only the path keys need JSON escaping
# (a quote or a backslash). The snapshot is rebuilt from scratch every
# run - no emitted output is ever stale - and removed on every exit path.
PRE="$WS.pre"
trap 'rm -f "$PRE"' EXIT
json_string() { # json_string <text>: escape the two characters JSON needs
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}
entries='"/tsc/ts_rust.wasm":{"b64":"'"$(base64 -w0 "$WASM")"'"}'
while IFS= read -r f; do
  key=$(json_string "/app/${f#"$PROJ"/}")
  entries="$entries,\"$key\":{\"b64\":\"$(base64 -w0 "$f")\"}"
done < <(find "$PROJ" -type f | LC_ALL=C sort)
printf '{"version":2,"dirs":["/","/tsc","/app"],"files":{%s}}' "$entries" >"$PRE"

rm -f "$WS"
status=0
"$OUT" --preload="$PRE" --dump="$WS" "$DIR/tsc-guest.js" || status=$?
exit "$status"
