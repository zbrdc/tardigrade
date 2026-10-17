// Regression for the P0 evidence in docs/SUPERVISOR_CONTRACT.md: a guest
// killed with SIGKILL must not leave the net-sidecar alive until its
// in-flight request times out; the sidecar arms PR_SET_PDEATHSIG and dies
// with the guest immediately.

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn children_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let tasks = format!("/proc/{pid}/task");
    let Ok(entries) = std::fs::read_dir(&tasks) else {
        return out;
    };
    for entry in entries.flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path().join("children")) {
            for token in text.split_whitespace() {
                if let Ok(child) = token.parse::<u32>() {
                    out.push(child);
                }
            }
        }
    }
    out
}

#[test]
fn sidecar_dies_with_sigkilled_guest() {
    // A listener that accepts and never answers holds the sidecar's HTTP
    // request in flight for the full net-timeout.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = std::thread::spawn(move || {
        if let Ok((mut conn, _)) = listener.accept() {
            // Keep the connection open (request in flight) until the guest
            // dies and the socket closes; a single read would drop it and
            // end the request immediately.
            loop {
                match conn.read(&mut [0u8; 256]) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    });

    let app = scratch("sidecar_lifetime_app.js");
    std::fs::write(
        &app,
        "vm.net.request(\"slow\", \"x\").then(\n  (r) => console.log(\"reply \" + JSON.stringify(r)),\n  (e) => console.error(\"err \" + String(e))\n);\n",
    )
    .unwrap();

    let mut guest = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .arg(&app)
        .arg(format!("--allow-call=slow=http://127.0.0.1:{port}/"))
        .arg("--net-allow-private")
        .arg("--net-timeout=30000")
        .arg("--timeout=0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let spawn_deadline = Instant::now() + Duration::from_secs(5);
    let sidecar = loop {
        if let Some(&pid) = children_of(guest.id()).first() {
            // Let the forked child finish arming its death signal and exec.
            std::thread::sleep(Duration::from_millis(200));
            break pid;
        }
        assert!(
            Instant::now() < spawn_deadline,
            "net-sidecar never appeared as a child of the guest"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    guest.kill().unwrap();
    let _ = guest.wait();

    // PDEATHSIG must end the sidecar far below the 30 s net-timeout.
    let gone_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: signal 0 only probes for the pid.
        let dead = unsafe { libc::kill(sidecar as libc::pid_t, 0) == -1 }
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if dead {
            break;
        }
        assert!(
            Instant::now() < gone_deadline,
            "net-sidecar {sidecar} outlived a SIGKILLed guest (PDEATHSIG regression)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(hold);
}
