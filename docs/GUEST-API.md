# Guest API

This is the JavaScript environment a tardigrade program runs in: the language as
V8 12.9 implements it (ES2023 plus parts of later editions, such as
`Object.groupBy`, `Promise.withResolvers` and the `Set` methods), including
WebAssembly, `SharedArrayBuffer` and `Atomics`, plus the globals below.

The entry file is compiled as a classic script, not a module: `import`,
`export` and top-level `await` are syntax errors. Wrap asynchronous code in
an async function. Load more code from the RAM filesystem with `vm.run`.

Not provided: `require`, `process`, `Buffer`, `fetch`, `URL`, `crypto`,
`setImmediate`, `AbortController`, and every other Node or browser API not
listed on this page.

## Globals

| API | Description |
|---|---|
| `console.log`, `console.info` | write a line to stdout |
| `console.warn`, `console.error` | write a line to stderr |
| `setTimeout`, `setInterval`, `clearTimeout`, `clearInterval` | timers with the usual id semantics |
| `queueMicrotask(fn)` | queue a microtask |
| `vm.version` | the V8 version string |
| `vm.args` | the command-line arguments after the entry path |
| `vm.exit(code)` | stop the program with `code` (see below) |
| `vm.run(path)` | load and evaluate a file from the RAM filesystem |
| `vm.net.port()` | a fresh port number |
| `vm.net.listen(port, (payload, from) => ...)` | receive messages on a port |
| `vm.net.send(fromPort, toPort, payload)` | queue a message; throws if nothing listens on `toPort` |
| `vm.net.close(port)` | stop listening on a port |
| `vm.stdin.read()` | a promise of the next stdin chunk as a string; `""` at end of file |
| `vm.fs.*`, `vm.snapshot()`, `vm.restore(json)` | the RAM filesystem, [below](#vmfs) |
| `new TextEncoder()` | `encode(string)`, `encodeInto(string, uint8array)`; UTF-8 only |
| `new TextDecoder([label][, options])` | `decode([bytes][, {stream}])`; UTF-8 only |
| `atob`, `btoa` | Latin-1 base64, per the HTML spec |
| `structuredClone(value[, options])` | V8's own structured clone |
| `performance.now()` | monotonic milliseconds since the run started |
| `Math.random` | V8's own, or splitmix64 under `--seed` |

`console` has exactly these four functions; `console.debug`,
`console.table` and the rest are undefined. Arguments are converted with
ToString and joined with spaces, so objects print as `[object Object]`; a
Symbol, or an object without a `toString`, makes the call throw
`TypeError`.

`vm.exit(code)` converts `code` with ToInt32, so `true` is 1 and `"7"` is 7;
no argument, `null`, and values that convert to `NaN`, ±Infinity or a
multiple of 2^32 give 0. All host effects stop at once: output, filesystem
writes and scheduling. The first exit wins. Pure JavaScript computation may
run briefly until V8 observes the termination. A negative code is currently
ignored; see [KNOWN_BUGS.md](KNOWN_BUGS.md).

`vm.run(path)` evaluates the file's text with an indirect `eval`, so
top-level `var` and function declarations become globals, while `let`,
`const` and `class` stay local to that file. It returns the completion
value of the file.

`vm.net` is a message switch inside the process, not a socket layer:
nothing sent on it leaves the run. `vm.net.port()` hands out numbers
counting up from 1. A payload is converted to a string with `String()`, so
send JSON text for structured data; the listener receives the payload
string and the sender's port as a number. Listening on a port that already
has a listener replaces it. Messages are delivered one generation per loop
turn, so a reply arrives no earlier than the next turn (see
[Event loop](#event-loop)).

`Intl` exists but does not work in the current build: ICU data is not
loaded. `Number.prototype.toLocaleString` and `Intl.NumberFormat` throw
`Internal error. Icu error.`; `Date.prototype.toLocale*String` and
`Intl.DateTimeFormat` abort the whole process (exit 133), and iterating an
`Intl.Segmenter` crashes it with SIGSEGV (exit 139). Do not call them. See
[KNOWN_BUGS.md](KNOWN_BUGS.md).

## Event loop

The process stays alive while guest work is pending: live timers, queued
network messages, or stdin reads. Each loop turn delivers one generation of
network messages, runs due timers, drains microtasks and polls stdin. When
nothing is pending the run ends, so a program that schedules nothing exits
as soon as its top level finishes.

In real time the loop sleeps between turns: up to 50 ms, or 2 ms while a
`vm.stdin.read()` is pending, and never past the next due timer. Queued
`vm.net` messages wait for the next turn, so each message hop can take up
to 50 ms; a request and its reply take about 100 ms.

Under `--clock=virtual` the loop never sleeps to wait for a timer: with no
runnable work it jumps the virtual clock to the next due timer. See
[OPTIONS.md](OPTIONS.md#--clockvirtual).

## Web APIs in detail

### TextEncoder and TextDecoder

- `TextEncoder.encode` returns a `Uint8Array`. Unpaired surrogates encode as
  U+FFFD (EF BF BD).
- `encodeInto(source, destination)` fills a `Uint8Array` (its own window,
  not the whole underlying buffer), returns `{read, written}` and never
  writes a partial code point. A detached destination has zero capacity.
- `TextEncoder` has the `encoding` getter (`"utf-8"`) and a
  `Symbol.toStringTag`.
- `TextDecoder` accepts only the UTF-8 labels (`"utf-8"`, `"utf8"`,
  `"unicode-1-1-utf-8"`, `"unicode11utf8"`, `"unicode20utf8"`,
  `"x-unicode20utf8"`, case-insensitive, with ASCII whitespace trimmed).
  Anything else throws `RangeError`.
- Invalid bytes decode to U+FFFD, one per maximal ill-formed subsequence;
  `{fatal: true}` makes them a `TypeError` instead.
- A leading U+FEFF byte order mark is stripped unless `{ignoreBOM: true}`,
  also when it arrives split across `{stream: true}` chunks.
  `{stream: true}` carries an incomplete trailing sequence over to the next
  call. A call without `stream` ends the stream: the next call starts fresh
  (new BOM decision, no carried bytes), also after a call that threw.
- Options are converted like a WebIDL dictionary: `null` is empty,
  non-objects throw `TypeError`, and getters may throw. Per-instance state
  lives in private fields, so instances have no enumerable properties.

### atob and btoa

- `btoa` accepts code units 0 to 0xFF only.
- `atob` decodes forgiving-base64: ASCII whitespace is ignored, padding is
  allowed only at the end, an unpadded tail decodes to its whole bytes, and
  trailing bits are not checked.
- Both throw an `Error` named `InvalidCharacterError` on bad input, require
  their argument, and throw `RangeError` when the result cannot be allocated
  or exceeds V8's string limit. `atob` validates its whole input before it
  allocates anything, so a malformed unit fails as `InvalidCharacterError`
  even under memory pressure.

### structuredClone

`structuredClone` handles `Map`, `Set`, `Date`, `RegExp`, typed arrays and
their views, `ArrayBuffer`s, cycles, and shared references within one
clone. Functions, symbols, WebAssembly modules and `SharedArrayBuffer`s
throw an `Error` named `DataCloneError`. Transfer is not supported: an
empty `{transfer: []}` is accepted, a non-empty transfer array throws
`DataCloneError`, and options that are not an object, or a transfer value
that is not an Array (a `Set` included), throw `TypeError`. Symbol-keyed
properties are dropped, as the clone algorithm requires.

### performance.now

`performance.now()` reports milliseconds on a monotonic clock, with the
start of the run as zero, so values start near zero in every run and never
go backwards within one. Under `--clock=virtual` it reads the virtual
timeline, which starts at exactly 0 and moves only when the event loop
jumps the clock.

### Memory behavior of these APIs

The encode and decode paths bound their own allocations: decoding reads its
input in pieces sized to the input, `atob` walks the string's units and
fails on the first invalid one before allocating, and every output buffer
grows through `try_reserve`. A memory-constrained embedder therefore sees a
guest `RangeError` instead of a process abort.

V8's own memory is outside that rule. A decode whose result string cannot
exist, a `btoa` result that no longer fits the heap, or a clone whose wire
buffer cannot be allocated ends with V8's own report: a `RangeError` where
V8 can build one, and a fatal out-of-memory abort where it cannot (see
[KNOWN_RISKS.md](KNOWN_RISKS.md#process-aborts)).

Every `ArrayBuffer` these APIs create or clone is charged to the run's
ArrayBuffer budget (`--memory`). A `TextEncoder.encode` result or a
`vm.fs.readBytes` copy that the budget refuses throws `RangeError`, and a
`structuredClone` whose buffer copy it refuses throws `DataCloneError`.
Both are catchable and the run continues.

## vm.fs

A RAM-only tree of directories and files holding bytes.

Paths must be absolute ("path must be absolute") and are normalized: `.`,
`..`, duplicate slashes and trailing slashes collapse. A path that would
climb above the root throws. A path containing NUL throws "path contains
NUL", and one containing a lone UTF-16 surrogate throws "path contains lone
surrogates", because two different guest strings could otherwise name the
same file; a real U+FFFD character is a legal path character. `exists`
returns false for all of these instead of throwing. If a path argument's
`toString` throws, that exception propagates. Writing a file creates its
missing parent directories. The root cannot be removed or renamed.

| API | Description |
|---|---|
| `vm.fs.write(path, data)` | write a file, replacing an existing one wholesale. `data` is a string, an `ArrayBuffer` (shared buffers included), a typed array or a `DataView`. Its length (UTF-8 length for strings) is checked against the quota before anything is copied. A detached buffer or view throws; a view left out of bounds by shrinking a resizable buffer has length zero, as JavaScript shows it, and writes an empty file |
| `vm.fs.read(path)` | read a file as a string; invalid UTF-8 decodes to U+FFFD; a file larger than V8's string limit throws |
| `vm.fs.readBytes(path)` | read a file as a `Uint8Array` |
| `vm.fs.list([dir])` | full paths of all files under `dir` (default `/`), recursively, sorted; directories are not listed. A missing `dir` gives `[]`; a file throws "not a directory" |
| `vm.fs.readdir(path)` | names of the immediate children (files and directories), sorted |
| `vm.fs.mkdir(path, {recursive})` | create a directory; `recursive` also creates parents and accepts an existing directory |
| `vm.fs.exists(path)` | true when the path is a file or a directory |
| `vm.fs.stat(path)` | `{type: 'file' \| 'dir', size}`: content bytes for a file, number of immediate children for a directory |
| `vm.fs.rename(from, to)` | POSIX-like: a file replaces a file, a directory moves onto an empty directory. Mismatched kinds, a move into the source's own subtree, or a missing destination parent throw |
| `vm.fs.remove(path, {recursive})` | delete a file or an empty directory; with `recursive`, a whole subtree |
| `vm.snapshot()` | the whole filesystem as a JSON string, [below](#snapshots) |
| `vm.restore(json)` | load a snapshot into the filesystem |

A mutating call that fails leaves the filesystem unchanged.

### Quota and limits

Usage counts content bytes, plus path bytes, plus 64 bytes of bookkeeping
for each file or directory entry (the root is free). The default quota is 64
MiB (`--fs-quota` counts in MiB, 2^20 bytes); `--fs-quota=<mb>` changes it
and 0 disables it. A write that would cross the quota throws `Error('fs
quota exceeded')` and changes nothing. Normalized paths are limited to 4096
bytes (a rename also checks every descendant it re-keys), and the tree to
100000 entries.

### Snapshots

Snapshots are JSON with sorted keys, so the same filesystem always dumps to
the same bytes. The format is:

    {"dirs":["/","/d"],"files":{"/a.txt":{"text":"..."},"/b.bin":{"b64":"..."}},"version":2}

A file is stored as `{"text": ...}` when its bytes are valid UTF-8 and the
JSON-escaped text is not larger than the base64 form, and as
`{"b64": ...}` otherwise. Base64 is decoded strictly: padding is allowed
only in the last four characters (its position within them is not
checked, so `AA=A` reads like `AAA=`), and trailing bits must be
canonical.

`vm.restore` and `--preload` also accept the older format, a flat
`{"/path": "text"}` map, so files written by earlier versions keep working.
Relative keys in it are read as relative to the root; keys that then
escape the root or exceed the path limit fail with an error naming the key
(shown with a leading `/`).

Restoring:

- creates the parent directories of every restored entry;
- never replaces an existing file with a directory or the reverse;
- fails with "duplicate path in snapshot" when one snapshot names a path
  twice in two spellings (`"/a"` and `"//a"`, or a legacy `"b"` and
  `"/b"`);
- fails with "unsupported snapshot version <v>" for any other `version`
  (the value is echoed, capped at 256 bytes);
- when a quota is set, refuses a payload larger than six times the quota
  plus 1 MiB before parsing it. JSON escapes a control character to six
  bytes, so a filesystem filled to its quota with them still restores from
  its own snapshot.

Version 2 snapshots cannot be loaded by older tardigrade versions, which would
read them as a flat map with files named `version`, `files` and `dirs`. For
the same reason, a legacy flat map containing a file literally named
`version` is read here as a versioned snapshot, not as a file list.
