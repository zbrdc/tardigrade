// tardigrade CLI: thin wrapper over the tardigrade library.

use std::io::Read;

struct Cli {
    entry: String, // "-" means stdin
    timeout_ms: u64,
    timeout_set: bool,
    command_timeout_ms: u64,
    heap_mb: usize,
    args: Vec<String>,
    preload: Option<std::path::PathBuf>,
    dump: Option<std::path::PathBuf>,
    serve: bool,
    sandbox: bool,
    fs_quota_bytes: u64,
    seed: Option<u64>,
    clock_virtual: bool,
    epoch_ms: u64,
    epoch_set: bool,
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
            return Err(format!(
                "bad --epoch: {} (above the ES time range)",
                v
            ));
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
        heap_mb: 256,
        args: Vec::new(),
        preload: None,
        dump: None,
        serve: false,
        sandbox: false,
        fs_quota_bytes: tardigrade::DEFAULT_FS_QUOTA_BYTES,
        seed: None,
        clock_virtual: false,
        epoch_ms: 0,
        epoch_set: false,
    };
    for a in argv {
        if a == "--" {
            continue;
        }
        if clock_pair(&mut c, a)? {
            continue;
        }
        if let Some(v) = a.strip_prefix("--timeout=") {
            c.timeout_ms = v.parse().map_err(|_| format!("bad --timeout: {}", v))?;
            c.timeout_set = true;
        } else if let Some(v) = a.strip_prefix("--command-timeout=") {
            c.command_timeout_ms = v.parse().map_err(|_| format!("bad --command-timeout: {}", v))?;
        } else if let Some(v) = a.strip_prefix("--memory=") {
            c.heap_mb = v.parse().map_err(|_| format!("bad --memory: {}", v))?;
            if c.heap_mb < 8 {
                return Err("--memory must be at least 8".to_string());
            }
        } else if let Some(v) = a.strip_prefix("--preload=") {
            c.preload = Some(v.into());
        } else if let Some(v) = a.strip_prefix("--dump=") {
            c.dump = Some(v.into());
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
            "usage: tardigrade [--timeout=<ms>] [--command-timeout=<ms>] [--memory=<mb>] [--fs-quota=<mb>] [--seed=<u64>] [--clock=virtual] [--epoch=<ms>] [--preload=<file>] [--dump=<file>] [--serve] [--sandbox] <entry.js or -> [args...]\n       tardigrade --sandbox-check"
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

    if std::env::var("TARDIGRADE_DEBUG").is_ok() {
        eprintln!("tardigrade: entry={:?} dump={:?} preload={:?} serve={}", c.entry, c.dump, c.preload, c.serve);
    }

    // The sandbox sits between the host boundary reads and V8 init.
    let preload_data = if c.sandbox { apply_sandbox(&c) } else { None };

    let cfg = tardigrade::Config {
        timeout_ms: if c.serve && !c.timeout_set { 0 } else { c.timeout_ms },
        heap_mb: c.heap_mb,
        args: c.args,
        preload: c.preload,
        dump: c.dump,
        stdin_consumed,
        preload_data,
        fs_quota_bytes: c.fs_quota_bytes,
        seed: c.seed,
        command_timeout_ms: c.command_timeout_ms,
        clock: if c.clock_virtual {
            tardigrade::Clock::Virtual { epoch_ms: c.epoch_ms }
        } else {
            tardigrade::Clock::Real
        },
    };

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
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).expect("read stdin");
        return (s, true);
    }
    match std::fs::read_to_string(&c.entry) {
        Ok(s) => (s, false),
        Err(_) => {
            eprintln!("tardigrade: cannot open {}", c.entry);
            std::process::exit(2);
        }
    }
}

/// Reads the preload file and applies the OS sandbox, returning the pre-read
/// preload contents. Any refusal exits 2 and the guest never runs.
fn apply_sandbox(c: &Cli) -> Option<String> {
    let preload_data = c.preload.as_ref().map(|path| match std::fs::read_to_string(path) {
        Ok(data) => Some(data),
        Err(_) => {
            eprintln!("tardigrade: cannot open preload {:?}", path);
            std::process::exit(2);
        }
    });
    if let Err(e) = tardigrade::sandbox::apply(c.dump.as_deref()) {
        eprintln!("tardigrade: --sandbox: {}", e);
        std::process::exit(2);
    }
    preload_data.flatten()
}
