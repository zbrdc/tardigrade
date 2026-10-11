# TypeScript

tardigrade itself runs plain JavaScript and gains no dependency for TypeScript:
the support lives at the agent-loop boundary, on the host. Two external
tools do the work (both are development tools of the loop, never tardigrade
runtime dependencies, and the guest never sees them):

- bun (1.3.5 tested) bundles the app with
  `bun build <entry> --target=browser --format=iife`. tardigrade has no ES module
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
never a silent pass - and TARDIGRADE_REQUIRE_TS=1 makes a missing toolchain
fail.

The loop honors three overrides: BUN selects the bun executable (also
found via PATH or ~/.bun/bin/bun), and TARDIGRADE_TSC_RS / TARDIGRADE_TSC replace the
two checker commands (each may be several words, for example a pinned
standalone build or a specific tsc); examples/ts-smoke.sh reads the same
variables.

Limits: type-checking and bundling are host-side loop steps, and vm.run
of a .ts file would evaluate raw TypeScript as JavaScript. The compiler
can also run inside the guest itself, though: examples/ts-in-guest loads a
WebAssembly build of ts-rust (its crates/ts_wasm, built for
wasm32-wasip1) from vm.fs, bridges the compiler's eight host-fs
operations onto vm.fs and answers its WASI imports from a ~60-line shim,
so the whole compiler runs under tardigrade's timeout and --sandbox (~0.6 s
for a two-file project). --memory does not cap it: the wasm linear memory
is outside the JS heap cap (like every WebAssembly.Memory in tardigrade), and a
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
a project type-checks against tardigrade instead of picking up DOM or Node
types: it assumes "lib": ["esnext"] (which the loop's generated tsconfig
sets), because tardigrade is not a DOM environment - the same file declares the
WebAssembly surface (Module, Instance, Memory, compile, instantiate,
validate) that lib esnext lacks. It is compared with the runtime by
tests/cli.rs (vm_dts_members_match_the_runtime): every vm.* member it
names exists in a live guest, and every runtime member is named.
