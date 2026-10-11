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

    make test    # cargo test --release: unit tests plus all five test targets
                 # (cli, webapi, library, stdin_hub, sandbox)
    make clippy  # cargo clippy --release --all-targets, warnings denied
    make check   # build, then scripted smoke checks over the examples
    make check-ts # the TypeScript agent loop; skips loudly when bun or
                 # every type checker is missing (V8VM_REQUIRE_TS=1 fails)
    make ci      # test, then clippy, then check, in that order; stops at the
                 # first failure. CI runs make ci CARGOFLAGS=--locked
                 # (.github/workflows/ci.yml)
    make test-heavy # the #[ignore]d memory-heavy tests (multi-hundred-megabyte
                 # guests probing V8's string limit); not part of ci

Environment overrides: CARGO selects the cargo binary (default cargo), and
CARGO_TARGET_DIR relocates all build output (default target; set it as an
environment variable or a make variable). CARGOFLAGS adds flags to build, test
and clippy (CI sets --locked). CARGO_HOME is cargo's own registry cache knob
and is not set by this repo.

## Use

    v8vm [--timeout=<ms>] [--memory=<mb>] [--fs-quota=<mb>] [--seed=<u64>] [--clock=virtual] [--epoch=<ms>] [--preload=<file>] [--dump=<file>] [--serve] [--sandbox] <entry.js or -> [args...]
    v8vm --sandbox-check

- entry "-" reads the program from stdin (then vm.stdin is at EOF)
- "--timeout=<ms>" wall-clock kill (default 60000, 0 disables); exit 124
- "--memory=<mb>" V8 heap cap in MB, minimum 8 (default 256); the CLI
  rejects smaller values with exit 2, library callers have them clamped up;
  a guest whose live JS heap V8 cannot collect below its limit exits 137
  (the exact rule is under Guarantees and limits)
- "--fs-quota=<mb>" RAM filesystem quota in MB (default 64, 0 disables)
- "--seed=<u64>" deterministic Math.random: two runs with the same seed
  draw identical sequences, different seeds differ, and concurrent seeded
  runs each keep their own sequence. Without it Math.random is V8's own
  (seeded from the OS)
- "--clock=virtual" virtual time for deterministic runs: Date.now(),
  new Date() with no argument, Date() called as a function and
  performance.now() read a virtual clock instead of the host clock, and
  it advances only when timers fire - when the event loop has no
  runnable work it jumps the clock straight to the next due timer
  instead of sleeping, so a run whose guest waits on nothing but timers
  finishes instantly regardless of the delays it schedules. The virtual
  clock starts at 0 for performance.now() and at --epoch (default 0) for
  the Date timeline. Date forms with an explicit argument (new
  Date(ms), new Date(y, m, ...)) are passed through untouched, so they
  stay exact. In virtual mode the prelude rebuilds Date over the
  virtual clock (the host installs one hidden time callback, the prelude
  captures and removes it, and the original Date constructor stays
  reachable only from that prelude closure), so no Date read - Date.now,
  the no-argument constructor, the function-call string form, or Date
  prototype methods on a no-argument date - can observe the host clock.
  Nor can any object-graph path reach a host-clock-bound constructor:
  the rebuilt Date's [[Prototype]] is Function.prototype (not the
  original Date), its statics are own copies (parse, UTC - neither reads
  a clock), and every intrinsic it uses at call time is captured with
  it, so replaced globals (Reflect.construct, Function.prototype
  methods, ...) neither change what runs nor trap the original
  constructor, and guest-installed index accessors on
  Array.prototype/Object.prototype are never run (the constructor's
  arguments go to Reflect.construct as the strict arguments object,
  whose indices are own data properties). The rebuilt constructor is
  shaped like the platform's: own length, name, prototype, now, parse,
  UTC - all non-enumerable - with a non-writable prototype property and
  a non-constructable, correctly named now. Two things remain
  detectable, though neither reads a clock: Function.prototype.toString
  of Date and Date.now shows the prelude wrapper's source instead of
  native code, and a virtual-mode construction carries one extra stack
  frame (the wrapper in prelude.js) inside `new Date`.
  --seed plus --clock=virtual makes a guest that touches nothing real
  byte-for-byte reproducible across runs.
