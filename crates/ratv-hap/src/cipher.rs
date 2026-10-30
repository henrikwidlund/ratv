//! ChaCha20-Poly1305 encryption for pairing messages and session frames.

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};

use crate::Error;

/// Length of the Poly1305 authentication tag.
pub const TAG_LEN: usize = 16;

/// Where the 64-bit frame counter goes in the 12-byte nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceLayout {
    /// Counter at offset 0, followed by 4 zero bytes. Used by Companion.
    CounterFirst,
    /// 4 zero bytes, then the counter. Used by AirPlay (HAP).
    CounterLast,
}

impl NonceLayout {
    fn nonce(self, counter: u64) -> Nonce {
        let mut n = [0u8; 12];
        match self {
            Self::CounterFirst => n[..8].copy_from_slice(&counter.to_le_bytes()),
            Self::CounterLast => n[4..].copy_from_slice(&counter.to_le_bytes()),
        }
        n.into()
    }
}

/// Session cipher with separate keys and counters per direction.
pub struct SessionCipher {
    seal: ChaCha20Poly1305,
    open: ChaCha20Poly1305,
    seal_counter: u64,
    open_counter: u64,
    layout: NonceLayout,
}

impl SessionCipher {
    /// Creates a cipher. `write_key` encrypts outgoing data, `read_key` decrypts incoming data.
    #[must_use]
    pub fn new(write_key: &[u8; 32], read_key: &[u8; 32], layout: NonceLayout) -> Self {
        Self {
            seal: ChaCha20Poly1305::new(write_key.into()),
            open: ChaCha20Poly1305::new(read_key.into()),
            seal_counter: 0,
            open_counter: 0,
            layout,
        }
    }

    /// Encrypts `buf` in place and returns the tag.
    pub fn seal_in_place(&mut self, aad: &[u8], buf: &mut [u8]) -> [u8; TAG_LEN] {
        let nonce = self.layout.nonce(self.seal_counter);
        self.seal_counter += 1;
        self.seal
            .encrypt_inout_detached(&nonce, aad, buf.into())
            .expect("ChaCha20 message length within limits")
            .into()
    }

    /// Decrypts `buf` in place after checking `tag`.
    pub fn open_in_place(
        &mut self,
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), Error> {
        let nonce = self.layout.nonce(self.open_counter);
        self.open
            .decrypt_inout_detached(&nonce, aad, buf.into(), &Tag::from(*tag))
            .map_err(|_| Error::Decrypt)?;
        self.open_counter += 1;
        Ok(())
    }
}

impl core::fmt::Debug for SessionCipher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionCipher")
            .field("seal_counter", &self.seal_counter)
            .field("open_counter", &self.open_counter)
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

fn label_nonce(label: [u8; 8]) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&label);
    n.into()
}

/// Encrypts a pairing message in place and appends the tag. The nonce is 4 zero bytes + `label`.
pub fn seal_message(key: &[u8; 32], label: &[u8; 8], data: &mut Vec<u8>) {
    let tag = ChaCha20Poly1305::new(key.into())
        .encrypt_inout_detached(&label_nonce(*label), &[], data.as_mut_slice().into())
        .expect("ChaCha20 message length within limits");
    data.extend_from_slice(&tag);
}

/// Decrypts a pairing message (ciphertext + tag) and returns the plaintext.
pub fn open_message(key: &[u8; 32], label: &[u8; 8], data: &[u8]) -> Result<Vec<u8>, Error> {
    let split = data.len().checked_sub(TAG_LEN).ok_or(Error::Decrypt)?;
    let (ct, tag) = data.split_at(split);
    let tag = Tag::try_from(tag).map_err(|_| Error::Decrypt)?;
    let mut out = ct.to_vec();
    ChaCha20Poly1305::new(key.into())
        .decrypt_inout_detached(&label_nonce(*label), &[], out.as_mut_slice().into(), &tag)
        .map_err(|_| Error::Decrypt)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: [u8; 32] = [1; 32];
    const K2: [u8; 32] = [2; 32];

    #[test]
    fn session_roundtrip_and_counters() {
        let mut client = SessionCipher::new(&K1, &K2, NonceLayout::CounterFirst);
        let mut server = SessionCipher::new(&K2, &K1, NonceLayout::CounterFirst);
        for i in 0..3u8 {
            let mut buf = vec![i; 10];
            let tag = client.seal_in_place(b"hdr", &mut buf);
            assert_ne!(buf, vec![i; 10]);
            server.open_in_place(b"hdr", &mut buf, &tag).unwrap();
            assert_eq!(buf, vec![i; 10]);
        }
        assert!(format!("{client:?}").contains("seal_counter: 3"));
    }

    #[test]
    fn wrong_aad_fails_and_keeps_counter() {
        let mut client = SessionCipher::new(&K1, &K2, NonceLayout::CounterLast);
        let mut server = SessionCipher::new(&K2, &K1, NonceLayout::CounterLast);
        let mut buf = b"hello".to_vec();
        let tag = client.seal_in_place(b"a", &mut buf);
        let mut copy = buf.clone();
        assert_eq!(
            server.open_in_place(b"b", &mut copy, &tag),
            Err(Error::Decrypt)
        );
        server.open_in_place(b"a", &mut buf, &tag).unwrap();
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn layouts_differ() {
        assert_eq!(
            NonceLayout::CounterFirst.nonce(1)[..],
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            NonceLayout::CounterLast.nonce(1)[..],
            [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
        );
        let mut a = SessionCipher::new(&K1, &K1, NonceLayout::CounterFirst);
        let mut b = SessionCipher::new(&K1, &K1, NonceLayout::CounterLast);
        a.seal_counter = 1;
        b.seal_counter = 1;
        let mut x = vec![0u8; 4];
        let mut y = vec![0u8; 4];
        a.seal_in_place(&[], &mut x);
        b.seal_in_place(&[], &mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn message_roundtrip() {
        let mut data = b"payload".to_vec();
        seal_message(&K1, b"PS-Msg05", &mut data);
        assert_eq!(data.len(), 7 + TAG_LEN);
        assert_eq!(open_message(&K1, b"PS-Msg05", &data).unwrap(), b"payload");
        assert_eq!(open_message(&K1, b"PS-Msg06", &data), Err(Error::Decrypt));
        assert_eq!(open_message(&K1, b"PS-Msg05", &[0; 3]), Err(Error::Decrypt));
    }
}
