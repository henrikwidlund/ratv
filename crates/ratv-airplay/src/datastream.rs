//! AirPlay data stream: carries MRP messages.
//!
//! Each message has a 32-byte header:
//!
//! | bytes | field |
//! |-------|-------|
//! | 4     | total size, big-endian, header included |
//! | 12    | type: `sync` or `rply`, zero padded |
//! | 4     | command: `comm` or zeros |
//! | 8     | sequence number, big-endian |
//! | 4     | zero |
//!
//! The payload of a `sync` message is a binary plist `{params: {data: ...}}`,
//! where `data` holds length-prefixed MRP messages. Every `sync` is answered
//! with an empty `rply` with the same sequence number.

use bytes::{Bytes, BytesMut};
use ratv_codec::bplist::{self, Value};

use crate::{Error, FrameCipher};

/// Header size.
pub const HEADER_LEN: usize = 32;

/// Maximum message size accepted.
pub const MAX_MESSAGE_LEN: usize = 4 * 1024 * 1024;

const SYNC: &[u8; 12] = b"sync\0\0\0\0\0\0\0\0";
const REPLY: &[u8; 12] = b"rply\0\0\0\0\0\0\0\0";
const COMMAND: &[u8; 4] = b"comm";

/// Data stream connection state.
#[derive(Debug)]
pub struct DataStream {
    cipher: FrameCipher,
    seqno: u64,
    rx: BytesMut,
    plain: BytesMut,
    tx: BytesMut,
    scratch: Vec<u8>,
}

impl DataStream {
    /// Creates the state. `first_seqno` is the sequence number of the first
    /// message we send; use a random value.
    #[must_use]
    pub fn new(cipher: FrameCipher, first_seqno: u64) -> Self {
        Self {
            cipher,
            seqno: first_seqno,
            rx: BytesMut::new(),
            plain: BytesMut::new(),
            tx: BytesMut::new(),
            scratch: Vec::new(),
        }
    }

    /// Buffer for received bytes, with room for at least `min` more bytes.
    pub fn receive_buffer(&mut self, min: usize) -> &mut BytesMut {
        self.rx.reserve(min);
        &mut self.rx
    }

    /// Queues MRP messages. `messages` holds one or more length-prefixed
    /// `ProtocolMessage`s. Does nothing if `messages` is empty.
    pub fn send(&mut self, messages: &[u8]) {
        if messages.is_empty() {
            return;
        }
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        buf.resize(HEADER_LEN, 0);
        write_payload(messages, &mut buf);
        let size = u32::try_from(buf.len()).expect("message size fits in u32");
        write_header(&mut buf[..HEADER_LEN], size, SYNC, *COMMAND, self.seqno);
        self.seqno = self.seqno.wrapping_add(1);
        self.cipher.seal(&buf, &mut self.tx);
        self.scratch = buf;
    }

