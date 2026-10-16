// tardigrade CLI: thin wrapper over the tardigrade library.

use std::io::Read;

#[path = "manifest_output.rs"]
mod manifest_output;
#[path = "run_manifest.rs"]
mod run_manifest;

struct Cli {
    entry: String, // "-" means stdin
    timeout_ms: u64,
    timeout_set: bool,
    command_timeout_ms: u64,
    command_timeout_set: bool,
    heap_mb: usize,
    max_output_bytes: u64,
    args: Vec<String>,
    preload: Option<std::path::PathBuf>,
    dump: Option<std::path::PathBuf>,
    run_manifest: Option<std::path::PathBuf>,
    serve: bool,
    sandbox: bool,
    fs_quota_bytes: u64,
    seed: Option<u64>,
    clock_virtual: bool,
    epoch_ms: u64,
    epoch_set: bool,
    allow_calls: Vec<String>,
    net_max_response: Option<u64>,
    net_total_bytes: Option<u64>,
    net_timeout_ms: Option<u64>,
    net_allow_private: bool,
    module: bool,
    rss_cap_mb: usize,
}

/// Parses --clock=virtual or --epoch=<ms>; Ok(false) means the argument
/// is neither and parse's chain continues. The epoch is stored plain and
/// validated after the whole argv walk, so flag order never matters.
fn clock_pair(c: &mut Cli, arg: &str) -> Result<bool, String> {
    if let Some(v) = arg.strip_prefix("--clock=") {
        if v != "virtual" {
            return Err(format!("bad --clock: {} (only virtual)", v));
        }
        c.clock_virtual = true;
        return Ok(true);
    }
    if let Some(v) = arg.strip_prefix("--epoch=") {
        c.epoch_ms = v.parse().map_err(|_| format!("bad --epoch: {}", v))?;
        if c.epoch_ms > tardigrade::Clock::MAX_EPOCH_MS {
            return Err(format!("bad --epoch: {} (above the ES time range)", v));
        }
        c.epoch_set = true;
        return Ok(true);
    }
    Ok(false)
}

