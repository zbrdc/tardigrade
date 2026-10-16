# Known bugs

Defects in the current version that are expected to be fixed. Permanent
limits of the design are described in [KNOWN_RISKS.md](KNOWN_RISKS.md).

## Runtime

- **Intl does not work.** ICU data is not loaded, so the prelude stubs
  `Intl` and all `toLocale*`/`localeCompare` methods to throw a catchable
  error; real locale formatting is unavailable until ICU data ships.
  `Date.prototype.toString` prints an empty time zone name.
- **Blocked stdout defeats `--timeout`.** The watchdog cannot interrupt a
  write that the operating system blocks. A guest that prints in a loop
  while nothing reads its stdout runs past `--timeout` for as long as the
  write stays blocked.

## Console mode (--serve)

- **Library serve sessions have no per-command limit by default.** The
  CLI defaults `--serve` to a 30 s `--command-timeout`, but a library
  caller who leaves `command_timeout_ms` at its default of 0 gets an
  unbounded session, and work a command leaves pending (an uncleared
  repeating timer) keeps blocking later commands' settling either way.
  See [OPTIONS.md](OPTIONS.md#--serve).

## HTTP bridge (serve_bridge)

`serve_bridge` is a demonstration bound to localhost; do not expose it
beyond that without a real HTTP server in front. Remaining limits:

- connections are handled one at a time (each socket now has a 15 s
  read/write timeout, so a silent client stalls the bridge for at most
  that long rather than forever);
- guest `console.log` output goes to the bridge's stderr (serve mode
  diverts it there), so it cannot be mistaken for a reply; handlers that
  print for debugging should still expect that noise on stderr.

Fixed since the first version: request bodies are capped at 1 MiB before
allocation (413 past it), header lines are capped, an unparsable
`Content-Length` is a 400, guest status codes must be 100–599 and content
types cannot contain CR/LF or other controls, and a dead guest is
answered 502 and restarted instead of panicking the bridge.
