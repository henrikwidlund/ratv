//! TXT record parser.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_discovery::Txt;

fuzz_target!(|data: &[u8]| {
    let txt = Txt::parse(data);
    let _ = txt.get("rpMd");
});
