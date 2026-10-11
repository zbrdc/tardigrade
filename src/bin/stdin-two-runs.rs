//! Test-only helper: two sequential v8vm guests in one process, each reading
//! one vm.stdin chunk. Prints what each run saw, so tests/library.rs can
//! verify that input unread by the first run is left for the second.

fn main() {
    let cfg = v8vm::Config::new();
    let first = r#"vm.stdin.read().then((c) => { console.log('first:' + JSON.stringify(c)); vm.exit(0); });"#;
    let second = r#"vm.stdin.read().then((c) => { console.log('second:' + JSON.stringify(c)); vm.exit(0); });"#;
    let a = v8vm::run(first, "one.js", &cfg);
    let b = v8vm::run(second, "two.js", &cfg);
    std::process::exit(if a == 0 && b == 0 { 0 } else { 1 });
}
