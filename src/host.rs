//! The one host thread that owns V8 initialization and begets every guest
//! thread.

use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::LazyLock;

use crate::serve::ReplySink;
use crate::Config;

/// One guest run request. `replies` carries serve-mode output lines to the
/// calling thread; `done` carries the exit code or the guest's panic
/// payload.
pub(crate) struct Job {
    pub(crate) source: String,
    pub(crate) entry: String,
    pub(crate) cfg: Config,
    pub(crate) replies: Option<ReplySink>,
    pub(crate) done: Sender<Result<i32, Box<dyn std::any::Any + Send>>>,
}

/// Lazily spawns the host thread on the first run()/serve() call (staying
/// lazy keeps V8 init after any host-side sandbox setup in main()). The
/// sender keeps the thread alive for the rest of the process.
static HOST: LazyLock<Sender<Job>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("v8vm-host".to_string())
        .spawn(move || host_thread(rx))
        .expect("spawn v8vm host thread");
    tx
});

pub(crate) fn submit(job: Job) {
    LazyLock::force(&HOST).send(job).expect("v8vm host thread alive");
}

/// True once the host thread has been spawned (it initializes V8 as its
/// first act). The sandbox must be applied before that point for its
/// restrictions to reach the host thread, V8's workers and every guest.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn engine_started() -> bool {
    LazyLock::get(&HOST).is_some()
}

fn host_thread(rx: Receiver<Job>) {
    // V8 must be initialized here, not on a caller's thread: the default
    // platform may protect JIT memory with pkeys, which requires all threads
    // that use V8 to descend from the thread that called V8::initialize.
    // Every guest below is spawned by this thread, so the lineage holds for
    // any caller of run()/serve().
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();
    for job in rx {
        let Job { source, entry, cfg, replies, done } = job;
        // Clone so a failed spawn still leaves a channel to answer on. A
        // failed spawn must not take the host thread down with it: only this
        // job fails, with exit code 70 (EX_SOFTWARE).
        let fail_done = done.clone();
        let spawned = std::thread::Builder::new()
            .name("v8vm-guest".to_string())
            .spawn(move || {
                // Panics are caught so the payload can be resumed on the
                // calling thread (see run/serve) instead of vanishing here.
                let code = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    crate::execute(&source, &entry, &cfg, replies.as_ref())
                }));
                drop(replies); // close the reply stream before the code
                let _ = done.send(code);
            });
        if spawned.is_err() {
            eprintln!("v8vm: cannot spawn guest thread");
            let _ = fail_done.send(Ok(70));
        }
    }
}
