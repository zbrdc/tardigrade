use tardigrade::{Clock, Config};

fn run_virtual(source: &str) -> i32 {
    let cfg = Config {
        clock: Clock::Virtual { epoch_ms: 0 },
        heap_mb: 32,
        timeout_ms: 5_000,
        stdin_consumed: true,
        ..Config::new()
    };
    tardigrade::run(source, "conformance-scheduling.js", &cfg)
}

#[test]
fn microtasks_run_between_timers_with_the_same_deadline() {
    let source = r#"
        const events = [];
        const now = () => performance.now();
        const record = (name) => events.push([name, now()]);
        const expect = [
          ["timer-1", 5],
          ["microtask-1", 5],
          ["promise-then", 5],
          ["nested-microtask", 5],
          ["timer-2", 5],
          ["microtask-2", 5],
          ["timer-3", 5],
        ];
        const check = () => {
          if (JSON.stringify(events) !== JSON.stringify(expect)) {
            throw new Error("unexpected events: " + JSON.stringify(events));
          }
        };

        setTimeout(() => {
          record("timer-1");
          queueMicrotask(() => {
            record("microtask-1");
            queueMicrotask(() => record("nested-microtask"));
          });
          Promise.resolve().then(() => record("promise-then"));
        }, 5);
        setTimeout(() => {
          record("timer-2");
          queueMicrotask(() => record("microtask-2"));
        }, 5);
        setTimeout(() => {
          record("timer-3");
          check();
        }, 5);
    "#;

    assert_eq!(run_virtual(source), 0);
}
