//! Checks allocations on hot paths. A test fails if a change adds
//! allocations. The counter is per thread.

use std::hint::black_box;

use allocation_counter::measure;
use bytes::BytesMut;
use ratv_mrp::proto::{NowPlayingInfo, SetStateMessage};
use ratv_mrp::{encode, frames};

fn set_state() -> SetStateMessage {
    SetStateMessage {
        now_playing_info: Some(NowPlayingInfo {
            artist: Some("Artist".into()),
            elapsed_time: Some(10.0),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn encode_into_used_buffer() {
    let msg = set_state();
    let mut out = BytesMut::new();
    // The first call sizes `out`.
    encode(&msg, None, &mut out);
    let info = measure(|| {
        out.clear();
        encode(black_box(&msg), None, &mut out);
    });
    assert_eq!(info.count_total, 0, "encode allocated {info:?}");
}

#[test]
fn frame_split() {
    let mut out = BytesMut::new();
    encode(&set_state(), None, &mut out);
    encode(&set_state(), Some("id"), &mut out);
    let bytes = out.freeze();
    // The first clone moves the buffer to shared storage.
    drop(bytes.clone());
    let info = measure(|| {
        for env in frames(black_box(bytes.clone())) {
            black_box(env.unwrap());
        }
    });
    assert_eq!(info.count_total, 0, "frame split allocated {info:?}");
}

#[test]
fn payload_decode() {
    let mut out = BytesMut::new();
    encode(&set_state(), None, &mut out);
    let env = frames(out.freeze()).next().unwrap().unwrap();
    // The first decode moves the buffer to shared storage.
    drop(env.payload::<SetStateMessage>());
    let info = measure(|| {
        black_box(env.payload::<SetStateMessage>().unwrap());
    });
    // The `artist` string.
    assert!(info.count_total <= 1, "payload decode allocated {info:?}");
}
