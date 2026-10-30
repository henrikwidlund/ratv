//! HomeKit TLV8: `type: u8, length: u8, value`.
//!
//! Values longer than 255 bytes are split into consecutive items with the same
//! type. The decoder joins them again.

use std::borrow::Cow;

/// Item types used in HAP pairing.
#[allow(missing_docs)]
pub mod tag {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const BACKOFF: u8 = 0x08;
    pub const CERTIFICATE: u8 = 0x09;
    pub const SIGNATURE: u8 = 0x0A;
    pub const PERMISSIONS: u8 = 0x0B;
    pub const FRAGMENT_DATA: u8 = 0x0C;
    pub const FRAGMENT_LAST: u8 = 0x0D;
    /// Device name (`0x11`). Companion pairing sends OPACK metadata here.
    pub const NAME: u8 = 0x11;
    pub const FLAGS: u8 = 0x13;
    pub const SEPARATOR: u8 = 0xFF;
}

/// TLV8 decode error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Item header or value is cut off.
    #[error("truncated TLV8 item at offset {0}")]
    Truncated(usize),
}

/// Appends one item to `out`, splitting it into 255-byte fragments when needed.
pub fn write(out: &mut Vec<u8>, ty: u8, value: &[u8]) {
    if value.is_empty() {
        out.extend_from_slice(&[ty, 0]);
        return;
    }
    out.reserve(value.len() + 2 * value.len().div_ceil(255));
    for chunk in value.chunks(255) {
        out.push(ty);
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
}

/// Builder for TLV8 messages.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a writer with pre-allocated capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Appends an item.
    #[must_use]
    pub fn item(mut self, ty: u8, value: &[u8]) -> Self {
        write(&mut self.buf, ty, value);
        self
    }

    /// Appends a single-byte item.
    #[must_use]
    pub fn byte(self, ty: u8, value: u8) -> Self {
        self.item(ty, &[value])
    }

    /// Returns the encoded bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Decoded TLV8 message. Items keep their order; fragments are joined.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tlv8<'a> {
    items: Vec<(u8, Cow<'a, [u8]>)>,
}

impl<'a> Tlv8<'a> {
    /// Decodes a TLV8 message. Values borrow from `input` unless fragmented.
    pub fn decode(input: &'a [u8]) -> Result<Self, Error> {
        let mut items: Vec<(u8, Cow<'a, [u8]>)> = Vec::new();
        let mut pos = 0;
        // True while the previous item was a full 255-byte fragment.
        let mut continues = false;
        while pos < input.len() {
            let header = input.get(pos..pos + 2).ok_or(Error::Truncated(pos))?;
            let (ty, len) = (header[0], usize::from(header[1]));
            let value = input
                .get(pos + 2..pos + 2 + len)
                .ok_or(Error::Truncated(pos))?;
            match items.last_mut() {
                Some((last_ty, last)) if continues && *last_ty == ty => {
                    last.to_mut().extend_from_slice(value);
                }
                _ => items.push((ty, Cow::Borrowed(value))),
            }
            continues = len == 255;
            pos += 2 + len;
        }
        Ok(Self { items })
    }

    /// Returns the first value with type `ty`.
    #[must_use]
    pub fn get(&self, ty: u8) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|(t, _)| *t == ty)
            .map(|(_, v)| v.as_ref())
    }

    /// Returns the first byte of the value with type `ty`.
    #[must_use]
    pub fn get_u8(&self, ty: u8) -> Option<u8> {
        self.get(ty).and_then(|v| v.first().copied())
    }

    /// Iterates over all items in order.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &[u8])> {
        self.items.iter().map(|(t, v)| (*t, v.as_ref()))
    }

    /// Number of items after joining fragments.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns true if there are no items.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn single_item() {
        let out = Writer::new().byte(tag::STATE, 1).finish();
        assert_eq!(out, [0x06, 0x01, 0x01]);
        let t = Tlv8::decode(&out).unwrap();
        assert_eq!(t.get_u8(tag::STATE), Some(1));
        assert_eq!(t.len(), 1);
        assert!(!t.is_empty());
    }

    #[test]
    fn empty_value() {
        let out = Writer::with_capacity(2).item(tag::SEPARATOR, &[]).finish();
        assert_eq!(out, [0xFF, 0x00]);
        let t = Tlv8::decode(&out).unwrap();
        assert_eq!(t.get(tag::SEPARATOR), Some(&[][..]));
        assert_eq!(t.get_u8(tag::SEPARATOR), None);
    }

    #[test]
    fn empty_input() {
        let t = Tlv8::decode(&[]).unwrap();
        assert!(t.is_empty());
        assert_eq!(t.get(0), None);
    }

    #[test]
    fn fragments_long_values() {
        let value = vec![0x42; 512];
        let out = Writer::new().item(tag::PUBLIC_KEY, &value).finish();
        assert_eq!(out.len(), 512 + 3 * 2);
        assert_eq!(out[..2], [0x03, 255]);
        assert_eq!(out[257..259], [0x03, 255]);
        assert_eq!(out[514..516], [0x03, 2]);
        let t = Tlv8::decode(&out).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(tag::PUBLIC_KEY), Some(&value[..]));
    }

    #[test]
    fn exact_255_is_one_fragment() {
        let value = vec![1; 255];
        let out = Writer::new()
            .item(tag::SALT, &value)
            .byte(tag::STATE, 2)
            .finish();
        let t = Tlv8::decode(&out).unwrap();
        assert_eq!(t.get(tag::SALT).map(<[u8]>::len), Some(255));
        assert_eq!(t.get_u8(tag::STATE), Some(2));
    }

    #[test]
    fn same_type_after_short_item_is_separate() {
        let input = [0x01, 0x01, 0xAA, 0x01, 0x01, 0xBB];
        let t = Tlv8::decode(&input).unwrap();
        let items: Vec<_> = t.iter().collect();
        assert_eq!(items, [(1, &[0xAA][..]), (1, &[0xBB][..])]);
    }

    #[test]
    fn multiple_fields_keep_order() {
        let out = Writer::new()
            .byte(tag::METHOD, 0)
            .byte(tag::STATE, 3)
            .item(tag::IDENTIFIER, b"test-device-12345")
            .item(tag::PUBLIC_KEY, &[0xAB; 32])
            .finish();
        let t = Tlv8::decode(&out).unwrap();
        let types: Vec<u8> = t.iter().map(|(ty, _)| ty).collect();
        assert_eq!(types, [0, 6, 1, 3]);
        assert_eq!(t.get(tag::IDENTIFIER), Some(&b"test-device-12345"[..]));
    }

    #[test]
    fn truncated() {
        assert_eq!(Tlv8::decode(&[0x01]), Err(Error::Truncated(0)));
        assert_eq!(Tlv8::decode(&[0x01, 0x02, 0xAA]), Err(Error::Truncated(0)));
        assert_eq!(
            Tlv8::decode(&[0x06, 0x01, 0x01, 0x02]),
            Err(Error::Truncated(3))
        );
    }

    proptest! {
        #[test]
        fn roundtrip(items in proptest::collection::vec((any::<u8>(), proptest::collection::vec(any::<u8>(), 0..700)), 0..6)) {
            // Adjacent items with the same type would merge, so make types unique.
            let mut w = Writer::new();
            let mut expected = Vec::new();
            for (i, (_, v)) in items.iter().enumerate() {
                let ty = i as u8;
                w = w.item(ty, v);
                expected.push((ty, v.as_slice()));
            }
            let out = w.finish();
            let t = Tlv8::decode(&out).unwrap();
            prop_assert_eq!(t.iter().collect::<Vec<_>>(), expected);
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = Tlv8::decode(&bytes);
        }
    }
}
