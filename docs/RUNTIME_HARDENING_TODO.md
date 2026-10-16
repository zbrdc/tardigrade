# Runtime hardening TODO

## Delivery status: hardening and CLI provenance verified

- Original isolated baseline: `e59ecaa825e07708de587a001542ea3d8a6e8a9c`.
- RH-01 through RH-11 are implemented to their approved scope. RH-09 supplied
  the design; RH-11 supplies its intentionally limited CLI implementation.
  Every integrated Luna handoff has independent Sol PASS on its exact tree.
- Verified provenance code commit: `af48432f9149e2285cf6bafd3b098ef1c6fbe2a3`;
  tree: `7f65f46a68a812412502cd5ba9ec85cd3fc68cb8`.
- Final `make ci CARGOFLAGS=--locked`: exit 0; all 16 mandatory sandbox probes
  ran with zero failures/skips. Separate mandatory `make check-ts`: exit 0,
  TS-OK with installed local tools. No package download or install was used.
- New provenance gates passed: model 4 tests, output 12 harness entries
  (including child helpers), CLI 7 end-to-end tests, and actual binary unit
  context. Independent reviewers verified causal failures and exact restoration.
- A real sandboxed CLI smoke passed: source digest, seed/virtual clock,
  new manifest mode 0600, unchanged RAM snapshot v2, and no extra stdio.
- Seven pre-existing tests remain ignored; two optional TypeScript paths skip
  explicitly (pinned fallback unavailable offline; wasm artifact absent).
  These skips are not conformance passes.
- Changed-file formatting passed. Whole-tree `cargo fmt --check` still exits 1
  only on unchanged baseline `tests/library.rs`, `tests/sandbox.rs`, and
  `tests/stdin_hub.rs`; no unrelated formatting cleanup was performed.
- `--run-manifest=FILE` is opt-in and CLI-only. It writes and closes a NEW
  private preparation record before sandbox/guest execution; existing outputs
  and dump aliases are refused. No guest/library API or sandbox exception was
  added. Snapshot v2 and ordinary stdout/protocol behavior stay unchanged.
- Build identity is unavailable/null; enabled secret-policy identity is
  redacted/null. Arguments, guest stdin, serve commands, loaded module graph
  and network traffic are not captured. Hashes reveal equality/guessing
  information: this is not encryption, deterministic replay or attestation.
- The existing locked ring 0.17.14 package was promoted to a direct edge;
  all 132 lock records, keyed by name/version/source, otherwise remain unchanged.
  A producer's incomplete name-only 127-record audit was independently corrected.
- One producer command used its own worktree-local Cargo target accidentally.
  Its owned artifacts were removed and that run excluded. Final validations
  used assigned private targets; the incident and ownership evidence remain
  disclosed in `provenance/reviews/model.md`.
- Verified feature branch: `task/run-provenance`. Final delivery target:
  `task/runtime-hardening` in
  `/media/dan/nvme/fifi/scrch/89a2563/runtime-hardening/integration/`.
  At that isolated handoff, main and the original runtime checkout were left
  unchanged. Subsequent local landing includes the pre-existing runtime, CI,
  fuzzing and documentation changes with explicit approval to commit them
  together. The original snapshot and index are retained for recovery under
  `landing/`; main advances only after local validation and final review.
  No remote push, tag or publication is authorized by this local landing.
- Evidence root: `/media/dan/nvme/fifi/scrch/89a2563/runtime-hardening/`.
  See `manifest.json`, `provenance/reviews/` and
  `provenance/scratch/integration/final-{ci,typescript,fmt}.log`.
- Workerd was research inspiration only; no source/tests were copied and no
  workerd dependency was added. No Pi installation repair, alternate agent CLI,
  remote push or main-branch merge was used.

## Purpose and authority

Use workerd only as inspiration research. Do not copy its source, tests, bindings,
configuration format, or architecture. Do not add workerd as a dependency.
Implement tardigrade-native changes in Rust and preserve the existing guest API,
offline default, Landlock/seccomp boundaries, and RAM-filesystem snapshot format.

This is a parent-planned multi-seam effort. Each worker implements one bounded
task; workers do not choose architecture, expand scope, or plan later work.
Every implementation gets its own fresh-context Sol 6.1 judge before integration.

