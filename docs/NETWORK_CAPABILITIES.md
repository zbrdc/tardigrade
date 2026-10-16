# Network capabilities: `vm.net.request(name, body)`

Status: IMPLEMENTED (phase 1) in `src/netcap.rs`, with the deviations
below folded back into this document. One capability = one
operator-declared endpoint. The guest never sees a URL, never opens a
socket, and can only name capabilities the operator granted. Everything
else stays exactly as documented in SANDBOX.md.

Deviations from the original design: the sidecar is a subcommand of the
main binary (`tardigrade net-sidecar`), not a separate binary; the wire
is a socketpair whose parent end `sandbox::apply` is told to preserve
(`keep_fd`), not a fixed fd 3; `--net-policy` (JSON policy file) is not
implemented — repeated `--allow-call` only; PUT is accepted alongside
GET/POST; at most 8 requests may be in flight per run; the sidecar
applies its own Landlock/seccomp profile at startup
(`sandbox::apply_sidecar_profile`) where the kernel supports it; and the
private-address check is enforced by the sidecar's DNS resolver, which
returns only addresses `ureq` may connect to, so DNS rebinding cannot
race it (see §2).

## 1. Goals and non-goals

Goals:

1. Let a guest call specific, operator-declared HTTP endpoints and receive the
   response text, as a promise that fits the existing event loop.
2. Preserve both containment walls: the guest remains syscall-free (seccomp
   still denies every socket call inside the cage), and egress is mediated by
   host/sidecar code the guest cannot influence.
3. Fail closed: no capability declared → no network API at all, byte-for-byte
   today's behavior.
4. Keep the agent loop inside one run: authenticate → call → paginate → post.

Non-goals: raw sockets, inbound listeners, general proxying, guest-supplied
URLs or headers, redirects, TLS client certificates, binary bodies (phase 2),
streaming responses (phase 2).

## 2. Threat model — what the capability must not allow

- **SSRF**: the URL is an operator literal, never guest-supplied. The guest can
  only pick a capability *name* and fill a body.
- **Exfiltration beyond grant**: a capability is a narrow pipe (method + URL +
  size cap); nothing else can cross. Undeclared names are refused before spawn.
- **LAN/metadata reachability**: the sidecar resolves each URL and refuses
  responses only after checking the connected address is public (guard against
  DNS rebinding and /etc/hosts tricks; explicit pinning available per
  capability).
- **Header injection / method smuggling**: headers and method are policy, not
  guest arguments.
- **Resource exhaustion**: per-response and per-grant byte budgets, request
  timeout, response timeout, no redirects, no retries by the host.
- **Sandbox escape**: the guest still cannot make a single syscall; the sidecar
  fd is the one pre-existing channel (see §5).

## 3. Operator surface (CLI)

    --allow-call NAME=URL[,METHOD]     grant one capability (repeatable)
                                       METHOD defaults to POST
    --net-policy FILE                  JSON file granting many capabilities
    --net-max-response BYTES           per-response cap (default 1 MiB)
    --net-total-bytes BYTES            sidecar/channel payload budget (default 8 MiB)
    --net-timeout MS                   per-request timeout (default 30 000)
    --net-allow-private                allow private/loopback addresses
                                       (default: refused; needed for tests)

Refusal is pre-spawn: an unknown name is a guest-visible rejection, and a run
that declares no capability spawns no sidecar at all.

Example:

    tardigrade --allow-call ingest=https://api.example.com/ingest \
               --net-max-response 65536 worker.js

JSON policy file (equivalent, for larger sets):

    { "capabilities": {
        "ingest":  { "url": "https://api.example.com/ingest", "method": "POST" },
        "catalog": { "url": "https://api.example.com/catalog", "method": "GET" }
    } }

## 4. Guest API

    vm.net.request(name, body) -> Promise<{ status: number, text: string }>

- `name` must match a granted capability, else the promise rejects with
  `Error: no such capability: <name>` (the URL is never in the message).
- `body` is stringified with `String()` (like `vm.net.send`). POST/PUT send it
  as the request body; GET does not transmit it, but its supplied UTF-8 bytes
  are still conservatively charged to the payload budget.
- Response `text` is the body decoded as UTF-8 (lossy, like `vm.fs.read`).
- Non-2xx responses are *not* rejections: the guest inspects `status`
  (matches fetch semantics; keeps error handling in the guest).
- Timeouts, cap refusals, and sidecar death reject with distinct, catchable
  errors (§7).

## 5. Runtime architecture — and the fd-3 problem

Under `--sandbox`, tardigrade closes every inherited fd ≥ 3 (SANDBOX.md,
"closes every inherited file descriptor"), so a sidecar socket cannot simply be
inherited. Design:

1. When at least one `--allow-call` is present, tardigrade spawns
   `tardigrade-net` (new helper binary, or a mode of the main binary) *before*
   the sandbox layers are applied and creates a socketpair.
2. The parent keeps one end. The other end is placed at **fd 3** for the guest
   process, and `sandbox::apply` is taught to preserve exactly fd 3 when
   network capabilities are present (close_range(4, ..) instead of (3, ..)).
   This is the only change inside the sandbox layer; a sandbox test must assert
   that fd 3 survives, that `socket()` is still EPERM, and that with no
   capabilities fd 3 is still closed.
3. The guest cannot touch fd 3: it has no raw-fd API. Only the `vm.net.request`
   binding writes to it. Landlock needs no change (writes to open descriptors
   are already permitted; no path is involved). Seccomp needs no change: the
   sidecar, not the cage, owns every socket.
