# tardigrade command-line options

## Synopsis

    tardigrade [options] <entry.js | -> [args...]
    tardigrade --serve [options] [entry.js]
    tardigrade --sandbox-check

The options, expanded:

    tardigrade [--timeout=<ms>] [--command-timeout=<ms>] [--memory=<mb>]
               [--fs-quota=<mb>] [--seed=<u64>] [--clock=virtual]
               [--epoch=<ms>] [--preload=<file>] [--dump=<file>]
               [--serve] [--sandbox] <entry.js | -> [args...]

## Description

tardigrade runs the JavaScript program `entry.js` in a fresh V8 isolate with a
RAM-only filesystem and no host APIs, then exits with the program's exit
code. With `-` as the entry, the program is read from stdin, and the guest's
`vm.stdin` is then at end of file.

Arguments after the entry are passed to the guest as `vm.args`. tardigrade
recognizes its own options anywhere on the command line, also after the
entry, and a bare `--` is dropped without ending option parsing. As a
result a guest argument cannot start with `-`: `tardigrade app.js --timeout=5`
sets the timeout, and `tardigrade app.js -x` fails with "unknown flag -x" (exit
2). A lone `-` after the entry is an ordinary guest argument. See
[KNOWN_BUGS.md](KNOWN_BUGS.md).

The guest API is described in [GUEST-API.md](GUEST-API.md), and a tutorial
is in [MANUAL.md](MANUAL.md).

## Options

### --timeout=\<ms\>

Wall-clock limit for the whole run, in milliseconds. When it expires the
guest is stopped and tardigrade exits with 124. Default 60000; 0 disables it.
Under `--serve` the default is 0 (no limit) unless `--timeout` is given.

