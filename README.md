# tardigrade

tardigrade runs small JavaScript and WebAssembly programs entirely in RAM on
the V8 engine. Each run gets its own V8 isolate, a filesystem that exists
only in process memory, timers, an in-process message network, streaming
stdin, a heap cap and a wall-clock timeout. Guest code has no host APIs: it
cannot open host files or sockets, start processes or make system calls of
its own.

WebAssembly is part of the guest language, so programs compiled from Rust,
C or C++ run under the same wall-clock timeout and sandbox; their linear
memory is not counted against the heap cap. A module's imports, WASI for
example, are supplied by guest JavaScript, not by the host;
examples/ts-in-guest shows a working WASI shim.

It is built for running code you do not fully trust, such as code written by
an AI agent, with all of its state in one place: dump the filesystem to JSON
at the end of a run, load it into the next one, and pin time and randomness
with `--clock=virtual` and `--seed` for repeatable runs. On Linux x86_64
with kernel 6.12 or newer, `--sandbox` adds kernel-level containment
(Landlock and seccomp) around the whole process. The exact limits of the
caps, including WebAssembly memory not counting against the heap cap, are in
[docs/KNOWN_RISKS.md](docs/KNOWN_RISKS.md).

tardigrade is a command-line tool and a Rust library, written on the `v8` crate
(rusty_v8 152, V8 15.2, built with pointer compression).

    $ tardigrade examples/hello.js
    hello from ram
    engine: V8 15.2.124.1-rusty

## Documentation

- [docs/INSTALL.md](docs/INSTALL.md): building tardigrade and its requirements
- [docs/MANUAL.md](docs/MANUAL.md): tutorial, from a first run to the agent loop
- [docs/OPTIONS.md](docs/OPTIONS.md): command-line options, exit codes and environment variables
- [docs/GUEST-API.md](docs/GUEST-API.md): the JavaScript API inside a run, including `vm.fs` and the snapshot format
- [docs/LIBRARY.md](docs/LIBRARY.md): embedding tardigrade in a Rust program
- [docs/SUPERVISOR_CONTRACT.md](docs/SUPERVISOR_CONTRACT.md): running one one-shot guest under an external deadline and process-group reaper
- [docs/SERVING_CONTRACT.md](docs/SERVING_CONTRACT.md): request deadlines, framing, cancellation and state lifetime for serving guests over `--serve`
- [docs/BRIDGE_CONTRACT.md](docs/BRIDGE_CONTRACT.md): the concurrent bridge: worker pool, admission limits and defined overload
- [docs/DEPLOYMENT_ARTIFACT.md](docs/DEPLOYMENT_ARTIFACT.md): the validated-before-execution bundle format and `--bundle`
- [docs/TYPESCRIPT.md](docs/TYPESCRIPT.md): TypeScript apps in the agent loop, and vm.d.ts
- [docs/SANDBOX.md](docs/SANDBOX.md): what `--sandbox` enforces, and what it does not
- [docs/KNOWN_RISKS.md](docs/KNOWN_RISKS.md): guarantees and limits of the caps and the isolation
- [docs/KNOWN_BUGS.md](docs/KNOWN_BUGS.md): open defects
- [docs/CONTRIBUTE.md](docs/CONTRIBUTE.md): development, tests and CI

## License

Apache License 2.0. See [LICENSE](LICENSE).

## Contact

Use [GitHub issues](https://github.com/zbrdc/tardigrade/issues) for problems and
questions, and pull requests for changes.

## Source code

    git clone https://github.com/zbrdc/tardigrade

## Security problems

Do not report suspected vulnerabilities in a public issue. Contact the
maintainer privately first. Read [docs/KNOWN_RISKS.md](docs/KNOWN_RISKS.md)
and [docs/SANDBOX.md](docs/SANDBOX.md) before reporting: they list the known
ways a guest can abort the process or exceed a cap.
