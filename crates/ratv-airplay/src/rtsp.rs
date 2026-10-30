//! Requests on the control connection and their bodies.
//!
//! Remote control setup order:
//!
//! 1. [`Session::pair_verify`] (two steps), then encrypt with [`keys::control`](crate::keys::control).
//! 2. [`Session::setup_events`], connect to the event port.
//! 3. [`Session::record`].
//! 4. [`Session::setup_data_stream`], connect to the data port.
//! 5. [`Session::feedback`] every 2 seconds.

use std::borrow::Cow;

use ratv_codec::bplist::{self, Value};

use crate::http::{Message, itoa};
use crate::{Channel, Error};

/// `User-Agent` sent with every request.
pub const USER_AGENT: &str = "AirPlay/550.10";

/// Client type UUID of Media Remote, used in the data stream SETUP.
pub const MEDIA_REMOTE_CLIENT_TYPE: &str = "1910A70F-DBC0-4242-AF95-115DB30604E1";

const RTSP: &str = "RTSP/1.0";
const HTTP: &str = "HTTP/1.1";
const PLIST: &str = "application/x-apple-binary-plist";
const OCTET_STREAM: &str = "application/octet-stream";

/// Client identity sent in [`Session::setup_events`].
#[derive(Debug, Clone, Copy)]
pub struct ClientInfo<'a> {
    /// Name shown on the device.
    pub name: &'a str,
    /// Model, for example `iPhone14,3`.
    pub model: &'a str,
    /// OS name, for example `iPhone OS`.
    pub os_name: &'a str,
    /// OS version, for example `17.0`.
    pub os_version: &'a str,
    /// OS build, for example `21A329`.
    pub os_build_version: &'a str,
    /// AirPlay source version, for example `550.10`.
    pub source_version: &'a str,
    /// Device id, `AA:BB:CC:DD:EE:FF`.
    pub device_id: &'a str,
    /// MAC address, same format as `device_id`.
    pub mac_address: &'a str,
    /// Random UUID for this session, upper case.
    pub session_uuid: &'a str,
}

/// Data stream parameters for [`Session::setup_data_stream`].
#[derive(Debug, Clone, Copy)]
pub struct DataStreamSetup<'a> {
    /// Random UUID, upper case.
    pub channel_id: &'a str,
    /// Random UUID, upper case.
    pub client_uuid: &'a str,
    /// Random seed. Also used in [`keys::data_stream`](crate::keys::data_stream).
    pub seed: u64,
}

/// RTSP session state: sequence numbers and client identifiers.
#[derive(Debug)]
pub struct Session {
    cseq: u32,
    url: String,
    dacp_id: String,
    active_remote: String,
}

impl Session {
    /// Creates a session. `local_host` is our IP address as the device sees it,
    /// `session_id`, `dacp_id` and `active_remote` should be random.
    #[must_use]
    pub fn new(local_host: &str, session_id: u32, dacp_id: u64, active_remote: u32) -> Self {
        Self {
            cseq: 0,
            url: format!("rtsp://{local_host}/{session_id}"),
            dacp_id: format!("{dacp_id:016X}"),
            active_remote: active_remote.to_string(),
        }
    }

    /// Session URL used in RTSP requests.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// `POST /pair-verify` with a TLV8 body.
    pub fn pair_verify(&mut self, ch: &mut Channel, tlv: &[u8]) -> u32 {
        self.pair_request(ch, "/pair-verify", tlv, None)
    }

    /// `POST /pair-setup` with a TLV8 body. `client_name` is shown with the PIN.
    pub fn pair_setup(&mut self, ch: &mut Channel, tlv: &[u8], client_name: Option<&str>) -> u32 {
        self.pair_request(ch, "/pair-setup", tlv, client_name)
    }

    /// `POST /pair-pin-start`: asks the device to show a PIN.
    /// tvOS 27 shows the PIN only if `client_name` is set.
    pub fn pair_pin_start(&mut self, ch: &mut Channel, client_name: Option<&str>) -> u32 {
        self.pair_request(ch, "/pair-pin-start", b"", client_name)
    }

    fn pair_request(
        &mut self,
        ch: &mut Channel,
        path: &str,
        body: &[u8],
        client_name: Option<&str>,
    ) -> u32 {
        let cseq = self.next();
        let headers = [
            ("User-Agent", USER_AGENT),
            ("Connection", "keep-alive"),
            ("X-Apple-HKP", "3"),
            ("Content-Type", OCTET_STREAM),
            ("X-Apple-Client-Name", client_name.unwrap_or_default()),
        ];
        let n = if client_name.is_some() { 5 } else { 4 };
        ch.send_request("POST", path, HTTP, &headers[..n], body);
        cseq
    }

