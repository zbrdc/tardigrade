# TypeScript inside the guest (ts-rust as WebAssembly)

`tsc-guest.js` runs the ts-rust compiler — a Rust port of the TypeScript 7
compiler — as a WebAssembly module **inside a v8vm guest**: the compiler
itself runs under v8vm's `--timeout` and `--sandbox`, reads the project
from `vm.fs`, and writes its emit back into `vm.fs`. `--memory` does not
cap it (the wasm linear memory is outside the JS heap cap; see Measured
and Caveats). No host-side Node, npx or bun is involved in the run.

This is a spike made real: it works, with the caveats at the end.

## Run it

    OUT=<path>/v8vm TSC_RUST_WASM=<path>/ts_rust.wasm \
      bash examples/ts-in-guest/run.sh <project-dir> [workspace.json]

`<project-dir>` needs a `tsconfig.json`; tsc runs as `-p .` with `/app` as
its working directory, so `outDir` and `include` are relative to it. The
exit code is tsc's own (2 on type errors, whose lines are printed tsc-style),
and after an emit run the workspace dump (`--dump`) holds `/app/out/...`.

The wasm artifact is deliberately **not committed** (5.9 MB). Build or fetch
it pinned:

- repo: https://github.com/pingdotgg/ts-rust at commit
  `4f72a4960cfd32697d4679ebf2be60374137fa39`
- build: `rustup target add wasm32-wasip1` then
  `CI=1 WASM_OPT=none CARGO_TARGET_DIR=<scratch> scripts/wasm/build.sh <out>/ts_rust.wasm`
  (rustc 1.98.0; `WASM_OPT=none` skips the binaryen pass, so the module is
  slightly larger than the shipped 4.2 MB optimum)
- the artifact this example was verified with:
  SHA-256 `c533ef3d71031fe9c5dfe05af0a897e1cd6f7c8a90c7f5307ac52e0a1d39873f`,
  5 920 032 bytes.

`tests/cli.rs` runs this example end to end when `TSC_RUST_WASM` names an
existing file, and skips loudly otherwise (V8VM_REQUIRE_TS=1 makes the skip
a failure).

## How it works

The module ABI is `crates/ts_wasm` of ts-rust (built for `wasm32-wasip1`):

- exports `memory`, `ts_input(len) -> ptr`, `ts_run() -> status`,
  `ts_output() -> ptr`, `ts_output_len() -> len`;
- imports `ts_host.fs(op, ptr, len) -> i32` and `ts_host.fs_take(ptr)`:
  the entire compiler file system — eight operations (read, stat, readdir,
  realpath, write, append, remove, chtimes) — bridged 1:1 onto `vm.fs`;
- imports a WASI preview1 subset (fd_write, proc_exit, clock_time_get,
  random_get, environ/args queries, fdstat): answered by a ~60-line shim in
  the guest; imports outside the shim answer ENOSYS, which the npm/wasm
  loader does too.

The request is NUL-separated `[cwd, flags, "-p", "."]` with
FLAG_DIAGNOSTICS_JSON, so diagnostics come back as JSON (file, line, column,
code, text) instead of text on fd 1.

## Measured (ts-rust at the commit above, wasm-opt skipped)

- module: 5.9 MB in `vm.fs` (7.5 MB as base64 in the preload snapshot);
  fits the default `--fs-quota=64` easily
- guest peak RSS: ~207 MB total process for a one-file project (the plain
  v8vm overhead is ~150-194 MB). `--memory` does NOT cap this: the wasm
  linear memory sits outside the JS heap cap (like every
  WebAssembly.Memory in v8vm), so a big compile can far exceed it; the JS
  heap side is small, and the run also works at `--memory=16`
- compile+instantiate: ~10 ms; checking a 2-file project: ~0.6 s
- works unchanged under `--sandbox` (the shim is pure JS; no extra
  syscalls) and under `--clock=virtual`

## Caveats

- The compiler runs under v8vm's timeout and `--sandbox`, but not under a
  memory cap: `--memory` governs the JS heap and ArrayBuffers, and the
  wasm linear memory grows outside it (see Measured). Budget host RAM
  accordingly for untrusted projects.
- In-guest emit is one file per module, keeping its `import`/`export`
  statements. v8vm has no ES modules, so `vm.run` of an emitted file that
  has them throws SyntaxError; a bundler is still needed to run a
  multi-file project's emit. (A file without imports/exports emits as a
  plain script and does run.)
- ts-rust is an experimental port that moves fast; the ABI above is its
  crate contract, not a stable interface. Pin the commit and re-verify.
- The `ts_host` bridge makes the compiler's whole file system the guest's
  `vm.fs`: a project can read (and, with emit, write) anything in the RAM
  filesystem, not only `/app`.
- No watch mode, no `-b` build mode (the module exits with an error for
  `--build` with JSON diagnostics).
