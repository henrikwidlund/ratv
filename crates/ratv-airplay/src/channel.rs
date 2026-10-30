use bytes::BytesMut;

use crate::http::{self, Message};
use crate::{Error, FrameCipher};

/// HTTP/RTSP connection state with optional encryption.
///
/// Write received bytes into [`Channel::receive_buffer`], read messages with
/// [`Channel::poll_message`], and send the bytes from [`Channel::take_transmit`].
#[derive(Debug, Default)]
pub struct Channel {
    cipher: Option<FrameCipher>,
    rx: BytesMut,
    plain: BytesMut,
    tx: BytesMut,
    scratch: BytesMut,
}

impl Channel {
    /// Creates an unencrypted channel.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encrypts everything sent and received from now on.
    pub fn enable_encryption(&mut self, cipher: FrameCipher) {
        self.cipher = Some(cipher);
    }

    /// True after [`Channel::enable_encryption`].
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Buffer for received bytes, with room for at least `min` more bytes.
    pub fn receive_buffer(&mut self, min: usize) -> &mut BytesMut {
        self.rx.reserve(min);
        &mut self.rx
    }

    /// Next complete message.
    pub fn poll_message(&mut self) -> Result<Option<Message>, Error> {
        match &mut self.cipher {
            Some(c) => {
                c.open(&mut self.rx, &mut self.plain)?;
                Message::parse(&mut self.plain)
            }
            None => Message::parse(&mut self.rx),
        }
    }

    /// Queues a request.
    pub fn send_request(
        &mut self,
        method: &str,
        uri: &str,
        protocol: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        http::write_request(method, uri, protocol, headers, body, &mut buf);
        self.queue(&buf);
        self.scratch = buf;
    }

    /// Queues a response.
    pub fn send_response(
        &mut self,
        protocol: &str,
        status: u16,
        reason: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        http::write_response(protocol, status, reason, headers, body, &mut buf);
        self.queue(&buf);
        self.scratch = buf;
    }

    fn queue(&mut self, plain: &[u8]) {
        match &mut self.cipher {
            Some(c) => c.seal(plain, &mut self.tx),
            None => self.tx.extend_from_slice(plain),
        }
    }

    /// Answers a device request with an empty 200, as the event channel must.
    pub fn acknowledge(&mut self, request: &Message) {
        let mut headers = [
            ("Content-Length", "0"),
            ("Audio-Latency", "0"),
            ("", ""),
            ("", ""),
        ];
        let mut n = 2;
        for name in ["Server", "CSeq"] {
            if let Some(v) = request.header(name) {
                headers[n] = (name, v);
                n += 1;
            }
        }
        self.send_response(request.protocol(), 200, "OK", &headers[..n], b"");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn encrypted_pair() -> (Channel, Channel) {
        let (a, b) = ([5; 32], [6; 32]);
        let mut client = Channel::new();
        client.enable_encryption(FrameCipher::new(&a, &b));
        let mut device = Channel::new();
        device.enable_encryption(FrameCipher::new(&b, &a));
        (client, device)
    }

    fn deliver(from: &mut Channel, to: &mut Channel) {
        let bytes = from.take_transmit();
        to.receive_buffer(bytes.len()).extend_from_slice(&bytes);
    }

    #[test]
    fn plain_request() {
        let mut a = Channel::new();
        let mut b = Channel::new();
        assert!(!a.is_encrypted());
        a.send_request("GET", "/info", "HTTP/1.1", &[], b"");
        assert!(a.has_transmit());
        deliver(&mut a, &mut b);
        assert!(!a.has_transmit());
        let m = b.poll_message().unwrap().unwrap();
        assert_eq!(m.uri(), Some("/info"));
        assert!(b.poll_message().unwrap().is_none());
    }

    #[test]
    fn encrypted_exchange() {
        let (mut client, mut device) = encrypted_pair();
        client.send_request(
            "RECORD",
            "rtsp://x/1",
            "RTSP/1.0",
            &[("CSeq", "4")],
            &[1; 3000],
        );
        deliver(&mut client, &mut device);
        let req = device.poll_message().unwrap().unwrap();
        assert_eq!(req.body().len(), 3000);

        device.acknowledge(&req);
        deliver(&mut device, &mut client);
        let resp = client.poll_message().unwrap().unwrap();
        assert_eq!(resp.status(), Some(200));
        assert_eq!(resp.protocol(), "RTSP/1.0");
        assert_eq!(resp.cseq(), Some(4));
        assert_eq!(resp.header("audio-latency"), Some("0"));
    }
}
