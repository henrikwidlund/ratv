//! Binary property list (`bplist00`), used by AirPlay.
//!
//! Layout: 8-byte header, objects, offset table, 32-byte trailer. Containers
//! hold indices into the offset table. All numbers are big-endian.

use std::borrow::Cow;

/// Maximum container nesting accepted by the decoder.
pub const MAX_DEPTH: usize = 32;

/// Maximum number of decoded values. Objects can be shared, so this also
/// limits plists that reference the same container many times.
pub const MAX_VALUES: usize = 100_000;

const MAGIC: &[u8; 8] = b"bplist00";
const TRAILER_LEN: usize = 32;

/// Binary plist decode error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Header is not `bplist00`.
    #[error("not a binary plist")]
    InvalidHeader,
    /// Trailer or offset table is invalid.
    #[error("invalid trailer")]
    InvalidTrailer,
    /// An offset, length or reference points outside the data.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// Object marker has no known meaning.
    #[error("invalid marker 0x{0:02x}")]
    InvalidMarker(u8),
    /// Object reference is not in the offset table.
    #[error("invalid object reference {0}")]
    InvalidReference(u64),
    /// String is not valid UTF-8 or UTF-16.
    #[error("invalid string")]
    InvalidString,
    /// Dictionary key is not a string.
    #[error("dictionary key is not a string")]
    InvalidKey,
    /// Integer does not fit in an `i64`.
    #[error("integer out of range")]
    IntegerOverflow,
    /// Containers are nested deeper than [`MAX_DEPTH`].
    #[error("nesting too deep")]
    TooDeep,
    /// More than [`MAX_VALUES`] values.
    #[error("too many values")]
    TooManyValues,
}

/// A plist value. ASCII strings and data borrow from the input.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Floating point number.
    Real(f64),
    /// Date, seconds since 2001-01-01 UTC.
    Date(f64),
    /// Raw bytes.
    Data(Cow<'a, [u8]>),
    /// String.
    String(Cow<'a, str>),
    /// Keyed archiver UID.
    Uid(u64),
    /// Array.
    Array(Vec<Self>),
    /// Dictionary with string keys, in plist order.
    Dict(Vec<(Cow<'a, str>, Self)>),
}

impl<'a> Value<'a> {
    /// Value for `key` if this is a dictionary.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_dict()?
            .iter()
            .find_map(|(k, v)| (k == key).then_some(v))
    }

    /// Boolean value.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Integer value.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int(v) => Some(v),
            _ => None,
        }
    }

    /// Integer value if it is not negative.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|v| u64::try_from(v).ok())
    }

    /// Number as `f64`. Integers are converted.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub const fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Real(v) => Some(v),
            Value::Int(v) => Some(v as f64),
            _ => None,
        }
    }

    /// String value.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Data value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Data(d) => Some(d),
            _ => None,
        }
    }

    /// Array items.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Dictionary entries.
    #[must_use]
    pub fn as_dict(&self) -> Option<&[(Cow<'a, str>, Self)]> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Copies borrowed data so the value no longer borrows the input.
    #[must_use]
    pub fn into_owned(self) -> Value<'static> {
        match self {
            Value::Bool(v) => Value::Bool(v),
            Value::Int(v) => Value::Int(v),
            Value::Real(v) => Value::Real(v),
            Value::Date(v) => Value::Date(v),
            Value::Uid(v) => Value::Uid(v),
            Value::Data(d) => Value::Data(Cow::Owned(d.into_owned())),
            Value::String(s) => Value::String(Cow::Owned(s.into_owned())),
            Value::Array(a) => Value::Array(a.into_iter().map(Value::into_owned).collect()),
            Value::Dict(d) => Value::Dict(
                d.into_iter()
                    .map(|(k, v)| (Cow::Owned(k.into_owned()), v.into_owned()))
                    .collect(),
            ),
        }
    }

    /// Encodes the value into a new buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        self.encode_into(&mut out);
        out
    }

    /// Appends the encoded plist to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let count = self.count();
        let ref_size = int_size(count as u64 - 1);
        let base = out.len();
        out.extend_from_slice(MAGIC);
        let mut w = Writer {
            out,
            base,
            ref_size,
            offsets: vec![0; count],
            next: 1,
        };
        w.write(self, 0);
        let Writer { out, offsets, .. } = w;

        let table = out.len() - base;
        let offset_size = int_size(table as u64);
        for off in offsets {
            put_uint(out, off as u64, offset_size);
        }
        out.extend_from_slice(&[0; 6]);
        out.push(offset_size as u8);
        out.push(ref_size as u8);
        out.extend_from_slice(&(count as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&(table as u64).to_be_bytes());
    }

    /// Number of objects this value encodes to.
    fn count(&self) -> usize {
        1 + match self {
            Value::Array(a) => a.iter().map(Value::count).sum(),
            Value::Dict(d) => d.iter().map(|(_, v)| 1 + v.count()).sum(),
            _ => 0,
        }
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Value::String(Cow::Borrowed(v))
    }
}

