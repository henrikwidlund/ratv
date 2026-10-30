//! OPACK: Apple's compact binary serialization format.
//!
//! All multi-byte numbers are little-endian. Strings, data and non-trivial
//! numbers are tracked in an object table. A later identical value may be
//! written as a one-byte reference (`0xA0..=0xC0`) or a sized reference
//! (`0xC1..=0xC4`). The encoder tracks exactly the same values as the decoder,
//! so references always resolve to the same index on both sides.

use std::borrow::Cow;

/// Maximum container nesting accepted by the decoder.
pub const MAX_DEPTH: usize = 32;

const TERMINATOR: u8 = 0x03;
const ENDLESS: usize = 0xF;

/// OPACK decode error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Input ended before the value was complete.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// Tag byte has no known meaning.
    #[error("invalid tag 0x{0:02x}")]
    InvalidTag(u8),
    /// Object reference points past the object table.
    #[error("invalid object reference {0}")]
    InvalidReference(usize),
    /// String is not valid UTF-8.
    #[error("invalid UTF-8 in string")]
    InvalidUtf8,
    /// Integer does not fit in 64 bits.
    #[error("integer out of range")]
    IntegerOverflow,
    /// Containers are nested deeper than [`MAX_DEPTH`].
    #[error("nesting too deep")]
    TooDeep,
    /// Bytes remain after the top-level value.
    #[error("{0} trailing bytes")]
    TrailingBytes(usize),
}

/// A decoded OPACK value. Strings and data borrow from the input when possible.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    /// `null` (`0x04`).
    Null,
    /// Boolean (`0x01` true, `0x02` false).
    Bool(bool),
    /// Integer. Unsigned 64-bit values above `i64::MAX` keep their bit pattern;
    /// use [`Value::as_u64`] to read them.
    Int(i64),
    /// Floating point number (`0x35` f32 or `0x36` f64).
    Float(f64),
    /// Absolute time (`0x06`), seconds since 2001-01-01 UTC.
    Date(f64),
    /// UUID (`0x05`), 16 raw bytes.
    Uuid([u8; 16]),
    /// UTF-8 string.
    Str(Cow<'a, str>),
    /// Raw bytes.
    Data(Cow<'a, [u8]>),
    /// Ordered list.
    Array(Vec<Value<'a>>),
    /// Ordered key/value pairs. Duplicate keys are kept as-is.
    Dict(Vec<(Value<'a>, Value<'a>)>),
}

impl<'a> Value<'a> {
    /// Looks up a string key in a dictionary.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        match self {
            Value::Dict(entries) => entries
                .iter()
                .find(|(k, _)| matches!(k, Value::Str(s) if s == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Returns the integer value.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the integer value reinterpreted as unsigned.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::Int(v) => Some(v.cast_unsigned()),
            _ => None,
        }
    }

    /// Returns a float, converting integers.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Float(v) => Some(v),
            #[allow(clippy::cast_precision_loss)]
            Value::Int(v) => Some(v as f64),
            _ => None,
        }
    }

    /// Returns the boolean value.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the string value.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the data value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Data(d) => Some(d),
            _ => None,
        }
    }

    /// Returns the array elements.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Value<'a>]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Returns the dictionary entries.
    #[must_use]
    pub fn as_dict(&self) -> Option<&[(Value<'a>, Value<'a>)]> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Copies all borrowed data so the value no longer borrows the input.
    #[must_use]
    pub fn into_owned(self) -> Value<'static> {
        match self {
            Value::Null => Value::Null,
            Value::Bool(v) => Value::Bool(v),
            Value::Int(v) => Value::Int(v),
            Value::Float(v) => Value::Float(v),
            Value::Date(v) => Value::Date(v),
            Value::Uuid(v) => Value::Uuid(v),
            Value::Str(s) => Value::Str(Cow::Owned(s.into_owned())),
            Value::Data(d) => Value::Data(Cow::Owned(d.into_owned())),
            Value::Array(a) => Value::Array(a.into_iter().map(Value::into_owned).collect()),
            Value::Dict(d) => Value::Dict(
                d.into_iter()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
            ),
        }
    }

    /// Encodes the value into a new buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        self.encode_into(&mut out);
        out
    }

    /// Appends the encoded value to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        Encoder::new(out).value(self);
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

