# Runtime follow-up TODO

Reason: track remaining defects, evidence gaps and explicitly optional extensions
after the combined runtime landing at `8b78f87`. This is a work queue, not a
claim that every design limitation can or should be removed.

Platform direction and priority order (supervisor and serving before
concurrency and deployment) are recorded in docs/PLATFORM_ROADMAP.md,
owner decision of 2026-10-17.

## Authority and completion rules

- Parent owns priorities, APIs, accounting, lifecycle and security contracts.
- Workers use `openai-codex/gpt-6-luna:xhigh`; each thread gets its own fresh
  `openai-codex/gpt-6.1-sol:high` PASS/FAIL judge before acceptance/integration.
- One writer per isolated worktree; exclusive edit allowlists. Read-only
  investigations may inspect shared seams but do not change them.
- No speculative runtime fixes: reproduce, map callers, freeze the smallest
  complete contract, implement, prove causal regression tests, then review.
- Preserve public defaults unless a separately frozen task changes them.
  A command timeout is not rollback or cancellation of pending session I/O.
- No downloads, installs, new resolved packages, full V8 builds, external agent
  CLIs, push or publication. Ask before any prerequisite outside this boundary.
- NVMe scratch/targets only, Cargo jobs=2 and offline; stop below 8 GiB free.
  Never run unbounded OOM probes or alter host-wide limits, signals or fd flags.
- Mark complete only after evidence and review. Record intentional limitations
  separately; do not turn an omitted feature into a defect without a decision.

## P0/P1: execution robustness and containment

| ID | State | Outstanding item and acceptance requirement |
|---|---|---|
| FU-01 | Investigation PASS; fix open | Both blocked stdout and stderr reproduced with bounded owned children. Keep the in-process limitation open; no global fd changes, unbounded queues or detached blocked writers. Freeze CLI/library/serve semantics before code. |
| FU-02 | P0 reaping gate landed; rest open | External deadline/reaping landed via `tardigrade-supervise` and the sidecar parent-death signal (docs/SUPERVISOR_CONTRACT.md): hung, crashed or OOM-killed guests are reaped and the net-sidecar cannot survive the guest. Still open: bridge pipe waits and fatal-path dumps remain unguaranteed (a SIGKILLed run still produces no dump). No unsafe crash probes on shared infrastructure. |
| FU-03 | Landed on main; envelope open | Fail-closed Linux monitoring landed as `eb78add` (rebase of privately reviewed `de11a66`, tree `2d8a40f`). Requested monitoring fails closed with 70 if sampling is unavailable; no permissions were added or in-cage measurement claimed. Integrated gates passed on the rebased tree: fmt, full offline test suite, release clippy `-D warnings` and `make check`. Broader Wasm/host/snapshot memory envelopes remain open. |
| FU-04 | Roadmap P0: landed | External supervisor contract frozen from live evidence and implemented as `tardigrade-supervise` plus the sidecar `PR_SET_PDEATHSIG`; regression tests prove sidecar death with a SIGKILLed guest and deadline/group enforcement (docs/SUPERVISOR_CONTRACT.md). Scope stayed one offline one-shot CLI guest: no library guarantee or default changed. Serve lifetime, descendants, output policy and OS bounds remain FU-07/FU-08 work. |
| FU-05 | Needs current evidence | Oversized preload diagnostics: an older runner probe reported allocation abort rather than clean quota refusal. Revalidate safely against this baseline with bounded synthetic data before any parser/accounting change. |
| FU-06 | Corrected evidence PASS; inconclusive | The invalid static `InitializeICU()` remedy was withdrawn. The offline native diagnostic trapped in allocator initialization before any Intl result; it proves no native capability or restoration. Hold the diagnostic outside integration. No stub removal, guessed data, binding or repeated fatal probe. |
| FU-07 | Roadmap P1: contract frozen (docs/SERVING_CONTRACT.md) | Decision: retain the library `command_timeout_ms = 0` default for embedding compatibility; every serving deployment MUST set an explicit deadline (CLI serve default 30 s, `serve_bridge` passes 30 s). Framing now bounded: command lines capped at 8 MiB; past-cap input ends the session like other malformed input. Cancellation stays interrupt-and-recover, no rollback; pending timers and I/O intentionally survive command timeout. |
| FU-08 | Roadmap P1: contract frozen; productionize at P2 | `serve_bridge` stays a localhost demonstration until the P2 concurrent bridge; the contract it must satisfy is now frozen (docs/SERVING_CONTRACT.md): one in-flight request per guest process, explicit deadlines, one JSON reply per command, predictable state lifetime per docs/SESSION_LIFETIMES.md. Concurrency via multiple sandboxed processes (FU-35), not multiple isolates in one process. |

## Validation and maintenance