impl From<String> for Value<'_> {
    fn from(v: String) -> Self {
        Value::String(Cow::Owned(v))
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(v: &'a [u8]) -> Self {
        Value::Data(Cow::Borrowed(v))
    }
}

impl From<bool> for Value<'_> {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value<'_> {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<f64> for Value<'_> {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}

impl From<Vec<Self>> for Value<'_> {
    fn from(v: Vec<Self>) -> Self {
        Value::Array(v)
    }
}

struct Writer<'o> {
    out: &'o mut Vec<u8>,
    base: usize,
    ref_size: usize,
    offsets: Vec<usize>,
    next: usize,
}

impl Writer<'_> {
    /// Writes `v` as object `index`. Child indices are assigned when the
    /// parent is written; objects may appear in any order in the file.
    fn write(&mut self, v: &Value<'_>, index: usize) {
        self.offsets[index] = self.out.len() - self.base;
        match v {
            Value::Bool(b) => self.out.push(if *b { 0x09 } else { 0x08 }),
            Value::Int(i) => self.int(*i),
            Value::Real(r) => {
                self.out.push(0x23);
                self.out.extend_from_slice(&r.to_be_bytes());
            }
            Value::Date(d) => {
                self.out.push(0x33);
                self.out.extend_from_slice(&d.to_be_bytes());
            }
            Value::Uid(u) => {
                let size = int_size(*u);
                self.out.push(0x80 | (size as u8 - 1));
                put_uint(self.out, *u, size);
            }
            Value::Data(d) => {
                self.marker(0x40, d.len());
                self.out.extend_from_slice(d);
            }
            Value::String(s) => self.string(s),
            Value::Array(items) => {
                self.marker(0xA0, items.len());
                let first = self.alloc(items.len());
                for i in 0..items.len() {
                    put_uint(self.out, (first + i) as u64, self.ref_size);
                }
                for (i, item) in items.iter().enumerate() {
                    self.write(item, first + i);
                }
            }
            Value::Dict(entries) => {
                let n = entries.len();
                self.marker(0xD0, n);
                let keys = self.alloc(n);
                let values = self.alloc(n);
                for i in 0..2 * n {
                    put_uint(self.out, (keys + i) as u64, self.ref_size);
                }
                for (i, (k, _)) in entries.iter().enumerate() {
                    self.offsets[keys + i] = self.out.len() - self.base;
                    self.string(k);
                }
                for (i, (_, v)) in entries.iter().enumerate() {
                    self.write(v, values + i);
                }
            }
        }
    }

    const fn alloc(&mut self, n: usize) -> usize {
        let first = self.next;
        self.next += n;
        first
    }

    fn int(&mut self, v: i64) {
        // Negative numbers always use 8 bytes.
        let size = u64::try_from(v).map_or(8, int_size);
        self.int_sized(v.cast_unsigned(), size);
    }

    fn int_sized(&mut self, v: u64, size: usize) {
        self.out.push(0x10 | size.trailing_zeros() as u8);
        put_uint(self.out, v, size);
    }

    fn string(&mut self, s: &str) {
        if s.is_ascii() {
            self.marker(0x50, s.len());
            self.out.extend_from_slice(s.as_bytes());
        } else {
            self.marker(0x60, s.encode_utf16().count());
            for unit in s.encode_utf16() {
                self.out.extend_from_slice(&unit.to_be_bytes());
            }
        }
    }

    /// Writes a marker with a length in the low nibble, or `0xF` and an int.
    fn marker(&mut self, kind: u8, len: usize) {
        if len < 0xF {
            self.out.push(kind | len as u8);
        } else {
            self.out.push(kind | 0xF);
            self.int_sized(len as u64, int_size(len as u64));
        }
    }
}