    /// Next received MRP data (length-prefixed messages). Replies to `sync`
    /// messages are queued for sending.
    pub fn poll(&mut self) -> Result<Option<Bytes>, Error> {
        self.cipher.open(&mut self.rx, &mut self.plain)?;
        while self.plain.len() >= HEADER_LEN {
            let size = u32::from_be_bytes(self.plain[..4].try_into().expect("4 bytes")) as usize;
            if !(HEADER_LEN..=MAX_MESSAGE_LEN).contains(&size) {
                return Err(Error::TooLarge(size));
            }
            if self.plain.len() < size {
                self.plain.reserve(size - self.plain.len());
                return Ok(None);
            }
            let msg = self.plain.split_to(size).freeze();
            let kind = &msg[4..8];
            if kind == b"rply" {
                continue;
            }
            if kind == b"sync" {
                let seqno = u64::from_be_bytes(msg[20..28].try_into().expect("8 bytes"));
                let mut reply = [0u8; HEADER_LEN];
                write_header(&mut reply, HEADER_LEN as u32, REPLY, [0; 4], seqno);
                self.cipher.seal(&reply, &mut self.tx);
            }
            if let Some(data) = mrp_data(&msg)? {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }

    /// True if bytes are waiting to be sent.
    #[must_use]
    pub fn has_transmit(&self) -> bool {
        !self.tx.is_empty()
    }

    /// Takes the bytes to send.
    pub fn take_transmit(&mut self) -> BytesMut {
        self.tx.split()
    }
}

/// Start of the payload plist up to the data object: header, the outer dict,
/// `"params"`, the inner dict and `"data"`. Dicts hold one key and value
/// as 1-byte object numbers.
const PAYLOAD_START: &[u8] = b"bplist00\xD1\x01\x02\x56params\xD1\x03\x04\x54data";
/// Offsets of objects 0 to 4 in the plist. Object 4 is the data.
const PAYLOAD_OFFSETS: [u8; 5] = [8, 11, 18, 21, 26];

/// Writes the binary plist `{"params": {"data": data}}` without building a value tree.
fn write_payload(data: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.reserve(PAYLOAD_START.len() + 9 + data.len() + PAYLOAD_OFFSETS.len() + 32);
    out.extend_from_slice(PAYLOAD_START);
    let len = data.len();
    if len < 15 {
        // Short data has its length in the marker.
        out.push(0x40 | len as u8);
    } else {
        // Long data: marker 0x4F, then the length as an integer object.
        out.push(0x4F);
        if let Ok(n) = u8::try_from(len) {
            out.extend_from_slice(&[0x10, n]);
        } else if let Ok(n) = u16::try_from(len) {
            out.push(0x11);
            out.extend_from_slice(&n.to_be_bytes());
        } else if let Ok(n) = u32::try_from(len) {
            out.push(0x12);
            out.extend_from_slice(&n.to_be_bytes());
        } else {
            out.push(0x13);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(data);
    let table = (out.len() - start) as u64;
    // The device needs offset entries large enough for the table position,
    // although all objects start in the first bytes.
    let offset_size: u8 = match table {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    };
    for offset in PAYLOAD_OFFSETS {
        let be = u64::from(offset).to_be_bytes();
        out.extend_from_slice(&be[8 - usize::from(offset_size)..]);
    }
    // Trailer: 6 unused bytes, offset size, reference size 1, object
    // count, top object 0, offset table position.
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, offset_size, 1]);
    out.extend_from_slice(&(PAYLOAD_OFFSETS.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&table.to_be_bytes());
}

fn write_header(out: &mut [u8], size: u32, kind: &[u8; 12], command: [u8; 4], seqno: u64) {
    out[..4].copy_from_slice(&size.to_be_bytes());
    out[4..16].copy_from_slice(kind);
    out[16..20].copy_from_slice(&command);
    out[20..28].copy_from_slice(&seqno.to_be_bytes());
    out[28..32].fill(0);
}

/// `params.data` of a message, as a slice of `msg`.
fn mrp_data(msg: &Bytes) -> Result<Option<Bytes>, Error> {
    let payload = &msg[HEADER_LEN..];
    if payload.is_empty() {
        return Ok(None);
    }
    if let Some(data) = known_layout_data(payload) {
        return Ok((!data.is_empty()).then(|| msg.slice_ref(data)));
    }
    let plist = bplist::decode(payload)?;
    Ok(plist
        .get("params")
        .and_then(|p| p.get("data"))
        .and_then(Value::as_bytes)
        .filter(|d| !d.is_empty())
        .map(|d| msg.slice_ref(d)))
}

/// `params.data` without decoding, if `payload` has the object layout that
/// [`write_payload`] writes, with any integer and offset width. `None` for
/// any other layout.
fn known_layout_data(payload: &[u8]) -> Option<&[u8]> {
    let rest = payload.strip_prefix(PAYLOAD_START)?;
    let (&marker, rest) = rest.split_first()?;
    let (len, rest) = match marker {
        0x40..=0x4E => (usize::from(marker & 0x0F), rest),
        0x4F => {
            let (&int, rest) = rest.split_first()?;
            let width = match int {
                0x10 => 1,
                0x11 => 2,
                0x12 => 4,
                0x13 => 8,
                _ => return None,
            };
            let (n, rest) = rest.split_at_checked(width)?;
            let mut be = [0u8; 8];
            be[8 - width..].copy_from_slice(n);
            (usize::try_from(u64::from_be_bytes(be)).ok()?, rest)
        }
        _ => return None,
    };
    let (data, tail) = rest.split_at_checked(len)?;
    let table = payload.len() - tail.len();
    let (offsets, trailer) = tail.split_at_checked(tail.len().checked_sub(32)?)?;
    let offset_size = usize::from(trailer[6]);
    let layout_ok = matches!(offset_size, 1 | 2 | 4 | 8)
        && trailer[7] == 1
        && trailer[8..16] == (PAYLOAD_OFFSETS.len() as u64).to_be_bytes()
        && trailer[16..24] == [0; 8]
        && trailer[24..32] == (table as u64).to_be_bytes()
        && offsets.len() == offset_size * PAYLOAD_OFFSETS.len()
        && offsets
            .chunks_exact(offset_size)
            .zip(PAYLOAD_OFFSETS)
            .all(|(entry, want)| {
                entry[offset_size - 1] == want && entry[..offset_size - 1].iter().all(|&b| b == 0)
            });
    layout_ok.then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (DataStream, DataStream) {
        let (a, b) = ([3; 32], [4; 32]);
        (
            DataStream::new(FrameCipher::new(&a, &b), 0x1_0000_0005),
            DataStream::new(FrameCipher::new(&b, &a), 77),
        )
    }

    fn deliver(from: &mut DataStream, to: &mut DataStream) {
        let bytes = from.take_transmit();
        to.receive_buffer(bytes.len()).extend_from_slice(&bytes);
    }

    /// `params.data` from a full plist decode.
    fn decoded_data(payload: &[u8]) -> Option<Vec<u8>> {
        bplist::decode(payload).ok().and_then(|plist| {
            plist
                .get("params")
                .and_then(|p| p.get("data"))
                .and_then(Value::as_bytes)
                .map(<[u8]>::to_vec)
        })
    }

    #[test]
    fn known_layout_matches_decoder() {
        for len in [0, 14, 15, 255, 256, 65_535, 65_536, 100_000] {
            let data: Vec<u8> = (0..=255u8).cycle().take(len).collect();
            let mut payload = Vec::new();
            write_payload(&data, &mut payload);
            assert_eq!(
                known_layout_data(&payload),
                Some(data.as_slice()),
                "len {len}"
            );
            assert_eq!(decoded_data(&payload).as_deref(), Some(data.as_slice()));
        }
    }

    #[test]
    fn other_layout_is_decoded() {
        // An extra key changes the layout: the full decoder still finds the data.
        let value = Value::Dict(vec![
            ("x".into(), Value::Int(1)),
            (
                "params".into(),
                Value::Dict(vec![("data".into(), Value::Data([1, 2, 3][..].into()))]),
            ),
        ]);
        let payload = value.encode();
        assert_eq!(known_layout_data(&payload), None);
        let mut msg = vec![0; HEADER_LEN];
        msg.extend_from_slice(&payload);
        let data = mrp_data(&Bytes::from(msg)).unwrap().unwrap();
        assert_eq!(&data[..], [1, 2, 3]);
    }

    #[test]
    fn known_layout_with_widest_fields() {
        // 8-byte data length and 8-byte offset entries are valid, though
        // `write_payload` uses smaller ones.
        let data = [5u8; 20];
        let mut payload = PAYLOAD_START.to_vec();
        payload.extend_from_slice(&[0x4F, 0x13]);
        payload.extend_from_slice(&(data.len() as u64).to_be_bytes());
        payload.extend_from_slice(&data);
        let table = payload.len() as u64;
        for offset in PAYLOAD_OFFSETS {
            payload.extend_from_slice(&u64::from(offset).to_be_bytes());
        }
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 8, 1]);
        payload.extend_from_slice(&(PAYLOAD_OFFSETS.len() as u64).to_be_bytes());
        payload.extend_from_slice(&0u64.to_be_bytes());
        payload.extend_from_slice(&table.to_be_bytes());
        assert_eq!(known_layout_data(&payload), Some(&data[..]));
        assert_eq!(decoded_data(&payload).as_deref(), Some(&data[..]));
    }

    #[test]
    fn tvos_sync_payload() {
        // Received from tvOS 27.2 (Apple TV 4K) in a `sync` message.
        let payload: &[u8] = &[
            0x62, 0x70, 0x6c, 0x69, 0x73, 0x74, 0x30, 0x30, 0xd1, 0x01, 0x02, 0x56, 0x70, 0x61,
            0x72, 0x61, 0x6d, 0x73, 0xd1, 0x03, 0x04, 0x54, 0x64, 0x61, 0x74, 0x61, 0x4f, 0x10,
            0x35, 0x34, 0x08, 0x32, 0x12, 0x01, 0x33, 0xb2, 0x03, 0x05, 0x0d, 0x00, 0x00, 0x00,
            0x00, 0xaa, 0x05, 0x24, 0x37, 0x38, 0x43, 0x42, 0x31, 0x37, 0x38, 0x46, 0x2d, 0x41,
            0x42, 0x42, 0x34, 0x2d, 0x34, 0x36, 0x36, 0x41, 0x2d, 0x42, 0x31, 0x30, 0x30, 0x2d,
            0x46, 0x38, 0x34, 0x33, 0x33, 0x37, 0x35, 0x42, 0x31, 0x30, 0x42, 0x43, 0x08, 0x0b,
            0x12, 0x15, 0x1a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x52,
        ];
        let data = &payload[29..82];
        assert_eq!(known_layout_data(payload), Some(data));
        assert_eq!(decoded_data(payload).as_deref(), Some(data));
        let mut written = Vec::new();
        write_payload(data, &mut written);
        assert_eq!(written, payload);
    }

    #[test]
    fn empty_data_is_none() {
        let mut msg = vec![0; HEADER_LEN];
        write_payload(&[], &mut msg);
        assert_eq!(known_layout_data(&msg[HEADER_LEN..]), Some(&[][..]));
        assert_eq!(mrp_data(&Bytes::from(msg)).unwrap(), None);
    }

    proptest::proptest! {
        /// A changed, removed or added byte, or a cut-off end, either keeps the
        /// known layout with the same result as the decoder, or makes the
        /// layout unknown.
        #[test]
        fn known_layout_never_differs_from_decoder(
            data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300),
            index in proptest::prelude::any::<proptest::sample::Index>(),
            byte in proptest::prelude::any::<u8>(),
            change in 0u8..4,
        ) {
            let mut payload = Vec::new();
            write_payload(&data, &mut payload);
            let i = index.index(payload.len());
            match change {
                0 => payload[i] = byte,
                1 => {
                    payload.remove(i);
                }
                2 => payload.truncate(i),
                _ => payload.insert(i, byte),
            }
            if let Some(fast) = known_layout_data(&payload) {
                proptest::prop_assert_eq!(Some(fast.to_vec()), decoded_data(&payload));
            }
        }
    }

