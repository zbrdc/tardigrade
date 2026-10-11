#!/usr/bin/env bash
# TypeScript agent loop: the agent-loop.sh harness with a TypeScript app
# (/app/*.ts in the RAM workspace). One turn:
#   1. extract the /app TypeScript sources from the workspace,
#   2. type-check them - type errors end the turn as FAIL, exactly like a
#      failing test, and are the agent's signal to fix the sources,
#   3. bundle the entry /app/app.ts with `bun build --target=browser
#      --format=iife` (v8vm has no ES modules, so a multi-file project
#      must become one IIFE),
#   4. load the bundle back into the workspace as /app/app.js,
#   5. run the driver, which reports PASS/FAIL.
#
# bun and tsc-rs are host-side development tools of this loop, not v8vm
# runtime dependencies: the guest still runs plain JavaScript, and the
# v8vm crate itself gains nothing.
#
#   bash examples/agent-loop-ts.sh
#
set -euo pipefail
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)
WS=${WS:-$(dirname "$OUT")/agent-ts-ws.json}

# bun is looked up in $BUN, then PATH, then its default install location.
BUN=${BUN:-bun}
command -v "$BUN" >/dev/null 2>&1 || BUN=$HOME/.bun/bin/bun

# The type checker: tsc-rs (pinned, fetched by npx) with tsc as fallback.
# Both exit 0 when the sources type-check and 2 on type errors; any other
# status means the checker itself did not run. V8VM_TSC_RS / V8VM_TSC
# override the two commands (each may be several words).
checker=(npx -y tsc-rs@0.3.0)
if [ -n "${V8VM_TSC_RS:-}" ]; then read -ra checker <<<"$V8VM_TSC_RS"; fi
fallback=(tsc)
if [ -n "${V8VM_TSC:-}" ]; then read -ra fallback <<<"$V8VM_TSC"; fi

ws_write() { # ws_write <path> <content>: store a file in the workspace
  local preload=()
  if [ -f "$WS" ]; then preload=(--preload="$WS"); fi
  printf 'vm.fs.write("%s", vm.args[0]);' "$1" \
    | "$OUT" - "${preload[@]}" --dump="$WS" "$2" >/dev/null
}

# The workspace is RAM-only, so the sources leave it through a tiny guest
# run (examples/ts-listing.js): one line per file, path and content each
# base64-encoded, so file content can never masquerade as framing. The
# host parser (examples/ts-extract.sh) validates every path before
# writing and rejects anything unexpected loudly - a FAIL turn.
extract() { # extract <dir>: write the workspace's /app sources into <dir>
  local dir=$1 rc=0
  "$OUT" --preload="$WS" "$DIR/ts-listing.js" \
    | bash "$DIR/ts-extract.sh" "$dir" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "FAIL: extraction rejected a workspace file (see message above)"
    return 1
  fi
}

# Checks the extracted sources (a strict tsconfig.json is generated there).
# Prints which checker decided. Returns 0 clean, 2 type errors, 1 when no
# checker could run at all.
typecheck() { # typecheck <dir>
  local dir=$1 out status who=${checker[*]}
  out=$(cd "$dir" && "${checker[@]}" -p . 2>&1) && status=0 || status=$?
  if [ "$status" -ne 0 ] && [ "$status" -ne 2 ]; then
    echo "checker: ${checker[*]} did not run (exit $status); trying ${fallback[*]}"
    if [ -n "$out" ]; then printf '%s\n' "$out"; fi
    who=${fallback[*]}
    out=$(cd "$dir" && "${fallback[@]}" -p . 2>&1) && status=0 || status=$?
    if [ "$status" -ne 0 ] && [ "$status" -ne 2 ]; then
      echo "checker: neither ${checker[*]} nor ${fallback[*]} could run"
      if [ -n "$out" ]; then printf '%s\n' "$out"; fi
      return 1
    fi
  fi
  echo "checker: $who"
  if [ -n "$out" ]; then printf '%s\n' "$out"; fi
  return "$status"
}

# The build half of a turn: extract, type-check, bundle, load /app/app.js
# back into the workspace. Returns 1 on the FAIL turn (type errors) and
# exits the whole loop when no type checker can run.
build_app() {
  local dir=$1 rc=0
  extract "$dir"
  if [ ! -f "$dir/app.ts" ]; then
    echo "workspace has no /app/app.ts to build" >&2
    exit 1
  fi
  # vm.d.ts types the guest API (vm, console, timers, TextEncoder, ...) so
  # the app type-checks against v8vm, not against a DOM or Node lib: the
  # tsconfig keeps lib.dom out with "lib": ["esnext"].
  cp "$DIR/vm.d.ts" "$dir/vm.d.ts"
  cat >"$dir/tsconfig.json" <<'EOF'
{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["esnext"] }, "include": ["**/*.ts"] }
EOF
  typecheck "$dir" || rc=$?
  if [ "$rc" -eq 1 ]; then
    echo "no TypeScript checker available (need npx for tsc-rs, or tsc)" >&2
    exit 1
  fi
  if [ "$rc" -ne 0 ]; then
    echo "FAIL: TypeScript type errors (see checker output above)"
    rm -rf "$dir"
    return 1
  fi
  "$BUN" build "$dir/app.ts" --target=browser --format=iife \
    --outfile="$dir/app.js" >/dev/null
  ws_write /app/app.js "$(cat "$dir/app.js")"
  rm -rf "$dir"
}

app_ts=$(cat <<'EOF'
import { sum } from "./lib/sum";
// vm comes from vm.d.ts; using it proves the guest API is typed.
const engine: string = vm.version;
(globalThis as any).sum = sum;
(globalThis as any).engine = engine;
EOF
)
sum_type_error=$(cat <<'EOF'
export const sum = (xs: string[]): string => xs.reduce((a, b) => a - b, 0);
EOF
)
sum_types_ok_logic_wrong=$(cat <<'EOF'
export const sum = (xs: number[]): number => xs.reduce((a, b) => a - b, 0);
EOF
)
sum_fixed=$(cat <<'EOF'
export const sum = (xs: number[]): number => xs.reduce((a, b) => a + b, 0);
EOF
)

echo "== seed workspace with an app that does not type-check =="
ws_write /app/app.ts "$app_ts"
ws_write /app/lib/sum.ts "$sum_type_error"

echo "== turn 1: type-check, observe the FAIL turn =="
if build_app "$(mktemp -d)"; then
  echo "unexpected: the seeded app must not type-check"; exit 1
fi

echo "== turn 2: agent edits the app (swap for an LLM call) =="
ws_write /app/lib/sum.ts "$sum_types_ok_logic_wrong"
build_app "$(mktemp -d)"
if "$OUT" --preload="$WS" --dump="$WS" "$DIR/agent-driver.js"; then
  echo "unexpected pass"; exit 1
fi

echo "== turn 3: agent fixes the logic, rerun =="
ws_write /app/lib/sum.ts "$sum_fixed"
build_app "$(mktemp -d)"
"$OUT" --preload="$WS" --dump="$WS" "$DIR/agent-driver.js"