## Execution policy

- Worker: latest Luna registered at dispatch, currently
  `openai-codex/gpt-6-luna`, thinking `xhigh`.
- Judge: `openai-codex/gpt-6.1-sol`, thinking `high`.
- One writer per isolated worktree. Review runs only after that writer stops.
- Preserve the original checkout, index, and pre-existing uncommitted changes.
  Freeze a separate baseline snapshot; never treat that snapshot as newly authored work.
- Maximize independent lanes, not overlapping edits. Shared source seams run
  sequentially; separate new integration-test files may run concurrently.
- Each lane has private NVMe scratch and Cargo output, with Cargo build jobs = 2.
  Never share a writable Cargo target across worktrees or run unbounded OOM probes.
- No implementation may merge before its paired judge returns PASS for that exact
  candidate. FAIL means hold; the parent specifies a smaller correction task.
- PASS may bind to a verified content-addressed tree before commit. No extraneous
  edits or unrestored mutations are allowed. After the judge stops, the parent
  must verify the eventual commit has exactly that tree; any mismatch needs review.
- Parent inspects the diff and evidence, integrates accepted changes serially,
  and runs `make ci CARGOFLAGS=--locked` on the integrated tree.
- No remote push, publication, broad cleanup, or dependency installation.

## Gate common to every implementation

Read the task's sources and all callers first. Add the lowest practical test.
Record the exact command, exit status, and test count. Prove each new behavior
test is causal with a temporary, narrowly scoped mutation; restore it before
handoff. New regression tests may instead show a reproducible failure on the
baseline and success on the candidate. Never mislabel baseline/environment
failure as an implementation failure or a pass.

Use lane-local logs for long output. Run `cargo fmt --check` only after noting
baseline formatting status; do not format unrelated baseline source.
The final integrated gate must include release tests, clippy, and smoke checks.

## Wave 1: independent ownership

### RH-01 — Fail closed when a watchdog cannot start

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: `src/watchdog.rs`; the watchdog-construction call sites in
`src/lib.rs` and `src/serve.rs`; focused watchdog tests in those files.
No other behavior changes.

Make run and command watchdog construction fallible. A spawn failure must not
evaluate the guest entry or command under a silently disabled safety limit.
Use the existing software-failure exit convention (70); emit a bounded
diagnostic without guest data. Preserve normal timeout recovery, first-wins
termination, and join-before-cancel ordering. A disabled command deadline remains
a valid no-op. Add private test-only failure injection; no public knob or
production environment-variable backdoor. Test both spawn failures and the
disabled-deadline case.

Validation: release watchdog unit tests plus the existing serve-timeout tests;
all callers compile. Judge must verify failure propagation reaches callers.

### RH-02 — Bound sidecar input before parsing or allocation

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: `src/netcap.rs`; new `tests/sidecar_frames.rs`.
This lane exclusively owns netcap production code during wave 1.

Replace unbounded policy/request line reads with bounded framing. Policy frames
have a 1 MiB maximum; refuse an oversized policy before spawning the sidecar
as well as when receiving it. Request-frame allowance must derive from the
configured total byte budget with saturating JSON-escape/framing headroom.
Include total_bytes in operator policy so the sidecar has that bound.
Do not add flags, dependencies, raw guest sockets, guest URLs, redirects, or
background retry behavior. Overflow and truncated/malformed frames must have
bounded, fail-closed handling; do not resume parsing an oversize frame tail.
Keep response framing bounded.

Validation: exact-boundary and over-boundary cases, EOF without a delimiter,
JSON escaping, valid ordinary requests, and an oversized request sent directly
to the real sidecar without going through the JavaScript binding.

### RH-03 — Declare and check conditional network APIs

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: `examples/vm.d.ts`; the API/type parity test and its immediate
helpers in `tests/cli.rs`. Do not edit unrelated CLI tests.

Declare `vm.net.request(name, body)` with its real input and promised response
shape. Document that availability requires an operator capability grant.
Extend parity checks to default/offline and capability-enabled configurations.
Use a loopback test endpoint only with the explicit private-address test opt-in.
Do not claim TypeScript can statically guarantee which runtime grants exist.
No generator, schema framework, or network API redesign.

