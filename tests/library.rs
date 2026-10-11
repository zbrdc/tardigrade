//! Library API: run() owns per-run state, so guests can be reused
//! sequentially and can also run concurrently in one process.

use std::sync::{Arc, Barrier};

use v8vm::Config;

#[test]
fn run_returns_the_guest_exit_code_and_can_run_again() {
    let cfg = v8vm::Config::new();
    assert_eq!(v8vm::run("vm.exit(3)", "a.js", &cfg), 3);
    assert_eq!(v8vm::run("", "empty.js", &cfg), 0);
    assert_eq!(v8vm::run("throw new Error('x')", "b.js", &cfg), 1);
    assert_eq!(v8vm::run("for (;;) {}", "spin.js", &v8vm::Config { timeout_ms: 50, ..cfg }), 124);
}

/// Guest A: slow timer, no watchdog, self-checks its fs, exits 7.
const GUEST_A: &str = r#"
vm.fs.write('/alfa', 'a');
setTimeout(() => {
  const names = vm.fs.list('/');
  if (names.length !== 1 || names[0] !== '/alfa') vm.exit(97);
  vm.exit(7);
}, 150);
"#;

/// Guest B: fast fs self-check, then spins until its own watchdog kills it
/// with 124 while guest A is still pending.
const GUEST_B: &str = r#"
vm.fs.write('/beta', 'b');
setTimeout(() => {
  const names = vm.fs.list('/');
  if (names.length !== 1 || names[0] !== '/beta') vm.exit(98);
  for (;;) {}
}, 40);
"#;

#[test]
fn two_guests_run_concurrently_without_sharing_state() {
    // Both guests start together and overlap (A lives 150 ms; B self-checks
    // at 40 ms and spins until its 80 ms watchdog). Any bleed of fs, exit
    // code, timers, or watchdog target turns a self-check exit (97/98) or a
    // wrong termination (124 for A) into an assertion failure.
    let start = Arc::new(Barrier::new(2));
    let spawn = |src: &str, timeout_ms: u64, gate: Arc<Barrier>| {
        let src = src.to_string();
        std::thread::spawn(move || {
            gate.wait();
            let cfg = Config {
                timeout_ms,
                // hermetic: neither guest may consume the test harness stdin
                stdin_consumed: true,
                ..Config::new()
            };
            v8vm::run(&src, "guest.js", &cfg)
        })
    };
    let a = spawn(GUEST_A, 0, start.clone());
    let b = spawn(GUEST_B, 80, start.clone());
    assert_eq!(a.join().unwrap(), 7, "guest A: isolated fs, own exit code");
    assert_eq!(b.join().unwrap(), 124, "guest B: killed by its own watchdog");
}