    #[test]
    fn payload_matches_plist_encoder() {
        for len in [0, 14, 15, 200, 254, 255, 256, 1000, 65_535, 65_536, 100_000] {
            let data: Vec<u8> = (0..=255u8).cycle().take(len).collect();
            let value = Value::Dict(vec![(
                "params".into(),
                Value::Dict(vec![("data".into(), Value::Data(data.as_slice().into()))]),
            )]);
            let mut new = Vec::new();
            write_payload(&data, &mut new);
            assert_eq!(new, value.encode(), "len {len}");
        }
    }

    #[test]
    fn payload_is_valid_plist() {
        for len in [0, 1, 14, 15, 255, 256, 65_535, 65_536] {
            let data: Vec<u8> = (0..=255u8).cycle().take(len).collect();
            let mut out = Vec::new();
            write_payload(&data, &mut out);
            let v = bplist::decode(&out).unwrap();
            let got = v.get("params").and_then(|p| p.get("data"));
            assert_eq!(got.and_then(Value::as_bytes), Some(&data[..]), "len {len}");
        }
    }

    #[test]
    fn send_and_reply() {
        let (mut client, mut device) = pair();
        client.send(b"\x02\x08\x01");
        client.send(&[0x55; 5000]);
        assert!(client.has_transmit());
        deliver(&mut client, &mut device);

        assert_eq!(&device.poll().unwrap().unwrap()[..], b"\x02\x08\x01");
        assert_eq!(device.poll().unwrap().unwrap().len(), 5000);
        assert!(device.poll().unwrap().is_none());

        // The device answered both `sync` messages.
        deliver(&mut device, &mut client);
        assert!(client.poll().unwrap().is_none());
        assert!(!client.has_transmit());
    }

