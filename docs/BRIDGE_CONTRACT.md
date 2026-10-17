# Concurrent bridge contract (P2, FU-35)

Reason: freeze the production bridge semantics before implementation,
per the roadmap decision that pooling follows - not shapes - the serving
contract. The guest interface is unchanged from `serve_bridge`: the app
defines `globalThis.handler = (req) => ({status, body, type?})` with
`req = {method, path, body}`.

## Invocation

    tardigrade-bridge [--addr=127.0.0.1:8078] [--workers=N] [--queue-depth=Q]
                      [--command-timeout=MS] -- APP [GUEST_FLAGS...]

GUEST_FLAGS are passed to every worker before the app path (for example
`--allow-call=...` grants). The bridge process is trusted host code.

## Worker recycling

`--max-worker-age=<seconds>` (default 0, disabled) replaces a worker that
is older than the bound **after** it finishes its in-flight request -
never mid-request - with a fresh process group. Recycling loses that
worker's session state exactly like a failure restart; for long-running
deployments it bounds per-worker drift (V8 heaps grow lazily) at the
cost of one cold start per age window.

## Pool and concurrency

- N workers (1..=64), each a `tardigrade --serve --command-timeout=MS`
  child in its **own process group** (P0 supervisor semantics: a restart
  group-kills, so a net-sidecar cannot survive its worker).
- One in-flight request per worker (docs/SERVING_CONTRACT.md); a request
  owns its worker until its reply line is written.
- Worker session state survives across requests **per worker** only. With
  N > 1 the bridge makes no cross-worker state promise: handlers must be
  stateless or tolerate any worker seeing any request. This is the
  documented state lifetime; sticky routing is explicitly not provided.
- A worker that dies mid-request gets that request a 502, is group-killed,
  reaped and replaced with a fresh process (state loss on restart is the
  documented failure semantics).

## Admission and overload

- Each connection is one request (`Connection: close`).
- If no worker is idle and Q requests (default 0) are already waiting, the
  connection is answered immediately with **503 Service Unavailable** and
  `Retry-After: 1`. Overload is a defined answer, never an unbounded
  queue and never a dropped connection.

## Reply mapping (shared with serve_bridge)

- Guest reply `{"$error":"command timed out"}` -> 504; any other guest
  `$error` -> 500; invalid status -> 502; missing status -> 200 with body.
- Request caps as the demo bridge: 1 MiB body (413 past it), 8 KiB lines,
  100 headers, unparsable Content-Length 400, 15 s socket timeouts,
  guest-supplied status/content-type validated before the response line.

## Non-goals

- No TLS, no HTTP/2, no keep-alive, no sticky sessions, no metrics: the
  trusted front (or a later milestone) owns those. Single-tenant,
  self-hosted deployments remain the product boundary.
- No max-request lifetime yet; --max-worker-age covers time-based
  recycling and a request-count bound remains a compatible addition.
