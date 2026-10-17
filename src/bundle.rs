//! Deployment bundle (P3, docs/DEPLOYMENT_ARTIFACT.md): one JSON artifact
//! carrying the guest entry, an optional preload snapshot, the capability
//! policy and the resource budgets, plus the runtime identity the bundle
//! was prepared for. `--bundle=FILE` validates all of it before any
//! guest evaluation or sidecar spawn; a mismatch refuses the run.

use ring::digest::{digest, SHA256};

pub struct NetCall {
    pub name: String,
    pub url: String,
    pub method: String,
}

pub struct Bundle {
    /// Diagnostics path (also drives .mjs module detection like an entry).
    pub entry_path: String,
    pub module: bool,
    pub entry_source: String,
    pub preload: Option<String>,
    pub timeout_ms: Option<u64>,
    pub heap_mb: Option<u64>,
    pub command_timeout_ms: Option<u64>,
    pub net_allow_private: bool,
    pub net_timeout_ms: Option<u64>,
    pub net_max_response_bytes: Option<u64>,
    pub net_total_bytes: Option<u64>,
    pub net_calls: Vec<NetCall>,
}

/// The engine identity a bundle must match to run here.
pub fn engine_version() -> String {
    v8::V8::get_version().to_string()
}

fn sha256(bytes: &[u8]) -> String {
    let digest = digest(&SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn text_of<'a>(v: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    v.get(field)
        .and_then(|f| f.as_str())
        .ok_or_else(|| format!("bundle: {field} must be a string"))
}

fn u64_of(v: &serde_json::Value, field: &str) -> Result<Option<u64>, String> {
    match v.get(field) {
        None => Ok(None),
        Some(f) => f
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("bundle: {field} must be a non-negative integer")),
    }
}

fn check_hash(v: &serde_json::Value, source: &str, what: &str) -> Result<(), String> {
    let want = text_of(v, "sha256")?;
    let got = sha256(source.as_bytes());
    if want == got {
        Ok(())
    } else {
        Err(format!(
            "bundle: {what} sha256 mismatch (bundle {want}, actual {got})"
        ))
    }
}

/// Parses and fully validates one bundle. Every rule that can refuse a run
/// lives here, before any guest code or sidecar exists.
pub fn load(bytes: &[u8]) -> Result<Bundle, String> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("bundle: not valid JSON: {e}"))?;
    if text_of(&v, "format")? != "tardigrade.deploy-bundle" {
        return Err("bundle: format must be tardigrade.deploy-bundle".into());
    }
    if v.get("version").and_then(|n| n.as_u64()) != Some(1) {
        return Err("bundle: version must be 1".into());
    }
    let runtime = v.get("runtime").ok_or("bundle: runtime is required")?;
    let want_engine = text_of(runtime, "v8")?;
    if want_engine != engine_version() {
        return Err(format!(
            "bundle: prepared for V8 {want_engine}, this runtime is {}",
            engine_version()
        ));
    }

    let entry = v.get("entry").ok_or("bundle: entry is required")?;
    let entry_path = text_of(entry, "path")?.to_string();
    if entry_path.is_empty() {
        return Err("bundle: entry.path must not be empty".into());
    }
    let module = entry
        .get("module")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let entry_source = text_of(entry, "source")?.to_string();
    check_hash(entry, &entry_source, "entry")?;

    let preload = match v.get("preload") {
        None => None,
        Some(p) => {
            let source = text_of(p, "source")?.to_string();
            // The snapshot itself is parsed by the preload restore path;
            // here only its integrity is the bundle's promise.
            check_hash(p, &source, "preload")?;
            Some(source)
        }
    };

    let budgets = v
        .get("budgets")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let timeout_ms = u64_of(&budgets, "timeout_ms")?;
    let heap_mb = u64_of(&budgets, "heap_mb")?;
    if let Some(mb) = heap_mb {
        if mb < 8 {
            return Err("bundle: heap_mb must be at least 8, like --memory".into());
        }
    }
    let command_timeout_ms = u64_of(&budgets, "command_timeout_ms")?;

    let mut net_calls = Vec::new();
    let mut net_allow_private = false;
    let mut net_timeout_ms = None;
    let mut net_max_response_bytes = None;
    let mut net_total_bytes = None;
    if let Some(net) = v.get("net") {
        net_allow_private = net
            .get("allow_private")
            .and_then(|f| f.as_bool())
            .unwrap_or(false);
        net_timeout_ms = u64_of(net, "timeout_ms")?;
        net_max_response_bytes = u64_of(net, "max_response_bytes")?;
        net_total_bytes = u64_of(net, "total_bytes")?;
        let calls = net
            .get("calls")
            .and_then(|c| c.as_object())
            .ok_or("bundle: net.calls must be an object when net is present")?;
        for (name, spec) in calls {
            if name.is_empty() {
                return Err("bundle: net capability names must not be empty".into());
            }
            let url = text_of(spec, "url")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "bundle: net capability {name} needs an http(s) URL"
                ));
            }
            let method = spec
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("POST")
                .to_string();
            if !matches!(method.as_str(), "GET" | "POST" | "PUT") {
                return Err(format!("bundle: net capability {name} has method {method}"));
            }
            net_calls.push(NetCall {
                name: name.clone(),
                url: url.to_string(),
                method,
            });
        }
    }

    Ok(Bundle {
        entry_path,
        module,
        entry_source,
        preload,
        timeout_ms,
        heap_mb,
        command_timeout_ms,
        net_allow_private,
        net_timeout_ms,
        net_max_response_bytes,
        net_total_bytes,
        net_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> serde_json::Value {
        let source = "console.log('hi');";
        serde_json::json!({
            "format": "tardigrade.deploy-bundle",
            "version": 1,
            "runtime": {"v8": engine_version()},
            "entry": {
                "path": "app.js",
                "source": source,
                "sha256": sha256(source.as_bytes()),
            }
        })
    }

    #[test]
    fn a_valid_bundle_loads() {
        let b = load(serde_json::to_vec(&valid()).unwrap().as_slice()).unwrap();
        assert_eq!(b.entry_path, "app.js");
        assert_eq!(b.entry_source, "console.log('hi');");
        assert!(b.preload.is_none());
    }

    #[test]
    fn a_tampered_entry_is_refused() {
        let mut v = valid();
        v["entry"]["source"] = "console.log('tampered');".into();
        let err = load(serde_json::to_vec(&v).unwrap().as_slice())
            .err()
            .unwrap();
        assert!(err.contains("mismatch"), "{err}");
    }

    #[test]
    fn a_foreign_runtime_is_refused() {
        let mut v = valid();
        v["runtime"]["v8"] = "0.0.0-not-this".into();
        let err = load(serde_json::to_vec(&v).unwrap().as_slice())
            .err()
            .unwrap();
        assert!(err.contains("prepared for V8"), "{err}");
    }

    #[test]
    fn bad_budgets_and_calls_are_refused() {
        let mut v = valid();
        v["budgets"] = serde_json::json!({"heap_mb": 4});
        assert!(load(serde_json::to_vec(&v).unwrap().as_slice()).is_err());
        let mut v = valid();
        v["net"] = serde_json::json!({"calls": {"x": {"url": "ftp://no"}}});
        assert!(load(serde_json::to_vec(&v).unwrap().as_slice()).is_err());
    }
}
