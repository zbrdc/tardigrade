# tardigrade command-line options

## Synopsis

    tardigrade [options] <entry.js | -> [args...]
    tardigrade --serve [options] [entry.js]
    tardigrade --sandbox-check

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

### --memory=\<mb\>

Heap cap in megabytes (MiB, 2^20 bytes), minimum 8, default 256. It caps the
JavaScript heap and, separately, ArrayBuffer memory, each at this size. A
guest whose live heap V8 cannot collect below its limit is stopped with exit
137; an ArrayBuffer allocation over the cap throws a catchable `RangeError`.
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
  it carries across commands, and each command's timers settle at once in
  the loop turn after it.

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

Console mode. tardigrade runs `entry.js` once (the entry is optional), then
reads one JavaScript expression per line from stdin and writes one line of
JSON per expression to stdout:

- the reply is `JSON.stringify` of the expression's value; `undefined` and
  functions give `null`, and a promise gives `{}` because it is not
  awaited;
- an expression that throws gives `{"$error":"<message>"}`, and the
  session continues;
- lines with no characters are skipped (a line of only blanks is a syntax
  error and ends the session), a trailing carriage return is stripped, a
  trailing `//` comment is allowed, and a last line without a newline is
  still a command;
- pending timers and `vm.net` messages settle after every command;
- the session ends at the end of stdin; input that is not valid UTF-8 also
  ends it.

The entry cannot be `-`, because stdin is the command channel; the guest's
`vm.stdin` is at end of file. There is no per-command time limit:
`--timeout` defaults to 0 in this mode, and when given it bounds the whole
session, not each command. A command still running at the deadline is
stopped at once; a deadline that passes while the session waits for input
takes effect when the next line or the end of input arrives (exit 124). See
[MANUAL.md](MANUAL.md#console-mode) for an example, and
[KNOWN_BUGS.md](KNOWN_BUGS.md#console-mode---serve) for the current limits
of this mode.

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
| *n* | the program called `vm.exit(n)`; the status is *n* modulo 256. Negative codes are currently ignored (see [KNOWN_BUGS.md](KNOWN_BUGS.md)) |
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

`TZ` sets the time zone that local-time `Date` methods use, also under
`--clock=virtual`, so the guest can observe it. Set `TZ=UTC` for runs that
must match across hosts. The guest cannot read any other environment
variable.

`serve_bridge` reads `OUT` as the path of the tardigrade binary (default:
`tardigrade` on `PATH`). The example scripts read `OUT` too, and some also
`BRIDGE`, `PORT` and `WS`; see the comments at the top of each script.

The variables used by the tests and `make` targets are listed in
[CONTRIBUTE.md](CONTRIBUTE.md).
