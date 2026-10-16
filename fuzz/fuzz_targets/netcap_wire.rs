#![no_main]
//! Fuzz the netcap wire: the capped line reader and the sidecar's
//! JSON reply shaping, with bytes a broken sidecar could send.
//! Run with: cargo +nightly fuzz run netcap_wire
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    tardigrade::netcap::fuzz_wire(data);
});