impl From<u32> for Value<'_> {
    fn from(v: u32) -> Self {
        Value::Int(v.into())
    }
}

impl From<f64> for Value<'_> {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Value::Str(Cow::Borrowed(v))
    }
}

impl From<String> for Value<'_> {
    fn from(v: String) -> Self {
        Value::Str(Cow::Owned(v))
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(v: &'a [u8]) -> Self {
        Value::Data(Cow::Borrowed(v))
    }
}

impl From<Vec<u8>> for Value<'_> {
    fn from(v: Vec<u8>) -> Self {
        Value::Data(Cow::Owned(v))
    }
}

/// Streaming OPACK encoder that writes directly into a byte buffer.
///
/// Use it to build messages without creating a [`Value`] tree first.
///
/// ```
/// use ratv_codec::opack::Encoder;
///
/// let mut out = Vec::new();
/// Encoder::new(&mut out).dict(1, |e| {
///     e.str("a");
///     e.int(12);
/// });
/// assert_eq!(out, [0xe1, 0x41, 0x61, 0x14]);
/// ```
pub struct Encoder<'b> {
    out: &'b mut Vec<u8>,
    // Byte ranges in `out` of values in the object table.
    objects: Vec<(usize, usize)>,
}

impl<'b> Encoder<'b> {
    /// Creates an encoder that appends to `out`.
    pub fn new(out: &'b mut Vec<u8>) -> Self {
        Self {
            out,
            objects: Vec::new(),
        }
    }

    /// Writes `null`.
    pub fn null(&mut self) -> &mut Self {
        self.out.push(0x04);
        self
    }

