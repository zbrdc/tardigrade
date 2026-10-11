# Contributing to tardigrade

## Make targets

    make build      # cargo build --release (the default target)
    make test       # cargo test --release: unit tests and the five test targets
    make clippy     # cargo clippy --release --all-targets, warnings denied
    make check      # build, then scripted smoke checks over the examples
    make ci         # test, clippy, then check; stops at the first failure
    make test-heavy # the #[ignore]d memory-heavy tests; not part of ci
    make clean      # cargo clean

`make ci` is the gate: CI runs `make ci CARGOFLAGS=--locked`
(`.github/workflows/ci.yml`). Run it before sending a change. `make check`
needs `bash`, `grep` and `curl`, because it drives the examples end to end,
and it binds TCP port 8077 on 127.0.0.1 for the HTTP bridge
(`examples/serve-demo.sh` reads `PORT` to change it).

The library has one non-default Cargo feature, `test-layers`, which only
this crate's own tests enable (through a dev-dependency on itself): it
exposes each sandbox layer separately so a test can tell which layer
denied a call.

`make test-heavy` runs guests of several hundred megabytes that probe V8's
string limit. Run it on a machine with memory to spare.

The variables `CARGO`, `CARGO_TARGET_DIR` and `CARGOFLAGS` are described in
[INSTALL.md](INSTALL.md#build-options).

## Tests

| Target | Covers |
|---|---|
| unit tests in `src/` | internals: filesystem, snapshot format, stdin hub, clock, world state |
| `tests/cli.rs` | the `tardigrade` binary end to end: options, exit codes, dump and preload, serve |
| `tests/webapi.rs` | the web-platform globals |
| `tests/library.rs` | the Rust API: concurrent runs, heap caps, clocks, preload data, quotas, seeds |
| `tests/stdin_hub.rs` | the process-wide stdin reader (its own binary, `harness = false`) |
| `tests/sandbox.rs` | the `--sandbox` layers, syscall by syscall (`harness = false`) |

Every behavior change needs a test at the lowest practical level. A test
must fail without the change; check that by breaking the code under test
once before you commit.

### Sandbox tests

The sandbox tests need Landlock ABI 6 (Linux 6.12). On a kernel without it,
for example on some hosted CI runners, they cannot exercise the real
layers, so they skip instead of failing:

- the runtime cases in `tests/sandbox.rs` and the `--sandbox` tests in
  `tests/cli.rs` ask the kernel for its Landlock ABI, print
  `SKIPPED (kernel lacks Landlock ABI 6): <test>` on stderr, and pass. The
  CLI tests still check what does hold there: exit 2 with a message, and
  the guest does not run.
- the fail-closed cases, which simulate a missing kernel feature, and the
  seccomp-only cases run on every kernel.
- `make check` skips its `--sandbox` line with a printed reason.

`TARDIGRADE_REQUIRE_SANDBOX=1` turns every one of those skips into a
failure. Set it where the kernel is known to support Landlock ABI 6; CI
leaves it unset. Scripts that need to know in advance use `tardigrade
--sandbox-check`.

The `tests/sandbox.rs` binary takes libtest-style arguments: substring
filters (`--exact` for whole names), `--skip <name>` and `--list`; other
flags are ignored. A filter that matches nothing exits 0 with "0 run", so
`cargo test <name of a cli test>` keeps working, but an unfiltered run in
which nothing ran fails.

On platforms other than Linux x86_64 the sandbox tests compile to nothing.

## Code layout

| Path | Contents |
|---|---|
| `src/main.rs` | the CLI: option parsing, sandbox setup, exit codes |
| `src/lib.rs` | the library entry points `run` and `serve`, and `Config` |
| `src/host.rs` | the dedicated V8 host thread |
| `src/world.rs` | per-run state and the termination reason |
| `src/event_loop.rs`, `src/timers.rs`, `src/net.rs`, `src/stdin.rs` | the event loop and its sources |
| `src/watchdog.rs`, `src/heap.rs` | the wall-clock timeout and the heap caps |
| `src/fs.rs`, `src/fs_api.rs`, `src/snapshot.rs`, `src/dump.rs` | the RAM filesystem, snapshots and the dump file |
| `src/api.rs`, `src/exit.rs`, `src/console.rs`, `src/webapi/` | the guest API |
| `src/prelude.js` | JavaScript run before every guest (virtual clock, wrappers) |
| `src/clock.rs` | real and virtual clocks |
| `src/serve.rs` | console mode |
| `src/sandbox.rs` | Landlock and seccomp |
| `src/bin/serve_bridge.rs` | the HTTP bridge |
| `examples/` | example programs, used by `make check` |

## Documentation

User documentation lives in `docs/`; `README.md` stays short and points
there. When a change alters behavior that a page describes, update that
page in the same commit.
