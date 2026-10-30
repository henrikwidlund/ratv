use bytes::{Buf, BytesMut};
use ratv_hap::SharedSecret;
use ratv_hap::cipher::{NonceLayout, SessionCipher, TAG_LEN};

use crate::Error;
use crate::frame::{Frame, FrameType, HEADER_LEN, header};

/// Default limit for a single received frame.
pub const DEFAULT_MAX_FRAME_LEN: usize = 1 << 20;

const WRITE_INFO: &[u8] = b"ClientEncrypt-main";
const READ_INFO: &[u8] = b"ServerEncrypt-main";

/// Companion framing and encryption state for one TCP connection.
///
/// It does no IO. Pass received bytes to [`Connection::receive`], read frames
/// with [`Connection::poll_frame`], queue frames with [`Connection::send`] and
/// write the bytes from [`Connection::take_transmit`] to the socket.
#[derive(Debug)]
pub struct Connection {
    rx: BytesMut,
    tx: BytesMut,
    cipher: Option<SessionCipher>,
    max_frame_len: usize,
}

impl Default for Connection {
    fn default() -> Self {
        Self::new()
    }
}

impl Connection {
    /// Creates a plain-text connection.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_frame_len(DEFAULT_MAX_FRAME_LEN)
    }

    /// Creates a connection that rejects frames larger than `max_frame_len`.
    #[must_use]
    pub fn with_max_frame_len(max_frame_len: usize) -> Self {
        Self {
            rx: BytesMut::with_capacity(4096),
            tx: BytesMut::with_capacity(1024),
            cipher: None,
            max_frame_len: max_frame_len.min(0xFF_FFFF),
        }
    }

    /// Turns on encryption with keys from the pair-verify secret.
    /// All later frames in both directions are encrypted.
    pub fn enable_encryption(&mut self, secret: &SharedSecret) {
        let write = secret.derive(&[], WRITE_INFO);
        let read = secret.derive(&[], READ_INFO);
        self.cipher = Some(SessionCipher::new(&write, &read, NonceLayout::CounterFirst));
    }

    /// Turns on encryption with the keys of the device side.
    /// Use this to implement a fake device, for example in tests.
    pub fn enable_device_encryption(&mut self, secret: &SharedSecret) {
        let write = secret.derive(&[], READ_INFO);
        let read = secret.derive(&[], WRITE_INFO);
        self.cipher = Some(SessionCipher::new(&write, &read, NonceLayout::CounterFirst));
    }

    /// Returns true after encryption is enabled.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Adds received bytes.
    pub fn receive(&mut self, data: &[u8]) {
        self.rx.extend_from_slice(data);
    }

    /// Read buffer with at least `min` bytes of spare capacity.
    /// Append received data to it directly (for example with `read_buf`) to avoid a copy.
    pub fn receive_buffer(&mut self, min: usize) -> &mut BytesMut {
        self.rx.reserve(min);
        &mut self.rx
    }

    /// Returns the next complete frame, if any.
    pub fn poll_frame(&mut self) -> Result<Option<Frame>, Error> {
        if self.rx.len() < HEADER_LEN {
            return Ok(None);
        }
        let hdr: [u8; HEADER_LEN] = self.rx[..HEADER_LEN].try_into().expect("header length");
        let len = u32::from_be_bytes([0, hdr[1], hdr[2], hdr[3]]) as usize;
        if len > self.max_frame_len {
            return Err(Error::FrameTooLarge(len));
        }
        if self.rx.len() < HEADER_LEN + len {
            self.rx.reserve(HEADER_LEN + len - self.rx.len());
            return Ok(None);
        }
        self.rx.advance(HEADER_LEN);
        let mut payload = self.rx.split_to(len);
        if let Some(cipher) = &mut self.cipher {
            let ct_len = len.checked_sub(TAG_LEN).ok_or(Error::FrameTooShort)?;
            let tag: [u8; TAG_LEN] = payload[ct_len..].try_into().expect("tag length");
            cipher.open_in_place(&hdr, &mut payload[..ct_len], &tag)?;
            payload.truncate(ct_len);
        }
        Ok(Some(Frame {
            ty: FrameType(hdr[0]),
            payload: payload.freeze(),
        }))
    }

    /// Queues a frame for sending, encrypting it when encryption is on.
    pub fn send(&mut self, ty: FrameType, payload: &[u8]) {
        let tag_len = if self.cipher.is_some() { TAG_LEN } else { 0 };
        let hdr = header(ty, payload.len() + tag_len);
        self.tx.reserve(HEADER_LEN + payload.len() + tag_len);
        self.tx.extend_from_slice(&hdr);
        let start = self.tx.len();
        self.tx.extend_from_slice(payload);
        if let Some(cipher) = &mut self.cipher {
            let tag = cipher.seal_in_place(&hdr, &mut self.tx[start..]);
            self.tx.extend_from_slice(&tag);
        }
    }

    /// Returns true if bytes are waiting to be sent.
    #[must_use]
    pub fn has_transmit(&self) -> bool {
        !self.tx.is_empty()
    }

    /// Takes all queued bytes to send.
    pub fn take_transmit(&mut self) -> BytesMut {
        self.tx.split()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Connection, Connection) {
        (Connection::new(), Connection::new())
    }

    /// Device side: read and write keys are swapped.
    fn server_cipher(secret: &SharedSecret) -> SessionCipher {
        let write = secret.derive(&[], READ_INFO);
        let read = secret.derive(&[], WRITE_INFO);
        SessionCipher::new(&write, &read, NonceLayout::CounterFirst)
    }

    #[test]
    fn plain_frames() {
        let (mut a, mut b) = pair();
        a.send(FrameType::U_OPACK, b"hello");
        a.send(FrameType::NO_OP, b"");
        assert!(a.has_transmit());
        let bytes = a.take_transmit();
        assert!(!a.has_transmit());
        assert_eq!(bytes[..4], [7, 0, 0, 5]);
        b.receive(&bytes);
        let f = b.poll_frame().unwrap().unwrap();
        assert_eq!(f.ty, FrameType::U_OPACK);
        assert_eq!(&f.payload[..], b"hello");
        let f = b.poll_frame().unwrap().unwrap();
        assert_eq!(f.ty, FrameType::NO_OP);
        assert!(f.payload.is_empty());
        assert_eq!(b.poll_frame().unwrap(), None);
    }

    #[test]
    fn partial_input() {
        let (mut a, mut b) = pair();
        a.send(FrameType::E_OPACK, &[9; 300]);
        let bytes = a.take_transmit();
        for (i, chunk) in bytes.chunks(7).enumerate() {
            assert_eq!(b.poll_frame().unwrap(), None, "chunk {i}");
            b.receive_buffer(chunk.len()).extend_from_slice(chunk);
        }
        assert_eq!(b.poll_frame().unwrap().unwrap().payload.len(), 300);
    }

    #[test]
    fn encrypted_frames() {
        let secret = test_secret();
        let mut client = Connection::new();
        client.enable_encryption(&secret);
        assert!(client.is_encrypted());
        client.send(FrameType::E_OPACK, b"secret message");
        client.send(FrameType::E_OPACK, b"second");
        let mut bytes = client.take_transmit();
        assert_eq!(bytes[..4], [8, 0, 0, 14 + 16]);

        // Decrypt as the device would.
        let mut server = server_cipher(&secret);
        for expected in [&b"secret message"[..], b"second"] {
            let hdr: [u8; 4] = bytes[..4].try_into().unwrap();
            let len = u32::from_be_bytes([0, hdr[1], hdr[2], hdr[3]]) as usize;
            let mut body = bytes[4..4 + len].to_vec();
            let tag: [u8; 16] = body[len - 16..].try_into().unwrap();
            server
                .open_in_place(&hdr, &mut body[..len - 16], &tag)
                .unwrap();
            assert_eq!(&body[..len - 16], expected);
            bytes.advance(4 + len);
        }

        // Device reply.
        let mut reply = b"reply".to_vec();
        let hdr = header(FrameType::E_OPACK, reply.len() + 16);
        let tag = server.seal_in_place(&hdr, &mut reply);
        client.receive(&hdr);
        client.receive(&reply);
        client.receive(&tag);
        assert_eq!(&client.poll_frame().unwrap().unwrap().payload[..], b"reply");

        // Device side connection talks to the client.
        let mut device = Connection::new();
        device.enable_device_encryption(&secret);
        let mut client = Connection::new();
        client.enable_encryption(&secret);
        device.send(FrameType::E_OPACK, b"from device");
        client.receive(&device.take_transmit());
        assert_eq!(
            &client.poll_frame().unwrap().unwrap().payload[..],
            b"from device"
        );
    }

    #[test]
    fn tampered_frame_fails() {
        let secret = test_secret();
        let mut client = Connection::new();
        client.enable_encryption(&secret);
        let mut server = server_cipher(&secret);
        let mut reply = b"reply".to_vec();
        let hdr = header(FrameType::E_OPACK, reply.len() + 16);
        let tag = server.seal_in_place(&hdr, &mut reply);
        reply[0] ^= 1;
        client.receive(&hdr);
        client.receive(&reply);
        client.receive(&tag);
        assert!(matches!(
            client.poll_frame(),
            Err(Error::Hap(ratv_hap::Error::Decrypt))
        ));
    }

    #[test]
    fn short_encrypted_frame() {
        let mut client = Connection::new();
        client.enable_encryption(&test_secret());
        client.receive(&[8, 0, 0, 3, 1, 2, 3]);
        assert_eq!(client.poll_frame(), Err(Error::FrameTooShort));
    }

    #[test]
    fn frame_limit() {
        let mut c = Connection::with_max_frame_len(10);
        c.receive(&[8, 0, 0, 11]);
        assert_eq!(c.poll_frame(), Err(Error::FrameTooLarge(11)));
        let mut c = Connection::default();
        c.receive(&[8, 0x20, 0, 0]);
        assert_eq!(c.poll_frame(), Err(Error::FrameTooLarge(0x20_0000)));
    }

    fn test_secret() -> SharedSecret {
        use ratv_hap::accessory::PairVerifyServer;
        use ratv_hap::{Credentials, pair_verify};
        // Build a secret through a real pair-verify with fixed keys.
        let device_seed = [0x22; 32];
        let client_seed = [0x11; 32];
        let creds = Credentials {
            accessory_ltpk: ed25519_public(&device_seed),
            client_ltsk: client_seed.into(),
            accessory_id: b"dev".to_vec(),
            client_id: b"cli".to_vec(),
        };
        let mut server = PairVerifyServer::new(
            b"dev",
            &device_seed,
            b"cli",
            ed25519_public(&client_seed),
            [3; 32],
        );
        let mut rng = ratv_hap::accessory::TestRng(1);
        let (m1, st) = pair_verify::start(&creds, &mut rng);
        let (m3, st) = st.handle_m2(&server.handle(&m1).unwrap()).unwrap();
        st.handle_m4(&server.handle(&m3).unwrap()).unwrap()
    }

    fn ed25519_public(seed: &[u8; 32]) -> [u8; 32] {
        ratv_hap::accessory::PairSetupServer::new(b"", seed, "0", [0; 16], &[1; 32]).ltpk()
    }
}
