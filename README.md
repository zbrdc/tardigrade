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

    make test    # cargo test --release: unit tests plus all four test targets
                 # (cli, library, stdin_hub, sandbox)
    make clippy  # cargo clippy --release --all-targets, warnings denied
    make check   # build, then scripted smoke checks over the examples
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

    v8vm [--timeout=<ms>] [--memory=<mb>] [--fs-quota=<mb>] [--seed=<u64>] [--preload=<file>] [--dump=<file>] [--serve] [--sandbox] <entry.js or -> [args...]
    v8vm --sandbox-check

- entry "-" reads the program from stdin (then vm.stdin is at EOF)
- "--timeout=<ms>" wall-clock kill (default 60000, 0 disables); exit 124
- "--memory=<mb>" V8 heap cap in MB (default 256)
- "--fs-quota=<mb>" RAM filesystem quota in MB (default 64, 0 disables)
- "--seed=<u64>" deterministic Math.random: two runs with the same seed
  draw identical sequences, different seeds differ. Without it Math.random
  is V8's own (seeded from the OS). The seed overrides Math.random for that
  run only, so several runs in one process each keep their own sequence
- "--preload=<file>" seed vm.fs from a snapshot() JSON dump before the app runs
- "--dump=<file>" write the snapshot to a file when the run ends: normal
  exit, vm.exit (also from a timer, microtask or stdin read), uncaught
  exception, unhandled rejection, timeout, or --serve reaching stdin EOF.
  No dump is written after a V8 fatal out-of-memory abort (the process is
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
- uncaught exceptions and unhandled promise rejections exit 1; usage errors 2
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
Isolation is per isolate within the process: a V8 out-of-memory abort or
other V8 fatal error ends the whole process and every guest in it.

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
literal must set the newer fields (`preload_data`, `fs_quota_bytes`, `seed`), or
use `..Default::default()`.

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

## Guest API

| API | Description |
|---|---|
| console.log/info/warn/error | stdout / stderr |
| setTimeout/setInterval/clearTimeout/clearInterval | timer id semantics as usual |
| queueMicrotask(fn) | Promise-based microtask |
| vm.version / vm.args | engine version, arguments after the entry path |
| vm.exit(code) | stop the program with code |
| vm.run(path) | load-and-eval a file from the RAM fs |
| vm.net.port() | fresh port number |
| vm.net.listen(port, (payload, from) => ...) | receive messages on a port |
| vm.net.send(fromPort, toPort, payload) | queue a message; throws if no listener |
| vm.net.close(port) | stop listening |
| vm.stdin.read() | Promise of the next stdin chunk; "" at EOF |
| TextEncoder() | encode / encodeInto, UTF-8 only. Unpaired surrogates
 encode as U+FFFD (EF BF BD); encodeInto fills a Uint8Array (its window,
 not the whole buffer), returns {read, written} and never writes a partial
 code point; a detached destination has zero capacity |
| TextDecoder([label][, {fatal, ignoreBOM}]) | decode, UTF-8 only (utf-8/
 utf8/unicode-1-1-utf-8 labels; anything else throws RangeError). Invalid
 bytes become U+FFFD, one per maximal ill-formed subsequence; fatal makes
 them a TypeError; a leading U+FEFF BOM is stripped unless ignoreBOM, also
 when it arrives split across stream:true chunks; {stream: true} carries
 an incomplete trailing sequence (or a possible BOM prefix) to the next
 call; a throwing decode leaves the state as it was before the call |
| atob / btoa | Latin-1 base64 per spec: btoa accepts code units 0..0xFF
 only, atob decodes forgiving-base64 (ASCII whitespace ignored, padding
 only at the end); both throw Errors named InvalidCharacterError |
| structuredClone(value[, options]) | V8's own structured clone (Map, Set,
 Date, RegExp, typed arrays, cycles, shared references inside the clone).
 Functions, symbols and WebAssembly.Module throw DataCloneError; there is
 no transfer list ({transfer: [...]} throws DataCloneError),
 SharedArrayBuffer is not cloneable here, and symbol-keyed properties are
 dropped, as the spec's clone algorithm dictates |
| performance.now() | milliseconds since process start on the real clock
 (a monotonic Instant); values are comparable across runs in one process |
| Math.random under --seed | splitmix64 draws, 53 bits scaled to [0, 1),
 per run (see --seed above) |

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
the canned edit for your LLM call) patch the workspace, rerun. Swap in
tsc-rs at this boundary to let agents write TypeScript.

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
JIT, futex, clocks, getrandom, and reads/writes on already-open descriptors.
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
  fatal error (for example the heap-cap abort below) ends the whole
  process and every guest in it. For hostile code, add `--sandbox`
  (above) and/or run v8vm itself under a container/seccomp profile.
- Hitting the heap cap aborts with the V8 out-of-memory report.
- No ES module resolution yet (keep sources in vm.fs and vm.run them).
