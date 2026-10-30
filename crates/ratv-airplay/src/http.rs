//! Minimal HTTP/1.1 and RTSP/1.0 message writer and incremental parser.

use std::ops::Range;

use bytes::{Bytes, BytesMut};

use crate::Error;

/// Maximum size of the start line and headers.
pub const MAX_HEAD_LEN: usize = 16 * 1024;

/// Maximum body size.
pub const MAX_BODY_LEN: usize = 1024 * 1024;

/// Appends a request. `Content-Length` is added if `body` is not empty.
pub fn write_request(
    method: &str,
    uri: &str,
    protocol: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    out: &mut BytesMut,
) {
    out.reserve(64 + body.len());
    for part in [method, " ", uri, " ", protocol, "\r\n"] {
        out.extend_from_slice(part.as_bytes());
    }
    write_rest(headers, body, out);
}

/// Appends a response. `Content-Length` is added if `body` is not empty.
pub fn write_response(
    protocol: &str,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    out: &mut BytesMut,
) {
    out.reserve(64 + body.len());
    let code = itoa(status.into());
    for part in [protocol, " ", code.as_str(), " ", reason, "\r\n"] {
        out.extend_from_slice(part.as_bytes());
    }
    write_rest(headers, body, out);
}

fn write_rest(headers: &[(&str, &str)], body: &[u8], out: &mut BytesMut) {
    let mut has_len = false;
    for (k, v) in headers {
        has_len |= k.eq_ignore_ascii_case("content-length");
        for part in [k, ": ", v, "\r\n"] {
            out.extend_from_slice(part.as_bytes());
        }
    }
    if !body.is_empty() && !has_len {
        out.extend_from_slice(b"Content-Length: ");
        out.extend_from_slice(itoa(body.len() as u64).as_str().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
}

/// Formats a number without allocating on the heap for the digits.
pub(crate) const fn itoa(mut v: u64) -> Digits {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    Digits { buf, start: i }
}

pub(crate) struct Digits {
    buf: [u8; 20],
    start: usize,
}

impl Digits {
    pub(crate) fn as_str(&self) -> &str {
        str::from_utf8(&self.buf[self.start..]).expect("ASCII digits")
    }
}

/// A parsed request or response. Owns its bytes; accessors do not copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    raw: Bytes,
    line: [Range<usize>; 3],
    headers: Vec<(Range<usize>, Range<usize>)>,
    body: Range<usize>,
}

impl Message {
    /// Takes one complete message from the start of `buf`.
    ///
    /// Returns `None` and leaves `buf` unchanged if more data is needed.
    pub fn parse(buf: &mut BytesMut) -> Result<Option<Self>, Error> {
        let Some(head_len) = find(buf, b"\r\n\r\n") else {
            return if buf.len() > MAX_HEAD_LEN {
                Err(Error::HeadTooLarge)
            } else {
                Ok(None)
            };
        };
        if head_len > MAX_HEAD_LEN {
            return Err(Error::HeadTooLarge);
        }
        let head =
            str::from_utf8(&buf[..head_len]).map_err(|_| Error::InvalidMessage("not UTF-8"))?;
        let mut lines = head.split("\r\n");
        let start = lines.next().unwrap_or_default();
        let mut parts = start.splitn(3, ' ');
        let (Some(p0), Some(p1), Some(p2)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(Error::InvalidMessage("start line"));
        };
        let line = [span(head, p0), span(head, p1), span(head, p2)];

        let mut headers = Vec::with_capacity(8);
        let mut body_len = 0;
        for l in lines {
            let (k, v) = l.split_once(':').ok_or(Error::InvalidMessage("header"))?;
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-length") {
                body_len = v
                    .parse()
                    .map_err(|_| Error::InvalidMessage("content-length"))?;
            }
            headers.push((span(head, k), span(head, v)));
        }
        if body_len > MAX_BODY_LEN {
            return Err(Error::TooLarge(body_len));
        }
        let total = head_len + 4 + body_len;
        if buf.len() < total {
            buf.reserve(total - buf.len());
            return Ok(None);
        }
        Ok(Some(Self {
            raw: buf.split_to(total).freeze(),
            line,
            headers,
            body: head_len + 4..total,
        }))
    }

    fn str(&self, r: &Range<usize>) -> &str {
        str::from_utf8(&self.raw[r.clone()]).unwrap_or_default()
    }

    /// True for a response (start line begins with the protocol).
    #[must_use]
    pub fn is_response(&self) -> bool {
        let first = self.str(&self.line[0]);
        first.starts_with("HTTP/") || first.starts_with("RTSP/")
    }

    /// Protocol and version, for example `RTSP/1.0`.
    #[must_use]
    pub fn protocol(&self) -> &str {
        let i = if self.is_response() { 0 } else { 2 };
        self.str(&self.line[i])
    }

    /// Status code of a response.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.is_response()
            .then(|| self.str(&self.line[1]).parse().ok())
            .flatten()
    }

    /// Method of a request.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        (!self.is_response()).then(|| self.str(&self.line[0]))
    }

    /// URI of a request.
    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        (!self.is_response()).then(|| self.str(&self.line[1]))
    }

    /// First header with `name`, compared without case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers()
            .find_map(|(k, v)| k.eq_ignore_ascii_case(name).then_some(v))
    }

    /// All headers in order.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers.iter().map(|(k, v)| (self.str(k), self.str(v)))
    }

    /// `CSeq` header.
    #[must_use]
    pub fn cseq(&self) -> Option<u32> {
        self.header("cseq")?.parse().ok()
    }

    /// Body bytes.
    #[must_use]
    pub fn body(&self) -> Bytes {
        self.raw.slice(self.body.clone())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Byte range of `part` inside `base`. `part` must be a slice of `base`.
fn span(base: &str, part: &str) -> Range<usize> {
    let start = part.as_ptr() as usize - base.as_ptr() as usize;
    start..start + part.len()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn request_roundtrip() {
        let mut out = BytesMut::new();
        write_request(
            "SETUP",
            "rtsp://10.0.0.2/123",
            "RTSP/1.0",
            &[
                ("CSeq", "3"),
                ("Content-Type", "application/x-apple-binary-plist"),
            ],
            b"body",
            &mut out,
        );
        assert_eq!(
            &out[..],
            b"SETUP rtsp://10.0.0.2/123 RTSP/1.0\r\nCSeq: 3\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: 4\r\n\r\nbody"
        );
        let m = Message::parse(&mut out).unwrap().unwrap();
        assert!(out.is_empty());
        assert!(!m.is_response());
        assert_eq!(m.method(), Some("SETUP"));
        assert_eq!(m.uri(), Some("rtsp://10.0.0.2/123"));
        assert_eq!(m.protocol(), "RTSP/1.0");
        assert_eq!(m.cseq(), Some(3));
        assert_eq!(
            m.header("content-type"),
            Some("application/x-apple-binary-plist")
        );
        assert_eq!(&m.body()[..], b"body");
        assert_eq!(m.status(), None);
    }

    #[test]
    fn response_roundtrip() {
        let mut out = BytesMut::new();
        write_response(
            "HTTP/1.1",
            200,
            "OK",
            &[("Content-Length", "0")],
            b"",
            &mut out,
        );
        write_response(
            "RTSP/1.0",
            470,
            "Connection Authorization Required",
            &[],
            b"xy",
            &mut out,
        );
        let a = Message::parse(&mut out).unwrap().unwrap();
        assert!(a.is_response());
        assert_eq!(a.status(), Some(200));
        assert_eq!(a.protocol(), "HTTP/1.1");
        assert!(a.body().is_empty());
        assert_eq!(a.method(), None);
        assert_eq!(a.uri(), None);
        let b = Message::parse(&mut out).unwrap().unwrap();
        assert_eq!(b.status(), Some(470));
        assert_eq!(&b.body()[..], b"xy");
        assert_eq!(b.headers().count(), 1);
    }

    #[test]
    fn partial_input() {
        let mut full = BytesMut::new();
        write_request("POST", "/feedback", "HTTP/1.1", &[], &[7; 100], &mut full);
        let mut buf = BytesMut::new();
        for chunk in full.chunks(9) {
            assert!(Message::parse(&mut buf).unwrap().is_none());
            buf.extend_from_slice(chunk);
        }
        assert_eq!(Message::parse(&mut buf).unwrap().unwrap().body().len(), 100);
    }

    #[test]
    fn errors() {
        let mut b = BytesMut::from(&b"BAD\r\n\r\n"[..]);
        assert!(matches!(
            Message::parse(&mut b),
            Err(Error::InvalidMessage(_))
        ));
        let mut b = BytesMut::from(&b"GET / HTTP/1.1\r\nNoColon\r\n\r\n"[..]);
        assert!(matches!(
            Message::parse(&mut b),
            Err(Error::InvalidMessage(_))
        ));
        let mut b = BytesMut::from(&b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n"[..]);
        assert!(matches!(
            Message::parse(&mut b),
            Err(Error::InvalidMessage(_))
        ));
        let mut b = BytesMut::from(&b"GET / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n"[..]);
        assert!(matches!(Message::parse(&mut b), Err(Error::TooLarge(_))));
        let mut b = BytesMut::from(&vec![b'a'; MAX_HEAD_LEN + 1][..]);
        assert!(matches!(Message::parse(&mut b), Err(Error::HeadTooLarge)));
        let mut b = BytesMut::from(&b"GET / HTTP/1.1\r\n\xff: x\r\n\r\n"[..]);
        assert!(matches!(
            Message::parse(&mut b),
            Err(Error::InvalidMessage(_))
        ));
    }

    #[test]
    fn numbers() {
        assert_eq!(itoa(0).as_str(), "0");
        assert_eq!(itoa(u64::MAX).as_str(), "18446744073709551615");
    }

    proptest! {
        #[test]
        fn parse_never_panics(data in proptest::collection::vec(any::<u8>(), 0..300)) {
            let mut b = BytesMut::from(&data[..]);
            let _ = Message::parse(&mut b);
        }
    }
}
