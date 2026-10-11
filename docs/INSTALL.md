# Installing tardigrade

tardigrade is built from source with cargo. There are no release binaries.

## Requirements

- A Rust toolchain. CI builds with Rust 1.98.0 (`.github/workflows/ci.yml`);
  the crate uses edition 2021.
- Network access on the first build. The `v8` crate downloads a prebuilt
  static V8 library from the rusty_v8 GitHub releases and caches it in the
  target directory (`target/release/gn_out`). Later builds reuse it.
- `make` and `bash` for the Makefile targets and the example scripts
  (optional; plain cargo works too).

tardigrade is developed and tested on Linux x86_64. The `v8` crate also ships
prebuilt V8 for other platforms, but tardigrade is not tested there, and
`--sandbox` exists only on Linux x86_64 (see [SANDBOX.md](SANDBOX.md)).

## Build

    make build

or, without make:

    cargo build --release

This produces two binaries in the cargo target directory (`target/` in the
checkout by default, ignored by git):

- `target/release/tardigrade`: the runtime
- `target/release/serve_bridge`: the HTTP bridge described in
  [MANUAL.md](MANUAL.md#an-http-front-end)

To check that the build works, run the smoke checks over the examples:

    make check

It builds first, then runs each example and prints `OK`.

## Build options

- `CARGO_TARGET_DIR` moves all build output elsewhere. Set it as an
  environment variable or a make variable.
- `CARGOFLAGS` adds flags to the make targets' cargo invocations, for
  example `make build CARGOFLAGS=--locked` to build with the committed
  `Cargo.lock`, as CI does.
- `CARGO` selects the cargo binary (default `cargo`).
- `RUSTY_V8_ARCHIVE` and `RUSTY_V8_MIRROR` are read by the `v8` crate's
  build script: the first names a local path or URL of the prebuilt V8
  archive, the second replaces the download host. Use them to build
  without access to GitHub.

## Using the library

To embed tardigrade in a Rust program, depend on this crate (the package name is
`tardigrade`) and see [LIBRARY.md](LIBRARY.md).
