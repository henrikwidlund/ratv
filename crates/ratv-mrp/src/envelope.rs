use bytes::{Buf, Bytes, BytesMut};
use prost::encoding::{
    WireType, decode_key, decode_varint, encode_key, encode_varint, encoded_len_varint, key_len,
};

use crate::{Error, Payload, proto};

const TYPE: u32 = 1;
const IDENTIFIER: u32 = 2;
const AUTH_TOKEN: u32 = 3;
const ERROR_CODE: u32 = 4;
const TIMESTAMP: u32 = 5;
const ERROR_DESCRIPTION: u32 = 78;
const UNIQUE_IDENTIFIER: u32 = 85;

/// A `ProtocolMessage` with decoded header fields and an encoded payload.
///
/// Decoding does not copy: strings and the payload are slices of the input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    kind: i32,
    identifier: Option<Bytes>,
    error_code: i32,
    timestamp: Option<u64>,
    error_description: Option<Bytes>,
    unique_identifier: Option<Bytes>,
    payload: Option<(u32, Bytes)>,
}

impl Envelope {
    /// Decodes one message without the length prefix.
    pub fn decode(mut buf: Bytes) -> Result<Self, Error> {
        let mut env = Self::default();
        while buf.has_remaining() {
            let (tag, wire) = decode_key(&mut buf)?;
            match wire {
                WireType::Varint => {
                    let v = decode_varint(&mut buf)?;
                    match tag {
                        TYPE => env.kind = to_i32(v),
                        ERROR_CODE => env.error_code = to_i32(v),
                        TIMESTAMP => env.timestamp = Some(v),
                        _ => {}
                    }
                }
                WireType::LengthDelimited => {
                    let len = usize::try_from(decode_varint(&mut buf)?)
                        .ok()
                        .filter(|&n| n <= buf.len())
                        .ok_or(Error::Truncated)?;
                    let field = buf.split_to(len);
                    match tag {
                        IDENTIFIER => env.identifier = Some(utf8(field)?),
                        ERROR_DESCRIPTION => env.error_description = Some(utf8(field)?),
                        UNIQUE_IDENTIFIER => env.unique_identifier = Some(utf8(field)?),
                        AUTH_TOKEN => {}
                        _ if env.payload.is_none() => env.payload = Some((tag, field)),
                        _ => {}
                    }
                }
                WireType::SixtyFourBit => skip(&mut buf, 8)?,
                WireType::ThirtyTwoBit => skip(&mut buf, 4)?,
                other => return Err(Error::WireType(other as u64)),
            }
        }
        Ok(env)
    }

    /// `ProtocolMessage.type`.
    #[must_use]
    pub const fn kind(&self) -> i32 {
        self.kind
    }

    /// Request identifier. Responses repeat the identifier of the request.
    #[must_use]
    pub fn identifier(&self) -> Option<&str> {
        self.identifier
            .as_deref()
            .and_then(|b| str::from_utf8(b).ok())
    }

    /// `ErrorCode` value, 0 for no error.
    #[must_use]
    pub const fn error_code(&self) -> i32 {
        self.error_code
    }

    /// Error text sent with a non-zero error code.
    #[must_use]
    pub fn error_description(&self) -> Option<&str> {
        self.error_description
            .as_deref()
            .and_then(|b| str::from_utf8(b).ok())
    }

    /// Sender timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    /// Unique message identifier.
    #[must_use]
    pub fn unique_identifier(&self) -> Option<&str> {
        self.unique_identifier
            .as_deref()
            .and_then(|b| str::from_utf8(b).ok())
    }

    /// Field number of the payload.
    #[must_use]
    pub fn payload_field(&self) -> Option<u32> {
        self.payload.as_ref().map(|(f, _)| *f)
    }

    /// Encoded payload.
    #[must_use]
    pub fn payload_bytes(&self) -> Option<&Bytes> {
        self.payload.as_ref().map(|(_, b)| b)
    }

    /// True if the payload is a `P`.
    #[must_use]
    pub fn is<P: Payload>(&self) -> bool {
        self.payload_field() == Some(P::FIELD)
    }

    /// Decodes the payload as `P`.
    pub fn payload<P: Payload>(&self) -> Result<P, Error> {
        match &self.payload {
            Some((field, bytes)) if *field == P::FIELD => Ok(P::decode(bytes.clone())?),
            other => Err(Error::UnexpectedPayload {
                expected: P::FIELD,
                actual: other.as_ref().map(|(f, _)| *f),
            }),
        }
    }

