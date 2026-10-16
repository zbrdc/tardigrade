#![no_main]
//! Fuzz the operator-flag parser and the private-address classifier:
//! both take untrusted text (a CLI flag, a resolved address) and must
//! only ever return a value or an error, never panic.
//!
//! Run with: cargo +nightly fuzz run parse_call
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(flag) = std::str::from_utf8(data) {
        if let Ok(call) = tardigrade::netcap::parse_call(flag) {
            // A parsed endpoint must round-trip its own scheme check.
            assert!(call.url.starts_with("http://") || call.url.starts_with("https://"));
            assert!(matches!(call.method.as_str(), "GET" | "POST" | "PUT"));
        }
    }
    if data.len() == 4 {
        let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(data[0], data[1], data[2], data[3]));
        let _ = tardigrade::netcap::addr_is_private(ip);
    }
    if data.len() == 16 {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(data);
        let _ = tardigrade::netcap::addr_is_private(std::net::IpAddr::V6(
            std::net::Ipv6Addr::from(octets),
        ));
    }
});
