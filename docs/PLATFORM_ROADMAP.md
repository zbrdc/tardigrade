# Platform roadmap: supervisor and serving first

Reason: record the owner direction from 2026-10-17 for turning the sandbox
into a reliably callable service. The priorities and gates below are
decisions, not proposals. Implementation still follows the completion rules
in docs/RUNTIME_FOLLOWUPS_TODO.md: reproduce, freeze the smallest contract,
prove causal tests, then review.

## Identity model (decided)

Three identities are separated immediately, before any concurrency work:

- **Worker** — a deployed program: its code, snapshot/preload state,
  capability policy and resource budgets. Stable across restarts.
- **Process** — a disposable execution container for one worker. The
  supervisor may stop and replace it at any time without changing the
  logical deployment.
- **Request** — one bounded invocation of a worker. It owns a deadline,
  response framing and cancellation, and never outlives its process.

This supports one-shot and persistent worker models without confusing
persistent state with process survival, and lets the supervisor restart a
process without touching the deployment identity.

## Serving model (decided)

- Each guest process serves **one in-flight request at a time**. Concurrency
  comes from multiple independently sandboxed processes, never from multiple
  isolates in one process. This holds while V8's internal sandbox and memory
  accounting remain open (FU-14, docs/KNOWN_RISKS.md).
- The worker/lifetime contract is frozen **before** pooling is implemented.
- The initial product target is self-hosted, single-tenant serverless
  execution behind a trusted network-facing service. No claim of
  Cloudflare-Workers-equivalent security or scale at this stage.

## Priority order

| Priority | Deliverable | Acceptance gate |
|---|---|---|
| P0 | FU-04 external supervisor contract (landed 2026-10-17, docs/SUPERVISOR_CONTRACT.md) | Hung, crashed or OOM-killed guests are reaped; sidecars cannot survive worker termination |
| P1 | FU-07/FU-08 serving contract (landed 2026-10-17, docs/SERVING_CONTRACT.md) | Requests have explicit deadlines, response framing, cancellation and predictable state lifetime |
| P2 | FU-35 bounded concurrency | Parallel requests run with admission limits and per-worker isolation; overload returns a defined response |
| P3 | FU-36 deployment artifact | Snapshot, provenance, capability policy, runtime version and resource budgets are validated before execution |
| P4 | Security hardening (FU-14 and open envelopes) | V8 sandbox evaluation, Wasm memory accounting, aggregate memory envelopes and hostile multi-tenant tests |

P2 starts only after the P1 contract is frozen: implementing pooling first
risks locking in the wrong state and failure semantics.

## Sidecar lifecycle security tests (FU-34, required)

The network sidecar is a critical security boundary, not an implementation
detail. Tests must cover redirects, DNS rebinding, private-network access,
credential forwarding, oversized responses, cancellation, and sidecar
survival after guest termination. Capability budgets apply across the
complete request lifecycle, not just individual wire frames.

## Deployment artifact contents (P3 sketch)

- Guest snapshot/preload state (existing RAM-filesystem dump format)
- Run-provenance manifest v1 (docs/RUN_PROVENANCE.md)
- Capability policy (netcap operator surface, docs/NETWORK_CAPABILITIES.md)
- Runtime identity (engine string) and resource budgets: heap cap, wall
  clock, RSS watchdog
- Validation happens before execution; a mismatch refuses the run.