    /// Returns the device error, if the error code is not 0.
    #[must_use]
    pub fn error(&self) -> Option<Error> {
        (self.error_code != 0).then(|| Error::Device {
            code: self.error_code,
            description: self.error_description().unwrap_or_default().to_owned(),
        })
    }
}

/// Appends `payload` as a length-prefixed `ProtocolMessage`.
///
/// Set `identifier` for requests that expect a response.
pub fn encode<P: Payload>(payload: &P, identifier: Option<&str>, out: &mut BytesMut) {
    let body = payload.encoded_len();
    let field = key_len(P::FIELD) + encoded_len_varint(body as u64) + body;
    write_header(P::TYPE, identifier, field, out);
    encode_key(P::FIELD, WireType::LengthDelimited, out);
    encode_varint(body as u64, out);
    payload.encode_raw(out);
}

/// Appends a length-prefixed `ProtocolMessage` that has only a type,
/// for example `GET_KEYBOARD_SESSION_MESSAGE`.
pub fn encode_type(
    kind: proto::protocol_message::Type,
    identifier: Option<&str>,
    out: &mut BytesMut,
) {
    write_header(kind as i32, identifier, 0, out);
}

/// Writes the length prefix, type and identifier. `rest` is the length of what follows.
fn write_header(kind: i32, identifier: Option<&str>, rest: usize, out: &mut BytesMut) {
    let kind = i64::from(kind).cast_unsigned();
    let mut len = key_len(TYPE) + encoded_len_varint(kind) + rest;
    if let Some(id) = identifier {
        len += key_len(IDENTIFIER) + encoded_len_varint(id.len() as u64) + id.len();
    }
    out.reserve(encoded_len_varint(len as u64) + len);
    encode_varint(len as u64, out);
    encode_key(TYPE, WireType::Varint, out);
    encode_varint(kind, out);
    if let Some(id) = identifier {
        encode_key(IDENTIFIER, WireType::LengthDelimited, out);
        encode_varint(id.len() as u64, out);
        out.extend_from_slice(id.as_bytes());
    }
}

/// Splits length-prefixed messages.
pub const fn frames(data: Bytes) -> Frames {
    Frames { buf: data }
}

/// Iterator returned by [`frames`]. Stops after the first error.
#[derive(Debug)]
pub struct Frames {
    buf: Bytes,
}

impl Iterator for Frames {
    type Item = Result<Envelope, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        let result = next_frame(&mut self.buf).and_then(Envelope::decode);
        if result.is_err() {
            self.buf.clear();
        }
        Some(result)
    }
}

fn next_frame(buf: &mut Bytes) -> Result<Bytes, Error> {
    let len = usize::try_from(decode_varint(buf)?)
        .ok()
        .filter(|&n| n <= buf.len())
        .ok_or(Error::Truncated)?;
    Ok(buf.split_to(len))
}

fn skip(buf: &mut Bytes, n: usize) -> Result<(), Error> {
    if buf.len() < n {
        return Err(Error::Truncated);
    }
    buf.advance(n);
    Ok(())
}

fn utf8(b: Bytes) -> Result<Bytes, Error> {
    str::from_utf8(&b).map_err(|_| Error::Utf8)?;
    Ok(b)
}

