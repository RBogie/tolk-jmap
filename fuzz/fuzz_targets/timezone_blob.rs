//! `TIME_ZONE_INFORMATION` blobs from devices.
#![no_main]

use tolk::eas::tz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    use base64::Engine;
    let blob = base64::engine::general_purpose::STANDARD.encode(data);
    let _ = tz::parse_eas_timezone(&blob);
});
