// v8vm CLI: thin wrapper over the v8vm library.

use std::io::Read;

struct Cli {
    entry: String, // "-" means stdin
    timeout_ms: u64,
    timeout_set: bool,
    heap_mb: usize,
    args: Vec<String>,
    preload: Option<std::path::PathBuf>,
    dump: Option<std::path::PathBuf>,
    serve: bool,
    sandbox: bool,
}

fn parse(argv: &[String]) -> Result<Cli, String> {
    let mut c = Cli {
        entry: String::new(),
        timeout_ms: 60000,
        timeout_set: false,
        heap_mb: 256,
        args: Vec::new(),
        preload: None,
        dump: None,
        serve: false,
        sandbox: false,
    };
    for a in argv {
        if a == "--" {
            continue;
        }
        if let Some(v) = a.strip_prefix("--timeout=") {
            c.timeout_ms = v.parse().map_err(|_| format!("bad --timeout: {}", v))?;
            c.timeout_set = true;
        } else if let Some(v) = a.strip_prefix("--memory=") {
            c.heap_mb = v.parse().map_err(|_| format!("bad --memory: {}", v))?;
        } else if let Some(v) = a.strip_prefix("--preload=") {
            c.preload = Some(v.into());
        } else if let Some(v) = a.strip_prefix("--dump=") {
            c.dump = Some(v.into());
        } else if a == "--serve" {
            c.serve = true;
        } else if a == "--sandbox" {
            c.sandbox = true;
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
            "usage: v8vm [--timeout=<ms>] [--memory=<mb>] [--preload=<file>] [--dump=<file>] [--serve] [--sandbox] <entry.js or -> [args...]"
                .to_string(),
        );
    }
    if c.serve && c.entry == "-" {
        return Err("--serve needs a file entry; stdin is the command channel".into());
    }
    Ok(c)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let c = match parse(&argv) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("v8vm: {}", msg);
            std::process::exit(2);
        }
    };

    let (source, stdin_consumed) = if c.entry.is_empty() {
        (String::new(), false)
    } else if c.entry == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).expect("read stdin");
        (s, true)
    } else {
        match std::fs::read_to_string(&c.entry) {
            Ok(s) => (s, false),
            Err(_) => {
                eprintln!("v8vm: cannot open {}", c.entry);
                std::process::exit(2);
            }
        }
    };

    if std::env::var("V8VM_DEBUG").is_ok() {
        eprintln!("v8vm: entry={:?} dump={:?} preload={:?} serve={}", c.entry, c.dump, c.preload, c.serve);
    }

    let mut preload_data = None;
    if c.sandbox {
        // The sandbox must come after the host boundary reads (entry source
        // above, preload here) and before V8 initializes in run()/serve().
        if let Some(path) = &c.preload {
            match std::fs::read_to_string(path) {
                Ok(data) => preload_data = Some(data),
                Err(_) => {
                    eprintln!("v8vm: cannot open preload {:?}", path);
                    std::process::exit(2);
                }
            }
        }
        if let Err(e) = v8vm::sandbox::apply(c.dump.as_deref()) {
            eprintln!("v8vm: --sandbox: {}", e);
            std::process::exit(2);
        }
    }

    let cfg = v8vm::Config {
        timeout_ms: if c.serve && !c.timeout_set { 0 } else { c.timeout_ms },
        heap_mb: c.heap_mb,
        args: c.args,
        preload: c.preload,
        dump: c.dump,
        stdin_consumed,
        preload_data,
    };


    let code = if c.serve {
        v8vm::serve(std::io::stdout(), &source, &c.entry, &cfg)
    } else {
        v8vm::run(&source, &c.entry, &cfg)
    };
    std::process::exit(code);
}
