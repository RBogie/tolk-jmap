//! Device request bodies: decoding must never panic, and whatever decodes
//! must re-encode and decode to the same tree.
#![no_main]

use tolk::wbxml;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(root) = wbxml::decode(data) {
        let _ = wbxml::to_xml(&root);
        let again = wbxml::decode(&wbxml::encode(&root)).expect("own output decodes");
        assert_eq!(wbxml::encode(&again), wbxml::encode(&root));
    }
});
