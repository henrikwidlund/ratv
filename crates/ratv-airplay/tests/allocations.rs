//! Checks allocations on hot paths. A test fails if a change adds
//! allocations. The counter is per thread.

use std::hint::black_box;

use allocation_counter::measure;
use bytes::BytesMut;
use ratv_airplay::FrameCipher;
use ratv_airplay::datastream::DataStream;

/// One MRP message with a varint length prefix.
const MESSAGES: [u8; 5] = [4, 1, 2, 3, 4];

/// One MRP message of 199 bytes. Data of 15 bytes or more has its length
/// in a separate field of the payload plist.
const LONG_MESSAGES: [u8; 201] = {
    let mut m = [7u8; 201];
    m[0] = 0xC7;
    m[1] = 0x01;
    m
};

/// Measured rounds. Enough to show that the buffers reuse their space.
const ROUNDS: usize = 64;

fn pair() -> (DataStream, DataStream) {
    let (a, b) = ([1; 32], [2; 32]);
    (
        DataStream::new(FrameCipher::new(&a, &b), 1),
        DataStream::new(FrameCipher::new(&b, &a), 1),
    )
}

#[test]
fn frame_cipher() {
    let (a, b) = ([1; 32], [2; 32]);
    let mut sender = FrameCipher::new(&a, &b);
    let mut receiver = FrameCipher::new(&b, &a);
    let plain = [7u8; 3000];
    let mut wire = BytesMut::with_capacity(4096);
    let mut out = BytesMut::with_capacity(4096);
    let mut round = || {
        sender.seal(black_box(&plain), &mut wire);
        receiver.open(&mut wire, &mut out).unwrap();
        out.clear();
    };
    // The first round moves `wire` to shared storage.
    round();
    let info = measure(|| (0..ROUNDS).for_each(|_| round()));
    assert_eq!(info.count_total, 0, "frame cipher allocated {info:?}");
}

#[test]
fn data_stream_send() {
    let (mut stream, _) = pair();
    let mut round = || {
        stream.send(black_box(&MESSAGES));
        black_box(stream.take_transmit());
    };
    // The first round sizes the buffers and moves `tx` to shared storage.
    round();
    let info = measure(|| (0..ROUNDS).for_each(|_| round()));
    assert_eq!(info.count_total, 0, "data stream send allocated {info:?}");
}

#[test]
fn data_stream_roundtrip() {
    let (mut sender, mut receiver) = pair();
    let mut round = || {
        for messages in [&MESSAGES[..], &LONG_MESSAGES[..]] {
            sender.send(black_box(messages));
            let wire = sender.take_transmit();
            receiver.receive_buffer(4096).extend_from_slice(&wire);
            let data = receiver.poll().unwrap().unwrap();
            assert_eq!(&data[..], messages);
            // The reply to the `sync` message goes back to the sender.
            let reply = receiver.take_transmit();
            sender.receive_buffer(4096).extend_from_slice(&reply);
            assert!(sender.poll().unwrap().is_none());
        }
    };
    // The first round sizes the buffers and moves them to shared storage.
    round();
    let info = measure(|| (0..ROUNDS).for_each(|_| round()));
    assert_eq!(
        info.count_total, 0,
        "data stream roundtrip allocated {info:?}"
    );
}

#[test]
fn data_stream_split_read() {
    let (mut sender, mut receiver) = pair();
    let mut round = || {
        sender.send(black_box(&MESSAGES));
        let wire = sender.take_transmit();
        let (first, second) = wire.split_at(wire.len() / 2);
        receiver.receive_buffer(4096).extend_from_slice(first);
        assert!(receiver.poll().unwrap().is_none());
        receiver.receive_buffer(4096).extend_from_slice(second);
        let data = receiver.poll().unwrap().unwrap();
        assert_eq!(&data[..], MESSAGES);
        black_box(receiver.take_transmit());
    };
    // The first round grows `rx` and moves the buffers to shared storage.
    round();
    let info = measure(|| (0..ROUNDS).for_each(|_| round()));
    assert_eq!(info.count_total, 0, "split read allocated {info:?}");
}
