#!/usr/bin/env bash
# Smoke check for --sandbox. The guest must run sandboxed. When this host
# cannot sandbox at all (not Linux x86_64, or a kernel without Landlock ABI 6,
# which needs Linux 6.12) the check is reported as a skip; TARDIGRADE_REQUIRE_SANDBOX=1
# makes that a failure. Any other refusal fails: the kernel was able to
# sandbox and did not.
set -u
OUT=${OUT:?set OUT to the tardigrade binary}
DIR=$(cd "$(dirname "$0")" && pwd)

# --sandbox-check exits 0 (can sandbox) or 2 (cannot). Any other status is a
# broken binary or a crash, which must not pass as a skip.
"$OUT" --sandbox-check >/dev/null 2>&1
check=$?
if [ "$check" -ne 0 ]; then
  if [ "$check" -ne 2 ]; then
    echo "sandbox smoke: --sandbox-check exited $check, expected 0 or 2" >&2
    exit 1
  fi
  if [ "${TARDIGRADE_REQUIRE_SANDBOX:-}" = 1 ]; then
    echo "sandbox smoke: TARDIGRADE_REQUIRE_SANDBOX=1 but this host cannot sandbox" >&2
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