/// Smallest of 1, 2, 4 or 8 bytes that holds `v`.
const fn int_size(v: u64) -> usize {
    match v {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    }
}

fn put_uint(out: &mut Vec<u8>, v: u64, size: usize) {
    out.extend_from_slice(&v.to_be_bytes()[8 - size..]);
}

/// Decodes a binary plist. ASCII strings and data borrow from `data`.
pub fn decode(data: &[u8]) -> Result<Value<'_>, Error> {
    if data.len() < MAGIC.len() + TRAILER_LEN || !data.starts_with(b"bplist0") {
        return Err(Error::InvalidHeader);
    }
    let t = &data[data.len() - TRAILER_LEN..];
    let offset_size = usize::from(t[6]);
    let ref_size = usize::from(t[7]);
    let count = u64::from_be_bytes(t[8..16].try_into().expect("8 bytes"));
    let top = u64::from_be_bytes(t[16..24].try_into().expect("8 bytes"));
    let table = u64::from_be_bytes(t[24..32].try_into().expect("8 bytes"));
    if !matches!(offset_size, 1 | 2 | 4 | 8) || !matches!(ref_size, 1 | 2 | 4 | 8) || top >= count {
        return Err(Error::InvalidTrailer);
    }
    // The offset table must lie between the header and the trailer.
    let table_end = usize::try_from(count)
        .ok()
        .and_then(|c| c.checked_mul(offset_size))
        .and_then(|len| usize::try_from(table).ok()?.checked_add(len))
        .filter(|&end| end <= data.len() - TRAILER_LEN)
        .ok_or(Error::InvalidTrailer)?;
    let table = usize::try_from(table).expect("checked above");
    let r = Reader {
        data,
        offsets: &data[table..table_end],
        offset_size,
        ref_size,
        objects_end: table,
    };
    let mut budget = MAX_VALUES;
    r.value(top, 0, &mut budget)
}

struct Reader<'a> {
    data: &'a [u8],
    offsets: &'a [u8],
    offset_size: usize,
    ref_size: usize,
    objects_end: usize,
}

