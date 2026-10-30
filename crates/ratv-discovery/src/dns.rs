//! Minimal DNS message encoding and parsing (RFC 1035), enough for mDNS service discovery.

use std::net::{Ipv4Addr, Ipv6Addr};

const HEADER_LEN: usize = 12;
const MAX_POINTERS: usize = 64;
const MAX_NAME_LEN: usize = 255;

/// DNS parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Message ends early.
    #[error("truncated DNS message")]
    Truncated,
    /// Name is malformed or has a pointer loop.
    #[error("invalid DNS name")]
    InvalidName,
}

/// DNS record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordType(pub u16);

#[allow(missing_docs)]
impl RecordType {
    pub const A: Self = Self(1);
    pub const PTR: Self = Self(12);
    pub const TXT: Self = Self(16);
    pub const AAAA: Self = Self(28);
    pub const SRV: Self = Self(33);
    pub const ANY: Self = Self(255);
}

/// Builder for a query message.
#[derive(Debug, Clone)]
pub struct Query {
    buf: Vec<u8>,
    count: u16,
}

impl Default for Query {
    fn default() -> Self {
        Self::new()
    }
}

impl Query {
    /// Creates an empty query with id 0.
    #[must_use]
    pub fn new() -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.resize(HEADER_LEN, 0);
        Self { buf, count: 0 }
    }

    /// Adds a question of class IN.
    pub fn question(&mut self, name: &str, ty: RecordType) -> &mut Self {
        write_name(&mut self.buf, name);
        self.buf.extend_from_slice(&ty.0.to_be_bytes());
        self.buf.extend_from_slice(&1u16.to_be_bytes());
        self.count += 1;
        self
    }

    /// Returns the encoded message.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.buf[4..6].copy_from_slice(&self.count.to_be_bytes());
        self.buf
    }
}

/// Writes `name` as uncompressed labels. A trailing dot is optional.
pub fn write_name(out: &mut Vec<u8>, name: &str) {
    for label in name
        .trim_end_matches('.')
        .split('.')
        .filter(|l| !l.is_empty())
    {
        let bytes = &label.as_bytes()[..label.len().min(63)];
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out.push(0);
}

/// Record data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RData<'a> {
    /// IPv4 address.
    A(Ipv4Addr),
    /// IPv6 address.
    Aaaa(Ipv6Addr),
    /// Pointer to a service instance name.
    Ptr(String),
    /// Service location.
    Srv {
        /// Port.
        port: u16,
        /// Host name.
        target: String,
    },
    /// Raw TXT data (length-prefixed strings).
    Txt(&'a [u8]),
    /// Other record types.
    Other,
}

/// A resource record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record<'a> {
    /// Owner name without trailing dot.
    pub name: String,
    /// Type.
    pub ty: RecordType,
    /// Time to live in seconds. 0 means the record was removed.
    pub ttl: u32,
    /// Data.
    pub data: RData<'a>,
}

/// Parses all answer, authority and additional records of a message.
pub fn parse(msg: &[u8]) -> Result<Vec<Record<'_>>, Error> {
    let header = msg.get(..HEADER_LEN).ok_or(Error::Truncated)?;
    let count = |i: usize| usize::from(u16::from_be_bytes([header[i], header[i + 1]]));
    let questions = count(4);
    let records = count(6) + count(8) + count(10);
    let mut pos = HEADER_LEN;
    for _ in 0..questions {
        pos = skip_name(msg, pos)? + 4;
    }
    let mut out = Vec::with_capacity(records.min(64));
    for _ in 0..records {
        let (name, next) = read_name(msg, pos)?;
        let fixed = msg.get(next..next + 10).ok_or(Error::Truncated)?;
        let ty = RecordType(u16::from_be_bytes([fixed[0], fixed[1]]));
        let ttl = u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]);
        let len = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
        let start = next + 10;
        let rdata = msg.get(start..start + len).ok_or(Error::Truncated)?;
        let data = match ty {
            RecordType::A if len == 4 => {
                RData::A(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]))
            }
            RecordType::AAAA if len == 16 => RData::Aaaa(Ipv6Addr::from(
                <[u8; 16]>::try_from(rdata).expect("16 bytes"),
            )),
            RecordType::PTR => RData::Ptr(read_name(msg, start)?.0),
            RecordType::SRV if len >= 7 => RData::Srv {
                port: u16::from_be_bytes([rdata[4], rdata[5]]),
                target: read_name(msg, start + 6)?.0,
            },
            RecordType::TXT => RData::Txt(rdata),
            _ => RData::Other,
        };
        out.push(Record {
            name,
            ty,
            ttl,
            data,
        });
        pos = start + len;
    }
    Ok(out)
}

