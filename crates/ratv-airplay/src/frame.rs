use bytes::BytesMut;
use ratv_hap::cipher::{NonceLayout, SessionCipher, TAG_LEN};

use crate::Error;

/// Maximum plaintext per encrypted frame.
pub const BLOCK_LEN: usize = 1024;

const LEN_SIZE: usize = 2;

/// HAP framing used by all AirPlay connections after pair-verify.
///
/// Each frame is a 2-byte little-endian length (also the AAD), the
/// ChaCha20-Poly1305 ciphertext and a 16-byte tag.
#[derive(Debug)]
pub struct FrameCipher {
    cipher: SessionCipher,
}

impl FrameCipher {
    /// Creates a cipher from session keys.
    #[must_use]
    pub fn new(write_key: &[u8; 32], read_key: &[u8; 32]) -> Self {
        Self {
            cipher: SessionCipher::new(write_key, read_key, NonceLayout::CounterLast),
        }
    }

    /// Encrypts `plain` and appends the frames to `out`.
    pub fn seal(&mut self, plain: &[u8], out: &mut BytesMut) {
        out.reserve(plain.len() + plain.len().div_ceil(BLOCK_LEN) * (LEN_SIZE + TAG_LEN));
        for chunk in plain.chunks(BLOCK_LEN) {
            let len = (chunk.len() as u16).to_le_bytes();
            out.extend_from_slice(&len);
            let start = out.len();
            out.extend_from_slice(chunk);
            let tag = self.cipher.seal_in_place(&len, &mut out[start..]);
            out.extend_from_slice(&tag);
        }
    }

    /// Decrypts all complete frames at the start of `input` and appends the
    /// plaintext to `out`. Incomplete data stays in `input`.
    pub fn open(&mut self, input: &mut BytesMut, out: &mut BytesMut) -> Result<(), Error> {
        while input.len() >= LEN_SIZE {
            let len = usize::from(u16::from_le_bytes([input[0], input[1]]));
            let total = LEN_SIZE + len + TAG_LEN;
            if input.len() < total {
                input.reserve(total - input.len());
                break;
            }
            let mut frame = input.split_to(total);
            let (aad, rest) = frame.split_at_mut(LEN_SIZE);
            let (data, tag) = rest.split_at_mut(len);
            let tag: &[u8; TAG_LEN] = (&*tag).try_into().expect("tag length");
            self.cipher.open_in_place(aad, data, tag)?;
            out.extend_from_slice(data);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (FrameCipher, FrameCipher) {
        let (a, b) = ([1; 32], [2; 32]);
        (FrameCipher::new(&a, &b), FrameCipher::new(&b, &a))
    }

    #[test]
    fn splits_into_blocks() {
        let (mut tx, mut rx) = pair();
        let plain: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let mut wire = BytesMut::new();
        tx.seal(&plain, &mut wire);
        assert_eq!(wire.len(), 2500 + 3 * (LEN_SIZE + TAG_LEN));
        assert_eq!(&wire[..2], &1024u16.to_le_bytes());

        let mut input = BytesMut::new();
        let mut out = BytesMut::new();
        for chunk in wire.chunks(100) {
            input.extend_from_slice(chunk);
            rx.open(&mut input, &mut out).unwrap();
        }
        assert!(input.is_empty());
        assert_eq!(&out[..], &plain[..]);
    }

    #[test]
    fn known_frame() {
        // Counter 0, nonce = 4 zero bytes + counter (LE u64).
        let mut tx = FrameCipher::new(&[0x42; 32], &[0; 32]);
        let mut wire = BytesMut::new();
        tx.seal(b"hi", &mut wire);
        let mut c =
            ratv_hap::cipher::SessionCipher::new(&[0; 32], &[0x42; 32], NonceLayout::CounterLast);
        let mut data = [wire[2], wire[3]];
        let tag: [u8; 16] = wire[4..].try_into().unwrap();
        c.open_in_place(&wire[..2], &mut data, &tag).unwrap();
        assert_eq!(&data, b"hi");
    }

    #[test]
    fn tampered_frame_fails() {
        let (mut tx, mut rx) = pair();
        let mut wire = BytesMut::new();
        tx.seal(b"secret", &mut wire);
        wire[3] ^= 1;
        let mut out = BytesMut::new();
        assert!(rx.open(&mut wire, &mut out).is_err());
    }

    #[test]
    fn empty_input() {
        let (mut tx, _) = pair();
        let mut wire = BytesMut::new();
        tx.seal(b"", &mut wire);
        assert!(wire.is_empty());
    }
}
