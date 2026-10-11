# v8-vm

Run small JavaScript applications entirely in RAM on the Chromium V8 engine,
implemented in Rust on the v8 crate (Deno's rusty_v8 bindings, V8 12.9).

Inspired by just-bash (vercel-labs): a simulated bash environment with an
in-memory filesystem for AI agents. v8-vm is the same idea on a real V8. An
application gets its own isolate and process, a virtual filesystem that
exists only in process memory, an event loop with timers, an in-memory
network, streaming stdin, a heap cap, and a wall-clock timeout. Guests never
make a syscall of their own.

## Build

Requires cargo. The v8 crate downloads prebuilt V8 bindings on first build
(CARGO_HOME/CARGO_TARGET_DIR are pointed at the scratch volume by the
Makefile).

    make check

Binaries land in the scratch target directory, not the repo.

## Use

    v8vm [--timeout=<ms>] [--memory=<mb>] [--preload=<file>] [--dump=<file>] [--serve] [--sandbox] <entry.js or -> [args...]

- entry "-" reads the program from stdin (then vm.stdin is at EOF)
- "--timeout=<ms>" wall-clock kill (default 60000, 0 disables); exit 124
- "--memory=<mb>" V8 heap cap in MB (default 256)
- "--preload=<file>" seed vm.fs from a snapshot() JSON dump before the app runs
- "--dump=<file>" write vm.snapshot() to a file when the run ends
- "--serve" console mode: one JS expression per stdin line, one JSON line out
  (untimed by default; stdin is the command channel, so vm.stdin is EOF)
- "--sandbox" OS self-containment for hostile code (Linux only), see below
- uncaught exceptions and unhandled promise rejections exit 1; usage errors 2

## Library API

The package is a library plus thin binaries; link it to drive guests
in-process from Rust:

    let code = v8vm::run(source, "app.js", &v8vm::Config::new());
    let code = v8vm::serve(std::io::stdout(), source, "app.js", &cfg);

Note: run()/serve() use process-global world state for the duration of a
call; one guest at a time per process. serve() reads commands from process
stdin.

For hostile code call `v8vm::sandbox::apply(dump)` once, before the first
run()/serve() and before any thread exists; it refuses otherwise. Read a
--preload file before applying and hand its contents to the library as
`Config.preload_data`: after apply, the only file the run can write is the
dump file. `Config` gained that field in 0.1.0, so embedders building a
`Config` with a struct literal must add it (or use `..Default::default()`).

## Event loop

The process stays alive while guest work is pending: live timers, queued
network messages, or stdin readers. Each loop turn delivers one generation of
network messages, runs due timers, drains microtasks, and polls stdin. Exit
when nothing is pending: scripts that schedule nothing terminate immediately.

## Guest API

| API | Description |
|---|---|
| console.log/info/warn/error | stdout / stderr |
| setTimeout/setInterval/clearTimeout/clearInterval | timer id semantics as usual |
| queueMicrotask(fn) | Promise-based microtask |
| vm.version / vm.args | engine version, arguments after the entry path |
| vm.exit(code) | stop the program with code |
| vm.fs.read/write/list/remove | RAM filesystem (throwing read/remove) |
| vm.snapshot() / vm.restore(json) | whole-filesystem JSON snapshot / restore |
| vm.run(path) | load-and-eval a file from the RAM fs |
| vm.net.port() | fresh port number |
| vm.net.listen(port, (payload, from) => ...) | receive messages on a port |
| vm.net.send(fromPort, toPort, payload) | queue a message; throws if no listener |
| vm.net.close(port) | stop listening |
| vm.stdin.read() | Promise of the next stdin chunk; "" at EOF |

WebAssembly, Intl/ICU, and all of ES2023 are available.

## HTTP bridge

serve_bridge owns a real localhost socket (trusted host code) and relays each
request into a guest handler as one JS expression over the console:

    $(cargo target dir)/release/serve_bridge examples/webapp.js 8077
    curl http://127.0.0.1:8077/

The guest keeps its zero-syscall guarantee: it never sees a socket.

## Agent loop

examples/agent-loop.sh shows the just-bash-style harness: seed a workspace
with --dump, run the driver contract (PASS/FAIL/exit code), let an agent (swap
the canned edit for your LLM call) patch the workspace, rerun. Swap in
tsc-rs at this boundary to let agents write TypeScript.

## OS self-sandbox (--sandbox)

For hostile code, `--sandbox` puts kernel-level containment around the run.
Linux x86_64 only (the deny list and the x32 guard are verified on that ABI);
on any other platform, or on a kernel missing the features, v8vm exits 2
without running the guest.

The layers are applied after the host reads the entry source and the preload
file, and before V8 initializes, so V8's worker threads, the watchdog, and
the stdin reader all inherit them. Inherited descriptors >= 3 are closed
first (anything the caller passed in would be a channel out of the sandbox),
and `PR_SET_NO_NEW_PRIVS` is set; the seccomp filters are installed with
TSYNC.

- Landlock: filesystem access is denied except for two files. `--dump` is
  created without truncation when the sandbox starts (a run that ends before
  the snapshot leaves an existing file untouched, or an empty new file) and
  is the only write the run can perform: the end-of-run snapshot truncates
  and rewrites that one file, and even reading it back is denied. /dev/urandom
  is readable because V8 opens it while seeding RNGs. Note what Landlock does
  not mediate: file metadata, file existence, and symlink targets stay
  observable through stat/access/readlink/chdir/statfs (xattr calls are
  denied instead).
- seccomp-BPF, a deny list where denied calls fail with EPERM: all socket
  calls, exec (execve, execveat, fork, vfork), ptrace and cross-process
  memory access, privilege and identity changes, mount and namespace calls,
  kernel modules, bpf, perf, io_uring, userfaultfd, keyctl, System V and
  POSIX message queues, personality, modify_ldt, xattr and other file
  metadata changes, and clock setting. Signals may only be sent to the run's
  own process (kill/tgkill with any other target, and tkill, are denied).
  ioctl is denied for the terminal input injection requests TIOCSTI and
  TIOCLINUX; other ioctl requests remain allowed on already-open descriptors,
  which is a residual surface.
- Process creation is filtered, not just denied: clone succeeds only with
  CLONE_THREAD set and no CLONE_NEW* namespace flag, and clone3 fails with
  ENOSYS so glibc falls back to clone (which is then checked). Threads still
  work; fork and user-namespace escapes do not.
- A small hand-written stacked filter kills the process on any syscall made
  through a different ABI: a non-x86_64 arch value (int 0x80) or a syscall
  number in the x32 range, which would otherwise bypass every deny rule.

Everything the runtime needs stays allowed: threads, mmap/mprotect for the
JIT, futex, clocks, getrandom, and reads/writes on already-open descriptors.
At startup V8 probes /sys/devices/system/cpu/online, /proc/stat and
/proc/self/maps; all are denied and V8 falls back (worker-pool sizing comes
from sched_getaffinity) — /dev/urandom is allowed as above.

What this does not do: it is not a VM. The guest runs as your user inside
your process; kernel bugs, hardware side channels, the stdio descriptors,
and the `--dump` file itself are out of scope. A compromised guest can still
burn CPU until --timeout and read the metadata of the filesystem around it.

Kernel requirements: Landlock ABI 5 (Linux 6.10) and seccomp with TSYNC. A
ruleset that is not fully enforced is refused rather than degraded. Verified
on Linux 6.17 reporting Landlock ABI 7; older kernels are untested. If
Landlock or seccomp is missing the run refuses to start — it never falls
back to unsandboxed execution.

## Guarantees and limits

- Guests make no syscalls: the only disk access is at the host boundary
  (entry source, --preload/--dump files). Verify: strace -f -e trace=openat
- vm.net is a same-process message switch, not sockets: it cannot leave the
  run. One delivery generation per loop turn.
- This is an application-level sandbox, not a container: isolation is by
  omission of host APIs, inside one ordinary process per run. For hostile
  code, add `--sandbox` (above) and/or run v8vm itself under a
  container/seccomp profile.
- Hitting the heap cap aborts with the V8 out-of-memory report.
- No ES module resolution yet (keep sources in vm.fs and vm.run them).