impl<'a> Reader<'a> {
    fn value(&self, index: u64, depth: usize, budget: &mut usize) -> Result<Value<'a>, Error> {
        *budget = budget.checked_sub(1).ok_or(Error::TooManyValues)?;
        let pos = self.offset(index)?;
        let marker = *self.data.get(pos).ok_or(Error::UnexpectedEof)?;
        let low = marker & 0xF;
        let mut pos = pos + 1;
        Ok(match marker >> 4 {
            0x0 => match marker {
                0x08 => Value::Bool(false),
                0x09 => Value::Bool(true),
                _ => return Err(Error::InvalidMarker(marker)),
            },
            0x1 => Value::Int(self.int(low, pos)?),
            0x2 => {
                let b = self.bytes(pos, 1 << low)?;
                match low {
                    2 => Value::Real(f64::from(f32::from_be_bytes(
                        b.try_into().expect("4 bytes"),
                    ))),
                    3 => Value::Real(f64::from_be_bytes(b.try_into().expect("8 bytes"))),
                    _ => return Err(Error::InvalidMarker(marker)),
                }
            }
            0x3 if marker == 0x33 => Value::Date(f64::from_be_bytes(
                self.bytes(pos, 8)?.try_into().expect("8 bytes"),
            )),
            0x4 => {
                let len = self.len(low, &mut pos)?;
                Value::Data(Cow::Borrowed(self.bytes(pos, len)?))
            }
            0x5 => {
                let len = self.len(low, &mut pos)?;
                let s = str::from_utf8(self.bytes(pos, len)?).map_err(|_| Error::InvalidString)?;
                Value::String(Cow::Borrowed(s))
            }
            0x6 => {
                let len = self.len(low, &mut pos)?;
                let b = self.bytes(pos, len.checked_mul(2).ok_or(Error::UnexpectedEof)?)?;
                let units = b.as_chunks::<2>().0.iter().map(|&c| u16::from_be_bytes(c));
                let s = char::decode_utf16(units)
                    .collect::<Result<String, _>>()
                    .map_err(|_| Error::InvalidString)?;
                Value::String(Cow::Owned(s))
            }
            0x8 => Value::Uid(self.uint(pos, usize::from(low) + 1)?),
            0xA => {
                let len = self.len(low, &mut pos)?;
                let depth = next_depth(depth)?;
                let refs = self.bytes(
                    pos,
                    len.checked_mul(self.ref_size).ok_or(Error::UnexpectedEof)?,
                )?;
                let mut items = Vec::with_capacity(len.min(*budget));
                for r in refs.chunks_exact(self.ref_size) {
                    items.push(self.value(read_uint(r), depth, budget)?);
                }
                Value::Array(items)
            }
            0xD => {
                let len = self.len(low, &mut pos)?;
                let depth = next_depth(depth)?;
                let size = len.checked_mul(self.ref_size).ok_or(Error::UnexpectedEof)?;
                let refs = self.bytes(pos, size.checked_mul(2).ok_or(Error::UnexpectedEof)?)?;
                let (keys, values) = refs.split_at(size);
                let mut entries = Vec::with_capacity(len.min(*budget));
                for (k, v) in keys
                    .chunks_exact(self.ref_size)
                    .zip(values.chunks_exact(self.ref_size))
                {
                    let Value::String(key) = self.value(read_uint(k), depth, budget)? else {
                        return Err(Error::InvalidKey);
                    };
                    entries.push((key, self.value(read_uint(v), depth, budget)?));
                }
                Value::Dict(entries)
            }
            _ => return Err(Error::InvalidMarker(marker)),
        })
    }

    fn offset(&self, index: u64) -> Result<usize, Error> {
        let start = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(self.offset_size))
            .filter(|&s| s < self.offsets.len())
            .ok_or(Error::InvalidReference(index))?;
        let off = read_uint(&self.offsets[start..start + self.offset_size]);
        usize::try_from(off)
            .ok()
            .filter(|&o| o >= MAGIC.len() && o < self.objects_end)
            .ok_or(Error::UnexpectedEof)
    }

    fn bytes(&self, pos: usize, len: usize) -> Result<&'a [u8], Error> {
        pos.checked_add(len)
            .filter(|&end| end <= self.objects_end)
            .map(|end| &self.data[pos..end])
            .ok_or(Error::UnexpectedEof)
    }

    fn uint(&self, pos: usize, len: usize) -> Result<u64, Error> {
        if len > 8 {
            return Err(Error::IntegerOverflow);
        }
        Ok(read_uint(self.bytes(pos, len)?))
    }

    /// Integer with `1 << exp` bytes. 1, 2 and 4 bytes are unsigned, 8 bytes
    /// signed, 16 bytes allowed when the value fits in an `i64`.
    fn int(&self, exp: u8, pos: usize) -> Result<i64, Error> {
        match exp {
            0..=2 => Ok(self.uint(pos, 1 << exp)?.cast_signed()),
            3 => Ok(self.uint(pos, 8)?.cast_signed()),
            4 => {
                let b = self.bytes(pos, 16)?;
                let hi = read_uint(&b[..8]);
                let lo = read_uint(&b[8..]);
                if hi == 0 {
                    i64::try_from(lo).map_err(|_| Error::IntegerOverflow)
                } else if hi == u64::MAX && lo.cast_signed() < 0 {
                    Ok(lo.cast_signed())
                } else {
                    Err(Error::IntegerOverflow)
                }
            }
            _ => Err(Error::IntegerOverflow),
        }
    }

    /// Length from the low nibble, or from the int object after it if `0xF`.
    fn len(&self, low: u8, pos: &mut usize) -> Result<usize, Error> {
        if low != 0xF {
            return Ok(usize::from(low));
        }
        let marker = *self.data.get(*pos).ok_or(Error::UnexpectedEof)?;
        if marker >> 4 != 0x1 || marker & 0xF > 3 {
            return Err(Error::InvalidMarker(marker));
        }
        let size = 1usize << (marker & 0xF);
        let v = self.uint(*pos + 1, size)?;
        *pos += 1 + size;
        usize::try_from(v).map_err(|_| Error::UnexpectedEof)
    }
}

