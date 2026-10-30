//! OPACK, TLV8 and binary plist encode/decode.

use std::borrow::Cow;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ratv_codec::tlv8::{self, Tlv8};
use ratv_codec::{bplist, opack};

/// Companion event similar to a `_iMC` media control update.
fn opack_message() -> opack::Value<'static> {
    use opack::Value::{Dict, Int, Str};
    let s = |v: &'static str| Str(Cow::Borrowed(v));
    let content = (0..16)
        .map(|i| (s("key"), if i % 2 == 0 { Int(i) } else { s("value") }))
        .collect();
    Dict(vec![
        (s("_i"), s("_iMC")),
        (s("_x"), Int(123_456)),
        (s("_t"), Int(1)),
        (s("_c"), Dict(content)),
    ])
}

/// Pair-verify M2: state, 32-byte public key and a 120-byte encrypted item.
fn tlv8_message() -> Vec<u8> {
    let mut out = Vec::new();
    tlv8::write(&mut out, 6, &[2]);
    tlv8::write(&mut out, 3, &[7; 32]);
    tlv8::write(&mut out, 5, &[9; 120]);
    // Certificate larger than one item, so it is fragmented.
    tlv8::write(&mut out, 9, &[1; 600]);
    out
}

/// AirPlay data stream SETUP body.
fn bplist_message() -> bplist::Value<'static> {
    use bplist::Value::{Array, Bool, Dict, Int, String};
    let s = |v: &'static str| String(Cow::Borrowed(v));
    Dict(vec![(
        "streams".into(),
        Array(vec![Dict(vec![
            ("controlType".into(), Int(2)),
            (
                "channelID".into(),
                s("2B3F4C5D-1A2B-3C4D-5E6F-708192A3B4C5"),
            ),
            ("seed".into(), Int(0x1234_5678_9ABC)),
            (
                "clientUUID".into(),
                s("6A7B8C9D-1A2B-3C4D-5E6F-708192A3B4C5"),
            ),
            ("type".into(), Int(130)),
            ("wantsDedicatedSocket".into(), Bool(true)),
            (
                "clientTypeUUID".into(),
                s("1910A70F-DBC0-4242-AF95-115DB30604E1"),
            ),
        ])]),
    )])
}

fn opack(c: &mut Criterion) {
    let value = opack_message();
    let bytes = value.encode();
    let mut g = c.benchmark_group("opack");
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    let mut out = Vec::with_capacity(bytes.len());
    g.bench_function("encode", |b| {
        b.iter(|| {
            out.clear();
            black_box(&value).encode_into(&mut out);
        });
    });
    g.bench_function("decode", |b| b.iter(|| opack::decode(black_box(&bytes))));
    g.finish();
}

fn tlv8(c: &mut Criterion) {
    let bytes = tlv8_message();
    let mut g = c.benchmark_group("tlv8");
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    g.bench_function("decode", |b| b.iter(|| Tlv8::decode(black_box(&bytes))));
    g.finish();
}

fn bplist(c: &mut Criterion) {
    let value = bplist_message();
    let bytes = value.encode();
    let mut g = c.benchmark_group("bplist");
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    let mut out = Vec::with_capacity(bytes.len());
    g.bench_function("encode", |b| {
        b.iter(|| {
            out.clear();
            black_box(&value).encode_into(&mut out);
        });
    });
    g.bench_function("decode", |b| b.iter(|| bplist::decode(black_box(&bytes))));
    g.finish();
}

criterion_group!(benches, opack, tlv8, bplist);
criterion_main!(benches);
