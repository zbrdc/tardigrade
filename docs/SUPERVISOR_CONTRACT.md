# External supervisor contract (P0, FU-04)

Reason: frozen from the evidence of 2026-10-17 (bounded probes under
/media/dan/nvme/fifi/scrch/1d00942/p0-supervisor/). Scope is exactly one
offline one-shot CLI guest per supervisor process: no library guarantee, no
serve semantics (those are FU-07/FU-08), no retry policy, no dump promise.

## Evidence the contract is built on

- A wedged guest is stopped in-process by the wall-clock timeout and exits
  124; this works only while the runtime's own threads are alive.
- A guest killed with SIGKILL leaves the net-sidecar alive for up to one
  in-flight `--net-timeout` (measured: 18-19 s survival at a 20 s timeout);
  the sidecar notices the closed wire only at its next read.
- The sidecar is a direct child of the guest and shares its process group,
  so a process-group kill reaps it deterministically. Today that group is
  the launcher's, so an external supervisor must give each worker its own.
- The CLI already reports: 2 usage, 124 timeout, 137 heap cap, 70 RSS
  sampling refused/unavailable.

## Contract

The supervisor (`tardigrade-supervise`) runs one command:

    tardigrade-supervise --deadline=MS [--term-grace=MS] -- CMD [ARGS...]

1. **Spawn**: the worker is started in its own process group
   (`setpgid`). The net-sidecar inherits that group.
2. **Deadline**: `--deadline` bounds total wall-clock time from spawn. On
   expiry the supervisor sends SIGTERM to the worker's process group, waits
   `--term-grace` (default 5 s), then sends SIGKILL to the group.
3. **Reaping**: the worker is waited on; after it exits the supervisor
   sweeps the group until it is empty (SIGKILL, bounded wait). Sidecars and
   any other descendants cannot survive worker termination.
4. **Status**: if the worker finished on its own, its exit status is
   propagated (exit code, or 128+signal). If the supervisor enforced the
   deadline, it exits 124. If the group could not be emptied within the
   bounded sweep, it exits 126 and says so on stderr.
5. **Sidecar death signal**: independent of any supervisor, the net-sidecar
   sets `PR_SET_PDEATHSIG(SIGKILL)`, so the death of the guest process
   (any cause, including SIGKILL) ends the sidecar immediately instead of
   after one in-flight request timeout. The classic fork race is closed by
   checking the parent after setting the signal.

## Non-goals (unchanged limits)

- No guaranteed dump or fatal-path report after SIGKILL (existing limit).
- No restart, backoff or admission policy: those belong to the P1/P2
  serving layers, not this contract.
- No change to library defaults or serve behavior.