- Wall-clock rules are unchanged under "--clock=virtual": --timeout
  measures real time and still kills a busy-looping guest with 124, and
  an endless timer schedule (a never-cleared setInterval) runs at full
  CPU until --timeout, because the clock jumps instantly. A
  self-sustaining net ping-pong is the same trap with the roles
  swapped: while runnable net work is pending the loop never jumps, so
  pending timers starve until --timeout - and between net generations
  the loop spins without the 50 ms poll pause real time takes, so keep
  --timeout set for such guests. stdin and net delivery
  stay real: net messages are delivered on the next loop turn without
  advancing the clock, and a pending stdin read stays pending while the
  clock jumps (the read resolves when its data arrives in real time; a
  run whose other work ends first exits with the read still pending).
  When only a stdin read is pending - no timers, no net - the loop
  waits in real time exactly as without the flag. Under --serve the
  virtual clock is one timeline for the whole session: it persists
  across commands, and each command's timers settle instantly in the
  loop turn after it.
- "--epoch=<ms>" the Date timeline's zero point under --clock=virtual
  (default 0); a usage error without --clock=virtual, and above the
  ECMAScript maximum time value (8640000000000000) - the Date timeline
  clamps there instead of leaving the Date range, while performance.now()
  keeps counting the raw timeline (a u64 of milliseconds, so it saturates
  at 18446744073709551615 rather than clamping)
- "--preload=<file>" seed vm.fs from a snapshot() JSON dump before the app runs
- "--dump=<file>" write the snapshot to a file when the run ends: normal
  exit, vm.exit (also from a timer, microtask or stdin read), uncaught
  exception, unhandled rejection, timeout, or --serve reaching stdin EOF.
  A run stopped by the heap cap (exit 137) still writes its dump, because
  the dump is serialized in Rust and runs no JavaScript. No dump is written
  after a V8 fatal out-of-memory abort (the process is
  gone), after a prelude failure (exit 1) or a preload failure (exit 2), or
  after a panic.
  A regular file, or a path that does not exist yet, is replaced
  atomically (not under --sandbox, below): the snapshot goes to a hidden
  sibling temp file (`.<name>.v8vm-*.tmp`) in the same directory, is
  fsynced, then renamed over the target, and the directory is fsynced.
  A failed write therefore leaves the previous dump intact, which matters
  when --dump equals --preload (the agent loop). The directory must allow
  creating a file. Rename semantics apply: the path gets a new inode, so
  hard links to the old dump are broken; an existing read-only (0400) dump
  is replaced, because rename needs no write permission on the file; only
  the permission bits are carried over, not owner, group, ACLs or xattrs;
  and a hard kill mid-write can leave a hidden `.name.v8vm-*.tmp` file.
  A symlink (even a dangling one), a device such as /dev/null, a FIFO or
  anything else that is not a regular file is written in place instead,
  with open(O_CREAT|O_TRUNC), so the kernel keeps the link and applies
  fs.protected_symlinks; a directory fails with exit 74. Under --sandbox
  Landlock allows writing only the dump file itself, so no temp file can
  be made: the file is truncated and rewritten in place, and a failure
  mid-write can leave it truncated
- "--serve" console mode: one JS expression per stdin line, one JSON line out
  (untimed by default; stdin is the command channel, so vm.stdin is EOF)
- "--sandbox" OS self-containment for hostile code (Linux only), see below
- "--sandbox-check" (only as the sole argument) exit 0 if --sandbox can work
  here, 2 if not; after the entry it is an ordinary guest argument
- uncaught exceptions exit 1; usage errors 2; unhandled promise rejections
  (still unhandled when the microtask queue drains) exit 1
- "--dump" failure (the file or its directory cannot be written; the message
  names the path and the OS error) exits 74 when the guest itself exited 0; a
  failed guest keeps its own code. A dump can exceed V8's string limit (a
  multi-hundred-megabyte filesystem, possible with --fs-quota=0): it is
  written, but --preload then refuses it with exit 2 ("too large to load")

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
that run (exit 137 for the JS heap, a catchable RangeError for
ArrayBuffers), while other V8 fatal errors end the whole process and
every guest in it. What --memory guarantees, and what it does not, is
spelled out under Guarantees and limits.