Validation: existing parity test, new enabled-capability parity case, and the
native TypeScript smoke check where its documented tools are available.

### RH-04 — Pin persistent-session I/O lifetime and timeout recovery

- [x] Implement regression tests and lifecycle contract.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: new `tests/serve_io_lifetime.rs`; new
`docs/SESSION_LIFETIMES.md`. No runtime source changes in this task.

Decision: the isolate, globals, filesystem, timers, and pending host I/O belong
to the serve SESSION, not automatically to one command. A command timeout
interrupts its execution; it is not rollback and does not undo HTTP side effects.
Preserve existing session persistence. Do not import workerd's per-request
ownership rules wholesale.

Use a controlled loopback server and real CLI child, bounded by an external
test deadline. Show: command A starts a delayed request then times out; a later
command can deliberately await the saved session promise; a normal command
still returns the right value; reply lines are not shifted by late completions;
closing the session does not deliver completions into a fresh run. Use
handshakes/barriers rather than timing-only sleeps.
If the contract fails, report the exact failing case and stop; the parent
will specify the production fix. Do not weaken the test to get PASS.

### RH-05 — Add focused UTF-8 conformance checks

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: new `tests/conformance_utf8.rs` only.
Read `tests/webapi.rs` to avoid duplicating existing cases.

Use small original assertions for every split point through a mixed UTF-8
stream with a BOM, and encodeInto with nonzero destination offsets and a
capacity boundary at a surrogate pair. Existing non-stream/flushing reset
coverage remains authoritative; do not duplicate it.
Parent correction: a fatal streaming error does not require resetting BOM
state (Encoding Standard TextDecoder.decode and Node 22.22.3 agree). Do not
change the runtime to satisfy that erroneous original assumption. A separate
post-error carry-state difference from Node is recorded for standards/browser
validation, not approved as a runtime fix in this test-only task.
Test only supported UTF-8 APIs. Cite the Encoding Standard in test comments;
do not copy workerd or vendor a standards suite.
If existing coverage already proves a case, identify it and add only missing
coverage. A runtime discrepancy is a parent decision, not permission to modify
the implementation.

Validation: this release integration-test target in isolation; causal evidence.

### RH-06 — Add focused forgiving-base64 conformance checks

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: new `tests/conformance_base64.rs` only.
Read existing base64 tests first.

Original table-driven assertions: ASCII whitespace at each legal position;
unpadded 2/3-character tails and forbidden remainder-1 lengths; misplaced and
excess padding; accepted nonzero trailing bits; Latin-1 versus non-Latin-1
btoa input. Assert error names, not engine-specific stack text.
Do not add unsupported APIs or edit production code on a discrepancy.

Validation: this release target in isolation; causal evidence.

### RH-07 — Add focused structured-clone conformance checks

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: new `tests/conformance_clone.rs` only.
Read existing clone tests first.

Original assertions for alias preservation across overlapping typed-array views,
cycles containing Map/Set entries, detached-buffer refusal, and the documented
unsupported transfer/SharedArrayBuffer behavior. Avoid assuming all browser
structuredClone options exist. Prove identity/byte relationships, not just
printed object counts. No production changes in this task.

Validation: this release target in isolation; causal evidence.

### RH-08 — Pin timer and microtask ordering

- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Ownership: new `tests/conformance_scheduling.rs` only.
Read scheduling coverage in `tests/cli.rs` and `tests/webapi.rs` first.

Use virtual time and exact event arrays to test microtask checkpoints between
same-deadline timers, FIFO ties with a timer added inside a callback, and
clearing a repeating timer from its own callback.
Keep the existing tardigrade scheduling contract; do not copy browser clamping
or workerd request-loop semantics. No wall-clock performance assertions.
No runtime edits on a discrepancy; return the failing trace to the parent.

Validation: this release target in isolation; causal evidence.

### RH-09 — Specify reproducible-run provenance without changing snapshots

- [x] Write the bounded design document.
- [x] Sol judge PASS.
- [x] Parent freezes the follow-on implementation packet.

Ownership: new `docs/RUN_PROVENANCE.md` only. This is a specification task,
not permission to add production code or choose a new CLI interface.