    /// SETUP for the event connection. See [`event_port`].
    pub fn setup_events(&mut self, ch: &mut Channel, info: &ClientInfo<'_>) -> u32 {
        let body = Value::Dict(vec![
            ("isRemoteControlOnly".into(), true.into()),
            ("osName".into(), info.os_name.into()),
            ("sourceVersion".into(), info.source_version.into()),
            ("timingProtocol".into(), "None".into()),
            ("model".into(), info.model.into()),
            ("deviceID".into(), info.device_id.into()),
            ("osVersion".into(), info.os_version.into()),
            ("osBuildVersion".into(), info.os_build_version.into()),
            ("macAddress".into(), info.mac_address.into()),
            ("sessionUUID".into(), info.session_uuid.into()),
            ("name".into(), info.name.into()),
        ])
        .encode();
        self.rtsp(ch, "SETUP", Some(PLIST), &body)
    }

    /// SETUP for the MRP data stream. See [`data_stream_port`].
    pub fn setup_data_stream(&mut self, ch: &mut Channel, setup: &DataStreamSetup<'_>) -> u32 {
        let stream = Value::Dict(vec![
            ("controlType".into(), Value::Int(2)),
            ("channelID".into(), setup.channel_id.into()),
            ("seed".into(), Value::Int(setup.seed.cast_signed())),
            ("clientUUID".into(), setup.client_uuid.into()),
            ("type".into(), Value::Int(130)),
            ("wantsDedicatedSocket".into(), true.into()),
            ("clientTypeUUID".into(), MEDIA_REMOTE_CLIENT_TYPE.into()),
        ]);
        let body =
            Value::Dict(vec![(Cow::Borrowed("streams"), Value::Array(vec![stream]))]).encode();
        self.rtsp(ch, "SETUP", Some(PLIST), &body)
    }

    /// RECORD, sent after the event connection is open.
    pub fn record(&mut self, ch: &mut Channel) -> u32 {
        self.rtsp(ch, "RECORD", None, b"")
    }

    /// TEARDOWN, ends the session.
    pub fn teardown(&mut self, ch: &mut Channel) -> u32 {
        self.rtsp(ch, "TEARDOWN", None, b"")
    }

    /// `POST /feedback` keep-alive.
    pub fn feedback(&mut self, ch: &mut Channel) -> u32 {
        let cseq = self.next();
        let c = itoa(cseq.into());
        ch.send_request(
            "POST",
            "/feedback",
            RTSP,
            &[("CSeq", c.as_str()), ("User-Agent", USER_AGENT)],
            b"",
        );
        cseq
    }

    fn rtsp(
        &mut self,
        ch: &mut Channel,
        method: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> u32 {
        let cseq = self.next();
        let c = itoa(cseq.into());
        let mut headers = [
            ("CSeq", c.as_str()),
            ("User-Agent", USER_AGENT),
            ("DACP-ID", self.dacp_id.as_str()),
            ("Active-Remote", self.active_remote.as_str()),
            ("Client-Instance", self.dacp_id.as_str()),
            ("Content-Type", ""),
        ];
        let n = match content_type {
            Some(t) => {
                headers[5].1 = t;
                6
            }
            None => 5,
        };
        ch.send_request(method, &self.url, RTSP, &headers[..n], body);
        cseq
    }

    fn next(&mut self) -> u32 {
        let c = self.cseq;
        self.cseq = self.cseq.wrapping_add(1);
        c
    }
}

/// Checks the status and returns the decoded plist body.
fn plist_body(resp: &Message) -> Result<bplist::Value<'static>, Error> {
    check(resp)?;
    Ok(bplist::decode(&resp.body())?.into_owned())
}

/// Returns an error unless the response status is 200.
pub fn check(resp: &Message) -> Result<(), Error> {
    match resp.status() {
        Some(200) => Ok(()),
        Some(s) => Err(Error::Status(s)),
        None => Err(Error::InvalidMessage("not a response")),
    }
}

/// Event port from the [`Session::setup_events`] response.
pub fn event_port(resp: &Message) -> Result<u16, Error> {
    plist_body(resp)?
        .get("eventPort")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or(Error::MissingField("eventPort"))
}

