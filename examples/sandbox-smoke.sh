#!/usr/bin/env bash
# Smoke check for --sandbox. The guest must run sandboxed. On a kernel or
# platform that cannot sandbox (Landlock ABI 6 needs Linux 6.12) v8vm refuses
# with exit 2 and a clear message instead; that is reported as a skip, unless
# V8VM_REQUIRE_SANDBOX=1, which makes it a failure.
set -u
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)

errfile=$(mktemp)
trap 'rm -f "$errfile"' EXIT
output=$("$OUT" --sandbox "$DIR/hello.js" 2>"$errfile")
status=$?
stderr=$(cat "$errfile")
if [ "$status" -eq 0 ]; then
  if printf '%s\n' "$output" | grep -qx 'hello from ram'; then
    exit 0
  fi
  echo "sandbox smoke: unexpected output: $output" >&2
  exit 1
fi
output=$stderr

case "$status:$output" in
  2:*"kernel refused the restrictions: Landlock"* | 2:*"only supported on Linux x86_64"*)
    if [ "${V8VM_REQUIRE_SANDBOX:-}" = 1 ]; then
      echo "sandbox smoke: V8VM_REQUIRE_SANDBOX=1 but --sandbox refused: $output" >&2
      exit 1
    fi
    echo "SKIPPED (kernel lacks Landlock ABI 6): --sandbox smoke check ($output)"
    ;;
  *)
    echo "sandbox smoke: exit $status: $output" >&2
    exit 1
    ;;
esac