Document a versioned companion manifest with tardigrade/build identity, V8
identity, entry/module-graph and preload content identities, seed, clock/epoch,
effective limits, and a redacted capability-policy identity. Never record
URLs, headers, credentials, response bodies, or secrets. An identity hash is
not encryption; explain guessing risks for low-entropy secret-bearing inputs.
Require network record/replay before claiming deterministic network replay;
record/replay itself is out of scope. Keep RAM-fs snapshot v2 unchanged.
Distinguish file-format identity, runtime identity, and semantic compatibility.
List exact future integration/test seams; no compatibility-date machinery.

Validation: source-backed field/availability mapping, example JSON that parses,
and judge inspection for secret leakage, unsupported determinism claims, and
unapproved interface decisions.

## Wave 2: sequential work on shared seams

### RH-10 — Enforce cumulative network budgets in the sidecar

- [x] Parent finalizes packet after RH-02 is reviewed.
- [x] Implement.
- [x] Sol judge PASS.
- [x] Integrate and verify.

Depends on RH-02 and a green integrated first-wave baseline. Ownership:
`src/netcap.rs`, new `tests/sidecar_budgets.rs`, quota-value corrections only
in existing positive-HTTP fixtures in `tests/sidecar_frames.rs`, and budget
accounting/lifetime paragraphs in `docs/NETWORK_CAPABILITIES.md`.
No parallel writer on netcap; no shared-test-framework refactor.

The sidecar independently enforces the operator total request+response byte
budget across requests, including direct protocol clients. Reject a request
before network I/O when its body would exceed the remaining grant; bound response
buffering by both per-response and remaining-total allowances. Preserve named
capabilities and normal non-2xx response handling. Pin exact-boundary,
cross-request accumulation, over-budget zero-egress, and oversized-response
behavior against a real controlled loopback server.

Parent-frozen accounting contract for dispatch:

- Count UTF-8 request/response body bytes, not JSON framing or HTTP headers.
- Check a declared request against the remaining budget before DNS or HTTP I/O.
  Debit an admitted body before forwarding; later DNS/TLS/HTTP failure does not
  refund that debit. An over-budget refusal leaves the remaining grant unchanged.
- Bound response buffering by min(per-response cap, remaining total allowance).
  Successful responses debit their exact body size. Overflow or a partial read
  failure exhausts the remaining allowance; no automatic retry/refund.
- Exact boundaries are allowed, including an empty response after the request
  consumes the last byte. A zero-byte budget is not an unlimited-budget sentinel.
- The sidecar is authoritative independently of the guest's conservative ledger.
  Preserve named capabilities, offline default, and normal non-2xx responses.
- The authoritative ledger belongs to one sidecar/channel grant, not a claimed
  guest-supplied execution ID. CLI runs/serve sessions map to that lifetime;
  embedders sharing the existing process-wide wire share the outer grant.
  Preserve the guest-side per-run ledger. Add no untrusted reset or new API.
- Count the supplied IPC body even for GET, consistently with the existing
  conservative guest binding; response charges use UTF-8 body bytes.

Implementation boundary: one small concrete sidecar-local ledger (remaining
grant and per-response cap), created once outside the request loop. Admit/debit
a known capability's body before resolver/HTTP work. Pass that ledger to the
existing response helper instead of adding a new framework or public module.
Its read allowance is min(per-response cap, remaining); retain the bounded
one-byte overflow probe. Update the two HTTP-status call sites consistently.

Required real-sidecar cases, bypassing the JavaScript binding:

1. Two requests exactly consume a cumulative UTF-8 payload allowance; a third
   nonempty body is refused before reaching the controlled HTTP endpoint.
2. An over-budget body is refused without spending the remaining grant; a later
   within-budget request can still succeed.
3. Exercise each response bound independently (remaining total versus response
   cap), then prove overflow exhausts the grant before another nonempty request.
4. A request may consume the last byte if its response is empty. Zero-byte
   grants allow only zero-byte payloads, not unlimited nonempty bodies.
5. An admitted request whose transport subsequently fails keeps its body debit.
6. A partial response-read failure exhausts the grant; no retry credit appears.
7. Non-2xx response bodies are charged normally and still returned as responses.

