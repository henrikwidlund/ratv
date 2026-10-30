//! Encrypted Companion frames: send, then receive on the device side.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ratv_companion::{Connection, FrameType};
use ratv_hap::SharedSecret;

fn connection(c: &mut Criterion) {
    let secret = SharedSecret::from_bytes(&[9; 32]);
    let mut client = Connection::new();
    client.enable_encryption(&secret);
    let mut device = Connection::new();
    device.enable_device_encryption(&secret);
    let payload = vec![0x42u8; 200];

    let mut g = c.benchmark_group("companion");
    g.throughput(Throughput::Bytes(payload.len() as u64));
    g.bench_function("frame_roundtrip", |b| {
        b.iter(|| {
            client.send(FrameType::E_OPACK, black_box(&payload));
            device.receive(&client.take_transmit());
            device.poll_frame().unwrap().unwrap()
        });
    });
    g.finish();
}

criterion_group!(benches, connection);
criterion_main!(benches);