For hostile code call `v8vm::sandbox::apply(dump)` once, before the first
run()/serve() and before any thread exists; it refuses otherwise. Read a
--preload file before applying and hand its contents to the library as
`Config.preload_data`: after apply, the only file the run can write is the
dump file. `preload_data` seeds the filesystem by itself (it needs no
`preload` path) and takes precedence over reading `preload`. It goes through
the same size check and `vm.restore` as a file, so legacy and version 2
snapshots behave identically. After `apply` the dump is rewritten in place
instead of through a temp file, which Landlock forbids; nothing needs
configuring for that. Embedders that build a `Config` with a struct
literal must set the newer fields (`preload_data`, `fs_quota_bytes`, `seed`,
`clock`), or use `..Default::default()`.

The restrictions apply to the whole embedding process, not only to v8vm's
threads. The filters cover every thread (apply refuses unless the caller is
the only one) and every thread started later inherits them, so all code of
the embedder after `apply` runs under the same denials, including the rule
that syscall numbers 452 and above fail with ENOSYS. Nothing can lift them.

## Event loop

The process stays alive while guest work is pending: live timers, queued
network messages, or stdin readers. Each loop turn delivers one generation of
network messages, runs due timers, drains microtasks, and polls stdin. Exit
when nothing is pending: scripts that schedule nothing terminate immediately.
Under --clock=virtual the same loop never sleeps to wait for a timer: with no
runnable work it jumps the virtual clock to the next due timer (see the flag
above).

## Guest API

| API | Description |
|---|---|
| console.log/info/warn/error | stdout / stderr |
| setTimeout/setInterval/clearTimeout/clearInterval | timer id semantics as usual |
| queueMicrotask(fn) | Promise-based microtask |
| vm.version / vm.args | engine version, arguments after the entry path |
| vm.exit(code) | stop the program with code; all host effects (output, writes, scheduling) stop immediately and the first exit wins - pure JS computation may run briefly until V8 observes the termination |
| vm.run(path) | load-and-eval a file from the RAM fs |
| vm.net.port() | fresh port number |
| vm.net.listen(port, (payload, from) => ...) | receive messages on a port |
| vm.net.send(fromPort, toPort, payload) | queue a message; throws if no listener |
| vm.net.close(port) | stop listening |
| vm.stdin.read() | Promise of the next stdin chunk; "" at EOF |
| TextEncoder() | encode / encodeInto, UTF-8 only |
| TextDecoder([label][, options]) | decode, UTF-8 only |
| atob / btoa | Latin-1 base64 per spec |
| structuredClone(value[, options]) | V8's own structured clone |
| performance.now() | monotonic ms since the run started (virtual ms under --clock=virtual) |
| Math.random under --seed | splitmix64 draws, 53 bits in [0, 1), per run |

Details and limits of the web APIs above:

- TextEncoder.encode returns a Uint8Array; unpaired surrogates encode as
  U+FFFD (EF BF BD). encodeInto fills a Uint8Array (its window, not the
  whole buffer), returns {read, written} and never writes a partial code
  point; a detached destination has zero capacity. TextEncoder has the
  `encoding` getter ("utf-8") and a Symbol.toStringTag.
- TextDecoder accepts only the UTF-8 labels ("utf-8", "utf8",
  "unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8",
  "x-unicode20utf8", case-insensitive, ASCII-whitespace-trimmed);
  anything else throws RangeError. Invalid bytes become U+FFFD, one per
  maximal ill-formed subsequence; {fatal: true} makes them a TypeError.
  A leading U+FEFF BOM is stripped unless {ignoreBOM: true}, also when it
  arrives split across {stream: true} chunks, and {stream: true} carries
  an incomplete trailing sequence to the next call. A non-stream decode
  ends the stream: the next call starts fresh (new BOM decision, no
  carry), also after a throwing one. Options convert like a WebIDL
  dictionary (null is empty, non-objects throw TypeError, getters may
  throw). Per-instance state lives in #private fields (nothing enumerable
  on instances).
- atob / btoa: btoa accepts code units 0..0xFF only; atob decodes
  forgiving-base64 (ASCII whitespace ignored, padding only at the end,
  unpadded tails decoded to their whole bytes, trailing bits
  unchecked). Both throw Errors named InvalidCharacterError, require
  their argument, and throw RangeError when the result cannot be
  allocated or exceeds V8's string limit; atob validates its whole
  input before allocating anything, so a malformed unit anywhere fails
  as InvalidCharacterError even under memory pressure.