| ID | State | Outstanding item and acceptance requirement |
|---|---|---|
| FU-09 | Landed on main | Baseline formatting landed as `1bf9764` (rebase of reviewed tree `955e56bc3c6e54133ce5d72f8bfb39ffdb30cf1d`). Three named test files only; integrated gates re-run and passed on the rebased tree. |
| FU-10 | Inventory PASS; execution open | All seven exact ignored cases and resource bounds are catalogued. Six have child-local bounds; the 540 MiB CLI case lacks a corresponding outer envelope. Do not run/unignore heavy cases without an approved bounded task. |
| FU-11 | Inventory PASS; execution open | Cached TypeScript 5.9.3 exists, but that is not proof the pinned npx fallback resolves offline or its test runs. Verify only with no-fetch prerequisites and a whole-test outer deadline. |
| FU-12 | Artifact blocked | Guest TypeScript/Wasm still lacks a supplied matching `TSC_RUST_WASM` artifact. Its pinned build/hash requirements are recorded; do not fetch/build it or claim a pass without approval. |
| FU-13 | Open | Fresh offline dependency-advisory and bounded fuzz validation where installed tools/databases permit it. Separate actual results from unavailable tooling; normal `make ci` does not run the fuzz workspace. |
| FU-14 | Decision | V8 internal sandbox is not enabled in the current prebuilt engine. Assess a matching sandbox-enabled build and cost only after approval; pointer compression is not that sandbox. |
| FU-15 | Needs current evidence | Reconcile the older NUMA/tty seccomp audit with current filters, documentation and tests. Close with current source/probe evidence if already fixed; otherwise create an owned-child-only regression task. |
| FU-16 | Needs current evidence | Revalidate prebuilt V8 archive integrity and cached `gn_out` trust against the locked crate/CI. Older observations are leads, not a new confirmed vulnerability. No corrupted-archive execution or cache mutation in this inventory step. |
| FU-17 | Open | Audit documentation drift after the V8 upgrade: old version/advisory references and incomplete test-target tables. Change only verified stale statements, not normative security claims. |
| FU-18 | Needs standards evidence | UTF-8 decoder carry state after a fatal streaming error: compare the standard and a current reference implementation before adding assertions or changing behavior. Previously withdrawn BOM assertions must not return by guesswork. |

## Provenance extensions: explicitly not yet promised

| ID | State | Outstanding item and decision/acceptance requirement |
|---|---|---|
| FU-19 | Decision | Reliable build identity: define injected versus derived identity, dirty/source-archive behavior and unknown/null handling. A package version is not a build identity or attestation. |
| FU-20 | Decision | Secret capability-policy identity: define privacy, key ownership/rotation and comparison semantics. Do not add an unkeyed hash that enables credential guessing. |
| FU-21 | Decision | Embedding-library manifest interface. Keep it separate from the completed binary-only flag; do not add public Config fields incidentally. |
| FU-22 | Decision | Optional arguments/stdin/serve-command capture with explicit secret handling, bounds and retention. Current omission is intentional. |
| FU-23 | Decision | Observe and hash the module graph from bytes actually consumed; define dynamic imports, ordering and failure cases. Do not hash an arbitrary directory and call it execution provenance. |
| FU-24 | Decision | Network record/replay journal with bounded storage, redaction, capability authority and a separate versioned format. Current traffic is not recorded. |
| FU-25 | Decision | Replay/compatibility guarantees across engine/build/runtime versions and scheduling. Seed, virtual clock and snapshots alone are insufficient. |

## Network phase-two options: no automatic scope expansion

| ID | State | Optional item requiring its own frozen contract |
|---|---|---|
| FU-26 | Decision | JSON policy-file input, validation limits and authority boundary. |
| FU-27 | Decision | Binary request/response bodies and byte-accounting/encoding rules. |
| FU-28 | Decision | Streaming responses, cancellation, backpressure and cumulative budgets. |
| FU-29 | Decision | Per-capability rate limits with defined clocks and grant ownership. |
| FU-30 | Decision | GET query templates and content-type selection. Current defaults remain opaque bodies and operator-controlled content type. |

## Platform roadmap items (docs/PLATFORM_ROADMAP.md)

