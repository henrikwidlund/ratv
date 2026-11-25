//! Checks allocations of encode and decode calls. A test fails if a change
//! adds allocations. The counter is per thread.

use std::borrow::Cow;
use std::hint::black_box;

use allocation_counter::measure;
use ratv_codec::tlv8::{self, Tlv8};
use ratv_codec::{bplist, opack};

/// Runs `work` and fails if it allocates more than `max` times.
fn assert_allocations(what: &str, max: u64, work: impl FnOnce()) {
    let info = measure(work);
    assert!(
        info.count_total <= max,
        "{what} allocated {info:?}, expected at most {max}"
    );
}

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

/// Array of 40 strings, each written twice. The object table gets larger
/// than the size from which the decoder uses a hash set.
fn opack_large_table() -> opack::Value<'static> {
    let items = (0..40)
        .map(|i| opack::Value::Str(Cow::Owned(format!("app{i}"))))
        .collect::<Vec<_>>();
    let mut all = items.clone();
    all.extend(items);
    opack::Value::Array(all)
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

#[test]
fn opack() {
    let msg = opack_message();
    let mut out = Vec::new();
    msg.encode_into(&mut out);
    let bytes = out.clone();
    assert_allocations("opack encode into used buffer", 0, || {
        out.clear();
        black_box(&msg).encode_into(&mut out);
    });
    assert_allocations("opack encoder", 0, || {
        out.clear();
        opack::Encoder::new(&mut out).dict(2, |e| {
            e.str("_i").str("_iMC").str("_x").int(123_456);
        });
    });
    let scalar = opack::Value::from("_iMC").encode();
    assert_allocations("opack decode scalar", 0, || {
        black_box(opack::decode(black_box(&scalar)).unwrap());
    });
    // One for each container. The endless inner dict grows once.
    assert_allocations("opack decode", 3, || {
        black_box(opack::decode(black_box(&bytes)).unwrap());
    });
}

#[test]
fn opack_large_table_decode() {
    let bytes = opack_large_table().encode();
    // The endless array allocates 4 times, the table entries after the first
    // 16 allocate 4 times, and the hash set once.
    assert_allocations("opack decode large table", 9, || {
        black_box(opack::decode(black_box(&bytes)).unwrap());
    });
}

#[test]
fn tlv8() {
    let mut small = Vec::new();
    tlv8::write(&mut small, 6, &[2]);
    tlv8::write(&mut small, 3, &[7; 32]);
    tlv8::write(&mut small, 5, &[9; 120]);
    let mut fragmented = small.clone();
    tlv8::write(&mut fragmented, 9, &[1; 600]);

    let mut out = Vec::with_capacity(1024);
    assert_allocations("tlv8 write into sized buffer", 0, || {
        tlv8::write(&mut out, 9, black_box(&[1; 600]));
    });
    // The item list.
    assert_allocations("tlv8 decode", 1, || {
        black_box(Tlv8::decode(black_box(&small)).unwrap());
    });
    // The item list, and 3 for joining the fragments.
    assert_allocations("tlv8 decode fragmented", 4, || {
        black_box(Tlv8::decode(black_box(&fragmented)).unwrap());
    });
}

#[test]
fn bplist() {
    let msg = bplist_message();
    let mut out = Vec::new();
    msg.encode_into(&mut out);
    let bytes = out.clone();
    // The offset table.
    assert_allocations("bplist encode into used buffer", 1, || {
        out.clear();
        black_box(&msg).encode_into(&mut out);
    });
    // One for each container.
    assert_allocations("bplist decode", 3, || {
        black_box(bplist::decode(black_box(&bytes)).unwrap());
    });
}
