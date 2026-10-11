//! The one host thread that owns V8 initialization and begets every guest
//! thread.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::LazyLock;

use crate::Config;

/// One guest run request. `lines` carries serve-mode output lines (already
/// newline-terminated) to the calling thread; `done` carries the exit code.
pub(crate) struct Job {
    pub(crate) source: String,
    pub(crate) entry: String,
    pub(crate) cfg: Config,
    pub(crate) lines: Option<Sender<String>>,
    pub(crate) done: Sender<i32>,
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
        // Spawn rather than run inline so concurrent callers overlap.
        std::thread::Builder::new()
            .name("v8vm-guest".to_string())
            .spawn(move || {
                let code = crate::execute(&job.source, &job.entry, &job.cfg, job.lines.as_ref());
                drop(job.lines); // close the line stream before reporting the code
                let _ = job.done.send(code);
            })
            .expect("spawn v8vm guest thread");
    }
}