    /// Writes a boolean.
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.out.push(if v { 0x01 } else { 0x02 });
        self
    }

    /// Writes an integer using the smallest encoding.
    /// Negative values keep their 64-bit pattern.
    pub fn int(&mut self, v: i64) -> &mut Self {
        if (0..0x28).contains(&v) {
            self.out.push(0x08 + v.cast_unsigned() as u8);
            return self;
        }
        let start = self.out.len();
        let u = v.cast_unsigned();
        let bytes = u.to_le_bytes();
        let (tag, len) = match u {
            0..=0xFF => (0x30, 1),
            0x100..=0xFFFF => (0x31, 2),
            0x1_0000..=0xFFFF_FFFF => (0x32, 4),
            _ => (0x33, 8),
        };
        self.out.push(tag);
        self.out.extend_from_slice(&bytes[..len]);
        self.track(start);
        self
    }

    /// Writes an integer with a fixed width of 1, 2, 4 or 8 bytes.
    ///
    /// # Panics
    /// Panics if `width` is not 1, 2, 4 or 8.
    pub fn sized_int(&mut self, v: u64, width: usize) -> &mut Self {
        let tag = match width {
            1 => 0x30,
            2 => 0x31,
            4 => 0x32,
            8 => 0x33,
            _ => panic!("invalid integer width {width}"),
        };
        let start = self.out.len();
        self.out.push(tag);
        self.out.extend_from_slice(&v.to_le_bytes()[..width]);
        self.track(start);
        self
    }

    /// Writes a 64-bit float.
    pub fn float(&mut self, v: f64) -> &mut Self {
        let start = self.out.len();
        self.out.push(0x36);
        self.out.extend_from_slice(&v.to_le_bytes());
        self.track(start);
        self
    }

    /// Writes an absolute time in seconds since 2001-01-01 UTC.
    pub fn date(&mut self, v: f64) -> &mut Self {
        let start = self.out.len();
        self.out.push(0x06);
        self.out.extend_from_slice(&v.to_le_bytes());
        self.track(start);
        self
    }

    /// Writes a UUID.
    pub fn uuid(&mut self, v: &[u8; 16]) -> &mut Self {
        let start = self.out.len();
        self.out.push(0x05);
        self.out.extend_from_slice(v);
        self.track(start);
        self
    }

    /// Writes a string.
    pub fn str(&mut self, v: &str) -> &mut Self {
        let start = self.out.len();
        self.sized(v.as_bytes(), 0x40, 0x61, [1, 2, 3, 4]);
        self.track(start);
        self
    }

    /// Writes raw bytes.
    pub fn data(&mut self, v: &[u8]) -> &mut Self {
        let start = self.out.len();
        self.sized(v, 0x70, 0x91, [1, 2, 4, 8]);
        self.track(start);
        self
    }

    /// Writes an array of `len` items. `f` must write exactly `len` values.
    pub fn array(&mut self, len: usize, f: impl FnOnce(&mut Self)) -> &mut Self {
        self.container(0xD0, len, f)
    }

    /// Writes a dictionary of `len` entries. `f` must write `len` key/value pairs.
    pub fn dict(&mut self, len: usize, f: impl FnOnce(&mut Self)) -> &mut Self {
        self.container(0xE0, len, f)
    }

    /// Writes any [`Value`].
    pub fn value(&mut self, v: &Value<'_>) -> &mut Self {
        match v {
            Value::Null => self.null(),
            Value::Bool(b) => self.bool(*b),
            Value::Int(i) => self.int(*i),
            Value::Float(f) => self.float(*f),
            Value::Date(d) => self.date(*d),
            Value::Uuid(u) => self.uuid(u),
            Value::Str(s) => self.str(s),
            Value::Data(d) => self.data(d),
            Value::Array(items) => self.array(items.len(), |e| {
                for item in items {
                    e.value(item);
                }
            }),
            Value::Dict(entries) => self.dict(entries.len(), |e| {
                for (k, v) in entries {
                    e.value(k).value(v);
                }
            }),
        }
    }

    fn container(&mut self, base: u8, len: usize, f: impl FnOnce(&mut Self)) -> &mut Self {
        self.out.push(base + len.min(ENDLESS) as u8);
        f(self);
        if len >= ENDLESS {
            self.out.push(TERMINATOR);
        }
        self
    }

    // `widths` lists the length-field sizes for tags `sized_base..sized_base + 4`.
    fn sized(&mut self, bytes: &[u8], inline_base: u8, sized_base: u8, widths: [usize; 4]) {
        let len = bytes.len();
        if len <= 0x20 {
            self.out.push(inline_base + len as u8);
        } else {
            let le = (len as u64).to_le_bytes();
            let i = widths
                .iter()
                .position(|&w| w == 8 || len >> (w * 8) == 0)
                .unwrap_or(3);
            self.out.push(sized_base + i as u8);
            self.out.extend_from_slice(&le[..widths[i]]);
        }
        self.out.extend_from_slice(bytes);
    }

    // Replaces the value written at `start..` with a reference if an identical
    // value is already in the object table, otherwise adds it to the table.
    fn track(&mut self, start: usize) {
        let end = self.out.len();
        if end - start <= 1 {
            return;
        }
        let new = &self.out[start..end];
        let found = self
            .objects
            .iter()
            .position(|&(s, e)| self.out[s..e] == *new);
        match found {
            Some(index) => {
                self.out.truncate(start);
                write_reference(self.out, index);
            }
            None => self.objects.push((start, end)),
        }
    }
}

fn write_reference(out: &mut Vec<u8>, index: usize) {
    if index <= 0x20 {
        out.push(0xA0 + index as u8);
        return;
    }
    let le = (index as u64).to_le_bytes();
    let width: usize = match index {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    };
    // 0xC1 = 1 byte, 0xC2 = 2 bytes, 0xC3 = 4 bytes, 0xC4 = 8 bytes.
    out.push(0xC0 + width.trailing_zeros() as u8 + 1);
    out.extend_from_slice(&le[..width]);
}

/// Decodes a single OPACK value. The whole input must be consumed.
pub fn decode(input: &[u8]) -> Result<Value<'_>, Error> {
    let (value, rest) = decode_prefix(input)?;
    if rest.is_empty() {
        Ok(value)
    } else {
        Err(Error::TrailingBytes(rest.len()))
    }
}