The timeout measures real time even under `--clock=virtual`. It is a
watchdog inside the process, so it cannot stop a guest whose thread is
blocked in a write to a full stdout or stderr pipe, or one of the V8 aborts
listed in [KNOWN_RISKS.md](KNOWN_RISKS.md#process-aborts). For hostile
code, drain or redirect the output and also bound the process from outside
(for example with `timeout -s KILL`).

### --command-timeout=\<ms\>

Console mode only: wall-clock limit for one command, in milliseconds.
Default 0, which disables it (also in the library's `Config`). When it
expires the command replies `{"$error":"command timed out"}`, V8's pending
termination is cancelled, and the session continues with its state -
variables, files, timers - intact. Unlike `--timeout`, which ends the whole
run with 124, a command timeout never ends the session by itself.

The limit covers the command's whole life: its evaluation and, after the
reply, the settling of the work it left pending. A `vm.exit` always wins,
even when it lands while the timeout is being recovered; a run-level
reason (`--timeout`, the heap cap) that races it still ends the session
with its own code. See [--serve](#--serve) for what a timeout stops and
what it leaves running.

### --memory=\<mb\>

Heap cap in megabytes (MiB, 2^20 bytes), minimum 8, default 256. It caps the
JavaScript heap and, separately, ArrayBuffer memory, each at this size. A
guest whose live heap V8 cannot collect below its limit is stopped with exit
137; an ArrayBuffer allocation over the cap throws a catchable `RangeError`,
and queued `vm.net` message bytes count against it (a send that would cross
it throws the same way).
The CLI rejects values below 8 with exit 2; library callers have them raised
to 8.

The cap has exact rules and known gaps, described in
[KNOWN_RISKS.md](KNOWN_RISKS.md#memory).

### --fs-quota=\<mb\>

Quota of the RAM filesystem in megabytes (MiB). Default 64; 0 disables it. What
counts against the quota is described in
[GUEST-API.md](GUEST-API.md#vmfs).

### --seed=\<u64\>

Makes `Math.random` deterministic: runs with the same seed draw the same
sequence, different seeds draw different sequences, and concurrent runs in
one process each keep their own sequence.
The generator is splitmix64, giving 53-bit values in [0, 1). Without this
option `Math.random` is V8's own, seeded from the operating system.

### --clock=virtual

Gives the guest a virtual clock. `Date.now()`, `new Date()` and `Date()`
without arguments, and `performance.now()` read the virtual clock instead of
the host clock. The virtual clock advances only when timers fire: when the
event loop has no runnable work, it jumps straight to the next due timer
instead of sleeping, so a program that waits only on timers finishes at
once, whatever delays it schedules.

`performance.now()` starts at 0. The `Date` timeline starts at `--epoch`
(default 0). `Date` forms with explicit arguments, such as `new Date(ms)` or
`new Date(y, m, d)`, are unaffected.

`--seed` together with `--clock=virtual` makes a guest that touches nothing
outside the VM byte-for-byte reproducible.

Real time still applies in these places:

- `--timeout` measures real time. A busy loop is still stopped with 124.
- A timer that never stops (a `setInterval` that is never cleared) runs at
  full CPU until `--timeout`, because the clock jumps instead of waiting.
- While network messages are runnable the clock does not jump, so a
  self-sustaining `vm.net` ping-pong starves pending timers until
  `--timeout`. Keep `--timeout` set for such guests.
- Network messages are delivered on the next loop turn without moving the
  clock.
- A pending `vm.stdin.read()` stays pending while the clock jumps, and
  resolves when its data arrives in real time. It also keeps the run
  alive: once no timers or messages are left, the loop waits in real time
  (polling every 2 ms) until stdin delivers data or reaches end of file, or
  until `--timeout` expires.
- A self-sustaining `vm.net` ping-pong spins at full CPU, without the
  pause between turns that real time takes.
- Under `--serve` the virtual clock is one timeline for the whole session:
  it carries across commands. A command's awaited promise still jumps the
  clock to the timer it waits on - and no further. The settling after a
  reply never jumps the clock: only work already due at the reply's
  virtual time runs, and because timer deadlines are clamped to at least
  1 ms, nothing the command scheduled is due at that instant, so leftover
  timers stay queued until a later command's await moves the clock past
  them. A leftover repeating timer therefore costs nothing between
  commands under the virtual clock.

How the virtual clock keeps the host clock out of reach of the guest is
described in [KNOWN_RISKS.md](KNOWN_RISKS.md#virtual-clock).

### --epoch=\<ms\>

Start of the `Date` timeline under `--clock=virtual`, in milliseconds since
1970. Default 0. Using it without `--clock=virtual`, or with a value above
the ECMAScript maximum time value (8640000000000000), is a usage error.
As the virtual clock advances, the `Date` timeline clamps at that maximum
instead of leaving the `Date` range, while
`performance.now()` keeps counting the raw timeline as an unsigned 64-bit
millisecond count, which saturates at 18446744073709551615.

### --preload=\<file\>

Loads a filesystem snapshot (the JSON that `--dump` and `vm.snapshot()`
write) into `vm.fs` before the program starts. Older flat-map snapshots are
accepted too; see [GUEST-API.md](GUEST-API.md#snapshots). A file that cannot
be read or parsed ends the run with exit 2, and so does a snapshot too large
to load ("too large to load").

### --dump=\<file\>

Writes the filesystem snapshot to `file` when the run ends. The dump is
written after a normal exit, `vm.exit()` (from any context, including a
timer, a microtask or a stdin read), an uncaught exception, an unhandled
rejection, a timeout, a heap-cap stop (137), and when `--serve` reaches the
end of stdin. The dump is serialized in Rust and runs no JavaScript, so it
works after the guest has been stopped.

No dump is written after a V8 fatal error (the process is gone), a prelude
failure (exit 1), a preload failure (exit 2) or a panic.

If writing the dump fails, tardigrade prints the path and the OS error, and
exits with 74 when the guest itself exited 0; a guest that failed keeps its
own code. A dump can exceed V8's string limit (a very large filesystem is
possible with `--fs-quota=0`): it is written, but `--preload` refuses it
later with exit 2.

How the file is written:

- A regular file, or a path that does not exist yet, is replaced
  atomically: the snapshot goes to a hidden temporary file
  (`.<name>.tardigrade-*.tmp`) in the same directory, which is synced and then
  renamed over the target, and the directory is synced. A failed write
  leaves the previous dump intact, which matters when `--dump` and
  `--preload` name the same file. The directory must allow creating a file.
  Because of the rename, the path gets a new inode: hard links to the old
  dump are broken, a read-only (0400) dump is still replaced, only the
  permission bits are carried over (not owner, group, ACLs or extended
  attributes), and a hard kill during the write can leave the hidden
  temporary file behind.
- A symlink (even a dangling one), a device such as `/dev/null`, a FIFO, or
  anything else that is not a regular file is written in place with
  `open(O_CREAT|O_TRUNC)`, so the kernel keeps the link and applies
  `fs.protected_symlinks`. A directory fails with exit 74.
- Under `--sandbox` the file is truncated and rewritten in place, because
  Landlock allows writing only that one file and no temporary file can be
  created. A failure during the write can leave it truncated.

### --serve

Console mode. tardigrade runs `entry.js` once (the entry is optional),
then reads one JavaScript expression per line from stdin and answers each
with exactly one line of JSON on stdout. The session shares one guest, so
globals, timers and files persist across commands; each line is compiled
as its own expression, so no host value ever sits in a guest-visible
global (replacing `globalThis.eval` or `JSON` changes only what later
commands read, never the protocol itself).

The reply is `JSON.stringify` of the expression's value (`undefined` and
functions give `null`), or an error object:

- a line that fails to compile, or an expression that throws, replies
  `{"$error":"<message>"}` and the session continues. `let x = 1` or two
  statements on one line are a syntax error, answered the same way; wrap
  statements in an expression, for example
  `(() => { let x = 1; return x })()`. The message is the error's own for
  plain errors, `Name: message` for named ones (`SyntaxError`,
  `TypeError`, ...), and `String(value)` for anything else - a thrown
  string says itself;
- a value that JSON cannot encode (a `BigInt`, a circular structure)
  replies `{"$error":"<message>"}` for the same reason;
- a thenable value (anything with a callable `then`; every promise) is
  awaited: the event loop runs, turn by turn, until it settles, and the
  reply is the fulfillment value as JSON, or `{"$error":...}` on
  rejection. The await counts as a handler, so a rejection of the
  command's own value is answered, not fatal. Other pending work (a
  repeating timer) cannot hold the reply hostage, and the work it leaves
  pending stays scheduled for the next command;
- a promise nothing can settle any more (the loop is idle with the
  promise still pending) replies `{"$error":"promise never settled"}`;
  with `--command-timeout` it replies `{"$error":"command timed out"}`
  at the deadline instead;
- if guest-replaced intrinsics hand back text with a newline in it, the
  fixed reply `{"$error":"<encoding failed>"}` is sent instead: one JSON
  line per command, whatever the guest does.

Unhandled rejections of promises other than the command's value still
end the session as in a plain run: `(void Promise.reject(1), 2)` replies
2, then the session exits 1 at the next microtask checkpoint.

Input details: empty lines are skipped, and a line of only blanks is a
syntax error like any other; one trailing carriage return is stripped,
so "cmd", "cmd\r\n" and a final unterminated "cmd\r" are one
command; a trailing `//` comment is allowed (the line sits between
newlines in the wrapper); a last line without a newline is still a
command. A line that is not valid UTF-8 ends the session with exit 0, and
input the session did not read is kept for the next reader.

Replies rendezvous with the caller's writer: a reply is on stdout before
anything the guest prints after it, and output the command's own
microtasks print still comes first. `console.log` output appears on
stdout between replies (see [KNOWN_BUGS.md](KNOWN_BUGS.md)).

After every command the loop settles the timers, `vm.net` messages and
microtasks the command left pending, under the same
[--command-timeout](#--command-timeoutms) deadline; an expiry there
stops the leftover work, prints one stderr line (`tardigrade: command
timed out after its reply`) and the session continues. A leftover
repeating timer means every later command first waits out up to its
whole `--command-timeout` budget while the timer keeps re-arming, and
without the flag the session never reaches the next command at all -
clear timers in handlers that schedule them (`clearInterval`,
`clearTimeout`) unless they are meant to outlive the command; for
`serve_bridge` that rule is one request per command timeout. An uncaught
exception from a leftover timer or listener during that settling ends the
session with exit 1, as in a plain run.

Under `--clock=virtual` the post-reply settling never jumps the clock
(see the [--clock=virtual](#--clockvirtual) list): a one-shot `setTimeout`
left by a command stays queued until a later command's await moves the
clock past it - a handler that schedules with `setTimeout` and does not
await will not see it run under virtual time.

Only host-level failure ends the session: input that is not valid UTF-8,
`vm.exit` (its code), the whole-run `--timeout` (124), the heap cap
(137), an unhandled rejection still unhandled at a microtask checkpoint
(1), an uncaught exception from leftover work during the settling (1),
or the reply consumer going away (0). At the end of stdin (or such an
end) a `--dump` is written as usual.

The entry cannot be `-`, because stdin is the command channel; the
guest's `vm.stdin` is at end of file. `--timeout` defaults to 0 in this
mode and bounds the whole session, not each command; use
`--command-timeout` for that. A session whose time has run out while it
waits for input ends with exit 124 when its next line or the end of
input arrives. See [MANUAL.md](MANUAL.md#console-mode) for an example,
and [KNOWN_BUGS.md](KNOWN_BUGS.md#console-mode---serve) for the current
limits of this mode.

### --sandbox

Applies kernel-level containment (Landlock and seccomp) to the whole
process before the guest starts. Linux x86_64 only. If the platform or
kernel cannot enforce every layer, tardigrade exits 2 without running the guest;
it never falls back to running unsandboxed. See [SANDBOX.md](SANDBOX.md).

### --sandbox-check

As the only argument: exits 0 if `--sandbox` can work on this platform and
kernel, and 2 with a message if not. Use it in scripts instead of parsing a
refusal message. Anywhere else on the command line it is an error before the
entry and an ordinary guest argument after it.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | the program finished, or called `vm.exit(0)` |
| *n* | the program called `vm.exit(n)`; the status is *n* modulo 256, so `vm.exit(-1)` ends 255 |
| 1 | uncaught exception, unhandled promise rejection (still unhandled when the microtask queue drains), or prelude failure |
| 2 | usage error, entry file that cannot be read, preload failure, or `--sandbox` not available |
| 70 | the guest thread could not be started |
| 74 | `--dump` could not be written and the program itself exited 0 |
| 101 | tardigrade panicked (for example, an entry read from stdin that is not valid UTF-8) |
| 124 | `--timeout` expired |
| 137 | the program exceeded the heap cap (`--memory`) |
| 128 + *n* | the process was killed by signal *n*, as shells report it: 133 is SIGTRAP from a V8 fatal error, 134 SIGABRT from a failed host allocation, 139 SIGSEGV. See [KNOWN_RISKS.md](KNOWN_RISKS.md#process-aborts) and [KNOWN_BUGS.md](KNOWN_BUGS.md) |

## Output

`console.log` and `console.info` write to stdout, `console.warn` and
`console.error` to stderr. Arguments are converted with ToString and
joined with spaces, so objects print as `[object Object]`; a Symbol, or an
object without a `toString`, makes the call throw `TypeError`. Use
`JSON.stringify` for structured output.

tardigrade's own messages go to stderr, prefixed with `tardigrade:`, for
example:

    tardigrade: uncaught exception: app.js:3: Uncaught Error: boom
    tardigrade: unhandled promise rejection: Error: boom
    tardigrade: heap limit exceeded (256 MB)

`--sandbox-check` prints `supported` on stdout when it exits 0.

## Environment

`TARDIGRADE_DEBUG`, when set, makes tardigrade print its parsed entry, dump,
preload and serve settings to stderr before the run.

Local-time `Date` methods always use UTC, also under `--clock=virtual`:
tardigrade sets `TZ=UTC` for itself before initializing the engine, and a
`TZ` in the environment is ignored. The guest cannot read any environment
variable.

`serve_bridge` reads `OUT` as the path of the tardigrade binary (default:
`tardigrade` on `PATH`). The example scripts read `OUT` too, and some also
`BRIDGE`, `PORT` and `WS`; see the comments at the top of each script.

The variables used by the tests and `make` targets are listed in
[CONTRIBUTE.md](CONTRIBUTE.md).
