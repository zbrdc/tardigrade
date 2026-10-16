# CLI run-preparation manifest

**Status:** implemented opt-in version-1 CLI preparation records. The contract
covers CLI `run` and `serve` bootstrap only, not library callers.

The opt-in `--run-manifest=FILE` writes one companion JSON record with
`phase: "prepared"` for a CLI run, or one initial record for a serve session.
A serve record describes bootstrap inputs and configuration only; it does not
describe later commands or session state. The record is published and closed
before sandbox application or guest evaluation. It records preparation, not
proof that the sandbox or run later succeeded.

## Version-1 record

The file contains one UTF-8 JSON record followed by one newline, with this
exact field set and labels:

```json
{
  "format": "tardigrade.run-provenance",
  "manifest_version": 1,
  "phase": "prepared",
  "scope": "cli-run",
  "runtime": {
    "tardigrade": {
      "package_version": "0.1.0",
      "build_identity": null
    },
    "v8": {
      "engine_version": "<exact value returned by V8::get_version()>"
    }
  },
  "snapshot_format": {
    "name": "tardigrade.ram-fs",
    "version": 2
  },
  "inputs": {
    "entry": {
      "mode": "script",
      "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    },
    "preload": {
      "present": false,
      "sha256": null
    }
  },
  "determinism": {
    "seed": null,
    "clock": {
      "kind": "real",
      "epoch_ms": null
    }
  },
  "effective_limits": {
    "run_timeout_ms": 60000,
    "command_timeout_ms": 0,
    "heap_mb": 256,
    "rss_cap_mb": 0,
    "max_output_bytes": 67108864,
    "fs_quota_bytes": 67108864,
    "network": null
  },
  "capability_policy": {
    "granted": false,
    "identity": null,
    "identity_status": "not_applicable"
  },
  "isolation": {
    "sandbox_requested": false
  },
  "coverage": {
    "module_graph": "not_observed",
    "arguments": "omitted",
    "guest_stdin": "omitted",
    "serve_commands": "omitted",
    "network_traffic": "not_recorded"
  }
}
```

This schema-shaped example uses the current package version and the SHA-256
of an empty entry string; the `engine_version` value in angle brackets is a
documentation placeholder, replaced in a real record with the exact V8
runtime string. The example uses `scope: "cli-run"`; a serve record uses
`"cli-serve-session"`.

Field mapping is to the effective CLI `tardigrade::Config`, built once after
CLI/serve defaults and unit conversions:

- `runtime.tardigrade.package_version` is `CARGO_PKG_VERSION`.
  `build_identity` is always `null`: no build identity is available, and a
  package version or source revision must not be presented as one.
  `runtime.v8.engine_version` is the string returned by
  `V8::get_version()`. Do not substitute a V8 crate version.
- `snapshot_format` identifies the existing RAM-filesystem snapshot format.
  Snapshot v2 and the default snapshot output remain unchanged.
- `inputs.entry.mode` is `module` when `--module` or the `.mjs` entry suffix
  makes the effective CLI configuration parse it as a module, otherwise
  `script`. `sha256` is lower-case hexadecimal SHA-256 of the exact consumed
  entry UTF-8 bytes. `inputs.preload.present`
  reports whether preload text was supplied; when present, `sha256` is the
  lower-case SHA-256 of the exact consumed preload UTF-8 text, otherwise it is
  `null`. Do not normalize snapshot content before hashing. This does not
  identify a loaded module graph or assert that a preload is valid.
- `determinism.seed` is the supplied u64 or `null`. Clock `kind` is `real` or
  `virtual`; `epoch_ms` is `null` for real time and the effective virtual Date
  epoch for virtual time.
- `effective_limits` contains the effective values, not merely explicitly
  supplied flags: `run_timeout_ms`, `command_timeout_ms`, `heap_mb`,
  `rss_cap_mb`, `max_output_bytes`, and `fs_quota_bytes`. Serve defaults and
  CLI unit conversions are included. Preserve zero's existing disabled or
  zero-budget meaning. `network` is `null` when no capability is granted;
  otherwise it is exactly `{ "max_response_bytes": N, "total_bytes": N,
  "timeout_ms": N, "allow_private": BOOL }` from effective configuration.
- `capability_policy.granted` reflects whether a network capability is
  enabled. For no grant, identity is `null` and status is `not_applicable`.
  For an enabled policy, identity is still `null` and status is `redacted`.
  Do not hash secret-bearing policy inputs or invent an identity, key, or
  label.
- `isolation.sandbox_requested` records the request only; it does not claim
  that the sandbox was applied.
- `coverage` has the exact values shown. Module graph is not observed;
  arguments, guest stdin, and serve commands are omitted; network traffic is
  not recorded.

The record never contains raw source or preload text, host or RAM paths,
capability names, URLs, headers, credentials, arguments, stdin or serve
payloads, a module graph, or network traffic. Source and preload hashes are
opt-in content identities, not encryption or redaction: they expose equality
and enable dictionary/guessing attacks against low-entropy inputs. Use a
trusted output directory and do not treat hashes as permission to include
secrets.
No timestamp, PID, environment data, or automatically selected path is added.

## Lifecycle and destination

`--run-manifest=FILE` is binary-only and applies to CLI `run` and `serve`.
The destination must be a new file: create it exclusively with mode `0600`;
never truncate or replace an existing file, symlink, directory, or special
file. Empty `FILE` and literal `-` are usage errors. A trusted output
directory is required.

The manifest and `--dump` must not name the same inode. Refuse inode aliases
including equivalent path spellings and a dangling dump symlink that resolves
to the newly created manifest. On any preparation/output failure, exit 2
before guest execution with a bounded static diagnostic. Write, sync, and
close the manifest successfully before applying the sandbox or evaluating the
guest. If failure cleanup is needed, remove only the new file still owned by
this call, after verifying its device and inode; never remove a replacement
or another process's artifact. A later sandbox, preload, or guest failure may
leave a valid `prepared` record.

The entry is already read once. When sandboxing **or** manifest output is
requested, read preload text once and pass those exact consumed bytes through
`Config.preload_data`; do not reopen the preload file for manifest hashing or
guest restoration. If neither option is requested, preserve the existing
lazy preload behavior. Build `Config` once after CLI/serve defaults and unit
conversions, then describe it. Close all manifest descriptors before
sandboxing. Do not add a sandbox write rule, retain a descriptor, or create a
new sandbox descriptor exception; existing sandbox and sidecar behavior is
unchanged.

When `--run-manifest` is absent, ordinary CLI behavior and default-mode I/O
remain unchanged. The manifest does not alter the guest API, library API,
snapshot format, or output defaults.

## Limits of the record

This is provenance about selected preparation inputs and effective
configuration, not deterministic replay, semantic compatibility, or
attestation. It omits arguments, guest stdin, serve commands, dynamic module
loads, and external network effects. Network record/replay is not included;
without it, the manifest cannot make a network-enabled run replayable.
File-format identity, runtime identity, and semantic compatibility are
separate claims: matching snapshot or runtime labels do not establish
compatibility or equivalent behavior.
