# v8-vm

Run small JavaScript applications entirely in RAM on the Chromium V8 engine,
implemented in Rust on the v8 crate (Deno's rusty_v8 bindings, V8 12.9).

Inspired by just-bash (vercel-labs): a simulated bash environment with an
in-memory filesystem for AI agents. v8-vm is the same idea on a real V8. An
application gets its own isolate, a virtual filesystem that
exists only in process memory, an event loop with timers, an in-memory
network, streaming stdin, a heap cap, and a wall-clock timeout. Guests never
make a syscall of their own.

## Build

Requires cargo. The v8 crate downloads a prebuilt static V8 on first build.

    make check

Binaries land in the cargo target directory (target/ by default), not the
repo.

## Development

    make test    # cargo test --release: unit, CLI, and library tests
    make clippy  # cargo clippy --release --all-targets, warnings denied
    make check   # build, then scripted smoke checks over the examples
    make ci      # test, then clippy, then check, in that order; stops at the first failure
                 # CI runs make ci CARGOFLAGS=--locked (.github/workflows/ci.yml)

Environment overrides: CARGO selects the cargo binary (default cargo), and
CARGO_TARGET_DIR relocates all build output (default target; set it as an
environment variable or a make variable). CARGOFLAGS adds flags to build, test
and clippy (CI sets --locked). CARGO_HOME is cargo's own registry cache knob
and is not set by this repo.

## Use

    v8vm [--timeout=<ms>] [--memory=<mb>] [--preload=<file>] [--dump=<file>] [--serve] <entry.js or -> [args...]

- entry "-" reads the program from stdin (then vm.stdin is at EOF)
- "--timeout=<ms>" wall-clock kill (default 60000, 0 disables); exit 124
- "--memory=<mb>" V8 heap cap in MB (default 256); hitting it exits 137
- "--preload=<file>" seed vm.fs from a snapshot() JSON dump before the app runs
- "--dump=<file>" write vm.snapshot() to a file when the run ends
- "--serve" console mode: one JS expression per stdin line, one JSON line out
  (untimed by default; stdin is the command channel, so vm.stdin is EOF)
- uncaught exceptions exit 1; usage errors 2; unhandled promise rejections
  (still unhandled when the microtask queue drains) exit 1

## Library API

The package is a library plus thin binaries; link it to drive guests
in-process from Rust:

    let code = v8vm::run(source, "app.js", &v8vm::Config::new());
    let code = v8vm::serve(std::io::stdout(), source, "app.js", &cfg);

Note: each run()/serve() call creates its own isolate and world; several
guests can run concurrently in one process and do not see each other's
state. Process stdin is served by one process-wide reader shared by vm.stdin
and serve() commands: input a run does not read is kept for the next run,
and when several consumers read at once the first one receives the chunks.
Isolation is per isolate within the process: hitting the heap cap ends only
that run (exit 137), while other V8 fatal errors end the whole process and
every guest in it.

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

## Guarantees and limits

- Guests make no syscalls: the only disk access a guest can trigger is at the
  host boundary (entry source, --preload and --dump files). Verify with
  strace -f -e trace=openat: besides those files, a run of examples/hello.js
  opens only dynamic-loader and runtime files at process start (ld.so.cache,
  libc, libm, libgcc_s) and V8 internals (/proc/self/maps, /dev/urandom,
  /sys/devices/system/cpu/online). None is guest-initiated.
- vm.net is a same-process message switch, not sockets: it cannot leave the
  run. One delivery generation per loop turn.
- This is an application-level sandbox, not a container: isolation is by
  omission of host APIs, per isolate inside one ordinary process. A V8
  fatal error ends the whole process and every guest in it. For hostile
  code, run v8vm itself under a container/seccomp profile.
- Hitting the heap cap terminates the guest: the run exits 137 with a
  one-line report on stderr; the process and other guests survive.
- No ES module resolution yet (keep sources in vm.fs and vm.run them).
