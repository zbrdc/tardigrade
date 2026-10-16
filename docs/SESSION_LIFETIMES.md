# Session and command lifetimes

In `--serve`, one guest session handles multiple command lines. The isolate,
globals, filesystem, timers, and pending host I/O belong to that session; a
command boundary does not create a fresh guest. A later command can therefore
use session state created by an earlier command.

`--command-timeout` limits one command's execution, including waiting for its
returned promise and settling work left after its reply. When that deadline
expires, tardigrade interrupts the command and replies
`{"$error":"command timed out"}`. It recovers the session so subsequent
commands can run, but this is not a transaction rollback: it does not undo an
HTTP request or other host-side effect that has already started.

For example, a command can save a request promise in a global and await it:

```js
globalThis.savedRequest = vm.net.request("service", "payload")
```

If the command times out while the granted request is pending, the promise
remains part of that serve session. A later command may deliberately await
`savedRequest` and receive the eventual result while the session remains open.
An unrelated synchronous command still receives its own reply; the late
completion is not an extra console-protocol reply. Each command line continues
to receive exactly one JSON reply line when it has not ended the session.

Closing the serve session ends its ownership of guest state and pending I/O.
A completion from a closed session is not delivered to a later, fresh run:
that run starts with a new isolate and fresh globals. This lifetime boundary
does not promise rollback at the remote endpoint; a request that reached a
server may have caused its side effect even if its client session closes before
the response arrives.

These guarantees concern session lifetime, not cancellation semantics for an
individual HTTP request. Network capability grants, request limits, and
timeouts are described in [NETWORK_CAPABILITIES.md](NETWORK_CAPABILITIES.md);
the console protocol and its command timeout are described in
[OPTIONS.md](OPTIONS.md#--serve).
