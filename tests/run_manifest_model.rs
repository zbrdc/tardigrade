#[path = "../src/run_manifest.rs"]
mod run_manifest;

use run_manifest::{describe, Inputs};
use serde_json::{json, Value};

fn inputs<'a>(source: &'a str, preload: Option<&'a str>) -> Inputs<'a> {
    Inputs {
        source,
        preload,
        serve: false,
        sandbox_requested: false,
    }
}

#[test]
fn exact_schema_and_known_sha256_vector() {
    let cfg = tardigrade::Config::default();
    let actual = describe(&cfg, inputs("abc", None));
    let expected = json!({
        "format": "tardigrade.run-provenance",
        "manifest_version": 1,
        "phase": "prepared",
        "scope": "cli-run",
        "runtime": {
            "tardigrade": {
                "package_version": env!("CARGO_PKG_VERSION"),
                "build_identity": null
            },
            "v8": {
                "engine_version": v8::V8::get_version()
            }
        },
        "snapshot_format": {
            "name": "tardigrade.ram-fs",
            "version": 2
        },
        "inputs": {
            "entry": {
                "mode": "script",
                "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
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
            "run_timeout_ms": cfg.timeout_ms,
            "command_timeout_ms": cfg.command_timeout_ms,
            "heap_mb": cfg.heap_mb,
            "rss_cap_mb": cfg.rss_cap_mb,
            "max_output_bytes": cfg.max_output_bytes,
            "fs_quota_bytes": cfg.fs_quota_bytes,
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
    });
    assert_eq!(actual, expected);
}

#[test]
fn changed_source_and_preload_have_distinct_stable_digests() {
    let cfg = tardigrade::Config {
        module: true,
        ..tardigrade::Config::default()
    };
    let first = describe(&cfg, inputs("export const x = 1;", Some("{\"files\":[]}")));
    let repeated = describe(&cfg, inputs("export const x = 1;", Some("{\"files\":[]}")));
    let source_changed = describe(&cfg, inputs("export const x = 2;", Some("{\"files\":[]}")));
    let preload_changed = describe(&cfg, inputs("export const x = 1;", Some("{\"files\":[1]}")));

    assert_eq!(first, repeated);
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&repeated).unwrap()
    );
    assert_eq!(first["inputs"]["entry"]["mode"], "module");
    assert_ne!(
        first["inputs"]["entry"]["sha256"],
        source_changed["inputs"]["entry"]["sha256"]
    );
    assert_ne!(
        first["inputs"]["preload"]["sha256"],
        preload_changed["inputs"]["preload"]["sha256"]
    );
    assert_eq!(first["inputs"]["preload"]["present"], true);
}

#[test]
fn real_unseeded_offline_configuration_uses_null_and_not_applicable() {
    let cfg = tardigrade::Config::default();
    let manifest = describe(&cfg, inputs("", None));

    assert_eq!(manifest["determinism"]["seed"], Value::Null);
    assert_eq!(manifest["determinism"]["clock"]["kind"], "real");
    assert_eq!(manifest["determinism"]["clock"]["epoch_ms"], Value::Null);
    assert_eq!(manifest["effective_limits"]["network"], Value::Null);
    assert_eq!(manifest["capability_policy"]["granted"], false);
    assert_eq!(manifest["capability_policy"]["identity"], Value::Null);
    assert_eq!(
        manifest["capability_policy"]["identity_status"],
        "not_applicable"
    );
}

#[test]
fn virtual_seeded_network_configuration_is_mapped_without_sensitive_plaintext() {
    let cfg = tardigrade::Config {
        timeout_ms: 1250,
        command_timeout_ms: 2200,
        heap_mb: 96,
        rss_cap_mb: 512,
        max_output_bytes: 777,
        fs_quota_bytes: 888,
        seed: Some(42),
        args: vec!["secret-argument-sentinel".to_string()],
        preload: Some("/secret/preload-name".into()),
        dump: Some("/secret/dump-name".into()),
        clock: tardigrade::Clock::Virtual { epoch_ms: 123_456 },
        net: Some(tardigrade::NetConfig {
            names: vec!["secret-capability-name".to_string()],
            max_response_bytes: 1001,
            total_bytes: 2002,
            timeout_ms: 3003,
            allow_private: true,
            wire_fd: -1,
        }),
        ..tardigrade::Config::default()
    };
    let mut supplied = inputs(
        "source plaintext sentinel",
        Some("preload plaintext sentinel"),
    );
    supplied.serve = true;
    supplied.sandbox_requested = true;

    let manifest = describe(&cfg, supplied);
    assert_eq!(manifest["scope"], "cli-serve-session");
    assert_eq!(
        manifest["determinism"],
        json!({
            "seed": 42,
            "clock": {"kind": "virtual", "epoch_ms": 123456}
        })
    );
    assert_eq!(
        manifest["effective_limits"],
        json!({
            "run_timeout_ms": 1250,
            "command_timeout_ms": 2200,
            "heap_mb": 96,
            "rss_cap_mb": 512,
            "max_output_bytes": 777,
            "fs_quota_bytes": 888,
            "network": {
                "max_response_bytes": 1001,
                "total_bytes": 2002,
                "timeout_ms": 3003,
                "allow_private": true
            }
        })
    );
    assert_eq!(
        manifest["capability_policy"],
        json!({"granted": true, "identity": null, "identity_status": "redacted"})
    );
    assert_eq!(manifest["isolation"]["sandbox_requested"], true);

    let serialized = serde_json::to_string(&manifest).unwrap();
    for sensitive in [
        "source plaintext sentinel",
        "preload plaintext sentinel",
        "secret-capability-name",
        "secret-argument-sentinel",
        "/secret/preload-name",
        "/secret/dump-name",
    ] {
        assert!(!serialized.contains(sensitive));
    }
}