| ID | State | Item and acceptance requirement |
|---|---|---|
| FU-34 | Matrix landed | tests/sidecar_security.rs proves against the real sidecar: redirects are returned, never followed (302 with exactly one connection); operator URL credentials never reach any error surface; a privately resolving hostname is refused before any connection (fail-closed resolver). tests/sidecar_lifetime.rs covers sidecar death with the guest; tests/sidecar_frames.rs and tests/sidecar_budgets.rs cover oversized responses and lifecycle-wide byte budgets; tests/cli.rs a_pending_net_request_settles_across_command_boundaries covers cancellation-era settling (pending request survives a command boundary, settles exactly once, no extra replies). Remaining honest limit: real mixed-record DNS rebinding cannot be reproduced offline; the enforced property is the resolver failing the whole resolution on any private answer. |
| FU-35 | Unblocked: P1 frozen, FU-34 landed | Bounded concurrency: multiple independently sandboxed processes, one in-flight request per process, admission limits and a defined overload response. The serving contract is frozen (docs/SERVING_CONTRACT.md) and the sidecar security matrix is landed (FU-34); ready to plan P2. |
| FU-36 | After P2 | Deployment artifact: snapshot, provenance manifest, capability policy, runtime identity and resource budgets validated before execution; mismatch refuses the run. Consumes the FU-19..FU-25 decision outputs; no new format without a frozen contract. |

## Delivery and deliberate limits

| ID | State | Remaining work |
|---|---|---|
| FU-31 | Open | Cross-platform validation is separate from the exercised Linux x86_64/kernel path. Do not claim equivalent OS containment elsewhere. |
| FU-32 | Gate | After accepted changes: integrated native CI, mandatory installed-tool TypeScript, all 16 mandatory sandbox probes, scope/blob/mode audit and reviewed TODO update. Disclose all skips and ignored tests. |
| FU-33 | Operator decision | Version/changelog/release/push. Current combined commit marks the four new Rust Config fields as a compatibility break; no tag or publication has occurred. |

Deliberate boundaries: no Node/browser compatibility promise, hardware VM,
unrestricted fetch/raw sockets/inbound guest networking, automatic session
rollback, guaranteed dump after fatal process death, or sandbox attestation from
a preparation manifest. Virtual Date wrappers also remain observably non-native.
These are not silently scheduled features.

## First-wave ownership and gates

| Thread | Task | Edit authority | Sol must verify |
|---|---|---|---|
| A | FU-01 blocked-output evidence and smallest remedy options | None; source read-only | Bounded owned-child reproduction, callers/lifecycle, no speculative fix. |
| B | FU-02/FU-04 process-boundary contract evidence | None; source read-only | Actual watchdog/child/sidecar lifetimes, exit/dump limits, no overstated guarantee. |
| C | FU-09 formatting | Only the three named test files | Native formatting delta only, exact candidate tree, focused tests and whole fmt. |
| D | FU-10/FU-11/FU-12 validation-debt inventory | None; source read-only | Exact test names/prerequisites and skips, no unbounded or fake validation. |
| E | FU-06 ICU feasibility | None; source read-only | Locked build/data evidence, exact prerequisite, no invented binding/artifact. |

Parent updates this queue and freezes implementation packets from accepted
investigations. Read-only lanes do not acquire later write authority. Larger
decisions are split into new exclusive seams before any implementation starts.

### Current execution checkpoint

- Initial wave stopped on a missing required file-only worker report; no lane
  was accepted or integrated. All worktrees, logs and partial evidence survive.
- C completed its formatter, focused tests, 16 sandbox probes and clippy;
  the unchanged candidate goes directly to a fresh Sol judge, not another writer.
- B completed its report and receives a replacement fresh judge. A completed
  its bounded stdout/stderr reproduction; replacement Luna finishes analysis
  only. D/E replacements use preserved checkpoints because stopped runs are
  not resumable. No probe guests remained at the recovery audit.
- Recovery uses inline Luna handoffs and independent per-lane failure handling.
  Every result still needs an explicit source/evidence-backed Sol PASS before
  parent acceptance. Runtime defects are not fixed by investigation completion.
- Recovery judgments: C/B/A/D PASS; E FAIL for the incorrect ICU remedy.
  Detailed reviews are under the external evidence root `reviews/`.
  The existing Cargo sandbox harness may use this same lane's private NVMe
  `targets/format/tmp` through compile-time `CARGO_TARGET_TMPDIR`; this disclosed
  path exception does not permit shared targets or broader fixture writes.
- Wave 2 RSS evidence passed independent review and confirmed non-enforcement;
  corrected ICU evidence passed only as an inconclusive diagnostic. The active
  RSS safety writer owns the completed formatting cwd/target serially, with a
  separate scratch directory and fresh Sol gate. No public API or sandbox
  permission change is authorized. Original main advanced independently to
  `9aa5e01`; preserve that other work and recheck before any future landing.

## Verification

Evidence and lane state live outside the repository in
`/media/dan/nvme/fifi/scrch/8b78f87/runtime-followups/`.
Verify accepted runtime changes with `make ci CARGOFLAGS=--locked`, then
`TARDIGRADE_REQUIRE_TS=1 make check-ts CARGOFLAGS=--locked`, using the assigned
private scratch/target and `TARDIGRADE_REQUIRE_SANDBOX=1`.
