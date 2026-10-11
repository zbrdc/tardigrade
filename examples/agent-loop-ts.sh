#!/usr/bin/env bash
# TypeScript agent loop: the agent-loop.sh harness with a TypeScript app
# (/app/*.ts in the RAM workspace). One turn:
#   1. extract the /app TypeScript sources from the workspace,
#   2. type-check them - type errors end the turn as FAIL, exactly like a
#      failing test, and are the agent's signal to fix the sources,
#   3. bundle the entry /app/app.ts with `bun build --target=browser
#      --format=iife` (tardigrade has no ES modules, so a multi-file project
#      must become one IIFE),
#   4. load the bundle back into the workspace as /app/app.js,
#   5. run the driver, which reports PASS/FAIL.
#
# bun and tsc-rs are host-side development tools of this loop, not tardigrade
# runtime dependencies: the guest still runs plain JavaScript, and the
# tardigrade crate itself gains nothing.
#
#   bash examples/agent-loop-ts.sh
#
set -euo pipefail
OUT=${OUT:?set OUT to the tardigrade binary}
# BASH_SOURCE, not $0: the cli tests source this file to drive build_app,
# and then $0 is the shell, not this script.
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WS=${WS:-$(dirname "$OUT")/agent-ts-ws.json}

# bun is looked up in $BUN, then PATH, then its default install location;
# command -v yields an absolute path so the -x probe below is meaningful.
BUN=$(command -v "${BUN:-bun}" 2>/dev/null || true)
[ -n "$BUN" ] || BUN=$HOME/.bun/bin/bun

# The type checker: tsc-rs (pinned by examples/ts-pin.sh, fetched by npx)
# with tsc as fallback. Both exit 0 when the sources type-check and 2 on
# type errors; any other status means the checker itself did not run.
# TARDIGRADE_TSC_RS / TARDIGRADE_TSC override the two commands (each may
# be several words); TARDIGRADE_TSC_RS_PKG overrides just the pin.
source "$DIR/ts-pin.sh"
checker=(npx -y "$TSC_RS_PKG")
if [ -n "${TARDIGRADE_TSC_RS:-}" ]; then read -ra checker <<<"$TARDIGRADE_TSC_RS"; fi
fallback=(tsc)
if [ -n "${TARDIGRADE_TSC:-}" ]; then read -ra fallback <<<"$TARDIGRADE_TSC"; fi

# A missing or unusable tool (bun, or every checker) is a loud skip, never
# a silent pass; TARDIGRADE_REQUIRE_TS=1 turns it into a failure. The checker
# probe is bounded and fetches tsc-rs, so "npx exists but offline with no
# cache and no tsc" is caught here, not as a confusing exit 2 later.
missing_tool() { # missing_tool <reason>
  if [ "${TARDIGRADE_REQUIRE_TS:-}" = 1 ]; then
    echo "agent-loop-ts: TARDIGRADE_REQUIRE_TS=1 but $1" >&2
    exit 1
  fi
  echo "SKIPPED ($1): TypeScript agent loop" >&2
  exit 77
}
[ -x "$BUN" ] || missing_tool "bun is not installed (set BUN, or install it)"
( cd / && timeout 60 "${checker[@]}" --version ) >/dev/null 2>&1 \
  || ( cd / && timeout 60 "${fallback[@]}" --version ) >/dev/null 2>&1 \
  || missing_tool "no usable type checker (npx cannot fetch tsc-rs, and tsc is absent)"

# One scratch directory for the whole loop, removed on every exit path.
WORK=$(mktemp -d "${TMPDIR:-/tmp}/tardigrade-agent-ts.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

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
  local dir="$WORK/turn" rc=0
  rm -rf "$dir"
  mkdir -p "$dir"
  # The guard matters inside `if build_app`: set -e is off there, so an
  # unguarded rejected extraction would continue on a partial tree and
  # report misleading TS2307 cannot-find-module errors instead of the
  # rejection. A rejected file FAILs the turn with the message only.
  extract "$dir" || return 1
  if [ ! -f "$dir/app.ts" ]; then
    echo "workspace has no /app/app.ts to build" >&2
    exit 1
  fi
  # vm.d.ts types the guest API (vm, console, timers, TextEncoder, ...) so
  # the app type-checks against tardigrade, not against a DOM or Node lib: the
  # tsconfig keeps lib.dom out with "lib": ["esnext"]. The rest of the
  # options pin modern syntax (Set for-of, #private, BigInt) for both
  # tsc-rs and tsc instead of either one's ES5 defaults, and "types": []
  # keeps random @types off the guest.
  cp "$DIR/vm.d.ts" "$dir/vm.d.ts"
  cat >"$dir/tsconfig.json" <<'EOF'
{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["esnext"], "target": "es2022", "module": "esnext", "moduleResolution": "bundler", "types": [] }, "include": ["**/*.ts"] }
EOF
  typecheck "$dir" || rc=$?
  if [ "$rc" -eq 1 ]; then
    echo "no TypeScript checker available (need npx for tsc-rs, or tsc)" >&2
    exit 1
  fi
  if [ "$rc" -ne 0 ]; then
    echo "FAIL: TypeScript type errors (see checker output above)"
    return 1
  fi
  "$BUN" build "$dir/app.ts" --target=browser --format=iife \
    --outfile="$dir/app.js" >/dev/null
  # The bundle travels as base64 on stdin, not as an argv entry: the
  # kernel caps one argument near 128 KiB and bundles are larger.
  base64 -w0 "$dir/app.js" \
    | "$OUT" "$DIR/ts-load-bundle.js" --preload="$WS" --dump="$WS" >/dev/null
}

app_ts=$(cat <<'EOF'
import { sum } from "./lib/sum";
import { Tally } from "./lib/stats";
// vm comes from vm.d.ts; using it proves the guest API is typed.
const engine: string = vm.version;
(globalThis as any).sum = sum;
(globalThis as any).engine = engine;
(globalThis as any).total = new Tally().add([1, 2, 3]);
EOF
)
stats_ts=$(cat <<'EOF'
// Modern syntax on purpose: Set for-of, a #private field and bigint
// would fail under a checker's ES5 defaults, pinning the generated
// tsconfig to target es2022.
export class Tally {
  #total: bigint = 0n;
  add(xs: Iterable<number>): bigint {
    for (const x of new Set(xs)) this.#total += BigInt(x);
    return this.#total;
  }
}
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

main() {
  # Start from a clean workspace: a stale file from an earlier run must not
  # leak into this one.
  rm -f "$WS"

  echo "== seed workspace with an app that does not type-check =="
  ws_write /app/app.ts "$app_ts"
  ws_write /app/lib/stats.ts "$stats_ts"
  ws_write /app/lib/sum.ts "$sum_type_error"

  echo "== turn 1: type-check, observe the FAIL turn =="
  if build_app; then
    echo "unexpected: the seeded app must not type-check"; exit 1
  fi

  echo "== turn 2: agent edits the app (swap for an LLM call) =="
  ws_write /app/lib/sum.ts "$sum_types_ok_logic_wrong"
  build_app
  if "$OUT" --preload="$WS" --dump="$WS" "$DIR/agent-driver.js"; then
    echo "unexpected pass"; exit 1
  fi

  echo "== turn 3: agent fixes the logic, rerun =="
  ws_write /app/lib/sum.ts "$sum_fixed"
  build_app
  "$OUT" --preload="$WS" --dump="$WS" "$DIR/agent-driver.js"
}

 # Run as a script; sourcing loads the helpers (the cli tests drive
 # build_app this way) without running the demo narrative.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main
fi
