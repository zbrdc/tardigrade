# Known bugs

Defects in the current version that are expected to be fixed. Permanent
limits of the design are described in [KNOWN_RISKS.md](KNOWN_RISKS.md).

## Runtime

- **Intl does not work, and several calls abort the process.** ICU data is
  not loaded. `Number.prototype.toLocaleString`, `localeCompare` and the
  `Intl` NumberFormat, Collator, PluralRules, ListFormat and
  RelativeTimeFormat throw `Internal error. Icu error.`
  `Date.prototype.toLocaleString`, `toLocaleDateString` and
  `toLocaleTimeString`, `new Intl.DateTimeFormat().format()` and
  `Intl.DateTimeFormat().resolvedOptions()` end the whole process with a V8
  fatal error (exit 133), and iterating `new Intl.Segmenter().segment(...)`
  kills it with SIGSEGV (exit 139). Any guest can therefore crash the
  process. `Date.prototype.toString` prints an empty time zone name.
- **A `vm.net` flood can crash the process.** Queued messages are not
  counted against `--memory`: an endless `vm.net.send` loop grows the host
  queue until the process dies with SIGSEGV, even under a small cap.
- **Blocked stdout defeats `--timeout`.** The watchdog cannot interrupt a
  write that the operating system blocks. A guest that prints in a loop
  while nothing reads its stdout runs past `--timeout` for as long as the
  write stays blocked.
- **`vm.net` is slow in real time.** The event loop sleeps up to 50 ms
  between turns even when messages are queued, so each message hop costs up
  to 50 ms: 20 request-and-reply round trips take about 2 seconds.
- **An entry that is not valid UTF-8 is mishandled.** From a file it is
  reported as "cannot open" (exit 2); from stdin tardigrade panics (exit 101).
- **Guest arguments cannot start with `-`.** tardigrade parses its own options
  anywhere on the command line, also after the entry, and `--` does not end
  option parsing. See [OPTIONS.md](OPTIONS.md#description).
- **The host time zone leaks into runs.** Local-time `Date` methods use the
  host's `TZ`, also under `--clock=virtual`, so a guest can read the host's
  time zone, and local-time output differs between hosts with different
  time zones. Set `TZ=UTC` in the environment of tardigrade for runs that must
  match across hosts.

## Console mode (--serve)

- **Guest output shares stdout with replies.** `console.log` and
  `console.info` output appears on stdout between the JSON replies.
- **No per-command time limit unless `--command-timeout` is given.** A
  command that never finishes blocks the session forever, because
  `--timeout` defaults to 0 in this mode; `--command-timeout` bounds each
  command, and work a command leaves pending (an uncleared repeating
  timer) keeps blocking later commands' settling with or without it. See
  [OPTIONS.md](OPTIONS.md#--serve).

## HTTP bridge (serve_bridge)

`serve_bridge` is a demonstration and is not hardened for exposure beyond
localhost:

- request size is not limited: the bridge allocates whatever
  `Content-Length` claims before reading the body (a huge value can abort
  it), an unparsable value is read as 0, and header lines are unbounded;
- sockets have no read or write timeouts, and connections are handled one
  at a time, so one silent client stalls every other client;
- anything the guest writes to stdout (for example `console.log` in
  `handler`) is taken as the reply to the current request, and every later
  response is one request late. Use `console.error` while debugging;
- if the guest process exits or crashes, the request in flight is answered
  with an empty 200 response, and the bridge panics on the next request
  instead of answering 502 and restarting the guest;
- the status code and content type returned by the guest are written into
  the response unchecked: a content type containing CR or LF can inject
  headers, and every status other than 200, 500 and 504 is sent with the reason
  phrase `OK`.
