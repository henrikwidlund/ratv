//! Binary plist decoder. Decoded values must encode to bytes that decode to the same bytes again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_codec::bplist;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = bplist::decode(data) else {
        return;
    };
    let first = value.encode();
    let again = bplist::decode(&first).expect("encoded value must decode");
    assert_eq!(first, again.encode());
});
