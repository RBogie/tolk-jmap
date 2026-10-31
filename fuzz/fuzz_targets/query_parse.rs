//! Request lines, plain and base64 encoded.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(query) = std::str::from_utf8(data) {
        let _ = tolk::eas::query::parse(Some(query));
    }
});