- structuredClone handles Map, Set, Date, RegExp, typed arrays and their
  views, ArrayBuffers, cycles and shared references inside one clone.
  Functions, symbols, WebAssembly modules and SharedArrayBuffers throw
  an Error named DataCloneError; there is no transfer list ({transfer:
  [...]} throws DataCloneError, a non-array transfer - a Set is not
  accepted either, only Arrays - a TypeError), and symbol-keyed
  properties are dropped, as the clone algorithm dictates.
- performance.now() reports milliseconds on the real clock (a monotonic
  Instant) with the start of the run as its zero point, so values begin
  near zero in every run and never go backwards within one; under
  --clock=virtual it is the virtual timeline, which starts at exactly 0
  and moves only when the event loop jumps the virtual clock.
- All of these decode/encode paths bound their Rust-side allocations
  (decode reads the input from its backing store in pieces sized to the
  input, atob walks the string's own units and fails on the first
  non-alphabet unit before allocating anything, and every output buffer
  grows through try_reserve): a memory-constrained embedder sees a guest
  RangeError instead of a process abort. What stays outside that rule
  is V8's own memory: a decode whose result string cannot exist, a btoa
  result that no longer fits the heap, or a clone whose wire buffer
  cannot be allocated still ends with V8's own report - a RangeError
  from V8 where it can build one, its fatal out-of-memory report where
  it cannot (one of the aborts listed under Guarantees and limits).
  Buffers are the exception: every ArrayBuffer these paths create or
  clone is charged to the run's ArrayBuffer budget (see --memory under
  Guarantees and limits). A TextEncoder.encode result or a
  vm.fs.readBytes copy that the budget refuses throws RangeError, and a
  structuredClone whose buffer copy it refuses throws DataCloneError;
  both are catchable and the run continues.


WebAssembly, Intl/ICU, and all of ES2023 are available.

### vm.fs

A RAM-only tree of directories and byte-valued files. Paths are
normalized (`.`, `..`, duplicate and trailing slashes collapse) and must
be absolute; a path that would climb out of the root throws, and
`exists` just returns false for it. Paths containing lone UTF-16
surrogates throw "path contains lone surrogates" (Rust cannot represent
them, and silently converting to U+FFFD would let two distinct guest
strings name one file); a genuinely typed U+FFFD character is a legal
path byte. If a path argument's `toString` throws, that exception
propagates. Writing a file creates its missing parent directories.

| API | Description |
|---|---|
| vm.fs.write(path, data) | write a file; data is a string, an ArrayBuffer (shared buffers included), or a typed array / DataView; an existing file is replaced wholesale. The data's length (UTF-8 length for strings) is checked against the quota before anything is copied. A detached buffer or view throws; a view left out of bounds by a resizable-buffer shrink behaves exactly as JS shows it (length zero) and writes an empty file |
| vm.fs.read(path) | read as a string; bytes that are not valid UTF-8 come back as U+FFFD; files larger than V8's string limit throw |
| vm.fs.readBytes(path) | read as a Uint8Array |
| vm.fs.list(dir) | recursive listing of file paths under dir, full paths, sorted; directories are not listed |
| vm.fs.readdir(path) | immediate child names (files and directories), sorted |
| vm.fs.mkdir(path, {recursive}) | create a directory; recursive also creates parents and tolerates an existing directory |
| vm.fs.exists(path) | true when the path is a file or directory |
| vm.fs.stat(path) | {type: 'file'\|'dir', size}: content bytes for files, immediate child count for directories |
| vm.fs.rename(from, to) | POSIX-shaped: a file replaces a file, a directory moves onto an empty directory; mismatched kinds, a move into the source's own subtree, or a missing destination parent throw |
| vm.fs.remove(path, {recursive}) | delete a file, an empty directory, or with recursive a whole subtree |
| vm.snapshot() / vm.restore(json) | whole-filesystem snapshot; see below |

A failed mutating call leaves the filesystem unchanged.

The tree is quota-capped: usage is content bytes plus path bytes plus a
flat 64 bytes of bookkeeping per file or directory entry (the root is
free). Default 64 MiB; `--fs-quota=<mb>` overrides it and 0 disables it.
A write that would cross the quota throws Error('fs quota exceeded') and
changes nothing. Normalized paths are capped at 4096 bytes (renames check
re-keyed descendants too) and the tree at 100000 entries.

