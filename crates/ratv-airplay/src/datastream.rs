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
    /// `ProtocolMessage`s.
    pub fn send(&mut self, messages: &[u8]) {
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
    let plist = bplist::decode(payload)?;
    Ok(plist
        .get("params")
        .and_then(|p| p.get("data"))
        .and_then(Value::as_bytes)
        .filter(|d| !d.is_empty())
        .map(|d| msg.slice_ref(d)))
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
