//! Session cipher seal and open.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ratv_hap::cipher::{NonceLayout, SessionCipher};

fn cipher(c: &mut Criterion) {
    let mut g = c.benchmark_group("cipher");
    for len in [64, 1024] {
        g.throughput(Throughput::Bytes(len as u64));
        let mut sender = SessionCipher::new(&[1; 32], &[2; 32], NonceLayout::CounterFirst);
        let mut buf = vec![7u8; len];
        g.bench_function(format!("seal_{len}"), |b| {
            b.iter(|| sender.seal_in_place(b"", black_box(&mut buf)));
        });

        // Each open needs the next nonce, so seal and open in pairs.
        let mut sender = SessionCipher::new(&[1; 32], &[2; 32], NonceLayout::CounterFirst);
        let mut receiver = SessionCipher::new(&[2; 32], &[1; 32], NonceLayout::CounterFirst);
        g.bench_function(format!("seal_open_{len}"), |b| {
            b.iter(|| {
                let tag = sender.seal_in_place(b"", &mut buf);
                receiver
                    .open_in_place(b"", black_box(&mut buf), &tag)
                    .unwrap();
            });
        });
    }
    g.finish();
}

criterion_group!(benches, cipher);
criterion_main!(benches);
