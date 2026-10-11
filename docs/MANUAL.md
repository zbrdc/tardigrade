# The tardigrade manual

This manual walks through tardigrade from a first run to an agent loop.
The examples assume `tardigrade` is on your `PATH`; after `make build` it
is in `target/release/`. Run them from the top of the source tree, where
`examples/` is. Every option is described in [OPTIONS.md](OPTIONS.md),
and the guest API in [GUEST-API.md](GUEST-API.md).

tardigrade started from the idea of vercel-labs' just-bash, a simulated
environment with an in-memory filesystem for AI agents, carried over to a
real JavaScript engine through Deno's rusty_v8 bindings.

## A first run

    $ tardigrade examples/hello.js
    hello from ram
    engine: V8 12.9.202.13-rusty

tardigrade runs the file in a fresh V8 isolate and exits when the program has
nothing left to do. The exit code is the program's: 0 when it finishes,
the code it passes to `vm.exit(code)`, or 1 for an uncaught exception.

With `-` as the entry, the program comes from stdin. Arguments after the
entry are in `vm.args`:

    $ echo 'console.log(vm.args.join("+"))' | tardigrade - a b
    a+b

Arguments after the entry cannot start with `-`; see
[KNOWN_BUGS.md](KNOWN_BUGS.md).

## The RAM filesystem

A guest cannot touch the host's files. It gets `vm.fs` instead, a
filesystem that exists only in memory and disappears when the run ends:

    $ tardigrade examples/ramfs-demo.js
    Hello, RAM
    files: /greeting.txt,/note.txt
    after remove: /greeting.txt

To keep the filesystem between runs, write it to a JSON snapshot with
`--dump` and load it into the next run with `--preload`:

    $ echo 'vm.fs.write("/a.txt", "one")' | tardigrade --dump=ws.json -
    $ cat ws.json
    {"dirs":["/"],"files":{"/a.txt":{"text":"one"}},"version":2}
    $ echo 'console.log(vm.fs.read("/a.txt"))' | tardigrade --preload=ws.json -
    one

`--dump` and `--preload` can name the same file. The dump is replaced
atomically, so a failed write leaves the previous one intact. A program can
also load code from its own filesystem with `vm.run("/lib/x.js")`.

## Timers, messages and stdin

The run stays alive while it has pending work: timers, `vm.net` messages
or a pending `vm.stdin.read()`.

    $ tardigrade examples/timers.js
    tick 1
    tick 2
    tick 3
    done after 3 ticks

`vm.net` is a message switch inside the run. Ports are numbers, nothing
leaves the process, and each loop turn delivers one generation of
messages:

    $ tardigrade examples/net-demo.js
    server got: ping
    client got: echo:ping

`vm.stdin.read()` returns a promise of the next chunk of stdin as a
string, and `""` at end of file:

    $ printf 'hello stdin' | tardigrade examples/echo-stdin.js
    stdin bytes: 11

## Limits

Every run has a wall-clock limit (`--timeout`, default 60 seconds, exit
124), a heap cap (`--memory`, default 256 MB, exit 137) and a filesystem
quota (`--fs-quota`, default 64 MB):

    $ tardigrade --timeout=200 examples/spin.js; echo $?
    124

The caps have exact rules and known gaps, described in
[KNOWN_RISKS.md](KNOWN_RISKS.md). Keep `--timeout` set for untrusted code.

## Repeatable runs

`--clock=virtual` replaces the clock the guest sees, and `--seed` makes
`Math.random` deterministic. Under the virtual clock, time advances only
when a timer fires, and the run jumps straight to the next timer instead
of waiting:

    $ echo 'setTimeout(() => console.log(Date.now(), performance.now(), Math.random()), 86400000)' > day.js
    $ time tardigrade --clock=virtual --seed=7 day.js
    86400000 86400000 0.3898297483912715
    real    0m0.103s

The one-day timer fired at once, and every run prints the same line.
`--epoch` sets where the `Date` timeline starts:

    $ tardigrade --clock=virtual --epoch=1700000000000 --seed=7 day.js
    1700086400000 86400000 0.3898297483912715

A guest that touches nothing outside the VM is byte-for-byte reproducible
this way, with one exception: local-time `Date` methods follow the host's
`TZ` (see [KNOWN_BUGS.md](KNOWN_BUGS.md)). The rules for stdin, `vm.net`
and `--timeout` under the virtual clock are in
[OPTIONS.md](OPTIONS.md#--clockvirtual).

## Console mode

`--serve` keeps a program loaded and evaluates one JavaScript expression
per line of stdin, answering each with one line of JSON:

    $ printf 'globalThis.n = 1\nn + 41\n({a: [1, 2]})\nnope.x\n' | tardigrade --serve
    1
    42
    {"a":[1,2]}
    {"$error":"nope is not defined"}

An entry file is optional; when given, it runs first, so it can define
functions for the commands to call. The current limits of this mode
(statements end the session, promises are not awaited, no per-command
timeout) are listed in [KNOWN_BUGS.md](KNOWN_BUGS.md#console-mode---serve).

## An HTTP front end

`serve_bridge` puts a guest behind a real HTTP port. The bridge owns the
socket, and the guest never sees one: each request becomes a call to the
guest's global `handler(req)` over console mode, and the returned
`{status, type, body}` becomes the response.

    $ OUT=target/release/tardigrade target/release/serve_bridge examples/webapp.js 8077 &
    serving examples/webapp.js from tardigrade on http://127.0.0.1:8077
    $ curl http://127.0.0.1:8077/
    <h1>hello from RAM</h1><p>19+23=42</p>
    $ curl -X POST --data '{"xs":[1,2,3]}' http://127.0.0.1:8077/api/sum
    6

`OUT` names the tardigrade binary; without it the bridge runs `tardigrade`
from your `PATH`. The arguments are the guest file (default
`examples/webapp.js`) and the port (default 8077); the bridge listens on
127.0.0.1 only. `examples/webapp.js` also shows WebAssembly running inside
the guest. The bridge is a demonstration: read
[KNOWN_BUGS.md](KNOWN_BUGS.md#http-bridge-serve_bridge) before exposing it
to anything.

## An agent loop

The intended workflow for AI agents keeps a whole project in one snapshot
file. Each turn loads the workspace, runs a driver that reports PASS or
FAIL, and writes the workspace back; the agent edits files between turns.
`examples/agent-loop.sh` shows it with a canned edit in place of the
language model call:

    $ OUT=target/release/tardigrade bash examples/agent-loop.sh
    == seed workspace with a broken app ==
    == turn 1: run tests, observe failure ==
    FAIL: sum([1,2,3]) returned -6, want 6
    == turn 2: agent edits the app (swap for an LLM call) ==
    == turn 3: rerun tests ==
    PASS

The driver, `examples/agent-driver.js`, runs `/app/app.js` from the
workspace with `vm.run`, checks it, and reports with `vm.exit(0)` or
`vm.exit(1)`. The workspace is plain JSON, so the agent can read and edit
it directly, or edit it through another tardigrade run as the example does.

## Running hostile code

tardigrade keeps guests away from the host by giving them no host APIs, but the
guest still runs inside an ordinary process. For code you do not trust,
add `--sandbox` on Linux, which puts Landlock and seccomp around the whole
process:

    $ tardigrade --sandbox-check && tardigrade --sandbox --timeout=5000 --dump=ws.json app.js

Read [SANDBOX.md](SANDBOX.md) for what it enforces and
[KNOWN_RISKS.md](KNOWN_RISKS.md) for what remains.

## Embedding

To run guests from your own Rust program instead of the command line, use
the library: see [LIBRARY.md](LIBRARY.md).
