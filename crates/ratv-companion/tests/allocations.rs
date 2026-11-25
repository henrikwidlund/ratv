//! Checks allocations on hot paths. A test fails if a change adds
//! allocations. The counter is per thread.

use std::hint::black_box;

use allocation_counter::measure;
use ratv_companion::{Connection, FrameType};
use ratv_hap::SharedSecret;

/// Measured rounds. Enough to show that the buffers reuse their space.
const ROUNDS: usize = 64;

fn pair() -> (Connection, Connection) {
    let secret = SharedSecret::from_bytes(&[9; 32]);
    let mut client = Connection::new();
    client.enable_encryption(&secret);
    let mut device = Connection::new();
    device.enable_device_encryption(&secret);
    (client, device)
}

#[test]
fn frame_roundtrip() {
    let (mut client, mut device) = pair();
    let payload = [0x42u8; 200];
    let mut round = || {
        client.send(FrameType::E_OPACK, black_box(&payload));
        device.receive(&client.take_transmit());
        black_box(device.poll_frame().unwrap().unwrap());
    };
    // The first round moves `rx` and `tx` to shared storage.
    round();
    let info = measure(|| (0..ROUNDS).for_each(|_| round()));
    assert_eq!(info.count_total, 0, "frame roundtrip allocated {info:?}");
}

#[test]
fn split_frame_through_receive_buffer() {
    let (mut client, mut device) = pair();
    let payload = [0x42u8; 200];
    let mut round = || {
        client.send(FrameType::E_OPACK, black_box(&payload));
        let wire = client.take_transmit();
        let (first, second) = wire.split_at(wire.len() / 2);
        device.receive_buffer(4096).extend_from_slice(first);
        assert!(device.poll_frame().unwrap().is_none());
        device.receive_buffer(4096).extend_from_slice(second);
        black_box(device.poll_frame().unwrap().unwrap());
    };
    // The first round grows `rx` and moves `rx` and `tx` to shared storage.
    round();
    let info = measure(|| (0..ROUNDS).for_each(|_| round()));
    assert_eq!(info.count_total, 0, "split frame allocated {info:?}");
}