fn parse(argv: &[String]) -> Result<Cli, String> {
    let mut c = Cli {
        entry: String::new(),
        timeout_ms: 60000,
        timeout_set: false,
        command_timeout_ms: 0,
        command_timeout_set: false,
        heap_mb: 256,
        max_output_bytes: 64 * 1024 * 1024,
        args: Vec::new(),
        preload: None,
        dump: None,
        run_manifest: None,
        serve: false,
        sandbox: false,
        fs_quota_bytes: tardigrade::DEFAULT_FS_QUOTA_BYTES,
        seed: None,
        clock_virtual: false,
        epoch_ms: 0,
        epoch_set: false,
        allow_calls: Vec::new(),
        net_max_response: None,
        net_total_bytes: None,
        net_timeout_ms: None,
        net_allow_private: false,
        module: false,
        rss_cap_mb: 0,
    };
    // `--` ends option parsing (the usual convention): everything after
    // it is the entry or a guest argument, even when it starts with `-`.
    // Before it, tardigrade's own flags are still recognised anywhere.
    let mut positional_only = false;
    for a in argv {
        if positional_only {
            if c.entry.is_empty() {
                c.entry = a.clone();
            } else {
                c.args.push(a.clone());
            }
            continue;
        }
        if a == "--" {
            positional_only = true;
            continue;
        }
        if clock_pair(&mut c, a)? {
            continue;
        }
        if let Some(v) = a.strip_prefix("--timeout=") {
            c.timeout_ms = v.parse().map_err(|_| format!("bad --timeout: {}", v))?;
            c.timeout_set = true;
        } else if let Some(v) = a.strip_prefix("--command-timeout=") {
            c.command_timeout_ms = v
                .parse()
                .map_err(|_| format!("bad --command-timeout: {}", v))?;
            c.command_timeout_set = true;
        } else if let Some(v) = a.strip_prefix("--memory=") {
            c.heap_mb = v.parse().map_err(|_| format!("bad --memory: {}", v))?;
            if c.heap_mb < 8 {
                return Err("--memory must be at least 8".to_string());
            }
        } else if let Some(v) = a.strip_prefix("--preload=") {
            c.preload = Some(v.into());
        } else if let Some(v) = a.strip_prefix("--dump=") {
            c.dump = Some(v.into());
        } else if let Some(v) = a.strip_prefix("--run-manifest=") {
            if v.is_empty() || v == "-" {
                return Err("--run-manifest requires a non-empty file path other than -".into());
            }
            c.run_manifest = Some(v.into());
        } else if let Some(v) = a.strip_prefix("--max-output=") {
            let mb: u64 = v.parse().map_err(|_| format!("bad --max-output: {}", v))?;
            c.max_output_bytes = mb
                .checked_mul(1024 * 1024)
                .ok_or_else(|| format!("bad --max-output: {}", v))?;
        } else if let Some(v) = a.strip_prefix("--fs-quota=") {
            let mb: u64 = v.parse().map_err(|_| format!("bad --fs-quota: {}", v))?;
            c.fs_quota_bytes = mb
                .checked_mul(1024 * 1024)
                .ok_or_else(|| format!("bad --fs-quota: {}", v))?;
        } else if a == "--serve" {
            c.serve = true;
        } else if let Some(v) = a.strip_prefix("--seed=") {
            c.seed = Some(v.parse().map_err(|_| format!("bad --seed: {}", v))?);
        } else if a == "--sandbox" {
            c.sandbox = true;
        } else if a == "--sandbox-check" {
            // main() handles the sole-argument form before parse(). Before
            // the entry this is a misplaced probe flag; after it, a guest arg.
            if c.entry.is_empty() {
                return Err("--sandbox-check must be the only argument".into());
            }
            c.args.push(a.clone());
        } else if let Some(v) = a.strip_prefix("--allow-call=") {
            c.allow_calls.push(v.to_string());
        } else if let Some(v) = a.strip_prefix("--net-max-response=") {
            c.net_max_response = Some(
                v.parse()
                    .map_err(|_| format!("bad --net-max-response: {}", v))?,
            );
        } else if let Some(v) = a.strip_prefix("--net-total-bytes=") {
            c.net_total_bytes = Some(
                v.parse()
                    .map_err(|_| format!("bad --net-total-bytes: {}", v))?,
            );
        } else if let Some(v) = a.strip_prefix("--net-timeout=") {
            c.net_timeout_ms = Some(v.parse().map_err(|_| format!("bad --net-timeout: {}", v))?);
        } else if let Some(v) = a.strip_prefix("--rss-cap=") {
            c.rss_cap_mb = v.parse().map_err(|_| format!("bad --rss-cap: {}", v))?;
        } else if a == "--module" {
            c.module = true;
        } else if a == "--net-allow-private" {
            c.net_allow_private = true;
        } else if a.starts_with('-') && a != "-" {
            return Err(format!("unknown flag {}", a));
        } else if c.entry.is_empty() {
            c.entry = a.clone();
        } else {
            c.args.push(a.clone());
        }
    }
    if c.entry.is_empty() && !c.serve {
        return Err(
            "usage: tardigrade [--timeout=<ms>] [--command-timeout=<ms>] [--memory=<mb>] [--fs-quota=<mb>] [--seed=<u64>] [--clock=virtual] [--epoch=<ms>] [--preload=<file>] [--dump=<file>] [--run-manifest=<file>] [--serve] [--sandbox] [--allow-call=NAME=URL[,METHOD]] [--net-max-response=<bytes>] [--net-total-bytes=<bytes>] [--net-timeout=<ms>] [--net-allow-private] <entry.js or -> [args...]\n       tardigrade --sandbox-check"
                .to_string(),
        );
    }
    if c.serve && c.entry == "-" {
        return Err("--serve needs a file entry; stdin is the command channel".into());
    }
    if c.epoch_set && !c.clock_virtual {
        return Err("--epoch needs --clock=virtual".into());
    }
    Ok(c)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(|s| s.as_str()) == Some("net-sidecar") {
        std::process::exit(tardigrade::netcap::run_sidecar());
    }
    if argv == ["--sandbox-check"] {
        // A gate scripts and CI can call without running a guest: exit 0 when
        // the platform and kernel can sandbox, 2 with the reason otherwise.
        if tardigrade::sandbox::supported() {
            println!("supported");
        } else {
            eprintln!("tardigrade: --sandbox is not available here (needs Linux x86_64 with Landlock ABI 6)");
            std::process::exit(2);
        }
        return;
    }
    let c = match parse(&argv) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("tardigrade: {}", msg);
            std::process::exit(2);
        }
    };

    let (source, stdin_consumed) = read_entry(&c);

    let net_plan = match build_net_plan(&c) {
        Ok(plan) => plan,
        Err(msg) => {
            eprintln!("tardigrade: {}", msg);
            std::process::exit(2);
        }
    };
    // The sidecar must be spawned before the sandbox is applied: the run's
    // only channel to it is an fd that close_range would otherwise close.
    let mut net_session = None;
    if let Some(plan) = &net_plan {
        let session = std::env::current_exe()
            .map_err(|e| format!("cannot resolve the current executable: {e}"))
            .and_then(|exe| tardigrade::netcap::spawn_sidecar(&exe, plan));
        match session {
            Ok(s) => net_session = Some(s),
            Err(msg) => {
                eprintln!("tardigrade: {}", msg);
                std::process::exit(2);
            }
        }
    }
    let keep_fd = net_session.as_ref().map(|s| s.wire_fd);

    if std::env::var("TARDIGRADE_DEBUG").is_ok() {
        eprintln!(
            "tardigrade: entry={:?} dump={:?} preload={:?} serve={}",
            c.entry, c.dump, c.preload, c.serve
        );
    }

    let preload_data = if c.sandbox || c.run_manifest.is_some() {
        read_preload(&c)
    } else {
        None
    };

    let cfg = tardigrade::Config {
        timeout_ms: if c.serve && !c.timeout_set {
            0
        } else {
            c.timeout_ms
        },
        heap_mb: c.heap_mb,
        max_output_bytes: c.max_output_bytes,
        // An .mjs entry is a module even without the flag.
        module: c.module || c.entry.ends_with(".mjs"),
        rss_cap_mb: c.rss_cap_mb,
        args: c.args.clone(),
        preload: c.preload.clone(),
        dump: c.dump.clone(),
        stdin_consumed,
        preload_data,
        fs_quota_bytes: c.fs_quota_bytes,
        seed: c.seed,
        // In --serve the CLI defaults a per-command bound (30 s) when the
        // operator did not set one: an unbounded command would otherwise
        // park the session (and anyone relaying to it) forever. Passing
        // --command-timeout=0 explicitly still disables it. The library
        // default stays 0; this is a CLI policy only.
        command_timeout_ms: if c.serve && !c.command_timeout_set {
            30_000
        } else {
            c.command_timeout_ms
        },
        clock: if c.clock_virtual {
            tardigrade::Clock::Virtual {
                epoch_ms: c.epoch_ms,
            }
        } else {
            tardigrade::Clock::Real
        },
        net: net_plan.map(|plan| tardigrade::netcap::NetConfig {
            names: plan.names(),
            max_response_bytes: plan.limits.max_response_bytes,
            total_bytes: plan.limits.total_bytes,
            timeout_ms: plan.limits.timeout_ms,
            allow_private: plan.limits.allow_private,
            wire_fd: net_session.as_ref().map(|s| s.wire_fd).unwrap_or(-1),
        }),
    };

    if let Some(path) = &c.run_manifest {
        let mut bytes = match serde_json::to_vec(&run_manifest::describe(
            &cfg,
            run_manifest::Inputs {
                source: &source,
                preload: cfg.preload_data.as_deref(),
                serve: c.serve,
                sandbox_requested: c.sandbox,
            },
        )) {
            Ok(bytes) => bytes,
            Err(_) => {
                eprintln!("tardigrade: cannot prepare run manifest");
                std::process::exit(2);
            }
        };
        bytes.push(b'\n');
        if manifest_output::write_new(path, &bytes, c.dump.as_deref()).is_err() {
            eprintln!("tardigrade: cannot prepare run manifest");
            std::process::exit(2);
        }
    }

    // Publish and close the optional manifest before applying the sandbox.
    if c.sandbox {
        apply_sandbox(&c, keep_fd);
    }

    let code = if c.serve {
        tardigrade::serve(std::io::stdout(), &source, &c.entry, &cfg)
    } else {
        tardigrade::run(&source, &c.entry, &cfg)
    };
    std::process::exit(code);
}

