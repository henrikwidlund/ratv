//! Protobuf fields as text, without the schema. For logs.

use core::fmt;

use prost::encoding::{WireType, decode_key, decode_varint};
use ratv_codec::bplist::{self, Value as Plist};

/// Nested messages deeper than this are shown as bytes.
const MAX_DEPTH: u8 = 8;
/// Fields after this many are not shown.
const MAX_FIELDS: usize = 256;
/// Longer strings are shown as bytes.
const MAX_STRING: usize = 128;

/// Shows encoded protobuf fields as `{tag: value, ...}`.
///
/// Strings are shown as text, nested messages as `{...}`, binary plists as
/// `plist{...}` and other bytes as their length only, for example `bytes(120)`.
/// Fixed 32-bit and 64-bit values are shown as `f32` and `f64`.
#[derive(Clone, Copy)]
pub struct Fields<'a>(pub &'a [u8]);

impl fmt::Display for Fields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut left = MAX_FIELDS;
        if valid(self.0, 0) {
            write_message(f, self.0, 0, &mut left)
        } else {
            write!(f, "invalid({})", self.0.len())
        }
    }
}

impl fmt::Debug for Fields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// One field value.
enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// Reads the next field. `None` at the end or on bad data.
fn next<'a>(buf: &mut &'a [u8]) -> Option<(u32, Value<'a>)> {
    if buf.is_empty() {
        return None;
    }
    let (tag, wire) = decode_key(buf).ok()?;
    let value = match wire {
        WireType::Varint => Value::Varint(decode_varint(buf).ok()?),
        WireType::SixtyFourBit => {
            let (v, rest) = buf.split_first_chunk::<8>()?;
            *buf = rest;
            Value::Fixed64(u64::from_le_bytes(*v))
        }
        WireType::ThirtyTwoBit => {
            let (v, rest) = buf.split_first_chunk::<4>()?;
            *buf = rest;
            Value::Fixed32(u32::from_le_bytes(*v))
        }
        WireType::LengthDelimited => {
            let len = usize::try_from(decode_varint(buf).ok()?).ok()?;
            let (v, rest) = buf.split_at_checked(len)?;
            *buf = rest;
            Value::Bytes(v)
        }
        WireType::StartGroup | WireType::EndGroup => return None,
    };
    Some((tag, value))
}

/// True if all of `buf` reads as fields.
fn valid(mut buf: &[u8], depth: u8) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    while !buf.is_empty() {
        if next(&mut buf).is_none() {
            return false;
        }
    }
    true
}

/// Text without control characters, or `None`.
fn text(v: &[u8]) -> Option<&str> {
    let s = core::str::from_utf8(v).ok()?;
    (v.len() <= MAX_STRING && !s.chars().any(char::is_control)).then_some(s)
}

fn write_message(
    f: &mut fmt::Formatter<'_>,
    mut buf: &[u8],
    depth: u8,
    left: &mut usize,
) -> fmt::Result {
    f.write_str("{")?;
    let mut first = true;
    while let Some((tag, value)) = next(&mut buf) {
        if !first {
            f.write_str(", ")?;
        }
        first = false;
        if *left == 0 {
            return f.write_str("...}");
        }
        *left -= 1;
        write!(f, "{tag}: ")?;
        match value {
            Value::Varint(v) => write!(f, "{v}")?,
            Value::Fixed64(v) => write!(f, "{:?}", f64::from_bits(v))?,
            Value::Fixed32(v) => write!(f, "{:?}", f32::from_bits(v))?,
            Value::Bytes(v) => {
                if let Some(s) = text(v) {
                    write!(f, "{s:?}")?;
                } else if let Some(p) = v
                    .starts_with(b"bplist00")
                    .then(|| bplist::decode(v).ok())
                    .flatten()
                {
                    f.write_str("plist")?;
                    write_plist(f, &p)?;
                } else if !v.is_empty() && valid(v, depth + 1) {
                    write_message(f, v, depth + 1, left)?;
                } else {
                    write!(f, "bytes({})", v.len())?;
                }
            }
        }
    }
    f.write_str("}")
}

fn write_plist(f: &mut fmt::Formatter<'_>, value: &Plist<'_>) -> fmt::Result {
    match value {
        Plist::Bool(v) => write!(f, "{v}"),
        Plist::Int(v) => write!(f, "{v}"),
        Plist::Real(v) | Plist::Date(v) => write!(f, "{v:?}"),
        Plist::Data(v) => write!(f, "bytes({})", v.len()),
        Plist::String(v) => write!(f, "{v:?}"),
        Plist::Uid(v) => write!(f, "uid({v})"),
        Plist::Array(items) => {
            f.write_str("[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_plist(f, item)?;
            }
            f.write_str("]")
        }
        Plist::Dict(items) => {
            f.write_str("{")?;
            for (i, (key, item)) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{key:?}: ")?;
                write_plist(f, item)?;
            }
            f.write_str("}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        // 1: 150, 2: "hi", 3: {1: 1}, 4: f64 1.5, 5: f32 2.0
        let mut buf = vec![
            0x08, 0x96, 0x01, 0x12, 2, b'h', b'i', 0x1a, 2, 0x08, 0x01, 0x21,
        ];
        buf.extend_from_slice(&1.5f64.to_le_bytes());
        buf.push(0x2d);
        buf.extend_from_slice(&2.0f32.to_le_bytes());
        assert_eq!(
            Fields(&buf).to_string(),
            r#"{1: 150, 2: "hi", 3: {1: 1}, 4: 1.5, 5: 2.0}"#
        );
    }

    #[test]
    fn bytes_as_length() {
        // A group start tag is not valid inside, so the value is bytes.
        let buf = [0x0a, 3, 0x0b, 0xff, 0x00];
        assert_eq!(Fields(&buf).to_string(), "{1: bytes(3)}");
        assert_eq!(Fields(&[0x0a, 0]).to_string(), r#"{1: ""}"#);
    }

    #[test]
    fn plist() {
        let plist = Plist::Dict(vec![
            ("rate".into(), Plist::Real(16000.0)),
            ("id".into(), Plist::Int(1)),
            ("on".into(), Plist::Bool(true)),
            ("data".into(), Plist::Data(vec![1, 2].into())),
            (
                "list".into(),
                Plist::Array(vec![Plist::String("a".into()), Plist::Uid(3)]),
            ),
        ])
        .encode();
        let mut buf = vec![0x0a];
        buf.extend_from_slice(&u8::try_from(plist.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&plist);
        assert_eq!(
            Fields(&buf).to_string(),
            r#"{1: plist{"rate": 16000.0, "id": 1, "on": true, "data": bytes(2), "list": ["a", uid(3)]}}"#
        );
    }

    #[test]
    fn invalid() {
        assert_eq!(Fields(&[0x0a, 5, 1]).to_string(), "invalid(3)");
        assert_eq!(Fields(&[]).to_string(), "{}");
    }

    #[test]
    fn field_limit() {
        let buf: Vec<u8> = [0x08, 0x01].repeat(MAX_FIELDS + 1);
        let text = Fields(&buf).to_string();
        assert!(text.ends_with(", ...}"), "{text}");
        assert_eq!(text.matches("1: 1").count(), MAX_FIELDS);
    }
}