/// Protobuf `int32` and enums are sign-extended to 64 bits.
const fn to_i32(v: u64) -> i32 {
    v.cast_signed() as i32
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use prost::Message as _;

    use super::*;

    fn roundtrip<P: Payload>(p: &P, id: Option<&str>) -> Envelope {
        let mut out = BytesMut::new();
        encode(p, id, &mut out);
        let mut it = frames(out.freeze());
        let env = it.next().unwrap().unwrap();
        assert!(it.next().is_none());
        env
    }

    #[test]
    fn encode_decode() {
        let msg = proto::SendButtonEventMessage {
            usage_page: Some(1),
            usage: Some(0x89),
            button_down: Some(true),
        };
        let env = roundtrip(&msg, Some("abc"));
        assert_eq!(env.kind(), 39);
        assert_eq!(env.identifier(), Some("abc"));
        assert_eq!(env.payload_field(), Some(43));
        assert!(env.is::<proto::SendButtonEventMessage>());
        assert_eq!(env.payload::<proto::SendButtonEventMessage>().unwrap(), msg);
        assert!(env.error().is_none());
    }

    #[test]
    fn matches_prost_encoding() {
        let msg = proto::SetConnectionStateMessage { state: Some(2) };
        let mut ours = BytesMut::new();
        encode(&msg, Some("id"), &mut ours);
        let full = proto::ProtocolMessage {
            r#type: Some(38),
            identifier: Some("id".into()),
            set_connection_state_message: Some(msg),
            ..Default::default()
        };
        let reference = full.encode_length_delimited_to_vec();
        assert_eq!(&ours[..], &reference[..]);
    }

    #[test]
    fn decodes_header_fields() {
        let full = proto::ProtocolMessage {
            r#type: Some(2),
            identifier: Some("x".into()),
            authentication_token: Some("t".into()),
            error_code: Some(6),
            timestamp: Some(99),
            error_description: Some("bad".into()),
            unique_identifier: Some("u".into()),
            send_command_result_message: Some(proto::SendCommandResultMessage::default()),
            ..Default::default()
        };
        let env = Envelope::decode(full.encode_to_vec().into()).unwrap();
        assert_eq!(env.kind(), 2);
        assert_eq!(env.identifier(), Some("x"));
        assert_eq!(env.error_code(), 6);
        assert_eq!(env.timestamp(), Some(99));
        assert_eq!(env.error_description(), Some("bad"));
        assert_eq!(env.unique_identifier(), Some("u"));
        assert!(env.is::<proto::SendCommandResultMessage>());
        match env.error().unwrap() {
            Error::Device { code, description } => {
                assert_eq!(code, 6);
                assert_eq!(description, "bad");
            }
            other => panic!("unexpected {other}"),
        }
    }

    #[test]
    fn negative_enum() {
        let mut out = BytesMut::new();
        encode_key(TYPE, WireType::Varint, &mut out);
        encode_varint((-1i64).cast_unsigned(), &mut out);
        assert_eq!(Envelope::decode(out.freeze()).unwrap().kind(), -1);
    }

    #[test]
    fn skips_fixed_fields() {
        let mut out = BytesMut::new();
        encode_key(200, WireType::SixtyFourBit, &mut out);
        out.extend_from_slice(&[0; 8]);
        encode_key(201, WireType::ThirtyTwoBit, &mut out);
        out.extend_from_slice(&[0; 4]);
        encode_key(TYPE, WireType::Varint, &mut out);
        encode_varint(5, &mut out);
        let env = Envelope::decode(out.freeze()).unwrap();
        assert_eq!(env.kind(), 5);
        assert_eq!(env.payload_field(), None);
    }

    #[test]
    fn type_only() {
        let mut out = BytesMut::new();
        encode_type(
            proto::protocol_message::Type::GetKeyboardSessionMessage,
            Some("k"),
            &mut out,
        );
        let env = frames(out.freeze()).next().unwrap().unwrap();
        assert_eq!(env.kind(), 24);
        assert_eq!(env.identifier(), Some("k"));
        assert_eq!(env.payload_field(), None);
    }

    #[test]
    fn wrong_payload() {
        let env = roundtrip(&proto::SetConnectionStateMessage::default(), None);
        assert!(matches!(
            env.payload::<proto::SetStateMessage>(),
            Err(Error::UnexpectedPayload {
                expected: 9,
                actual: Some(_)
            })
        ));
        assert_eq!(env.identifier(), None);
    }

    #[test]
    fn errors() {
        assert!(matches!(
            Envelope::decode(Bytes::from_static(&[0x12, 0x05, b'a'])),
            Err(Error::Truncated)
        ));
        assert!(matches!(
            Envelope::decode(Bytes::from_static(&[0x12, 0x01, 0xff])),
            Err(Error::Utf8)
        ));
        assert!(matches!(
            Envelope::decode(Bytes::from_static(&[0x0b])),
            Err(Error::WireType(3))
        ));
        assert!(matches!(
            Envelope::decode(Bytes::from_static(&[0x09, 0x00])),
            Err(Error::Truncated)
        ));
        let mut it = frames(Bytes::from_static(&[0x05, 0x08]));
        assert!(matches!(it.next(), Some(Err(Error::Truncated))));
        assert!(it.next().is_none());
    }

    #[test]
    fn multiple_frames() {
        let mut out = BytesMut::new();
        encode_type(
            proto::protocol_message::Type::GetKeyboardSessionMessage,
            Some("1"),
            &mut out,
        );
        encode(
            &proto::SetConnectionStateMessage { state: Some(2) },
            None,
            &mut out,
        );
        let kinds: Vec<i32> = frames(out.freeze()).map(|e| e.unwrap().kind()).collect();
        assert_eq!(kinds, [24, 38]);
    }

    proptest! {
        #[test]
        fn decode_never_panics(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = Envelope::decode(Bytes::from(data.clone()));
            let _ = frames(Bytes::from(data)).count();
        }
    }
}
