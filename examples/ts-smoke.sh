#!/usr/bin/env bash
# Smoke check for the TypeScript agent loop (examples/agent-loop-ts.sh).
# bun must be installed (it bundles the app) and a type checker must be
# usable: the npx probe must actually fetch tsc-rs (bounded by timeout 60,
# so offline with no cache counts as missing), or tsc must run. Either
# missing is a loud skip - exit 77, the automake skip code, reason on
# stderr - never a silent pass; V8VM_REQUIRE_TS=1 makes the skip a
# failure. bun and the checker are host-side dev tools, not v8vm runtime
# dependencies.
set -u
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)
# The same command variables as the loop itself, so an override here and
# there is one setting (each may be several words).
TSC_RS=${V8VM_TSC_RS:-npx -y tsc-rs@0.3.0}
TSC=${V8VM_TSC:-tsc}

find_bun() {
  if [ -n "${BUN:-}" ] && command -v "$BUN" >/dev/null 2>&1; then
    command -v "$BUN"
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
  echo "SKIPPED ($1): TypeScript agent-loop smoke check" >&2
  exit 77
}

find_bun >/dev/null || skip "bun is not installed (bun build bundles the app)"
# A checker that is present but unusable is the same skip as a missing
# one. The probes run from / so they cannot pick up files in the repo.
# shellcheck disable=SC2086
( cd / && timeout 60 $TSC_RS --version ) >/dev/null 2>&1 \
  || ( cd / && timeout 60 $TSC --version ) >/dev/null 2>&1 \
  || skip "no usable type checker (npx cannot fetch tsc-rs, and tsc is absent)"

output=$(BUN=$(find_bun) bash "$DIR/agent-loop-ts.sh" 2>&1)
status=$?
if [ "$status" -eq 77 ]; then
  printf '%s\n' "$output" >&2
  skip "the agent loop itself skipped (message above)"
fi
# The loop exits 0 only when every turn had its expected outcome, and the
# last driver run must have printed PASS; both together prove the cycle.
if [ "$status" -eq 0 ] && printf '%s\n' "$output" | grep -qx PASS; then
  exit 0
fi
echo "ts smoke: agent-loop-ts.sh exited $status:" >&2
printf '%s\n' "$output" >&2
exit 1