fn skip_name(msg: &[u8], mut pos: usize) -> Result<usize, Error> {
    loop {
        let len = *msg.get(pos).ok_or(Error::Truncated)?;
        match len {
            0 => return Ok(pos + 1),
            l if l & 0xC0 == 0xC0 => return Ok(pos + 2),
            l if l & 0xC0 == 0 => pos += 1 + usize::from(l),
            _ => return Err(Error::InvalidName),
        }
    }
}

/// Reads a possibly compressed name. Returns it (dot separated, no trailing dot)
/// and the position after the name in the original data.
fn read_name(msg: &[u8], mut pos: usize) -> Result<(String, usize), Error> {
    let mut name = String::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos).ok_or(Error::Truncated)?;
        match len {
            0 => {
                return Ok((name, end.unwrap_or(pos + 1)));
            }
            l if l & 0xC0 == 0xC0 => {
                let b2 = *msg.get(pos + 1).ok_or(Error::Truncated)?;
                end.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > MAX_POINTERS {
                    return Err(Error::InvalidName);
                }
                pos = usize::from(u16::from_be_bytes([l & 0x3F, b2]));
            }
            l if l & 0xC0 == 0 => {
                let label = msg
                    .get(pos + 1..pos + 1 + usize::from(l))
                    .ok_or(Error::Truncated)?;
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(&String::from_utf8_lossy(label));
                if name.len() > MAX_NAME_LEN {
                    return Err(Error::InvalidName);
                }
                pos += 1 + usize::from(l);
            }
            _ => return Err(Error::InvalidName),
        }
    }
}

/// Builds a response message. Used by tests and fake responders.
#[derive(Debug, Clone, Default)]
pub struct Response {
    buf: Vec<u8>,
    count: u16,
}

impl Response {
    /// Creates an empty response (flags: authoritative answer).
    #[must_use]
    pub fn new() -> Self {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[2] = 0x84;
        Self { buf, count: 0 }
    }

