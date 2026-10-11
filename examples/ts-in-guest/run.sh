#!/usr/bin/env bash
# Host side of the in-guest TypeScript check: embed a ts_rust.wasm build
# and a project into a RAM-fs snapshot, then run the whole compiler inside
# a v8vm guest (tsc-guest.js). The guest's exit code is tsc's own.
#
# usage: OUT=<v8vm> TSC_RUST_WASM=<ts_rust.wasm> bash run.sh <project-dir> [workspace.json]
#
# The project directory needs a tsconfig.json; tsc runs as `-p .` with /app
# as its working directory, so outDir and include are relative to it. Each
# source file is seeded through one argv entry, so files above the kernel's
# per-argument limit (128 KiB) cannot be seeded this way.
set -euo pipefail
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)
WASM=${TSC_RUST_WASM:?set TSC_RUST_WASM to a ts_rust.wasm build}
PROJ=${1:?usage: run.sh <project-dir> [workspace.json]}
WS=${2:-$(dirname "$OUT")/ts-guest-ws.json}
[ -f "$WASM" ] || { echo "run.sh: no wasm at $WASM" >&2; exit 1; }
[ -f "$PROJ/tsconfig.json" ] || { echo "run.sh: $PROJ needs a tsconfig.json" >&2; exit 1; }

# The guest cannot read host files, so the wasm enters the RAM filesystem
# through a version-2 snapshot (base64), built with coreutils only. The
# .pre file is rebuilt from scratch every run, so no emitted output is
# ever stale; the post-run dump lands in the workspace file.
PRE="$WS.pre"
{ printf '{"version":2,"dirs":["/","/tsc","/app"],"files":{"/tsc/ts_rust.wasm":{"b64":"'
  base64 -w0 "$WASM"
  printf '"}}}'
} >"$PRE"

seed() { # seed <guest-path> <content>
  printf 'vm.fs.write("%s", vm.args[0]);' "$1" \
    | "$OUT" - --preload="$PRE" --dump="$PRE" "$2" >/dev/null
}
while IFS= read -r f; do
  seed "/app/${f#"$PROJ"/}" "$(cat "$f")"
done < <(find "$PROJ" -type f | LC_ALL=C sort)

rm -f "$WS"
status=0
"$OUT" --preload="$PRE" --dump="$WS" "$DIR/tsc-guest.js" || status=$?
rm -f "$PRE"
exit "$status"
