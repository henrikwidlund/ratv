//! Credentials text. Parsed credentials must give the same text back, in lower case.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_hap::{Credentials, ExposeSecret};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let Ok(credentials) = text.parse::<Credentials>() else {
        return;
    };
    let again = credentials.to_secret_string();
    assert_eq!(again.expose_secret(), text.trim().to_ascii_lowercase());
    assert_eq!(again.expose_secret().parse::<Credentials>().unwrap(), credentials);
});