    fn record(&mut self, name: &str, ty: RecordType, ttl: u32, data: &[u8]) -> &mut Self {
        write_name(&mut self.buf, name);
        self.buf.extend_from_slice(&ty.0.to_be_bytes());
        self.buf.extend_from_slice(&1u16.to_be_bytes());
        self.buf.extend_from_slice(&ttl.to_be_bytes());
        self.buf
            .extend_from_slice(&(data.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(data);
        self.count += 1;
        self
    }

    /// Adds a PTR record.
    pub fn ptr(&mut self, name: &str, target: &str) -> &mut Self {
        let mut d = Vec::new();
        write_name(&mut d, target);
        self.record(name, RecordType::PTR, 4500, &d)
    }

    /// Adds an SRV record.
    pub fn srv(&mut self, name: &str, port: u16, target: &str) -> &mut Self {
        let mut d = vec![0, 0, 0, 0];
        d.extend_from_slice(&port.to_be_bytes());
        write_name(&mut d, target);
        self.record(name, RecordType::SRV, 120, &d)
    }

    /// Adds a TXT record from `key=value` entries.
    pub fn txt(&mut self, name: &str, entries: &[&str]) -> &mut Self {
        let mut d = Vec::new();
        for e in entries {
            d.push(e.len() as u8);
            d.extend_from_slice(e.as_bytes());
        }
        self.record(name, RecordType::TXT, 4500, &d)
    }

    /// Adds an A record.
    pub fn a(&mut self, name: &str, addr: Ipv4Addr) -> &mut Self {
        self.record(name, RecordType::A, 120, &addr.octets())
    }

    /// Adds an AAAA record.
    pub fn aaaa(&mut self, name: &str, addr: Ipv6Addr) -> &mut Self {
        self.record(name, RecordType::AAAA, 120, &addr.octets())
    }

    /// Returns the encoded message with all records in the answer section.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.buf[6..8].copy_from_slice(&self.count.to_be_bytes());
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn query_layout() {
        let mut q = Query::default();
        q.question("_airplay._tcp.local.", RecordType::PTR);
        let msg = q.finish();
        assert_eq!(msg[4..6], [0, 1]);
        assert_eq!(&msg[12..21], b"\x08_airplay");
        assert_eq!(&msg[msg.len() - 4..], &[0, 12, 0, 1]);
        // A query has no records.
        assert!(parse(&msg).unwrap().is_empty());
    }

    #[test]
    fn response_roundtrip() {
        let mut r = Response::new();
        r.ptr("_airplay._tcp.local", "Living Room._airplay._tcp.local")
            .srv("Living Room._airplay._tcp.local", 7000, "Living-Room.local")
            .txt("Living Room._airplay._tcp.local", &["model=AppleTV14,1"])
            .a("Living-Room.local", Ipv4Addr::new(10, 0, 0, 2))
            .aaaa("Living-Room.local", Ipv6Addr::LOCALHOST);
        let msg = r.finish();
        let records = parse(&msg).unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records[0].data,
            RData::Ptr("Living Room._airplay._tcp.local".into())
        );
        assert_eq!(
            records[1].data,
            RData::Srv {
                port: 7000,
                target: "Living-Room.local".into()
            }
        );
        assert_eq!(records[2].data, RData::Txt(b"\x11model=AppleTV14,1"));
        assert_eq!(records[3].data, RData::A(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(records[4].data, RData::Aaaa(Ipv6Addr::LOCALHOST));
        assert_eq!(records[1].ttl, 120);
    }

    #[test]
    fn compressed_names() {
        // Header, one answer: name "a.local" at 12, PTR pointing to "x" + pointer to 14 ("local").
        let mut msg = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        msg.extend_from_slice(b"\x01a\x05local\x00");
        msg.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 10]);
        let rdata = [1, b'x', 0xC0, 14];
        msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(&rdata);
        let records = parse(&msg).unwrap();
        assert_eq!(records[0].name, "a.local");
        assert_eq!(records[0].data, RData::Ptr("x.local".into()));
    }

    #[test]
    fn pointer_loop_rejected() {
        let mut msg = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        msg.extend_from_slice(&[0xC0, 12]);
        assert_eq!(parse(&msg), Err(Error::InvalidName));
    }

    #[test]
    fn errors() {
        assert_eq!(parse(&[0; 5]), Err(Error::Truncated));
        let mut msg = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        msg.push(0x80);
        assert_eq!(parse(&msg), Err(Error::InvalidName));
        let mut msg = vec![0, 0, 0x84, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        msg.push(0x80);
        assert_eq!(parse(&msg), Err(Error::InvalidName));
        let mut msg = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0];
        msg.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 1, 0, 9]);
        assert_eq!(parse(&msg), Err(Error::Truncated));
        let long = format!("{}.local", "a".repeat(60)).repeat(5);
        let mut r = Response::new();
        r.a(&long, Ipv4Addr::LOCALHOST);
        assert_eq!(parse(&r.finish()), Err(Error::InvalidName));
    }

    #[test]
    fn unknown_type_and_bad_lengths() {
        let mut msg = vec![0, 0, 0x84, 0, 0, 0, 0, 2, 0, 0, 0, 0];
        msg.extend_from_slice(b"\x00\x00\x63\x00\x01\x00\x00\x00\x01\x00\x01\xAA");
        msg.extend_from_slice(b"\x00\x00\x01\x00\x01\x00\x00\x00\x01\x00\x02\xAA\xBB");
        let records = parse(&msg).unwrap();
        assert_eq!(records[0].data, RData::Other);
        assert_eq!(records[1].data, RData::Other);
        assert_eq!(records[0].name, "");
    }

    proptest! {
        #[test]
        fn parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = parse(&bytes);
        }
    }
}