Assert exact response/error shape AND observed HTTP request counts/bodies;
another downstream error must not mask forbidden egress. Every child, socket,
server and pipe wait must have a deadline/cleanup guard. Provide causal records
for each new invariant (including both minimum bounds and failure exhaustion),
not compilation failures or changed expected answers. Keep previous framing
proofs; only fix zero quotas in fixtures that legitimately expect HTTP traffic.

### RH-11 — Implement opt-in CLI preparation manifests

- [x] Operator requested continued implementation; parent selects CLI first.
- [x] Parent freezes interface/security and partitions exclusive component owners.
- [x] Each component and the CLI consumer obtains its own fresh Sol PASS.
- [x] Integrate, run full gates, and verify original-checkout preservation.

Historical code baseline: `5a93d6cdb197099532a68a69a7189b00682f9413`.
Work stays isolated; do not merge main or change the original runtime checkout.

#### Frozen interface and authority

- New binary-only option: `--run-manifest=FILE`. Empty FILE and literal `-`
  are usage errors. Existing option/`--` parsing stays unchanged.
- CLI run and serve each produce exactly one initial `phase: "prepared"`
  record. A serve record covers bootstrap inputs, not later commands/state.
- Publish and close the manifest before sandbox application or guest evaluation.
  No new sandbox rule, retained descriptor, guest API, public Config field,
  library metadata API, snapshot change or default-mode I/O is permitted.
- Destination must be NEW: create_new, Unix mode 0600, never truncate/replace
  an existing file, symlink, directory or special file. A trusted output
  directory is required. Write/sync/close must succeed before proceeding.
- Refuse any dump/manifest inode alias, including equivalent spellings and a
  dangling dump symlink resolving to the newly created manifest. Clean up only
  the new file this call still owns (check symlink metadata device/inode).
  Never remove a replacement file or someone else's artifact.
- Preparation/output failure exits 2 before guest execution with a bounded
  static diagnostic. A prepared file is not proof the cage/run later succeeded.
- Read preload once when sandbox OR manifest is requested, and pass those
  exact consumed bytes through Config.preload_data to the library. Otherwise
  preserve existing lazy preload behavior. Entry source is already read once.
- Build Config once, after CLI/serve defaults and unit conversions, before
  serialization. Borrow/clone CLI-owned values as needed; no duplicate defaults.
  All ordinary behavior when the new flag is absent must remain unchanged.

#### Frozen version-1 JSON

No raw source/preload, host/RAM paths, capability names, URLs, headers,
credentials, arguments or stdin/serve payloads are serialized.

- `format: "tardigrade.run-provenance"`, `manifest_version: 1`,
  `phase: "prepared"`, `scope: "cli-run" | "cli-serve-session"`.
- `runtime.tardigrade`: package_version from CARGO_PKG_VERSION;
  build_identity is null (unavailable, not guessed from a version/revision).
  `runtime.v8.engine_version` is V8::get_version(). No invented crate version.
- `snapshot_format: {name: "tardigrade.ram-fs", version: 2}`.
- `inputs.entry: {mode: "script" | "module", sha256: lower-case hex}`,
  digesting exact consumed UTF-8 bytes. `inputs.preload: {present: bool,
  sha256: hex | null}`, digesting exact consumed preload text, not normalized
  state. This does not assert preload validity or capture a loaded module graph.
- `determinism.seed` is supplied u64 or null. `determinism.clock` has kind
  real/virtual and epoch_ms null/actual virtual epoch, respectively.
- `effective_limits`: run_timeout_ms, command_timeout_ms, heap_mb,
  rss_cap_mb, max_output_bytes, fs_quota_bytes, and network null or
  {max_response_bytes, total_bytes, timeout_ms, allow_private}, from effective
  CLI Config. Zero keeps its existing disabled/zero-budget meaning.
- `capability_policy: {granted: bool, identity: null, identity_status:
  "not_applicable" | "redacted"}`. Enabled policy identity is deliberately
  omitted, NOT an unkeyed hash of secret-bearing policy or a misleading label.
- `isolation: {sandbox_requested: bool}` describes a request, not enforcement.
- `coverage: {module_graph: "not_observed", arguments: "omitted",
  guest_stdin: "omitted", serve_commands: "omitted",
  network_traffic: "not_recorded"}`.
