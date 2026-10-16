#![no_main]
//! Fuzz snapshot restore: the JSON a --preload file or vm.restore call
//! carries is guest/operator-shaped and must only parse or refuse.
//! Run with: cargo +nightly fuzz run snapshot_restore
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    tardigrade::fuzz_hooks::snapshot_restore(data);
});
