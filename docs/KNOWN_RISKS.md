# Guarantees and limits

This page states what tardigrade's isolation and caps guarantee, and where they
stop. Read it before running untrusted code. Defects that are expected to
be fixed are listed separately in [KNOWN_BUGS.md](KNOWN_BUGS.md).

## Isolation model

tardigrade is an application-level sandbox, not a container. A guest is isolated
by the absence of host APIs, in its own V8 isolate, inside one ordinary
process:

- Guests make no system calls of their own. The only disk access a guest
  can cause is at the host boundary: reading the entry source and the
  `--preload` file, and writing the `--dump` file. You can check this with
  `strace -f -e trace=openat`: apart from those files, a run of
  `examples/hello.js` opens only dynamic-loader and runtime files at
  process start (`ld.so.cache`, libc, libm, libgcc_s) and V8 internals
  (`/proc/self/maps`, `/dev/urandom`, `/sys/devices/system/cpu/online`).
  None of these is caused by the guest.
- `vm.net` is a message switch inside the process, not sockets. Nothing
  sent on it can leave the run.
- `vm.net.request` (only with `--allow-call`) is scoped egress, not a
  general proxy: the guest names a capability and supplies a body, the
  URL and headers are operator policy enforced again in the
  `net-sidecar` subprocess, redirects are not followed, and the
  sidecar's resolver returns only addresses it may connect to (private
  ranges refused unless `--net-allow-private`), so DNS rebinding cannot
  race the check. The sidecar runs under its own Landlock/seccomp
  profile (`sandbox::apply_sidecar_profile`: resolver-only filesystem
  view, no exec/ptrace/namespaces, sockets allowed) where the kernel
  supports it, and logs a warning where it does not; it remains part of
  the trusted computing base, and a granted endpoint must be treated as
  reachable by design.