- No timestamp/PID/environment data or automatic paths. Same captured inputs
  and config serialize identically. Input hashes reveal equality and permit
  guessing low-entropy secrets: opting in is not redaction or encryption.
  No deterministic replay, semantic compatibility or attestation promise.

#### Exclusive ownership and frozen internal seams

A — model/digests: Cargo.toml/Cargo.lock, new src/run_manifest.rs and
new tests/run_manifest_model.rs. Reuse the already locked ring SHA-256;
no new package/version/download. Direct dependency/lock-root edge only.
Private Inputs<'a> has source: &'a str, preload: Option<&'a str>, serve: bool,
sandbox_requested: bool. Private-to-binary `pub(crate) fn describe(cfg:
&tardigrade::Config, inputs: Inputs<'_>) -> serde_json::Value` implements the
exact mapping above; cfg is already effective CLI configuration.
Staged explicitly for C: test imports the real source with a path module so
actual code executes now. C must register it in main and use it in production.

B — output boundary: new src/manifest_output.rs and
tests/run_manifest_output.rs only. Frozen `pub(crate) fn write_new(path:
&Path, bytes: &[u8], dump: Option<&Path>) -> io::Result<()>`. Standard-library
filesystem handling; no new dependency/permission. Staged explicitly for C
and imported by the component test as above. All descriptors close on return.

D — documentation: docs/RUN_PROVENANCE.md, docs/OPTIONS.md and the manifest
security paragraph only in docs/SANDBOX.md. Describe this exact contract,
privacy/availability limits, prepared lifecycle, destination refusals and
examples. No invented knobs, keys, policy identities or library APIs.

C — integration-only CLI consumer after A/B/D PASS and parent integration:
src/main.rs and new tests/run_provenance.rs. A/B/D have now passed. Ownership
also transfers the three staged-status notices only in docs/RUN_PROVENANCE.md,
docs/OPTIONS.md and docs/SANDBOX.md to C for activation after wiring is green;
their normative schema, privacy and authority contract must not change.
Register both modules,
recognize the flag, reuse exact preload bytes, serialize once with newline,
call output boundary with the actual dump path, then apply the UNCHANGED
sandbox and call the existing run/serve API. No component redesign.

#### Required evidence

- A: exact schema/version/known SHA-256 vector; changed source/preload identities
  and stable serialization; real/unseeded/offline nulls; virtual/seeded/network
  field mapping and no sensitive plaintext. At least one actual causal failure
  for every new test, with exact restoration. No fake runtime/build identity.
- B: restrictive mode under a child-only known umask, descriptors closed;
  existing-file/symlink preservation; dump alias refusals including dangling
  alias and owned-new-file cleanup; partial-write error cleanup in a bounded
  RLIMIT_FSIZE child; inode guard preserves a replacement file. Relevant
  causal mutations per test, all restored. No host-global signal/limit changes.
- C: actual CLI opt-in and unchanged stdout/exit/snapshot v2; option separator
  and empty/- refusal; run and serve effective defaults/one initial record;
  sandbox+manifest+distinct dump success; destination/alias failure before
  guest effects; source-from-stdin and enabled-network redaction; exact preload
  reuse. For reuse use a bounded nonblocking FIFO feeder supplying distinct
  first/second valid snapshots so a reread gives an assertion-visible mismatch,
  not a timing race or an unbounded hang. Mutations per new test.
- Every child/socket/pipe/feeder wait needs a deadline, cleanup and reaping.
  Tests use fresh scratch fixtures and controlled loopback only.
- Component Cargo targets are private and offline, jobs=2; stop below 8 GiB
  NVMe free. Do not repeat accepted evidence or leave temporary mutations.
- Full code gate after consumer PASS: native make ci CARGOFLAGS=--locked,
  mandatory sandbox probes, mandatory installed-tool TypeScript check,
  changed-file formatting, scope/source/index preservation audit. Existing
  unrelated baseline formatting failures and optional skips remain recorded.

## Explicitly not included

Replacing V8 or tardigrade with workerd; copying upstream implementation/tests;
a general browser/Node runtime; unrestricted fetch; nanoservice deployment;
isolate pooling; automatic retries; a new generic limit framework; full WPT
vendoring; compatibility-date infrastructure; cancelling all session timers
after every command; CPU-accounting or performance claims without measurements.