Snapshots are JSON, written with sorted keys so dumps are deterministic.
The current format is
`{"dirs":["/","/d"],"files":{"/a.txt":{"text":"..."},"/b.bin":{"b64":"..."}},"version":2}`:
each file becomes `{"text":...}` when its bytes are valid UTF-8 whose
JSON-escaped form is not larger than the base64 form, `{"b64":...}`
otherwise, and base64 is decoded strictly (padding only in the final
chunk, canonical trailing bits). vm.restore also accepts the legacy
format, a flat `{"/path": "text"}` map — relative keys are read as
root-relative, and keys that then escape the root or break the path cap
fail naming the key — so --preload files written by earlier versions
keep working; restore materializes parent directories of every restored
entry and never replaces an existing file with a directory or vice
versa, and two spellings of one path in a single snapshot ("/a" and
"//a", or a legacy "b" and "/b") fail with "duplicate path in snapshot".
A snapshot naming any other `version` fails with "unsupported snapshot
version <v>" (the value is echoed capped at 256 bytes). When a quota is
set, restore refuses a payload larger than six quotas plus 1 MiB before
parsing it (JSON escapes control characters to six bytes each, so a
full-quota filesystem of them still restores from its own snapshot).
Version 2 dumps are not loadable by older v8vm versions, which would see
files named "version", "files" and "dirs" - and for the same reason a
legacy flat map that contains a file literally named "version" parses as
a versioned snapshot here, not as a file list. Legacy-key errors show the
"/"-prefixed key.

## HTTP bridge

serve_bridge owns a real localhost socket (trusted host code) and relays each
request into a guest handler as one JS expression over the console:

    $(cargo target dir)/release/serve_bridge examples/webapp.js 8077
    curl http://127.0.0.1:8077/

The guest keeps its zero-syscall guarantee: it never sees a socket.

## Agent loop

examples/agent-loop.sh shows the just-bash-style harness: seed a workspace
with --dump, run the driver contract (PASS/FAIL/exit code), let an agent (swap
the canned edit for your LLM call) patch the workspace, rerun.
examples/agent-loop-ts.sh is the same loop for a TypeScript app; see
TypeScript below.

## TypeScript

v8vm itself runs plain JavaScript and gains no dependency for TypeScript:
the support lives at the agent-loop boundary, on the host. Two external
tools do the work (both are development tools of the loop, never v8vm
runtime dependencies, and the guest never sees them):

- bun (1.3.5 tested) bundles the app with
  `bun build <entry> --target=browser --format=iife`. v8vm has no ES module
  resolution, so a multi-file project must become one IIFE before it can
  run: bundling is required, not an optimization. bun does not type-check.
- tsc-rs (pinned 0.3.0, fetched by npx) type-checks the sources with
  `--noEmit` through a generated strict tsconfig. tsc-rs is an experimental
  Rust port of the TypeScript 7 compiler (it reports 7.1.0-dev and moves
  fast); when it is unavailable or crashes, the loop falls back to
  `tsc --noEmit`, and its output names which checker decided.

examples/agent-loop-ts.sh runs the whole cycle against the same driver
contract as agent-loop.sh: the app is TypeScript in the RAM workspace
(/app/app.ts and its imports); each turn extracts the sources from the
workspace (a tiny guest run reads them out - the host cannot read the RAM
fs), type-checks them, bundles the entry, loads the bundle back into the
workspace as /app/app.js, and runs the driver. A type error ends its turn
with a FAIL line exactly like a failing test - it is the agent's signal
to fix the sources - and a project that type-checks is what gets bundled
and run.

`make check-ts` smoke-tests the loop: it skips with a printed reason when
bun (required) or every checker (npx for tsc-rs, or tsc) is missing -
never a silent pass - and V8VM_REQUIRE_TS=1 makes a missing toolchain
fail.

The loop honors three overrides: BUN selects the bun executable (also
found via PATH or ~/.bun/bin/bun), and V8VM_TSC_RS / V8VM_TSC replace the
two checker commands (each may be several words, for example a pinned
standalone build or a specific tsc); examples/ts-smoke.sh reads the same
variables.

