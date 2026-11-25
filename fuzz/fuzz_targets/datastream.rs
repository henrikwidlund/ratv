//! AirPlay data stream: an encrypted `sync` message with the rest of the fuzz
//! input as payload. The first byte holds flags:
//!
//! - bit 0: the payload starts with the plist prefix that the sender writes,
//!   so the known layout is reached more often;
//! - bit 1: an empty `rply` message comes first, which must be skipped;
//! - bit 2: the `sync` message is sent twice in one read.
//!
//! The MRP data must be the same as a non-empty `params.data` from a full
//! plist decode. Empty data gives none.
#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use ratv_airplay::FrameCipher;
use ratv_airplay::datastream::{DataStream, HEADER_LEN, MAX_MESSAGE_LEN};
use ratv_codec::bplist::{self, Value};

/// Plist start of `{"params": {"data": ...}}` up to the data object.
const PAYLOAD_START: &[u8] = b"bplist00\xD1\x01\x02\x56params\xD1\x03\x04\x54data";

/// Message with a header of `kind` and `payload`.
fn message(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut msg = vec![0u8; HEADER_LEN];
    msg.extend_from_slice(payload);
    let size = u32::try_from(msg.len()).expect("size checked by caller");
    msg[..4].copy_from_slice(&size.to_be_bytes());
    msg[4..8].copy_from_slice(kind);
    if kind == b"sync" {
        msg[16..20].copy_from_slice(b"comm");
    }
    msg
}

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let mut payload = Vec::new();
    if flags & 1 != 0 {
        payload.extend_from_slice(PAYLOAD_START);
    }
    payload.extend_from_slice(rest);
    if HEADER_LEN + payload.len() > MAX_MESSAGE_LEN {
        return;
    }
    let sync = message(b"sync", &payload);
    let copies = if flags & 4 != 0 { 2 } else { 1 };

    let mut plain = Vec::new();
    if flags & 2 != 0 {
        plain.extend_from_slice(&message(b"rply", &[]));
    }
    for _ in 0..copies {
        plain.extend_from_slice(&sync);
    }

    let (a, b) = ([1; 32], [2; 32]);
    let mut sender = FrameCipher::new(&a, &b);
    let mut wire = BytesMut::new();
    sender.seal(&plain, &mut wire);
    let mut stream = DataStream::new(FrameCipher::new(&b, &a), 1);
    stream.receive_buffer(wire.len()).extend_from_slice(&wire);

    let expected = bplist::decode(&payload).ok().and_then(|plist| {
        plist
            .get("params")
            .and_then(|p| p.get("data"))
            .and_then(Value::as_bytes)
            .filter(|d| !d.is_empty())
            .map(<[u8]>::to_vec)
    });
    // Messages without data are skipped, so `poll` returns each copy only
    // when there is data.
    let polls = if expected.is_some() { copies } else { 1 };
    for _ in 0..polls {
        match stream.poll() {
            Ok(got) => assert_eq!(got.as_deref(), expected.as_deref()),
            // An empty payload is not decoded, so only a full decode can fail.
            Err(_) => {
                assert!(!payload.is_empty() && bplist::decode(&payload).is_err());
                return;
            }
        }
    }
    if expected.is_some() {
        assert_eq!(stream.poll().ok().flatten(), None);
    }
});
