//! Library API: run() twice in one process. Kept to a single test because
//! run() owns process-global world state (one guest at a time per process).

#[test]
fn run_returns_the_guest_exit_code_and_can_run_again() {
    let cfg = v8vm::Config::new();
    assert_eq!(v8vm::run("vm.exit(3)", "a.js", &cfg), 3);
    assert_eq!(v8vm::run("", "empty.js", &cfg), 0);
    assert_eq!(v8vm::run("throw new Error('x')", "b.js", &cfg), 1);
    assert_eq!(v8vm::run("for (;;) {}", "spin.js", &v8vm::Config { timeout_ms: 50, ..cfg }), 124);
}
