#!/usr/bin/env bash
# Smoke check for --sandbox. The guest must run sandboxed. When this host
# cannot sandbox at all (not Linux x86_64, or a kernel without Landlock ABI 6,
# which needs Linux 6.12) the check is reported as a skip; V8VM_REQUIRE_SANDBOX=1
# makes that a failure. Any other refusal fails: the kernel was able to
# sandbox and did not.
set -u
OUT=${OUT:?set OUT to the v8vm binary}
DIR=$(cd "$(dirname "$0")" && pwd)

if ! "$OUT" --sandbox-check >/dev/null 2>&1; then
  if [ "${V8VM_REQUIRE_SANDBOX:-}" = 1 ]; then
    echo "sandbox smoke: V8VM_REQUIRE_SANDBOX=1 but this host cannot sandbox" >&2
    exit 1
  fi
  echo "SKIPPED (cannot sandbox here): --sandbox smoke check"
  exit 0
fi

errfile=$(mktemp)
trap 'rm -f "$errfile"' EXIT
output=$("$OUT" --sandbox "$DIR/hello.js" 2>"$errfile")
status=$?
if [ "$status" -eq 0 ] && printf '%s\n' "$output" | grep -qx 'hello from ram'; then
  exit 0
fi
echo "sandbox smoke: exit $status: $output $(cat "$errfile")" >&2
exit 1