Limits: type-checking and bundling are host-side loop steps, and vm.run
of a .ts file would evaluate raw TypeScript as JavaScript. The compiler
can also run inside the guest itself, though: examples/ts-in-guest loads a
WebAssembly build of ts-rust (its crates/ts_wasm, built for
wasm32-wasip1) from vm.fs, bridges the compiler's eight host-fs
operations onto vm.fs and answers its WASI imports from a ~60-line shim,
so the whole compiler runs under v8vm's timeout and --sandbox (~0.6 s
for a two-file project). --memory does not cap it: the wasm linear memory
is outside the JS heap cap (like every WebAssembly.Memory in v8vm), and a
one-file project peaks near 207 MB of process RSS. Its emit is one file
per module with import/export kept, so vm.run of it throws SyntaxError -
a bundler is still needed to run a multi-file project's emit. The 5.9 MB
wasm artifact is not committed; examples/ts-in-guest/README.md has the
pinned commit, the build recipe and the artifact's SHA-256, and the test
covering it skips loudly until TSC_RUST_WASM points at a build. In-guest
checking remains a spike against an experimental compiler whose wasm ABI
is not a stable interface; the agent loop above is the supported path.

examples/vm.d.ts types the guest API for TypeScript guests - vm and its
fs, net and stdin namespaces, console, the timers, queueMicrotask,
TextEncoder/TextDecoder, atob/btoa, structuredClone and performance - so
a project type-checks against v8vm instead of picking up DOM or Node
types: it assumes "lib": ["esnext"] (which the loop's generated tsconfig
sets), because v8vm is not a DOM environment - the same file declares the
WebAssembly surface (Module, Instance, Memory, compile, instantiate,
validate) that lib esnext lacks. It is compared with the runtime by
tests/cli.rs (vm_dts_members_match_the_runtime): every vm.* member it
names exists in a live guest, and every runtime member is named.

## OS self-sandbox (--sandbox)

For hostile code, `--sandbox` puts kernel-level containment around the run.
Linux x86_64 only (the deny list and the x32 guard are verified on that ABI);
on any other platform, or on a kernel missing the features, v8vm exits 2
without running the guest.

The layers are applied after the host reads the entry source and the preload
file, and before the first run()/serve(). V8 is initialized lazily on the
host thread that the first run()/serve() spawns, so that thread, V8's worker
threads, every guest thread, the watchdog, and the stdin reader all inherit
the layers, and they cover every guest later run in the same process. The
library refuses to apply them once that host thread exists. Inherited
descriptors >= 3 are closed first (anything the caller passed in would be a
channel out of the sandbox), and `PR_SET_NO_NEW_PRIVS` is set; the seccomp
filters are installed with TSYNC.

- Landlock: filesystem access is denied except for two files. `--dump` is
  created without truncation when the sandbox starts (a run that ends before
  the snapshot leaves an existing file untouched, or an empty new file) and
  is the only write the run can perform: the end-of-run snapshot truncates
  and rewrites that one file in place (the atomic temp-file replace of the
  unsandboxed dump cannot create a file here), and even reading it back is
  denied. `--dump`
  must name a regular file: one that cannot be opened for writing at all is
  refused with "cannot prepare dump", and one that opens but is not a
  regular file (a pipe, `--dump=/dev/stdout` with stdout piped) with
  "--dump must be a regular file", because Landlock can only anchor its rule
  on a file or directory inode.
  /dev/urandom is readable because V8 opens it while seeding RNGs. Note what
  Landlock does not mediate: file metadata, file existence, and symlink
  targets stay observable through stat/access/readlink/chdir/statfs (xattr
  calls are denied instead).
- Landlock also scopes the process itself (ABI 6): signals may not be sent
  to processes outside the sandbox, and abstract unix sockets outside it may
  not be connected. This is the layer that stops a same-uid neighbour from
  being signalled at all.
- seccomp-BPF, a deny list where denied calls fail with EPERM: all socket
  calls, exec (execve, execveat, fork, vfork), ptrace and cross-process
  memory access, process comparison and pidfds (kcmp, pidfd_open,
  pidfd_getfd, pidfd_send_signal), privilege and identity changes, mount and
  namespace calls, kernel modules, bpf, perf, io_uring, userfaultfd, keyctl,
  every System V IPC family (shared memory, semaphores, message queues) and
  every POSIX message-queue call (open, unlink, send, receive, notify,
  attributes), memfd, personality, modify_ldt, xattr and other file
  metadata changes, file-event notification (inotify, fanotify), fd-to-fd
  plumbing (sendfile, splice, vmsplice, tee, copy_file_range), file handles
  (name_to_handle_at, open_by_handle_at), and clock setting.
