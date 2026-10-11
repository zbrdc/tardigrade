#!/usr/bin/env bash
# Host side of source extraction for examples/agent-loop-ts.sh: reads one
# "<path b64> <content b64>" line per file (examples/ts-listing.js) on
# stdin and writes the files under <dir>. Every path is validated before
# anything is written - workspace content is untrusted agent output and
# must never choose host paths: only /app/... paths whose remainder is
# plain name segments (no . or .. segment, no odd characters) and only
# .ts/.tsx/.d.ts/.json files are extracted. A line that fails validation
# ends extraction with exit 1 naming the path, so the loop reports a FAIL
# turn instead of silently skipping it.
set -euo pipefail
dir=${1:?usage: ts-extract.sh <dir>}

reject() { # reject <path> <reason>
  echo "ts-extract: refusing '$1': $2" >&2
  exit 1
}

# read also reports the final line when it lacks a trailing newline.
while read -r p64 c64 || [ -n "${p64:-}" ]; do
  [ -n "${p64:-}" ] || continue
  path=$(printf '%s' "$p64" | base64 -d 2>/dev/null) \
    || reject "?" "undecodable path"
  case $path in
    /app/*) ;;
    *) reject "$path" "not under /app" ;;
  esac
  rel=${path#/app/}
  printf '%s' "$rel" | grep -Eq '^([A-Za-z0-9._-]+)(/[A-Za-z0-9._-]+)*$' \
    || reject "$path" "path segment has characters outside [A-Za-z0-9._-]"
  seg=$rel
  while [ -n "$seg" ]; do
    part=${seg%%/*}
    if [ "$part" = "." ] || [ "$part" = ".." ]; then
      reject "$path" "path has a '$part' segment"
    fi
    if [ "$part" = "$seg" ]; then seg=""; else seg=${seg#*/}; fi
  done
  case $rel in
    *.d.ts|*.ts|*.tsx|*.json) ;;
    *) reject "$path" "not a .ts/.tsx/.d.ts/.json file" ;;
  esac
  mkdir -p "$dir/$(dirname "$rel")"
  printf '%s' "$c64" | base64 -d >"$dir/$rel" \
    || reject "$path" "undecodable content"
done
