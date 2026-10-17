# Deployment artifact (P3, FU-36)

Reason: one file a deployment can hand to a host, validated before
execution (docs/PLATFORM_ROADMAP.md). Frozen 2026-10-17; the loader is
`tardigrade::bundle` and the CLI surface is `--bundle=<file>`.

## Format

One UTF-8 JSON object (`format: "tardigrade.deploy-bundle"`,
`version: 1`):

- `runtime.v8`: the exact engine string; a mismatch refuses the run.
- `entry`: `{path, source, sha256, module?}` - the guest program. The
  path drives diagnostics and `.mjs` module detection like an entry
  argument; `sha256` is over the UTF-8 `source` bytes.
- `preload` (optional): `{source, sha256}` - a RAM-filesystem snapshot
  in the documented v2 format, hash-checked here and parsed by the
  existing restore path.
- `budgets` (optional): `timeout_ms`, `heap_mb` (>= 8, like
  `--memory`), `command_timeout_ms`.
- `net` (optional): `allow_private`, `timeout_ms`,
  `max_response_bytes`, `total_bytes`, and `calls`: named
  `{url, method?}` entries; URLs must be http(s), methods GET/POST/PUT.
  They are injected as if passed via `--allow-call`, so the sidecar
  policy and every FU-34 boundary apply unchanged.

## Validation-before-execution

`--bundle=FILE` loads and validates the artifact before the sidecar is
spawned, the sandbox applied, or any guest code evaluated. Any failure -
malformed JSON, wrong format/version, sha256 mismatch, foreign runtime
identity, impossible budgets, malformed capability policy - is a usage-
style exit 2 with the reason on stderr. No bundle field can weaken the
sandbox; `--sandbox` still applies as a flag.

## Precedence

Bundle budgets and policy override the matching CLI defaults; explicit
flags still compose (for example `--dump`, `--run-manifest`,
`--sandbox`). One authority per knob: the deployed artifact.

## Non-goals

No signing or attestation yet (FU-19/FU-20 decisions remain open: a
package version is not a build identity); no multi-entry bundles; no
bundle creation command yet - the artifact is authored as JSON or by a
future exporter from a verified run.
