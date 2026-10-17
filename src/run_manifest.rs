//! Binary-private description of the effective CLI preparation inputs.

pub(crate) struct Inputs<'a> {
    pub(crate) source: &'a str,
    pub(crate) preload: Option<&'a str>,
    pub(crate) serve: bool,
    pub(crate) sandbox_requested: bool,
}

pub(crate) fn describe(cfg: &tardigrade::Config, inputs: Inputs<'_>) -> serde_json::Value {
    let (clock_kind, epoch_ms) = match cfg.clock {
        tardigrade::Clock::Real => ("real", serde_json::Value::Null),
        tardigrade::Clock::Virtual { epoch_ms } => ("virtual", epoch_ms.into()),
    };
    let network = cfg.net.as_ref().map_or(serde_json::Value::Null, |net| {
        serde_json::json!({
            "max_response_bytes": net.max_response_bytes,
            "total_bytes": net.total_bytes,
            "timeout_ms": net.timeout_ms,
            "allow_private": net.allow_private,
        })
    });
    let (preload_present, preload_sha256) = match inputs.preload {
        Some(preload) => (true, serde_json::Value::String(sha256(preload.as_bytes()))),
        None => (false, serde_json::Value::Null),
    };

    serde_json::json!({
        "format": "tardigrade.run-provenance",
        "manifest_version": 1,
        "phase": "prepared",
        "scope": if inputs.serve { "cli-serve-session" } else { "cli-run" },
        "runtime": {
            "tardigrade": {
                "package_version": env!("CARGO_PKG_VERSION"),
                "build_identity": tardigrade::BUILD_ID,
            },
            "v8": {
                "engine_version": v8::V8::get_version(),
            },
        },
        "snapshot_format": {
            "name": "tardigrade.ram-fs",
            "version": 2,
        },
        "inputs": {
            "entry": {
                "mode": if cfg.module { "module" } else { "script" },
                "sha256": sha256(inputs.source.as_bytes()),
            },
            "preload": {
                "present": preload_present,
                "sha256": preload_sha256,
            },
        },
        "determinism": {
            "seed": cfg.seed,
            "clock": {
                "kind": clock_kind,
                "epoch_ms": epoch_ms,
            },
        },
        "effective_limits": {
            "run_timeout_ms": cfg.timeout_ms,
            "command_timeout_ms": cfg.command_timeout_ms,
            "heap_mb": cfg.heap_mb,
            "rss_cap_mb": cfg.rss_cap_mb,
            "max_output_bytes": cfg.max_output_bytes,
            "fs_quota_bytes": cfg.fs_quota_bytes,
            "network": network,
        },
        "capability_policy": {
            "granted": cfg.net.is_some(),
            "identity": null,
            "identity_status": if cfg.net.is_some() { "redacted" } else { "not_applicable" },
        },
        "isolation": {
            "sandbox_requested": inputs.sandbox_requested,
        },
        "coverage": {
            "module_graph": "not_observed",
            "arguments": "omitted",
            "guest_stdin": "omitted",
            "serve_commands": "omitted",
            "network_traffic": "not_recorded",
        },
    })
}

fn sha256(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
