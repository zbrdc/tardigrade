# Serving contract (P1, FU-07/FU-08)

Reason: freeze the request semantics a trusted host (today
`serve_bridge`, later the P2 concurrent bridge) may rely on, before any
pooling exists. Frozen 2026-10-17 from the implemented `--serve` behavior
and docs/SESSION_LIFETIMES.md; the one framing gap (unbounded command
lines) is closed alongside this contract.

## Model

- One guest process serves **one in-flight request at a time** (roadmap
  decision). A request is exactly one serve command line; its reply is
  exactly one JSON reply line.
- Session state (isolate, globals, filesystem, timers, pending host I/O)
  belongs to the session and survives across requests; a request boundary
  does not create a fresh guest. A closed session's completions are never
  delivered to a later one.

## Deadlines (FU-07 decision)

- Every serving deployment sets an explicit per-request deadline:
  `--command-timeout`. The CLI `--serve` default is 30 s;
  `serve_bridge` passes 30 s explicitly.
- The library default `command_timeout_ms = 0` (unbounded) is retained
  for embedding compatibility; a serving layer built on the library MUST
  set it. This is a decision, not an oversight.
- `--timeout` optionally bounds the whole session (default unbounded in
  serve mode); expiry ends the session with 124.

## Framing

- Commands are newline-terminated UTF-8 lines (one trailing CR tolerated);
  the final line may lack a newline.
- **A command line is capped at 8 MiB.** Past the cap the session ends
  like other malformed input - no reply, no execution. 8 MiB leaves room
  for the bridge's 1 MiB body cap JSON-escaped worst-case plus framing.
- Replies are single JSON lines bounded by the run's heap cap; the
  command timeout reply is `{"$error":"command timed out"}`.

## Cancellation and failure

- Deadline expiry interrupts the request, replies the timeout error, and
  recovers the session for later requests. It is not a rollback: host-side
  effects that already started stand, and work the request left pending
  (an awaited promise, a repeating timer) stays part of the session by
  design.
- Only host-level failure ends the session: input that is not valid UTF-8
  or over the line cap, `vm.exit`, the whole-run timeout (124), the heap
  cap (137), an unhandled rejection at a microtask checkpoint (1), an
  uncaught exception from leftover work (1), or the reply consumer going
  away (0).

## Trust boundary

The component holding the real socket (today `serve_bridge`) is trusted
host code: it validates, caps and frames before anything reaches a guest,
and stays bound to localhost until the P2 bridge exists.