/// Reads the entry program; "-" means stdin (then guest stdin is at EOF).
fn read_entry(c: &Cli) -> (String, bool) {
    if c.entry.is_empty() {
        return (String::new(), false);
    }
    if c.entry == "-" {
        // Read bytes and convert explicitly: invalid UTF-8 from stdin is
        // a usage error (exit 2), not a panic (which used to exit 101).
        let mut buf = Vec::new();
        if std::io::stdin().read_to_end(&mut buf).is_err() {
            eprintln!("tardigrade: cannot read entry from stdin");
            std::process::exit(2);
        }
        return match String::from_utf8(buf) {
            Ok(s) => (s, true),
            Err(_) => {
                eprintln!("tardigrade: entry from stdin is not valid UTF-8");
                std::process::exit(2);
            }
        };
    }
    match std::fs::read(&c.entry) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => (s, false),
            Err(_) => {
                eprintln!("tardigrade: entry {} is not valid UTF-8", c.entry);
                std::process::exit(2);
            }
        },
        Err(_) => {
            eprintln!("tardigrade: cannot open {}", c.entry);
            std::process::exit(2);
        }
    }
}

/// Builds the network plan from --allow-call/--net-* flags; None keeps the
/// guest fully offline (and the --net-* knobs are then an error, so a
/// forgotten --allow-call cannot look like a working grant).
fn build_net_plan(c: &Cli) -> Result<Option<tardigrade::netcap::NetPlan>, String> {
    if c.allow_calls.is_empty() {
        if c.net_max_response.is_some()
            || c.net_total_bytes.is_some()
            || c.net_timeout_ms.is_some()
            || c.net_allow_private
        {
            return Err("--net-* flags need at least one --allow-call".into());
        }
        return Ok(None);
    }
    let mut calls = Vec::new();
    for flag in &c.allow_calls {
        calls.push(tardigrade::netcap::parse_call(flag)?);
    }
    let mut limits = tardigrade::netcap::NetLimits::default();
    if let Some(v) = c.net_max_response {
        limits.max_response_bytes = v;
    }
    if let Some(v) = c.net_total_bytes {
        limits.total_bytes = v;
    }
    if let Some(v) = c.net_timeout_ms {
        limits.timeout_ms = v;
    }
    limits.allow_private = c.net_allow_private;
    Ok(Some(tardigrade::netcap::NetPlan { calls, limits }))
}

/// Reads preload text before sandboxing or manifest construction, when needed.
fn read_preload(c: &Cli) -> Option<String> {
    c.preload
        .as_ref()
        .map(|path| match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => {
                eprintln!("tardigrade: cannot open preload {:?}", path);
                std::process::exit(2);
            }
        })
}

/// Applies the OS sandbox. keep_fd, when set, is the network sidecar's fd the
/// sandbox must preserve.
fn apply_sandbox(c: &Cli, keep_fd: Option<i32>) {
    if let Err(e) = tardigrade::sandbox::apply(c.dump.as_deref(), keep_fd) {
        eprintln!("tardigrade: --sandbox: {}", e);
        std::process::exit(2);
    }
}