- Cross-process interference is filtered by argument, on top of the scopes:
  kill, tgkill, rt_sigqueueinfo and rt_tgsigqueueinfo are denied unless the
  target is the run's own pid (tkill, which names no process, is denied
  outright); fcntl is denied for F_SETOWN, F_SETOWN_EX and F_SETSIG, and
  ioctl for their FIOSETOWN and SIOCSPGRP spellings, which is how an async fd
  would aim SIGIO at another process (Landlock's Signal scope blocks that
  delivery anyway, so these denials are defense in depth); prlimit64,
  sched_setaffinity, sched_setscheduler, sched_setattr and sched_setparam are
  denied unless the target is 0 (the caller) or the run's own pid;
  setpriority and ioprio_set are denied unless they address a single process
  (PRIO_PROCESS, IOPRIO_WHO_PROCESS) that is 0 or the run's own pid.
- ioctl is also denied for the terminal input injection requests TIOCSTI and
  TIOCLINUX; other ioctl requests remain allowed on already-open descriptors,
  which is a residual surface.
- Process creation is filtered, not just denied: clone succeeds only with
  CLONE_THREAD set and no CLONE_NEW* namespace flag, and clone3 fails with
  ENOSYS so glibc falls back to clone (which is then checked). Threads still
  work; fork and user-namespace escapes do not.
- A small hand-written stacked filter kills the process on any syscall made
  through a different ABI: a non-x86_64 arch value (int 0x80) or a syscall
  number in the x32 range, which would otherwise bypass every deny rule. The
  same filter refuses every syscall newer than the deny list's newest entry
  (any number from fchmodat2, 452, upwards) with ENOSYS, so syscalls a
  future kernel adds cannot bypass the documented denials the way fchmodat2
  and the *xattrat family once did; they fail exactly like unknown numbers.

Everything the runtime needs stays allowed: threads, mmap/mprotect for the
JIT, futex, nanosleep, clocks, getrandom, and reads/writes on already-open
descriptors. The heap cap and the wall-clock timeout work under --sandbox:
the watchdog thread is created after `apply` with clone(CLONE_THREAD) and
needs only futex and nanosleep.
At startup V8 probes /sys/devices/system/cpu/online, /proc/stat and
/proc/self/maps; all are denied and V8 falls back (worker-pool sizing comes
from sched_getaffinity) — /dev/urandom is allowed as above.

What this does not do: it is not a VM. The guest runs as your user inside
your process; kernel bugs, hardware side channels, the stdio descriptors,
and the `--dump` file itself are out of scope. A compromised guest can still
burn CPU until --timeout and read the filesystem's shape around it: stat,
access, readlink, chdir and statfs are not mediated by Landlock, and
getpriority and sched_getaffinity on other processes remain readable. The
process's own pid is a valid signal target by design (raise and abort need
it).

Kernel requirements: Landlock ABI 6 (Linux 6.12, for the filesystem rights
and the signal/abstract-socket scopes) and seccomp with TSYNC. A ruleset
that is not fully enforced is refused rather than degraded. Verified on
Linux 6.17 reporting Landlock ABI 7; older kernels are untested. If Landlock
or seccomp is missing the run refuses to start with exit 2 and a message
naming the layer, and it never falls back to unsandboxed execution.

On a kernel below that (GitHub's hosted runners may be), the sandbox tests
cannot exercise the real layers, so they skip instead of failing:

- the runtime cases in tests/sandbox.rs and the `--sandbox` tests in
  tests/cli.rs query the kernel with `landlock_create_ruleset(NULL, 0,
  LANDLOCK_CREATE_RULESET_VERSION)`, print `SKIPPED (kernel lacks Landlock
  ABI 6): <test>` on stderr, and pass. The CLI tests still check the
  contract that does hold there: exit 2 with a message, guest not run.
- the fail-closed cases, which fake a missing kernel feature, and the
  seccomp-only cases run on every kernel.
- `make check` skips its `--sandbox` line with a printed reason.
- `V8VM_REQUIRE_SANDBOX=1` turns every one of those skips into a failure.
  Set it where the kernel is known to have support; CI leaves it unset.
- `v8vm --sandbox-check` exits 0 when this platform and kernel can sandbox
  (x86_64 Linux with Landlock ABI 6) and 2 with a message otherwise; scripts
  gate their skips on it instead of guessing from a refusal message.

The sandbox test binary takes libtest-style arguments: substring filters
(`--exact` for whole names), `--skip <name>` and `--list`; other flags are
ignored. A filter that matches nothing exits 0 with "0 run", so
`cargo test <name of a cli test>` keeps working; an unfiltered run in which
nothing ran fails.

On other platforms (not Linux x86_64) the sandbox tests compile away, and
`--sandbox` exits 2 with "--sandbox is only supported on Linux x86_64".

## Guarantees and limits

- Guest-reachable process aborts remain (V8 fatal error, observed as
  SIGTRAP, exit 133). Plainly: `const a = []; for (;;) a.push(new
  Array(100).fill(1))` (and the same with 1000-element arrays) aborts the
  process at every cap, and not even the wall-clock timeout is observed,
  because V8 runs its allocation retry there without any interrupt
  checkpoint. Other aborting shapes: a single `.fill` of an array too
  large for V8's raised limit (`new Array(1e8).fill(0)` at any cap; a
  bare `new Array(1e9)` is fine, it is a sparse array), string splitting
  past V8's maximum string size (`'ab'.repeat(2e8).split('')`), and long
  native builtins that ignore termination (`a.length = 2**32 - 1;
  a.fill(0, 0, 2e8)` at small caps).
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
  fatal error (see the aborts listed first above) ends the whole
  process and every guest in it. For hostile code, add `--sandbox`
  (above) and/or run v8vm itself under a container/seccomp profile.