/// Data port and stream id from the [`Session::setup_data_stream`] response.
pub fn data_stream_port(resp: &Message) -> Result<(u16, Option<u64>), Error> {
    let body = plist_body(resp)?;
    let stream = body
        .get("streams")
        .and_then(Value::as_array)
        .and_then(<[_]>::first)
        .ok_or(Error::MissingField("streams"))?;
    let port = stream
        .get("dataPort")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or(Error::MissingField("dataPort"))?;
    Ok((port, stream.get("streamID").and_then(Value::as_u64)))
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::*;

    fn sent(ch: &mut Channel) -> Message {
        let mut b = ch.take_transmit();
        Message::parse(&mut b).unwrap().unwrap()
    }

    fn info() -> ClientInfo<'static> {
        ClientInfo {
            name: "ratv",
            model: "iPhone14,3",
            os_name: "iPhone OS",
            os_version: "17.0",
            os_build_version: "21A329",
            source_version: "550.10",
            device_id: "AA:BB:CC:DD:EE:FF",
            mac_address: "AA:BB:CC:DD:EE:FF",
            session_uuid: "11111111-2222-3333-4444-555555555555",
        }
    }

    #[test]
    fn rtsp_requests() {
        let mut ch = Channel::new();
        let mut s = Session::new("10.0.0.5", 1234, 0xAB, 99);
        assert_eq!(s.url(), "rtsp://10.0.0.5/1234");

        assert_eq!(s.setup_events(&mut ch, &info()), 0);
        let m = sent(&mut ch);
        assert_eq!(m.method(), Some("SETUP"));
        assert_eq!(m.uri(), Some("rtsp://10.0.0.5/1234"));
        assert_eq!(m.cseq(), Some(0));
        assert_eq!(m.header("DACP-ID"), Some("00000000000000AB"));
        assert_eq!(m.header("Client-Instance"), Some("00000000000000AB"));
        assert_eq!(m.header("Active-Remote"), Some("99"));
        assert_eq!(m.header("Content-Type"), Some(PLIST));
        let body = bplist::decode(&m.body()).unwrap().into_owned();
        assert_eq!(
            body.get("isRemoteControlOnly").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            body.get("timingProtocol").and_then(Value::as_str),
            Some("None")
        );
        assert_eq!(body.get("name").and_then(Value::as_str), Some("ratv"));

        s.record(&mut ch);
        let m = sent(&mut ch);
        assert_eq!(m.method(), Some("RECORD"));
        assert_eq!(m.cseq(), Some(1));
        assert_eq!(m.header("Content-Type"), None);

        s.setup_data_stream(
            &mut ch,
            &DataStreamSetup {
                channel_id: "C",
                client_uuid: "U",
                seed: u64::MAX,
            },
        );
        let m = sent(&mut ch);
        let body = bplist::decode(&m.body()).unwrap().into_owned();
        let stream = &body.get("streams").and_then(Value::as_array).unwrap()[0];
        assert_eq!(stream.get("type").and_then(Value::as_i64), Some(130));
        assert_eq!(stream.get("seed").and_then(Value::as_i64), Some(-1));
        assert_eq!(
            stream.get("clientTypeUUID").and_then(Value::as_str),
            Some(MEDIA_REMOTE_CLIENT_TYPE)
        );

        s.feedback(&mut ch);
        let m = sent(&mut ch);
        assert_eq!(m.uri(), Some("/feedback"));
        assert_eq!(m.cseq(), Some(3));

        s.teardown(&mut ch);
        assert_eq!(sent(&mut ch).method(), Some("TEARDOWN"));
    }

    #[test]
    fn pairing_requests() {
        let mut ch = Channel::new();
        let mut s = Session::new("h", 1, 1, 1);
        s.pair_pin_start(&mut ch, Some("Living Room Remote"));
        let m = sent(&mut ch);
        assert_eq!(m.uri(), Some("/pair-pin-start"));
        assert_eq!(m.protocol(), "HTTP/1.1");
        assert_eq!(m.header("X-Apple-Client-Name"), Some("Living Room Remote"));
        s.pair_setup(&mut ch, b"\x06\x01\x01", None);
        let m = sent(&mut ch);
        assert_eq!(m.uri(), Some("/pair-setup"));
        assert_eq!(m.header("X-Apple-Client-Name"), None);
        s.pair_verify(&mut ch, b"\x06\x01\x01");
        let m = sent(&mut ch);
        assert_eq!(m.header("X-Apple-HKP"), Some("3"));
        assert_eq!(&m.body()[..], b"\x06\x01\x01");
    }

    fn response(status: u16, body: &Value<'_>) -> Message {
        let mut b = BytesMut::new();
        crate::http::write_response(RTSP, status, "OK", &[], &body.encode(), &mut b);
        Message::parse(&mut b).unwrap().unwrap()
    }

    #[test]
    fn parse_responses() {
        let r = response(
            200,
            &Value::Dict(vec![("eventPort".into(), Value::Int(50123))]),
        );
        assert_eq!(event_port(&r).unwrap(), 50123);
        let r = response(200, &Value::Dict(vec![]));
        assert!(matches!(
            event_port(&r),
            Err(Error::MissingField("eventPort"))
        ));

        let stream = Value::Dict(vec![
            ("dataPort".into(), Value::Int(4000)),
            ("streamID".into(), Value::Int(7)),
        ]);
        let r = response(
            200,
            &Value::Dict(vec![("streams".into(), Value::Array(vec![stream]))]),
        );
        assert_eq!(data_stream_port(&r).unwrap(), (4000, Some(7)));
        let r = response(
            200,
            &Value::Dict(vec![("streams".into(), Value::Array(vec![]))]),
        );
        assert!(matches!(
            data_stream_port(&r),
            Err(Error::MissingField("streams"))
        ));

        let r = response(470, &Value::Dict(vec![]));
        assert!(matches!(event_port(&r), Err(Error::Status(470))));
        let mut b = BytesMut::new();
        crate::http::write_request("GET", "/", RTSP, &[], b"", &mut b);
        let req = Message::parse(&mut b).unwrap().unwrap();
        assert!(matches!(check(&req), Err(Error::InvalidMessage(_))));
    }
}
