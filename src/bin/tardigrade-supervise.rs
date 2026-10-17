//! External supervisor for exactly one one-shot CLI guest.
//!
//! Contract: docs/SUPERVISOR_CONTRACT.md. The worker runs in its own
//! process group; a wall-clock deadline escalates SIGTERM -> SIGKILL to the
//! whole group, so the net-sidecar cannot survive worker termination. No
//! restart policy, no dump promise, no serve semantics.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Poll cadence for the wait/escalation loop.
const TICK: Duration = Duration::from_millis(25);
/// Bounded wait for the group to empty after the worker is reaped.
const SWEEP_BOUND: Duration = Duration::from_secs(5);

struct Args {
    deadline: Duration,
    term_grace: Duration,
    cmd: Vec<String>,
}

fn usage() -> ! {
    eprintln!("usage: tardigrade-supervise --deadline=<ms> [--term-grace=<ms>] -- CMD [ARGS...]");
    std::process::exit(2);
}

fn parse_ms(flag: &str, value: &str) -> Duration {
    let ms: u64 = value.parse().unwrap_or_else(|_| {
        eprintln!("tardigrade-supervise: {flag} needs a millisecond number");
        usage();
    });
    Duration::from_millis(ms)
}

fn parse_args() -> Args {
    let mut deadline = None;
    let mut term_grace = Duration::from_secs(5);
    let mut cmd = Vec::new();
    let mut argv = std::env::args().skip(1).peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--" => {
                cmd.extend(argv.by_ref());
            }
            _ if arg.starts_with("--deadline=") => {
                deadline = Some(parse_ms(&arg, arg.trim_start_matches("--deadline=")));
            }
            _ if arg.starts_with("--term-grace=") => {
                term_grace = parse_ms(&arg, arg.trim_start_matches("--term-grace="));
            }
            _ => usage(),
        }
    }
    let deadline = deadline.unwrap_or_else(|| {
        eprintln!("tardigrade-supervise: --deadline=<ms> is required");
        usage();
    });
    if cmd.is_empty() {
        usage();
    }
    Args {
        deadline,
        term_grace,
        cmd,
    }
}

fn signal_group(pgid: i32, sig: libc::c_int) -> bool {
    // SAFETY: kill(2) with a negative pgid and a signal number only.
    unsafe { libc::kill(-pgid, sig) == 0 }
}

fn group_empty(pgid: i32) -> bool {
    // SAFETY: signal 0 probes existence without delivering anything.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn sweep_group(pgid: i32) -> bool {
    let start = Instant::now();
    while start.elapsed() < SWEEP_BOUND {
        if group_empty(pgid) {
            return true;
        }
        let _ = signal_group(pgid, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(100));
    }
    group_empty(pgid)
}

fn child_exit_code(status: std::process::ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return code as u8;
    }
    let sig = signal_of(&status);
    128u8.saturating_add(sig as u8)
}

fn signal_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().unwrap_or(0)
}

fn main() {
    let args = parse_args();
    let mut child = Command::new(&args.cmd[0])
        .args(&args.cmd[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        // Own process group: the net-sidecar inherits it, so a group kill
        // cannot miss it and cannot touch this supervisor.
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("tardigrade-supervise: cannot start {}: {e}", args.cmd[0]);
            return_run(2);
        });
    let pgid = child.id() as i32;
    let start = Instant::now();
    let mut term_sent = None;
    let mut kill_sent = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                eprintln!("tardigrade-supervise: cannot wait for the worker: {e}");
                return_run(126);
            }
        }
        if term_sent.is_none() && start.elapsed() >= args.deadline {
            eprintln!("tardigrade-supervise: deadline reached, terminating the worker group");
            let _ = signal_group(pgid, libc::SIGTERM);
            term_sent = Some(Instant::now());
        } else if let Some(t) = term_sent {
            if !kill_sent && t.elapsed() >= args.term_grace {
                eprintln!("tardigrade-supervise: grace over, killing the worker group");
                let _ = signal_group(pgid, libc::SIGKILL);
                kill_sent = true;
            }
        }
        std::thread::sleep(TICK);
    };
    if !sweep_group(pgid) {
        eprintln!("tardigrade-supervise: worker group {pgid} did not empty");
        return_run(126);
    }
    if term_sent.is_some() {
        return_run(124);
    }
    return_run(child_exit_code(status) as u32);
}

fn return_run(code: u32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code as i32);
}