    #[test]
    fn send_empty_does_nothing() {
        let (mut client, _) = pair();
        client.send(b"");
        assert!(!client.has_transmit());
    }

    #[test]
    fn header_layout() {
        let (mut client, _) = pair();
        client.send(b"x");
        let mut wire = client.take_transmit();
        let mut dec = FrameCipher::new(&[4; 32], &[3; 32]);
        let mut plain = BytesMut::new();
        dec.open(&mut wire, &mut plain).unwrap();
        let size = u32::from_be_bytes(plain[..4].try_into().unwrap()) as usize;
        assert_eq!(size, plain.len());
        assert_eq!(&plain[4..16], SYNC);
        assert_eq!(&plain[16..20], b"comm");
        assert_eq!(
            u64::from_be_bytes(plain[20..28].try_into().unwrap()),
            0x1_0000_0005
        );
        let v = bplist::decode(&plain[32..]).unwrap();
        assert_eq!(
            v.get("params")
                .and_then(|p| p.get("data"))
                .and_then(Value::as_bytes),
            Some(&b"x"[..])
        );
    }

    #[test]
    fn reply_echoes_seqno() {
        let (mut client, mut device) = pair();
        client.send(b"a");
        deliver(&mut client, &mut device);
        device.poll().unwrap();
        let mut wire = device.take_transmit();
        let mut dec = FrameCipher::new(&[3; 32], &[4; 32]);
        let mut plain = BytesMut::new();
        dec.open(&mut wire, &mut plain).unwrap();
        assert_eq!(plain.len(), HEADER_LEN);
        assert_eq!(&plain[4..16], REPLY);
        assert_eq!(
            u64::from_be_bytes(plain[20..28].try_into().unwrap()),
            0x1_0000_0005
        );
    }