- The heap cap terminates the guest, not the process. --memory=N caps the
  JS heap and, separately, ArrayBuffer memory, each at N MB.
  - JS heap: the guest is stopped (exit 137, one-line report on stderr)
    only when V8 cannot collect its heap below V8's configured limit,
    which is the cap plus max(2 x cap, 32 MiB): 5x the cap at cap 8, 3x
    at cap 32 and above. A guest whose live heap stays below that limit
    completes normally, even when it sits above the cap itself, and
    churn whose collections succeed (multi-megabyte temporaries,
    snapshots, JSON round-trips) never trips it. The enforcement is V8's
    near-heap-limit callback, which marks the termination and raises the
    limit so the pending allocation succeeds and the termination is
    observed in JavaScript.
  - Memory use: peak RSS is a fixed process overhead of about 150 MB
    plus a heap part that grows with the cap for growth shapes.
    Measured peaks: about 150-194 MB at cap 8, about 194 MB at cap 32,
    about 1.67 GB at cap 256. Budget roughly 150 MB plus up to 6x the
    cap; churn stays lower.
  - Virtual reservations scale with the cap too: V8 sizes the
    per-isolate CodeRange from the heap limit (min(512 MB, limit)), so
    the 3x headroom above the cap adds up to ~260 MB of address space
    per isolate at the default --memory; RLIMIT_AS embedders must
    budget for it.
  - Worst case: a native builtin that ignores interrupts can keep
    allocating after the callback fired, up to the raised limit of 3 x
    V8's limit, about 9 x the cap (the raise is `limit_ceiling` in
    src/heap.rs). Past that V8 aborts the process as above.
  - ArrayBuffers: allocations that would cross the cap are refused by
    the counting allocator and surface in the guest as a catchable
    RangeError. Buffers the host builds from Rust bytes
    (TextEncoder.encode, vm.fs.readBytes) draw on the same budget and
    throw RangeError when it refuses them; a structuredClone whose
    buffer copy is refused throws DataCloneError. Garbage buffers are
    collected and the charge retried before a refusal, so churn never
    trips it.
  - The smallest supported cap is 8 MB. Growth that hits V8's structural
    limits (Map size, array length) or string limits fails as an
    ordinary uncaught RangeError (exit 1), as Node does.
  - Residual: object-key growth with primitive values
    (`const o = {}; for (let i = 0;; i++) o["k" + i] = i;`) at large caps
    (measured at 256) never reaches the heap limit - V8's name
    dictionary slows down first - and ends only at the wall-clock
    timeout (exit 124). Keep --timeout set for untrusted code.
- Resizable ArrayBuffers, growable SharedArrayBuffers, and
  WebAssembly.Memory are NOT capped: their pages are committed inside
  V8's sandbox reservation, which does not go through the ArrayBuffer
  allocator (there is no V8 12.9 flag to disable the feature;
  `--no-harmony-rab-gsab` does not exist). A guest can therefore exceed
  --memory with `new ArrayBuffer(n, { maxByteLength })` plus resize or
  `new WebAssembly.Memory({ initial })`; treat --memory as advisory
  where such memory is trusted.
- No ES module resolution yet (keep sources in vm.fs and vm.run them).