4. The sidecar enforces the policy independently (defense in depth): it accepts
   only requests naming declared capabilities, applies the same private-address,
   size, timeout and redirect rules, and exits when stdin closes. Its remaining
   request+response payload ledger belongs to this sidecar/channel grant and is
   initialized once for that wire. Runs or serve commands sharing the process
   wire share this outer allowance; embedders sharing the wire do too. The
   guest-side per-run ledger remains an additional conservative check, not a
   reset authority for the sidecar.

Why a sidecar at all: the cage must not gain socket syscalls, and the sandboxed
process must not hold the policy (a parser bug there must not bypass it). The
sidecar is a separate, unsandboxed, ~200-line process whose only job is
"validated name/body in → HTTP → validated response out".

## 6. Wire protocol (fd 3, line-delimited JSON)

Request:  {"id": 7, "cap": "ingest", "body": "..."}
Response: {"id": 7, "status": 200, "text": "...", "bytes": 1234}
Error:    {"id": 7, "error": "timeout" | "policy" | "dns" | "network" | "overflow"}

The wire is single-flight (no interleaved request/reply frames); the guest can
issue the next `vm.net.request` after the previous resolves. The sidecar
authoritatively counts UTF-8 request and response body bytes, excluding HTTP
headers and JSON framing, against one cumulative channel-grant allowance.
Admitted request bodies are charged before DNS/HTTP work, including supplied
GET bodies; a refused oversized body leaves credit unchanged. A later DNS,
transport, or HTTP failure does not refund an admitted request. Response reads
are bounded by both the per-response cap and remaining grant; successful
responses debit their exact UTF-8 body size, including normal non-2xx
responses. Overflow or a partial response-read failure exhausts the remaining
grant. Exact limits are allowed, including an empty response after a request
uses the final byte; a zero-byte budget is not an unlimited sentinel. There is
no guest-resettable execution ID or per-isolate authority: CLI runs/serve
sessions map to the existing sidecar lifetime, and embedders sharing the wire
share this outer budget.

## 7. Failure taxonomy (all catchable in the guest)

| error     | cause                                             |
|-----------|---------------------------------------------------|
| policy    | name not granted, request body over remaining grant, private address |
| timeout   | request exceeded --net-timeout                    |
| overflow  | response over either byte bound or partial read failure |
| network   | DNS/TCP/TLS failure at the sidecar                |
| sidecar   | sidecar died or fd broke (pending promises reject)|

Host-side behavior on budget refusal or response overflow: no partial response
is delivered and the run continues (the guest decides what to do). A budget
refusal does not spend credit; overflow and partial response-read failure
exhaust remaining sidecar credit. Admitted request bytes are never refunded
after a later failure.

## 8. Determinism: record and replay (phase 2)

Network breaks byte-identical replay. Restore it by journaling:

    --net-record FILE    append {capability, body-hash, request, response}
    --net-replay FILE    serve from the journal; mismatched body-hash = error

Under `--clock=virtual --seed` + replay, runs are byte-identical again, so
agent traces with network stay reproducible and diffable. The journal is also
the audit log: an operator can see exactly what left the box.

## 9. Security invariants and test plan

Invariants (each becomes a test):

1. No capability declared → no sidecar spawned, no fd 3 kept, `vm.net.request`
   is undefined; behavior identical to today (existing suite must pass).
2. `socket()` remains EPERM inside the cage with capabilities enabled
   (extends tests/sandbox.rs; tardigrade's own audit finding about the deny
   list stays testable).
3. fd 3 survives `--sandbox` only when capabilities are present.
4. Undeclared name → catchable policy error, zero bytes leave the process.
5. Guest cannot discover the URL: it appears in no error, no reply, no dump.
6. Response over either byte bound → overflow error, no partial body delivered,
   remaining sidecar allowance exhausted.
7. Redirect (3xx) → policy error, not followed.
8. Private/loopback address → policy error unless --net-allow-private.
9. Sidecar killed mid-run → pending promise rejects with sidecar error; run can
   still finish; dump still written.
10. Sidecar/channel request+response payload budget is enforced across
    requests, independently of the guest-side per-run ledger.

## 10. Docs to update on implementation

GUEST-API.md (new vm.net.request section), OPTIONS.md (flags), SANDBOX.md (the
fd-3 exception, stated as loudly as the close_range rule it modifies),
KNOWN_RISKS.md (network is egress-scoped, not a general proxy; policy is
operator trust), MANUAL.md (agent-loop example), runner: operator-level
capability flags (never per-call tool parameters; the MCP runner refuses
net-capable runs unless the operator enables them, mirroring TARDIGRADE_RUNNER_MEM).

## 11. Phases

- Phase 1 (this spec): repeated --allow-call, text bodies, one-shot
  request/response, sidecar, caps/timeouts, invariants 1-10, docs.
- Phase 2: JSON policy file, record/replay journals, binary bodies (base64),
  response streaming, per-capability rate limits.
- Explicitly rejected: raw sockets (guest socket syscalls), guest-supplied
  URLs/headers, redirects, inbound listeners.

## 12. Open questions

1. Should GET capabilities support a query-template (e.g. url with {q}
   placeholder filled from the body)? Default: no — keep bodies opaque.
2. Content-Type policy: fixed per capability, or guest-chosen from an
   allowlist? Default: fixed by policy ("application/json" default).
3. Does the sidecar belong in the main binary (`tardigrade net-sidecar`) or a
   separate crate? Default: subcommand, so the security review stays one repo.
