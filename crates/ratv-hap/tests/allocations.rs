//! Checks allocations on hot paths. A test fails if a change adds
//! allocations. The counter is per thread.

use std::hint::black_box;

use allocation_counter::measure;
use ratv_hap::cipher::{NonceLayout, SessionCipher};

#[test]
fn session_cipher() {
    let mut sender = SessionCipher::new(&[1; 32], &[2; 32], NonceLayout::CounterFirst);
    let mut receiver = SessionCipher::new(&[2; 32], &[1; 32], NonceLayout::CounterFirst);
    let mut buf = vec![7u8; 1024];
    let info = measure(|| {
        let tag = sender.seal_in_place(b"aad", black_box(&mut buf));
        receiver
            .open_in_place(b"aad", black_box(&mut buf), &tag)
            .unwrap();
    });
    assert_eq!(info.count_total, 0, "seal and open allocated {info:?}");
}
