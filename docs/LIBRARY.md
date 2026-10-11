# Embedding tardigrade in Rust

The `tardigrade` package is a library plus two thin binaries. Link the
library to run guests inside your own process.

## Running a guest

    let cfg = tardigrade::Config::new();
    let code = tardigrade::run(source, "app.js", &cfg);

`run(source, entry, cfg)` runs one guest program to completion and returns
its exit code, with the same codes as the CLI (see
[OPTIONS.md](OPTIONS.md#exit-codes)). `entry` is the name used in stack
traces and error messages.

    let code = tardigrade::serve(std::io::stdout(), source, "app.js", &cfg);

`serve(output, source, entry, cfg)` runs the program, then serves the
console protocol of `--serve`: it reads one expression per line from
process stdin and writes one JSON line per expression to `output`. The
guest cannot run ahead of a reply: each reply is written and flushed on the
calling thread before the guest continues.

## Config

`Config::new()` returns the defaults. The fields:

| Field | Default | Meaning |
|---|---|---|
| `timeout_ms: u64` | 60000 | wall-clock limit; 0 disables it |
| `command_timeout_ms: u64` | 0 | per-command wall-clock limit in console mode; 0 disables it |
| `heap_mb: usize` | 256 | heap cap in MB; values below 8 are raised to 8 |
| `args: Vec<String>` | empty | the guest's `vm.args` |
| `preload: Option<PathBuf>` | `None` | snapshot file to load into `vm.fs` before the run |
| `preload_data: Option<String>` | `None` | snapshot JSON to load instead of reading `preload`; works without `preload` |
| `dump: Option<PathBuf>` | `None` | file to write the snapshot to when the run ends |
| `stdin_consumed: bool` | false | true when the entry source came from stdin, so the guest's stdin is at end of file |
| `fs_quota_bytes: u64` | 64 MiB (`tardigrade::DEFAULT_FS_QUOTA_BYTES`) | filesystem quota; 0 disables it |
| `seed: Option<u64>` | `None` | deterministic `Math.random` seed |
| `clock: Clock` | `Clock::Real` | `Clock::Virtual { epoch_ms }` gives the guest a virtual clock |

`serve()` applies `timeout_ms` to the whole session and
`command_timeout_ms` to each command (the limit that answers a slow
command with `{"$error":"command timed out"}` and keeps the session;
see [OPTIONS.md](OPTIONS.md#--command-timeoutms)). The CLI uses 0 for
both in `--serve` unless the matching flag is given; a library caller who
wants an unbounded session sets `timeout_ms: 0`. A session whose time has
run out while it waits for input ends with exit 124 when its next input
line or the end of input arrives.

`preload_data` goes through the same size check and `vm.restore` as a
preload file, so legacy and version 2 snapshots behave the same. If you
build a `Config` with a struct literal, set every field or end with
`..Default::default()`, so that new fields do not break your build.

`Clock::MAX_EPOCH_MS` is the largest epoch the CLI accepts (the ECMAScript
maximum time value). The library does not refuse larger epochs; the `Date`
timeline clamps at that value.

## Several guests in one process

Each `run` or `serve` call creates its own isolate and its own world, so
guests can run concurrently in one process without seeing each other's
state. Process stdin is read by one process-wide reader that serves both
`vm.stdin` and `serve` commands: input one run does not read is kept for
the next one, and when several consumers read at once, the first one gets
the chunks.

Isolation is per isolate. Reaching the heap cap ends only that run (exit
137 for the JavaScript heap, a catchable `RangeError` for ArrayBuffers).
Any other V8 fatal error ends the whole process and every guest in it; the
guest-reachable ones are listed in
[KNOWN_RISKS.md](KNOWN_RISKS.md#process-aborts).

V8 runs on a dedicated host thread that tardigrade starts on the first `run` or
`serve` call; guest threads are its descendants. A panic on a guest thread
is resumed on the thread that called `run` or `serve`, with its original
payload.

## Sandboxing the embedding process

`tardigrade::sandbox::supported()` reports whether this platform and kernel can
sandbox (x86_64 Linux with Landlock ABI 6). It installs nothing.

`tardigrade::sandbox::apply(dump: Option<&Path>)` applies the kernel layers
of `--sandbox` described in [SANDBOX.md](SANDBOX.md). Call it once, before
the first `run` or `serve`, while the calling thread is the only thread in
the process; it refuses otherwise. `dump` is the only file the process may
write afterwards. It returns a `SandboxError` when it refuses or fails:
`Unsupported` (not Linux x86_64), `EngineStarted` (a run already started
V8), `ThreadsActive` (other threads exist), `Dump` (the dump file cannot be
prepared), `DumpNotRegular` (the dump path is not a regular file), or
`Kernel` (the kernel lacks Landlock or seccomp, or a setup step failed). Its
`Display` text is the message the CLI prints.

Read any preload file before calling `apply` and pass its contents as
`Config.preload_data`, because the sandbox denies opening it later. After
`apply`, the dump is rewritten in place instead of through a temporary
file; nothing needs configuring for that.

The restrictions cover the whole embedding process, not only tardigrade's
threads. `apply` closes every file descriptor numbered 3 or higher and
sets `PR_SET_NO_NEW_PRIVS`. The filters apply to every thread, threads
started later inherit them, and all of your own code after `apply` runs
under the same denials, including the rule that syscall numbers 452 and
above fail with `ENOSYS`. Nothing can lift them.