    #[test]
    fn partial_and_invalid() {
        let (mut client, mut device) = pair();
        client.send(&[1; 2000]);
        let bytes = client.take_transmit();
        for chunk in bytes.chunks(500) {
            assert!(device.poll().unwrap().is_none());
            device.receive_buffer(chunk.len()).extend_from_slice(chunk);
        }
        assert_eq!(device.poll().unwrap().unwrap().len(), 2000);

        // A header with an impossible size.
        let mut enc = FrameCipher::new(&[3; 32], &[4; 32]);
        let mut wire = BytesMut::new();
        let mut bad = [0u8; HEADER_LEN];
        write_header(&mut bad, 8, SYNC, *COMMAND, 1);
        enc.seal(&bad, &mut wire);
        let mut dev = DataStream::new(FrameCipher::new(&[4; 32], &[3; 32]), 0);
        dev.receive_buffer(wire.len()).extend_from_slice(&wire);
        assert!(matches!(dev.poll(), Err(Error::TooLarge(8))));
    }

    #[test]
    fn sync_without_data_is_skipped() {
        let mut enc = FrameCipher::new(&[3; 32], &[4; 32]);
        let mut wire = BytesMut::new();
        let mut msg = vec![0u8; HEADER_LEN];
        Value::Dict(vec![("params".into(), Value::Dict(vec![]))]).encode_into(&mut msg);
        let n = msg.len() as u32;
        write_header(&mut msg[..HEADER_LEN], n, SYNC, *COMMAND, 9);
        enc.seal(&msg, &mut wire);
        let mut dev = DataStream::new(FrameCipher::new(&[4; 32], &[3; 32]), 0);
        dev.receive_buffer(wire.len()).extend_from_slice(&wire);
        assert!(dev.poll().unwrap().is_none());
        assert!(dev.has_transmit(), "reply is still sent");
    }
}
