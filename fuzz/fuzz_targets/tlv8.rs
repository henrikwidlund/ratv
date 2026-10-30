//! TLV8 decoder.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_codec::tlv8::Tlv8;

fuzz_target!(|data: &[u8]| {
    if let Ok(tlv) = Tlv8::decode(data) {
        for ty in 0..=u8::MAX {
            let _ = tlv.get(ty);
        }
    }
});