- A V8 fatal error ends the whole process and every guest in it (see
  [Process aborts](#process-aborts)).
- The V8 build uses pointer compression but not the V8 sandbox
  (the rusty_v8 152 crate exposes `v8_enable_sandbox`, but its
  prebuilt sandbox artifact is missing upstream, so enabling it means
  building V8 from source). A memory-corruption bug in V8 therefore
  gives the guest the whole address space of the process, not a
  bounded cage.

For hostile code, add `--sandbox` ([SANDBOX.md](SANDBOX.md)), which is the
only kernel-enforced layer, run tardigrade itself under a container or an OS
resource limit, or both.

## Process aborts

A guest can still crash the whole process. A V8 fatal error kills it with
SIGTRAP, which shells report as exit 133; other crashes end in SIGSEGV
(139) or SIGABRT (134). No dump is written. Known shapes:

- `const a = []; for (;;) a.push(new Array(100).fill(1))` (also with
  1000-element arrays) aborts at every cap. Not even `--timeout` is
  observed, because V8 retries the allocation without any interrupt check.
- A single `.fill` of an array too large for V8's raised limit, such as
  `new Array(1e8).fill(0)`, at any cap. A bare `new Array(1e9)` is fine,
  because it is sparse.
- Splitting a string past V8's maximum string size, such as
  `'ab'.repeat(2e8).split('')`.
- Long native builtins that ignore termination, such as
  `a.length = 2**32 - 1; a.fill(0, 0, 2e8)` at small caps.

## Memory

`--memory=N` caps the JavaScript heap and, separately, ArrayBuffer memory,
each at N MiB. Reaching a cap stops the guest, not the process, except
for the shapes listed under [Process aborts](#process-aborts).

### JavaScript heap

The guest is stopped with exit 137 and a one-line report on stderr only
when V8 cannot collect its heap below V8's configured limit. That limit is
the cap plus max(2 × cap, 32 MiB): 5 × the cap at a cap of 8, and 3 × the
cap at 16 and above. A guest whose live heap stays below the limit
completes normally, even when it sits above the cap itself, and churn that
collections can reclaim (multi-megabyte temporaries, snapshots, JSON round
trips) never trips it.

The cap is enforced through V8's near-heap-limit callback, which marks the
termination and raises the limit so that the pending allocation succeeds
and the termination is observed in JavaScript.

Worst case: a native builtin that ignores interrupts can keep allocating
after the callback fired, up to the raised limit of 3 × V8's limit
(`limit_ceiling` in `src/heap.rs`): 15 × the cap at a cap of 8, and 9 × the
cap at 16 and above. Past that, V8 aborts the process as described above.

### ArrayBuffers

An allocation that would cross the cap is refused by the counting
allocator and surfaces in the guest as a catchable `RangeError`. Buffers
the host builds from Rust bytes (`TextEncoder.encode`, `vm.fs.readBytes`)
draw on the same budget and throw `RangeError` when it refuses them; a
`structuredClone` whose buffer copy is refused throws `DataCloneError`.
Unreachable buffers are collected and the charge is retried before a
refusal, so churn does not trip it.

Queued `vm.net` message bytes are charged to the same budget: a send
whose payload would cross the cap throws a catchable `RangeError` in the
sender, and the charge ends when the message is delivered or the run
ends. A send flood therefore cannot grow host memory past the cap.

### Memory that is not capped

Resizable `ArrayBuffer`s, growable `SharedArrayBuffer`s and
`WebAssembly.Memory` do not go through the host's ArrayBuffer allocator,
so the prelude wraps their constructors: reservations (declared
`maximum`/`maxByteLength`, plus memory grown past the declared maximum)
charge a shared ledger bounded by `--memory` and refuse past it with a
catchable `RangeError`, with charges released when objects are
collected. That covers the guest-created shapes —
`new WebAssembly.Memory({ initial })`, `grow`, and
`new ArrayBuffer(n, { maxByteLength })` followed by `resize`. One path
remains outside it: a memory a WebAssembly *module* defines internally
is created by the engine, not by the JS constructor, and growing it
through the module's own exports cannot be intercepted from the
prelude; that path is what `--rss-cap=<mb>` exists for (the watchdog
samples the process RSS and stops the run as a heap-cap hit past it).
Against untrusted code, still treat the in-process caps as one layer
and set an OS limit (for example `prlimit --as`, or a cgroup memory
limit) around the process as well.

Host-side state is not counted against `--memory` either: the RAM
filesystem (bounded by `--fs-quota`, unbounded with `--fs-quota=0`) and
the copies made for snapshots and dumps. The timer table (8192 live
timers) and the `vm.net` listener table (1024 listeners) have their own
hard per-run bounds, because each entry holds a persistent V8 handle.
(Queued `vm.net` bytes are counted; see ArrayBuffers above.)

For hostile code, run one guest per process under an OS bound, for
example:

    prlimit --as=$((2 * 1024 * 1024 * 1024)) --cpu=60 \
      timeout -s KILL 65 tardigrade --sandbox --memory=64 --rss-cap=512 guest.js

or the equivalent cgroup `memory.max`/`cpu.max`. The in-process caps
above cannot substitute for this: they do not cover WebAssembly memory,
resizable buffers, or the abort shapes under [Process aborts](#process-aborts).

### Other limits

- The smallest cap is 8 MB.
- Growth that reaches V8's structural limits (Map size, array length) or
  string limits fails as an ordinary uncaught `RangeError` (exit 1), as in
  Node.
- Object-key growth with primitive values
  (`const o = {}; for (let i = 0;; i++) o["k" + i] = i;`) at large caps
  (measured at 256) never reaches the heap limit, because V8's name
  dictionary slows down first. It ends only at `--timeout` (exit 124). Keep
  `--timeout` set for untrusted code.

### Process memory use

- An idle run uses about 15 MB of resident memory. Guests that grow their
  heap until they are stopped with 137 peaked, in measurements on Linux
  6.17, at 62 to 77 MB with a cap of 8, 124 to 165 MB at 32, and 0.83 to
  1.16 GB at 256. A shape that aborts instead (a `new Array(1000).fill()`
  push loop) went further: 147 MB, 322 MB and 2.43 GB. Budget about 150 MB
  plus 15 × the cap per guest at a cap of 8, and 10 × from 16 up; churning
  guests stay lower.
- Address-space reservations grow with the cap: V8 sizes each isolate's code
  range from the heap limit (min(512 MB, limit)). A process running one
  guest reserves about 0.74 GB of address space at `--memory=8` and about
  1.2 GB at the default 256. Embedders that set `RLIMIT_AS` must budget for
  it per concurrent guest.

## Time

`--timeout` stops the guest after the given wall-clock time, also under
`--clock=virtual`. It is a watchdog inside the process, so it is best
effort: it cannot interrupt the aborting shapes above, which never reach an
interrupt check, or a guest thread blocked in a write to a full stdout or
stderr pipe (see [KNOWN_BUGS.md](KNOWN_BUGS.md)). For hostile code, also
bound the process from outside, for example with `timeout -s KILL`.

## Virtual clock

Under `--clock=virtual`, no `Date` read can observe the host clock:
`Date.now`, the constructor without arguments, `Date()` called as a
function, and `Date` prototype methods on a date built without arguments
all read the virtual clock. This is how:

- The host installs one hidden time callback. The prelude captures it,
  removes it, and rebuilds `Date` over the virtual clock. The original
  `Date` constructor stays reachable only from that prelude closure.
- No object-graph path leads to a constructor bound to the host clock. The
  rebuilt `Date`'s prototype is `Function.prototype`, not the original
  `Date`; its statics (`parse`, `UTC`, neither of which reads a clock) are
  own copies; and every intrinsic it uses at call time is captured with it.
  Replacing globals such as `Reflect.construct` or `Function.prototype`
  methods neither changes what runs nor exposes the original constructor.
- Index accessors that the guest installs on `Array.prototype` or
  `Object.prototype` never run: the constructor's arguments reach
  `Reflect.construct` as the strict arguments object, whose indices are own
  data properties.
- The rebuilt constructor is shaped like the platform's: own `length`,
  `name`, `prototype`, `now`, `parse` and `UTC`, all non-enumerable, with a
  non-writable `prototype` and a correctly named `now` that is not a
  constructor.

Two differences remain detectable, though neither reveals the clock:
`Function.prototype.toString` of `Date` and `Date.now` shows the prelude's
source instead of native code, and constructing a date in virtual mode adds
one stack frame (the wrapper in `prelude.js`) inside `new Date`.
