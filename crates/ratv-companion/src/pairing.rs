//! HAP pairing over Companion frames.
//!
//! Pairing TLV8 is sent in the `_pd` field of an OPACK dictionary.
//! Pair-setup messages also carry `_pwTy: 1` (PIN), pair-verify messages `_auTy: 4`.

use std::borrow::Cow;

use ratv_codec::opack::{self, Encoder, Value};

use crate::{Error, FrameType};

/// Pairing flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Pair-setup (PIN).
    Setup,
    /// Pair-verify.
    Verify,
}

impl Flow {
    /// Frame type for the first (`first == true`) or a later message.
    #[must_use]
    pub const fn frame_type(self, first: bool) -> FrameType {
        match (self, first) {
            (Self::Setup, true) => FrameType::PAIR_SETUP_START,
            (Self::Setup, false) => FrameType::PAIR_SETUP_NEXT,
            (Self::Verify, true) => FrameType::PAIR_VERIFY_START,
            (Self::Verify, false) => FrameType::PAIR_VERIFY_NEXT,
        }
    }
}

/// Wraps pairing TLV8 in an OPACK payload.
pub fn encode(flow: Flow, tlv: &[u8], out: &mut Vec<u8>) {
    let (key, value) = match flow {
        Flow::Setup => ("_pwTy", 1),
        Flow::Verify => ("_auTy", 4),
    };
    Encoder::new(out).dict(2, |e| {
        e.str("_pd").data(tlv);
        e.str(key).int(value);
    });
}

/// Extracts the pairing TLV8 from a received payload.
pub fn decode(payload: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    let value = opack::decode(payload)?;
    let Value::Dict(entries) = value else {
        return Err(Error::InvalidField("_pd"));
    };
    entries
        .into_iter()
        .find_map(|(k, v)| match (k, v) {
            (Value::Str(k), Value::Data(d)) if k == "_pd" => Some(d),
            _ => None,
        })
        .ok_or(Error::InvalidField("_pd"))
}

/// Device information sent to the Apple TV during pair-setup (M5).
#[derive(Debug, Clone, Copy)]
pub struct ClientInfo<'a> {
    /// Pairing identifier. Must match the HAP identifier.
    pub account_id: &'a str,
    /// Name shown in the Apple TV settings.
    pub name: &'a str,
    /// Device model, for example `iPhone10,6`.
    pub model: &'a str,
    /// MAC address bytes.
    pub mac: [u8; 6],
    /// Random 16-byte identity resolving key.
    pub alt_irk: [u8; 16],
}

impl ClientInfo<'_> {
    /// Encodes the info as OPACK for the HAP `NAME` item.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        Encoder::new(&mut out).dict(6, |e| {
            e.str("accountID").str(self.account_id);
            e.str("altIRK").data(&self.alt_irk);
            e.str("mac").data(&self.mac);
            e.str("model").str(self.model);
            e.str("name").str(self.name);
            e.str("wifiMAC").data(&self.mac);
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_types() {
        assert_eq!(Flow::Setup.frame_type(true), FrameType::PAIR_SETUP_START);
        assert_eq!(Flow::Setup.frame_type(false), FrameType::PAIR_SETUP_NEXT);
        assert_eq!(Flow::Verify.frame_type(true), FrameType::PAIR_VERIFY_START);
        assert_eq!(Flow::Verify.frame_type(false), FrameType::PAIR_VERIFY_NEXT);
    }

    #[test]
    fn roundtrip() {
        for flow in [Flow::Setup, Flow::Verify] {
            let mut out = Vec::new();
            encode(flow, &[6, 1, 1], &mut out);
            assert_eq!(&*decode(&out).unwrap(), &[6, 1, 1]);
            let v = opack::decode(&out).unwrap();
            let expected = if flow == Flow::Setup {
                ("_pwTy", 1)
            } else {
                ("_auTy", 4)
            };
            assert_eq!(v.get(expected.0).and_then(Value::as_i64), Some(expected.1));
        }
    }

    #[test]
    fn decode_errors() {
        assert!(matches!(decode(&[0x09]), Err(Error::InvalidField("_pd"))));
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(1, |e| {
            e.str("_pd").str("not data");
        });
        assert!(matches!(decode(&out), Err(Error::InvalidField("_pd"))));
        assert!(matches!(decode(&[0x00]), Err(Error::Opack(_))));
    }

    #[test]
    fn client_info() {
        let info = ClientInfo {
            account_id: "id",
            name: "ratv",
            model: "iPhone10,6",
            mac: [1, 2, 3, 4, 5, 6],
            alt_irk: [7; 16],
        };
        let v = opack::decode(&info.encode()).unwrap().into_owned();
        assert_eq!(v.get("accountID").and_then(Value::as_str), Some("id"));
        assert_eq!(v.get("name").and_then(Value::as_str), Some("ratv"));
        assert_eq!(
            v.get("wifiMAC").and_then(Value::as_bytes),
            Some(&[1, 2, 3, 4, 5, 6][..])
        );
        assert_eq!(
            v.get("altIRK").and_then(Value::as_bytes),
            Some(&[7; 16][..])
        );
    }
}
