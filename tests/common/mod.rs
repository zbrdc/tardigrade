//! Kernel-support checks shared by the test targets that run `--sandbox` for
//! real. Each target uses a subset of these helpers.
#![allow(dead_code)]

/// The Landlock ABI the sandbox needs: every filesystem right through
/// IoctlDev plus the signal and abstract-socket scopes (Linux 6.12).
pub const REQUIRED_LANDLOCK_ABI: i64 = 6;

/// `landlock_create_ruleset` flag that returns the ABI version instead of a
/// ruleset (linux/landlock.h).
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_ulong = 1;

/// The kernel's Landlock ABI version, or -1 when Landlock is unavailable.
pub fn landlock_abi() -> i64 {
    let rv = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0 as libc::size_t,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    rv as i64
}

pub fn kernel_supports_sandbox() -> bool {
    landlock_abi() >= REQUIRED_LANDLOCK_ABI
}

/// TARDIGRADE_REQUIRE_SANDBOX=1 turns "kernel lacks support" from a skip into a
/// failure, for hosts known to have it. CI does not set it: hosted runners
/// may run kernels older than 6.12.
pub fn sandbox_required() -> bool {
    std::env::var("TARDIGRADE_REQUIRE_SANDBOX").is_ok_and(|v| v == "1")
}

/// A skip notice that survives libtest's output capture (a direct stderr
/// write is not captured), so a skipped test is never mistaken for a pass.
pub fn skipped(what: &str) {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED (kernel lacks Landlock ABI {}): {}",
        REQUIRED_LANDLOCK_ABI,
        what
    );
}