const fn next_depth(depth: usize) -> Result<usize, Error> {
    if depth >= MAX_DEPTH {
        Err(Error::TooDeep)
    } else {
        Ok(depth + 1)
    }
}

fn read_uint(b: &[u8]) -> u64 {
    b.iter().fold(0, |acc, &x| (acc << 8) | u64::from(x))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn sample() -> Value<'static> {
        Value::Dict(vec![
            ("str".into(), "hello".into()),
            ("utf16".into(), "héllo ✓".into()),
            ("int".into(), Value::Int(300)),
            ("neg".into(), Value::Int(-5)),
            ("big".into(), Value::Int(1 << 40)),
            ("real".into(), Value::Real(1.5)),
            ("date".into(), Value::Date(1000.0)),
            ("yes".into(), true.into()),
            ("no".into(), false.into()),
            ("data".into(), Value::Data(Cow::Owned(vec![0xAB; 20]))),
            ("uid".into(), Value::Uid(7)),
            (
                "array".into(),
                vec![Value::Int(1), "two".into(), Value::Array(vec![])].into(),
            ),
            ("empty".into(), Value::Dict(vec![])),
        ])
    }

    #[test]
    fn roundtrip_sample() {
        let v = sample();
        assert_eq!(decode(&v.encode()).unwrap(), v);
    }

    // plistlib.dumps({"a": 1, "b": [True, "x"], "c": b"\x01\x02", "d": 2.5,
    //                 "e": "é", "f": -1}, fmt=plistlib.FMT_BINARY)
    #[test]
    fn decodes_python_plistlib() {
        let data = hex(concat!(
            "62706c6973743030d601020304050607080b0c0d0e5161516251635164516551661001a2090a09517842",
            "01022340040000000000006100e913ffffffffffffffff081517191b1d1f21232627292c3538",
            "0000000000000101000000000000000f00000000000000000000000000000041"
        ));
        let v = decode(&data).unwrap();
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
        let b = v.get("b").and_then(Value::as_array).unwrap();
        assert_eq!(b[0].as_bool(), Some(true));
        assert_eq!(b[1].as_str(), Some("x"));
        assert_eq!(v.get("c").and_then(Value::as_bytes), Some(&[1u8, 2][..]));
        assert_eq!(v.get("d").and_then(Value::as_f64), Some(2.5));
        assert_eq!(v.get("e").and_then(Value::as_str), Some("é"));
        assert_eq!(v.get("f").and_then(Value::as_i64), Some(-1));
    }

    #[test]
    fn long_lengths() {
        let s = "x".repeat(300);
        let v = Value::Dict(vec![
            ("s".into(), s.as_str().into()),
            ("d".into(), Value::Data(Cow::Owned(vec![1; 70_000]))),
            ("a".into(), Value::Array((0..20).map(Value::Int).collect())),
        ]);
        let enc = v.encode();
        let dec = decode(&enc).unwrap();
        assert_eq!(dec, v);
        // ASCII strings and data borrow from the input.
        assert!(matches!(
            dec.get("s"),
            Some(Value::String(Cow::Borrowed(_)))
        ));
        assert!(matches!(dec.get("d"), Some(Value::Data(Cow::Borrowed(_)))));
        assert_eq!(dec.clone().into_owned(), v);
    }

    #[test]
    fn accessors() {
        let v = sample();
        assert_eq!(v.get("int").and_then(Value::as_u64), Some(300));
        assert_eq!(v.get("neg").and_then(Value::as_u64), None);
        assert_eq!(v.get("int").and_then(Value::as_f64), Some(300.0));
        assert!(v.get("missing").is_none());
        assert!(Value::Int(1).get("x").is_none());
        assert!(Value::Int(1).as_str().is_none());
        assert!(Value::Int(1).as_bytes().is_none());
        assert!(Value::Int(1).as_bool().is_none());
        assert!(Value::Int(1).as_array().is_none());
        assert!(Value::Bool(true).as_i64().is_none());
        assert!(Value::Bool(true).as_f64().is_none());
        assert_eq!(Value::from(String::from("s")).as_str(), Some("s"));
        assert_eq!(Value::from(&b"ab"[..]).as_bytes(), Some(&b"ab"[..]));
        assert_eq!(Value::from(2.0).as_f64(), Some(2.0));
    }

    #[test]
    fn sixteen_byte_int() {
        // Built by hand: marker 0x14 is a 16-byte integer.
        let mut data = b"bplist00".to_vec();
        data.push(0x14);
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(&42u64.to_be_bytes());
        data.push(8); // offset table
        let mut trailer = [0u8; 32];
        trailer[6] = 1;
        trailer[7] = 1;
        trailer[15] = 1;
        trailer[31] = 25;
        data.extend_from_slice(&trailer);
        assert_eq!(decode(&data).unwrap(), Value::Int(42));
        data[9] = 1;
        assert_eq!(decode(&data), Err(Error::IntegerOverflow));
    }

    #[test]
    fn errors() {
        assert_eq!(decode(b"bplist00"), Err(Error::InvalidHeader));
        let mut good = Value::Array(vec![Value::Int(1)]).encode();
        assert_eq!(decode(&good[..good.len() - 1]), Err(Error::InvalidTrailer));

        // Bad marker.
        let mut bad = good.clone();
        bad[10] = 0x70;
        assert_eq!(decode(&bad), Err(Error::InvalidMarker(0x70)));

        // Reference past the offset table.
        let mut bad = good.clone();
        bad[9] = 9;
        assert_eq!(decode(&bad), Err(Error::InvalidReference(9)));

        // Top object out of range.
        let n = good.len();
        good[n - 9] = 5;
        assert_eq!(decode(&good), Err(Error::InvalidTrailer));

        // Non-string dict key.
        let mut d = Value::Dict(vec![("k".into(), Value::Int(1))]).encode();
        d[11] = 0x10;
        assert_eq!(decode(&d), Err(Error::InvalidKey));

        // Invalid UTF-16.
        let mut s = Value::from("é").encode();
        s[9..11].copy_from_slice(&[0xD8, 0x00]);
        assert_eq!(decode(&s), Err(Error::InvalidString));
    }

    #[test]
    fn too_deep() {
        let mut v = Value::Int(0);
        for _ in 0..=MAX_DEPTH {
            v = Value::Array(vec![v]);
        }
        assert_eq!(decode(&v.encode()), Err(Error::TooDeep));
    }

    #[test]
    fn shared_objects_are_limited() {
        // Each array references the next one twice: 2^30 values if expanded.
        let depth = 30u8;
        let mut data = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for i in 0..depth {
            offsets.push(data.len() as u8);
            data.extend_from_slice(&[0xA2, i + 1, i + 1]);
        }
        offsets.push(data.len() as u8);
        data.push(0x09);
        let table = data.len();
        data.extend_from_slice(&offsets);
        let mut trailer = [0u8; 32];
        trailer[6] = 1;
        trailer[7] = 1;
        trailer[15] = depth + 1;
        trailer[31] = table as u8;
        data.extend_from_slice(&trailer);
        assert_eq!(decode(&data), Err(Error::TooManyValues));
    }

    fn value() -> impl Strategy<Value = Value<'static>> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::Int),
            any::<f64>()
                .prop_filter("not NaN", |f| !f.is_nan())
                .prop_map(Value::Real),
            any::<u64>().prop_map(Value::Uid),
            ".*".prop_map(Value::from),
            proptest::collection::vec(any::<u8>(), 0..40).prop_map(|d| Value::Data(Cow::Owned(d))),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..20).prop_map(Value::Array),
                proptest::collection::vec((".*", inner), 0..20).prop_map(|e| Value::Dict(
                    e.into_iter().map(|(k, v)| (Cow::Owned(k), v)).collect()
                )),
            ]
        })
    }

    proptest! {
        #[test]
        fn roundtrip(v in value()) {
            let enc = v.encode();
            prop_assert_eq!(decode(&enc).unwrap(), v);
        }

        #[test]
        fn decode_never_panics(data in proptest::collection::vec(any::<u8>(), 0..200)) {
            let mut input = b"bplist00".to_vec();
            input.extend_from_slice(&data);
            let _ = decode(&input);
        }
    }
}