/// Decodes one OPACK value from the start of `input` and returns the remaining bytes.
pub fn decode_prefix(input: &[u8]) -> Result<(Value<'_>, &[u8]), Error> {
    let mut d = Decoder {
        input,
        pos: 0,
        objects: Vec::new(),
    };
    let value = d.value(0)?;
    Ok((value, &input[d.pos..]))
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    // Encoded byte ranges of values in the object table.
    objects: Vec<&'a [u8]>,
}

impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEof)?;
        let bytes = self.input.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        self.pos = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn peek(&self) -> Result<u8, Error> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(Error::UnexpectedEof)
    }

    fn uint(&mut self, width: usize) -> Result<u64, Error> {
        let bytes = self.take(width)?;
        let mut buf = [0u8; 8];
        buf[..width].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(buf))
    }

    fn len(&mut self, width: usize) -> Result<usize, Error> {
        usize::try_from(self.uint(width)?).map_err(|_| Error::UnexpectedEof)
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        let start = self.pos;
        let tag = self.byte()?;
        let value = match tag {
            0x01 => return Ok(Value::Bool(true)),
            0x02 => return Ok(Value::Bool(false)),
            0x04 => return Ok(Value::Null),
            0x07 => return Ok(Value::Int(-1)),
            0x08..=0x2F => return Ok(Value::Int(i64::from(tag - 0x08))),
            0x05 => Value::Uuid(self.take(16)?.try_into().expect("16 bytes")),
            0x06 => Value::Date(f64::from_le_bytes(
                self.take(8)?.try_into().expect("8 bytes"),
            )),
            0x30..=0x33 => Value::Int(self.uint(1 << (tag & 0x0F))?.cast_signed()),
            0x34 => {
                let bytes = self.take(16)?;
                if bytes[8..].iter().any(|&b| b != 0) {
                    return Err(Error::IntegerOverflow);
                }
                Value::Int(
                    u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")).cast_signed(),
                )
            }
            0x35 => Value::Float(f64::from(f32::from_le_bytes(
                self.take(4)?.try_into().expect("4 bytes"),
            ))),
            0x36 => Value::Float(f64::from_le_bytes(
                self.take(8)?.try_into().expect("8 bytes"),
            )),
            0x40..=0x60 => self.str(usize::from(tag - 0x40))?,
            0x61..=0x64 => {
                let n = self.len(usize::from(tag - 0x60))?;
                self.str(n)?
            }
            0x6F => {
                let rest = &self.input[self.pos..];
                let n = rest
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or(Error::UnexpectedEof)?;
                let v = self.str(n)?;
                self.pos += 1;
                v
            }
            0x70..=0x90 => Value::Data(Cow::Borrowed(self.take(usize::from(tag - 0x70))?)),
            0x91..=0x94 => {
                let n = self.len(1 << (tag - 0x91))?;
                Value::Data(Cow::Borrowed(self.take(n)?))
            }
            0xA0..=0xC0 => return self.reference(usize::from(tag - 0xA0)),
            0xC1..=0xC4 => {
                let index = self.len(1 << (tag - 0xC1))?;
                return self.reference(index);
            }
            0xD0..=0xDF => return self.array(usize::from(tag & 0x0F), depth),
            0xE0..=0xEF => return self.dict(usize::from(tag & 0x0F), depth),
            _ => return Err(Error::InvalidTag(tag)),
        };
        let span = &self.input[start..self.pos];
        if span.len() > 1 && !self.objects.contains(&span) {
            self.objects.push(span);
        }
        Ok(value)
    }

    fn str(&mut self, n: usize) -> Result<Value<'a>, Error> {
        let bytes = self.take(n)?;
        let s = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
        Ok(Value::Str(Cow::Borrowed(s)))
    }

    fn reference(&mut self, index: usize) -> Result<Value<'a>, Error> {
        let span = *self
            .objects
            .get(index)
            .ok_or(Error::InvalidReference(index))?;
        // Table entries are always scalars, so this never recurses further.
        let mut sub = Decoder {
            input: span,
            pos: 0,
            objects: Vec::new(),
        };
        sub.value(MAX_DEPTH)
    }

    fn items(
        &mut self,
        count: usize,
        depth: usize,
        mut f: impl FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        if count == ENDLESS {
            while self.peek()? != TERMINATOR {
                f(self)?;
            }
            self.pos += 1;
        } else {
            for _ in 0..count {
                f(self)?;
            }
        }
        Ok(())
    }

    fn array(&mut self, count: usize, depth: usize) -> Result<Value<'a>, Error> {
        let mut items = Vec::with_capacity(count.min(ENDLESS - 1));
        self.items(count, depth, |d| {
            items.push(d.value(depth + 1)?);
            Ok(())
        })?;
        Ok(Value::Array(items))
    }

    fn dict(&mut self, count: usize, depth: usize) -> Result<Value<'a>, Error> {
        let mut entries = Vec::with_capacity(count.min(ENDLESS - 1));
        self.items(count, depth, |d| {
            let k = d.value(depth + 1)?;
            let v = d.value(depth + 1)?;
            entries.push((k, v));
            Ok(())
        })?;
        Ok(Value::Dict(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn enc(v: &Value<'_>) -> Vec<u8> {
        v.encode()
    }

    fn s(v: &str) -> Value<'_> {
        Value::from(v)
    }

    fn dict<'a>(entries: impl IntoIterator<Item = (Value<'a>, Value<'a>)>) -> Value<'a> {
        Value::Dict(entries.into_iter().collect())
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn scalars() {
        assert_eq!(enc(&Value::Bool(true)), [0x01]);
        assert_eq!(enc(&Value::Bool(false)), [0x02]);
        assert_eq!(enc(&Value::Null), [0x04]);
        assert_eq!(enc(&Value::Float(1.0)), hex("36000000000000f03f"));
        assert_eq!(
            enc(&Value::Uuid(
                *b"\x12\x34\x56\x78\x12\x34\x56\x78\x12\x34\x56\x78\x12\x34\x56\x78"
            )),
            hex("0512345678123456781234567812345678")
        );
    }

    #[test]
    fn integers() {
        assert_eq!(enc(&Value::Int(0)), [0x08]);
        assert_eq!(enc(&Value::Int(0xF)), [0x17]);
        assert_eq!(enc(&Value::Int(0x27)), [0x2F]);
        assert_eq!(enc(&Value::Int(0x28)), [0x30, 0x28]);
        assert_eq!(enc(&Value::Int(0x1FF)), [0x31, 0xFF, 0x01]);
        assert_eq!(enc(&Value::Int(0x1FF_FFFF)), [0x32, 0xFF, 0xFF, 0xFF, 0x01]);
        assert_eq!(
            enc(&Value::Int(0x1FF_FFFF_FFFF_FFFF)),
            hex("33ffffffffffffff01")
        );
        assert_eq!(enc(&Value::Int(-1)), hex("33ffffffffffffffff"));
        assert_eq!(decode(&[0x07]).unwrap(), Value::Int(-1));
    }

    #[test]
    fn sized_integers() {
        for (width, expected) in [
            (1, hex("3001")),
            (2, hex("310100")),
            (4, hex("3201000000")),
            (8, hex("330100000000000000")),
        ] {
            let mut out = Vec::new();
            Encoder::new(&mut out).sized_int(1, width);
            assert_eq!(out, expected);
            assert_eq!(decode(&out).unwrap(), Value::Int(1));
        }
    }

    #[test]
    #[should_panic(expected = "invalid integer width")]
    fn sized_integer_bad_width() {
        Encoder::new(&mut Vec::new()).sized_int(1, 3);
    }

    #[test]
    fn u64_max_roundtrip() {
        let v = Value::Int(u64::MAX.cast_signed());
        let out = enc(&v);
        assert_eq!(out, hex("33ffffffffffffffff"));
        assert_eq!(decode(&out).unwrap().as_u64(), Some(u64::MAX));
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn strings() {
        assert_eq!(enc(&s("a")), [0x41, 0x61]);
        assert_eq!(enc(&s("abc")), [0x43, 0x61, 0x62, 0x63]);
        let a32 = "a".repeat(0x20);
        let mut expected = vec![0x60];
        expected.extend_from_slice(a32.as_bytes());
        assert_eq!(enc(&s(&a32)), expected);
        let a33 = "a".repeat(33);
        assert_eq!(enc(&s(&a33))[..2], [0x61, 0x21]);
        let a256 = "a".repeat(256);
        assert_eq!(enc(&s(&a256))[..3], [0x62, 0x00, 0x01]);
        let a64k = "a".repeat(0x1_0000);
        assert_eq!(enc(&s(&a64k))[..4], [0x63, 0x00, 0x00, 0x01]);
        let a16m = "a".repeat(0x100_0000);
        assert_eq!(enc(&s(&a16m))[..5], [0x64, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(decode(&enc(&s(&a64k))).unwrap(), s(&a64k));
    }

    #[test]
    fn data() {
        assert_eq!(enc(&Value::from(&[0xAC][..])), [0x71, 0xAC]);
        assert_eq!(
            enc(&Value::from(&[0x12, 0x34, 0x56][..])),
            [0x73, 0x12, 0x34, 0x56]
        );
        let d32 = [0xAD; 0x20];
        assert_eq!(enc(&Value::from(&d32[..]))[0], 0x90);
        assert_eq!(enc(&Value::from(&[0x61; 33][..]))[..2], [0x91, 0x21]);
        assert_eq!(enc(&Value::from(&[0x61; 256][..]))[..3], [0x92, 0x00, 0x01]);
        let big = vec![0x61; 0x1_0000];
        let out = enc(&Value::from(&big[..]));
        assert_eq!(out[..5], [0x93, 0x00, 0x00, 0x01, 0x00]);
        assert_eq!(decode(&out).unwrap().as_bytes(), Some(&big[..]));
    }

    #[test]
    fn arrays() {
        assert_eq!(enc(&Value::Array(vec![])), [0xD0]);
        assert_eq!(
            enc(&Value::Array(vec![
                Value::Int(1),
                s("test"),
                Value::Bool(false)
            ])),
            hex("d309447465737402")
        );
        assert_eq!(
            enc(&Value::Array(vec![Value::Array(vec![Value::Bool(true)])])),
            [0xD1, 0xD1, 0x01]
        );
    }

    #[test]
    fn endless_array() {
        let v = Value::Array(vec![s("a"); 15]);
        let mut expected = vec![0xDF, 0x41, 0x61];
        expected.extend([0xA0; 14]);
        expected.push(0x03);
        assert_eq!(enc(&v), expected);
        assert_eq!(decode(&expected).unwrap(), v);
    }

    #[test]
    fn dicts() {
        assert_eq!(enc(&Value::Dict(vec![])), [0xE0]);
        assert_eq!(
            enc(&dict([
                (s("a"), Value::Int(12)),
                (Value::Bool(false), Value::Null)
            ])),
            [0xE2, 0x41, 0x61, 0x14, 0x02, 0x04]
        );
        assert_eq!(
            enc(&dict([(
                Value::Bool(true),
                dict([(s("a"), Value::Int(2))])
            )])),
            [0xE1, 0x01, 0xE1, 0x41, 0x61, 0x0A]
        );
    }

    #[test]
    fn endless_dict() {
        let keys: Vec<String> = (97u8..127).map(|c| (c as char).to_string()).collect();
        let v = dict(keys.iter().map(|k| (s(k), s("A"))));
        let out = enc(&v);
        assert_eq!(out[0], 0xEF);
        assert_eq!(*out.last().unwrap(), 0x03);
        assert_eq!(decode(&out).unwrap(), v);
    }

    #[test]
    fn references() {
        assert_eq!(
            enc(&Value::Array(vec![s("a"), s("a")])),
            [0xD2, 0x41, 0x61, 0xA0]
        );
        assert_eq!(
            enc(&Value::Array(vec![s("foo"), s("bar"), s("foo"), s("bar")])),
            hex("d443666f6f43626172a0a1")
        );
        let v = dict([
            (s("a"), s("b")),
            (s("c"), dict([(s("d"), s("a"))])),
            (s("d"), Value::Bool(true)),
        ]);
        let out = enc(&v);
        assert_eq!(out, hex("e3416141624163e14164a0a301"));
        assert_eq!(decode(&out).unwrap(), v);
    }

    #[test]
    fn references_track_numbers() {
        // 0x30 0x80 enters the object table, so "x" is index 1, not 0.
        let v = Value::Array(vec![Value::Int(0x80), s("x1"), s("x1"), Value::Int(0x80)]);
        let out = enc(&v);
        assert_eq!(out, hex("d43080427831a1a0"));
        assert_eq!(decode(&out).unwrap(), v);
    }

    #[test]
    fn sized_references() {
        let items: Vec<Value<'_>> = (0..300u32)
            .map(|i| Value::from(format!("s{i}")))
            .chain([s("s40"), s("s299")])
            .collect();
        let v = Value::Array(items);
        let out = enc(&v);
        // s40 -> 1-byte index, s299 -> 2-byte index.
        assert_eq!(out[out.len() - 6..], [0xC1, 40, 0xC2, 0x2B, 0x01, 0x03]);
        assert_eq!(decode(&out).unwrap(), v);
    }

    #[test]
    fn decodes_pyatv_system_info() {
        let bytes = hex(concat!(
            "e4425f694b5f73797374656d496e666f425f7809425f740a425f63ea435f626608435f63663100",
            "02455f636c466c3080435f7366310001435f7376463137302e3138a05145313a34383a31463a37",
            "313a46373a3845465f6964734944612434383734623136622d346237372d343939382d62396435",
            "2d303931656638636634383661465f70756249445130323a45333a41303a44393a44333a343045",
            "6d6f64656c4a6950686f6e6531302c36446e616d654d42756e4154562052656d6f7465"
        ));
        let v = decode(&bytes).unwrap();
        assert_eq!(v.get("_i").and_then(Value::as_str), Some("_systemInfo"));
        assert_eq!(v.get("_x").and_then(Value::as_i64), Some(1));
        let c = v.get("_c").unwrap();
        assert_eq!(c.get("_cf").and_then(Value::as_i64), Some(512));
        assert_eq!(c.get("_clFl").and_then(Value::as_i64), Some(128));
        assert_eq!(
            c.get("_i").and_then(Value::as_str),
            Some("E1:48:1F:71:F7:8E")
        );
        assert_eq!(c.get("name").and_then(Value::as_str), Some("BunATV Remote"));
        assert_eq!(enc(&v), bytes);
    }

    #[test]
    fn decode_misc_tags() {
        assert_eq!(decode(&hex("350000803f")).unwrap(), Value::Float(1.0));
        assert_eq!(
            decode(&hex("06000000000000f03f")).unwrap(),
            Value::Date(1.0)
        );
        assert_eq!(decode(b"\x6fabc\x00").unwrap(), s("abc"));
        let mut big = vec![0x34, 5];
        big.extend([0; 15]);
        assert_eq!(decode(&big).unwrap(), Value::Int(5));
        big[16] = 1;
        assert_eq!(decode(&big), Err(Error::IntegerOverflow));
    }

    #[test]
    fn date_roundtrip() {
        let v = Value::Date(123.5);
        assert_eq!(decode(&enc(&v)).unwrap(), v);
    }

    #[test]
    fn decode_errors() {
        assert_eq!(decode(&[]), Err(Error::UnexpectedEof));
        assert_eq!(decode(&[0x00]), Err(Error::InvalidTag(0x00)));
        assert_eq!(decode(&[0x03]), Err(Error::InvalidTag(0x03)));
        assert_eq!(
            decode(&[0xE1, 0x42, 0x61, 0x41, 0xA5]),
            Err(Error::InvalidReference(5))
        );
        assert_eq!(decode(&[0x43, 0x61]), Err(Error::UnexpectedEof));
        assert_eq!(decode(&[0x42, 0xFF, 0xFE]), Err(Error::InvalidUtf8));
        assert_eq!(decode(&[0x08, 0x08]), Err(Error::TrailingBytes(1)));
        assert_eq!(decode(&[0xDF, 0x08]), Err(Error::UnexpectedEof));
        assert_eq!(decode(b"\x6fabc"), Err(Error::UnexpectedEof));
        let deep = vec![0xD1; MAX_DEPTH + 1];
        assert_eq!(decode(&deep), Err(Error::TooDeep));
    }

    #[test]
    fn decode_prefix_returns_rest() {
        let (v, rest) = decode_prefix(&[0x09, 0xAA]).unwrap();
        assert_eq!(v, Value::Int(1));
        assert_eq!(rest, [0xAA]);
    }

    #[test]
    fn decode_borrows_input() {
        let bytes = [0x43, b'a', b'b', b'c'];
        assert!(matches!(
            decode(&bytes).unwrap(),
            Value::Str(Cow::Borrowed("abc"))
        ));
    }

    #[test]
    fn accessors() {
        let v = dict([
            (s("i"), Value::Int(3)),
            (s("f"), Value::Float(1.5)),
            (s("b"), Value::Bool(true)),
            (s("d"), Value::from(vec![1u8])),
            (s("a"), Value::Array(vec![])),
            (s("o"), Value::Dict(vec![])),
        ]);
        assert_eq!(v.get("i").and_then(Value::as_f64), Some(3.0));
        assert_eq!(v.get("f").and_then(Value::as_f64), Some(1.5));
        assert_eq!(v.get("b").and_then(Value::as_bool), Some(true));
        assert_eq!(v.get("d").and_then(Value::as_bytes), Some(&[1u8][..]));
        assert_eq!(
            v.get("a").and_then(Value::as_array).map(<[_]>::len),
            Some(0)
        );
        assert_eq!(v.get("o").and_then(Value::as_dict).map(<[_]>::len), Some(0));
        assert!(v.get("missing").is_none());
        assert!(Value::Null.get("i").is_none());
        assert!(Value::Null.as_i64().is_none());
        assert!(Value::Null.as_u64().is_none());
        assert!(Value::Null.as_f64().is_none());
        assert!(Value::Null.as_bool().is_none());
        assert!(Value::Null.as_str().is_none());
        assert!(Value::Null.as_bytes().is_none());
        assert!(Value::Null.as_array().is_none());
        assert!(Value::Null.as_dict().is_none());
    }

    #[test]
    fn into_owned_preserves_value() {
        let bytes = Value::Array(vec![
            dict([(s("k"), Value::from(&[1u8, 2][..]))]),
            Value::Date(1.0),
            Value::Uuid([7; 16]),
            Value::Null,
            Value::Float(2.0),
            Value::Bool(true),
            Value::from(String::from("x")),
            Value::from(7u32),
            Value::from(-5i64),
            Value::from(1.25f64),
            Value::from(false),
        ])
        .encode();
        let owned: Value<'static> = decode(&bytes).unwrap().into_owned();
        assert_eq!(owned.encode(), bytes);
    }

    fn arb_value() -> impl Strategy<Value = Value<'static>> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::Int),
            any::<f64>()
                .prop_filter("NaN != NaN", |f| !f.is_nan())
                .prop_map(Value::Float),
            any::<[u8; 16]>().prop_map(Value::Uuid),
            "[a-c]{0,40}".prop_map(Value::from),
            proptest::collection::vec(0u8..3, 0..300).prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 64, 20, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..20).prop_map(Value::Array),
                proptest::collection::vec((inner.clone(), inner), 0..20).prop_map(Value::Dict),
            ]
        })
    }

    proptest! {
        #[test]
        fn roundtrip(v in arb_value()) {
            let out = v.encode();
            prop_assert_eq!(decode(&out).unwrap(), v);
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode(&bytes);
        }
    }
}
