#!/usr/bin/env bash
# Smoke check for the TypeScript agent loop (examples/agent-loop-ts.sh).
# bun must be installed (it bundles the app), and a type checker must be
# reachable: npx (which fetches tsc-rs) or tsc. Either missing is a loud
# skip, never a silent pass; V8VM_REQUIRE_TS=1 turns the skip into a
# failure. Both are host-side dev tools, not v8vm runtime dependencies.
set -u
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)

find_bun() {
  if [ -n "${BUN:-}" ] && command -v "$BUN" >/dev/null 2>&1; then
    printf '%s\n' "$BUN"
  elif command -v bun >/dev/null 2>&1; then
    command -v bun
  elif [ -x "$HOME/.bun/bin/bun" ]; then
    printf '%s\n' "$HOME/.bun/bin/bun"
  else
    return 1
  fi
}

skip() {
  if [ "${V8VM_REQUIRE_TS:-}" = 1 ]; then
    echo "ts smoke: V8VM_REQUIRE_TS=1 but $1" >&2
    exit 1
  fi
  echo "SKIPPED ($1): TypeScript agent-loop smoke check"
  exit 0
}

find_bun >/dev/null || skip "bun is not installed (bun build bundles the app)"
command -v npx >/dev/null 2>&1 || command -v tsc >/dev/null 2>&1 \
  || skip "no type checker: npx (for tsc-rs) and tsc are both missing"

output=$(BUN=$(find_bun) bash "$DIR/agent-loop-ts.sh" 2>&1)
status=$?
# The loop exits 0 only when every turn had its expected outcome, and the
# last driver run must have printed PASS; both together prove the cycle.
if [ "$status" -eq 0 ] && printf '%s\n' "$output" | grep -qx PASS; then
  exit 0
fi
echo "ts smoke: agent-loop-ts.sh exited $status:" >&2
printf '%s\n' "$output" >&2
exit 1
