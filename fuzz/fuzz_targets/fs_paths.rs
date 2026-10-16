#![no_main]
//! Fuzz RAM-filesystem path handling and quota accounting with
//! NUL-separated path-shaped chunks.
//! Run with: cargo +nightly fuzz run fs_paths
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    tardigrade::fuzz_hooks::fs_paths(data);
});
