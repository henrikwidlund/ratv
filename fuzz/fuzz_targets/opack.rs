//! OPACK decoder. Decoded values must encode to bytes that decode to the same bytes again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_codec::opack;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = opack::decode(data) else {
        return;
    };
    let first = value.encode();
    let again = opack::decode(&first).expect("encoded value must decode");
    assert_eq!(first, again.encode());
});
