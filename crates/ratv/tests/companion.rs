//! End-to-end tests against a fake Apple TV over an in-memory stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratv::companion::{
    self, AttentionState, ClientOptions, Event, HidCommand, MediaControlFlags, PairOptions,
};
use ratv::{Credentials, Error};
use ratv_codec::opack::{Encoder, Value};
use ratv_companion::message::{Message, MessageType};
use ratv_companion::{Connection, FrameType, pairing};
use ratv_hap::accessory::{PairSetupServer, PairVerifyServer, PairedController};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const DEVICE_ID: &[u8] = b"58:55:CA:00:00:01";
const DEVICE_LTSK: [u8; 32] = [0x42; 32];
const PIN: &str = "1234";

#[derive(Default)]
struct Log {
    hid: Vec<(i64, i64)>,
    launched: Vec<String>,
    account: Option<String>,
    volume: Option<f64>,
    session_stopped: bool,
    /// `_hidT` events as (x, y, phase).
    touches: Vec<(i64, i64, i64)>,
}

/// `_tiD` archive of a focused text field, shaped like the one tvOS sends.
fn text_field_archive(text: &str) -> Vec<u8> {
    use ratv_codec::bplist::Value;
    Value::Dict(vec![
        (
            "$top".into(),
            Value::Dict(vec![
                ("sessionUUID".into(), Value::Uid(1)),
                ("documentState".into(), Value::Uid(2)),
            ]),
        ),
        (
            "$objects".into(),
            Value::Array(vec![
                Value::from("$null"),
                Value::from(&[7u8; 16][..]),
                Value::Dict(vec![("docSt".into(), Value::Uid(3))]),
                Value::Dict(vec![("contextBeforeInput".into(), Value::Uid(4))]),
                Value::from(text),
            ]),
        ),
    ])
    .encode()
}

/// Applies an archived `RTITextOperations` from `_tiC` to `text`.
fn apply_text_operation(data: &[u8], text: &mut String) {
    use ratv_codec::bplist::{self, Value};
    let v = bplist::decode(data).unwrap();
    let objects = v.get("$objects").and_then(Value::as_array).unwrap();
    let object = |v: &Value<'_>| -> Value<'static> {
        match v {
            Value::Uid(i) => objects[usize::try_from(*i).unwrap()].clone().into_owned(),
            other => other.clone().into_owned(),
        }
    };
    let ops = object(v.get("$top").unwrap().get("textOperations").unwrap());
    let target = object(ops.get("targetSessionUUID").unwrap());
    assert_eq!(
        target.get("NS.uuidbytes").and_then(Value::as_bytes),
        Some(&[7u8; 16][..])
    );
    if ops.get("textToAssert").is_some() {
        text.clear();
    }
    let output = object(ops.get("keyboardOutput").unwrap());
    if let Some(insert) = output.get("insertionText") {
        text.push_str(object(insert).as_str().unwrap());
    }
}

async fn read_frame(s: &mut DuplexStream, conn: &mut Connection) -> Option<ratv_companion::Frame> {
    loop {
        if let Some(f) = conn.poll_frame().unwrap() {
            return Some(f);
        }
        if s.read_buf(conn.receive_buffer(4096)).await.unwrap() == 0 {
            return None;
        }
    }
}

async fn send(s: &mut DuplexStream, conn: &mut Connection, ty: FrameType, payload: &[u8]) {
    conn.send(ty, payload);
    s.write_all(&conn.take_transmit()).await.unwrap();
}

fn pair_reply(tlv: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    Encoder::new(&mut out).dict(1, |e| {
        e.str("_pd").data(tlv);
    });
    out
}

/// Fake device side of pair-setup. Returns the paired controller.
async fn device_pair_setup(mut s: DuplexStream) -> PairedController {
    let mut conn = Connection::new();
    let mut server = PairSetupServer::new(DEVICE_ID, &DEVICE_LTSK, PIN, [9; 16], &[0x77; 32]);
    while server.controller().is_none() {
        let Some(frame) = read_frame(&mut s, &mut conn).await else {
            break;
        };
        let tlv = pairing::decode(&frame.payload).unwrap();
        let reply = server.handle(&tlv).unwrap();
        send(
            &mut s,
            &mut conn,
            FrameType::PAIR_SETUP_NEXT,
            &pair_reply(&reply),
        )
        .await;
    }
    server.controller().cloned().expect("paired")
}

fn response(xid: u32, id: &str, content: impl FnOnce(&mut Encoder<'_>)) -> Vec<u8> {
    let mut out = Vec::new();
    Encoder::new(&mut out).dict(4, |e| {
        e.str("_i").str(id);
        e.str("_x").int(xid.into());
        e.str("_t").int(MessageType::Response as i64);
        e.str("_c");
        content(e);
    });
    out
}

fn error_response(xid: u32, code: i64, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    Encoder::new(&mut out).dict(4, |e| {
        e.str("_x").int(xid.into());
        e.str("_t").int(3);
        e.str("_ec").int(code);
        e.str("_em").str(message);
    });
    out
}

fn empty(e: &mut Encoder<'_>) {
    e.dict(0, |_| {});
}

/// Fake device side of a session: pair-verify, then answers requests.
#[allow(clippy::too_many_lines)]
async fn device_session(mut s: DuplexStream, controller: PairedController, log: Arc<Mutex<Log>>) {
    let mut conn = Connection::new();
    let mut server = PairVerifyServer::new(
        DEVICE_ID,
        &DEVICE_LTSK,
        &controller.id,
        controller.ltpk,
        [0x55; 32],
    );
    while server.shared_secret().is_none() {
        let Some(frame) = read_frame(&mut s, &mut conn).await else {
            return;
        };
        let tlv = pairing::decode(&frame.payload).unwrap();
        let reply = server.handle(&tlv).unwrap();
        send(
            &mut s,
            &mut conn,
            FrameType::PAIR_VERIFY_NEXT,
            &pair_reply(&reply),
        )
        .await;
        if reply.windows(2).any(|w| w == [0x07, 0x01]) {
            return;
        }
    }
    conn.enable_device_encryption(server.shared_secret().unwrap());

    // A real device ends the session when another app is launched.
    let mut session_gone = false;
    // Focused text field on the device.
    let mut text = String::from("hel");
    // tvOS 26 ignores `Fetch*` requests until a TV Remote session is started.
    let mut tv_remote = false;
    while let Some(frame) = read_frame(&mut s, &mut conn).await {
        assert_eq!(frame.ty, FrameType::E_OPACK);
        let msg = Message::decode(&frame.payload).unwrap();
        let xid = msg.xid().unwrap();
        let id = msg.id().unwrap().to_owned();
        let int = |k: &str| msg.field(k).and_then(Value::as_i64);
        let reply = match id.as_str() {
            "_interest" => {
                let mut ev = Vec::new();
                Encoder::new(&mut ev).dict(4, |e| {
                    e.str("_i").str("_iMC");
                    e.str("_x").int(1);
                    e.str("_t").int(MessageType::Event as i64);
                    e.str("_c").dict(1, |e| {
                        e.str("_mcF").int(0x103);
                    });
                });
                Some(ev)
            }
            "_systemInfo" | "_tiStop" | "_touchStart" => Some(response(xid, &id, empty)),
            "_hidT" => {
                let touch = (
                    int("_cx").unwrap(),
                    int("_cy").unwrap(),
                    int("_tPh").unwrap(),
                );
                log.lock().unwrap().touches.push(touch);
                None
            }
            "_sessionStart" => Some(response(xid, &id, |e| {
                e.dict(1, |e| {
                    e.str("_sid").int(77);
                });
            })),
            "TVRCSessionStart" => {
                assert_eq!(
                    msg.field("ProtocolVersionKey").and_then(Value::as_str),
                    Some("1.2")
                );
                tv_remote = true;
                Some(response(xid, &id, empty))
            }
            "FetchAttentionState" | "FetchLaunchableApplicationsEvent" if !tv_remote => None,
            "_sessionStop" if session_gone => Some(error_response(xid, 58809, "Session not found")),
            "_sessionStop" => {
                log.lock().unwrap().session_stopped = true;
                Some(response(xid, &id, empty))
            }
            "_hidC" => {
                let command = int("_hidC").unwrap();
                log.lock()
                    .unwrap()
                    .hid
                    .push((command, int("_hBtS").unwrap()));
                // Like a real device, no reply when the screensaver starts.
                (command != 11).then(|| response(xid, &id, empty))
            }
            "_mcc" => match int("_mcc").unwrap() {
                5 => Some(response(xid, &id, |e| {
                    e.dict(1, |e| {
                        e.str("_vol").float(0.25);
                    });
                })),
                6 => {
                    log.lock().unwrap().volume = msg.field("_vol").and_then(Value::as_f64);
                    Some(response(xid, &id, empty))
                }
                _ => Some(response(xid, &id, empty)),
            },
            "FetchAttentionState" => Some(response(xid, &id, |e| {
                e.dict(1, |e| {
                    e.str("state").int(3);
                });
            })),
            "FetchUserAccountsEvent" => Some(response(xid, &id, |e| {
                e.dict(2, |e| {
                    e.str("acc-1").str("Anna");
                    e.str("acc-2").str("Ben");
                });
            })),
            "SwitchUserAccountEvent" => {
                let account = msg.field("SwitchAccountID").and_then(Value::as_str);
                log.lock().unwrap().account = account.map(str::to_owned);
                Some(response(xid, &id, empty))
            }
            "FetchLaunchableApplicationsEvent" => Some(response(xid, &id, |e| {
                e.dict(1, |e| {
                    e.str("com.netflix.Netflix").str("Netflix");
                });
            })),
            "_launchApp" => {
                let bundle = msg
                    .field("_bundleID")
                    .or_else(|| msg.field("_urlS"))
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned();
                if bundle == "missing" {
                    Some(error_response(xid, -6727, "App not found"))
                } else {
                    session_gone = true;
                    log.lock().unwrap().launched.push(bundle);
                    Some(response(xid, &id, empty))
                }
            }
            "_tiStart" => {
                let archive = text_field_archive(&text);
                Some(response(xid, &id, |e| {
                    e.dict(1, |e| {
                        e.str("_tiD").data(&archive);
                    });
                }))
            }
            "_tiC" => {
                assert_eq!(int("_tiV"), Some(1));
                let data = msg.field("_tiD").and_then(Value::as_bytes).unwrap();
                apply_text_operation(data, &mut text);
                None
            }
            "silent" => None,
            _ => panic!("unexpected request {id}"),
        };
        if let Some(reply) = reply {
            send(&mut s, &mut conn, FrameType::E_OPACK, &reply).await;
        }
    }
}

async fn pair() -> (Credentials, PairedController) {
    let (client, device) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_pair_setup(device));
    let prompt = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap();
    assert!(format!("{prompt:?}").contains("client_id"));
    let creds = prompt.finish(PIN).await.unwrap();
    let controller = device.await.unwrap();
    (creds, controller)
}

fn options() -> ClientOptions {
    ClientOptions {
        timeout: Duration::from_secs(2),
        ..ClientOptions::default()
    }
}

#[tokio::test]
async fn pairing_creates_credentials() {
    let (creds, controller) = pair().await;
    assert_eq!(creds.accessory_id, DEVICE_ID);
    assert_eq!(creds.client_id, controller.id);
    let meta = ratv_codec::opack::decode(controller.metadata.as_deref().unwrap()).unwrap();
    assert_eq!(meta.get("name").and_then(Value::as_str), Some("ratv"));
    // Credentials are the same after conversion to text and back.
    assert_eq!(creds.to_string().parse::<Credentials>().unwrap(), creds);
}

#[tokio::test]
async fn pairing_with_wrong_pin_fails() {
    let (client, device) = tokio::io::duplex(8192);
    tokio::spawn(device_pair_setup(device));
    let prompt = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap();
    let err = prompt.finish("0000").await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Hap(ratv_hap::Error::Device(
                ratv_hap::DeviceError::Authentication
            ))
        ),
        "{err}"
    );
}

#[tokio::test]
async fn session_commands() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_session(device_io, controller, log.clone()));

    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    assert!(client.is_connected());
    assert_eq!(client.session_id() >> 32, 77);
    let mut events = client.events();

    client.press(HidCommand::Select).await.unwrap();
    client.press(HidCommand::Screensaver).await.unwrap();
    client
        .hold(HidCommand::Home, Duration::from_millis(1))
        .await
        .unwrap();
    client.turn_on().await.unwrap();
    client.turn_off().await.unwrap();
    assert!((client.volume().await.unwrap() - 0.25).abs() < f64::EPSILON);
    client.set_volume(1.5).await.unwrap();
    client.media(companion::MediaControl::Play).await.unwrap();
    assert_eq!(client.power_state().await.unwrap(), AttentionState::Awake);
    let accounts = client.accounts().await.unwrap();
    assert_eq!(accounts[1].name, "Ben");
    client.switch_account(&accounts[1].id).await.unwrap();
    let apps = client.apps().await.unwrap();
    assert_eq!(apps[0].name, "Netflix");
    client.launch_app("com.netflix.Netflix").await.unwrap();
    client.open_url("https://example.com").await.unwrap();

    assert_eq!(client.text().await.unwrap().as_deref(), Some("hel"));
    client.append_text("lo").await.unwrap();
    assert_eq!(client.text().await.unwrap().as_deref(), Some("hello"));
    client.set_text("abc").await.unwrap();
    assert_eq!(client.text().await.unwrap().as_deref(), Some("abc"));
    client.clear_text().await.unwrap();
    assert_eq!(client.text().await.unwrap().as_deref(), Some(""));

    client
        .swipe((0.8, 0.5), (0.2, 0.5), Duration::from_millis(48))
        .await
        .unwrap();
    client.tap(0.25, 0.75).await.unwrap();

    match client.launch_app("missing").await.unwrap_err() {
        Error::Companion(ratv_companion::Error::Device { code, message, .. }) => {
            assert_eq!(code, -6727);
            assert_eq!(message, "App not found");
        }
        other => panic!("unexpected {other}"),
    }

    // Event sent by the device after `_interest` (may already be consumed before subscribe).
    if let Ok(Ok(Event::MediaControl(flags))) =
        tokio::time::timeout(Duration::from_millis(50), events.recv()).await
    {
        assert!(flags.contains(MediaControlFlags::VOLUME));
    }

    client.close().await.unwrap();
    device.await.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(&log.hid[..4], &[(6, 1), (6, 2), (11, 1), (11, 2)]);
    assert_eq!(log.hid.len(), 10);
    assert_eq!(log.volume, Some(1.0));
    assert_eq!(log.launched, ["com.netflix.Netflix", "https://example.com"]);
    assert_eq!(log.account.as_deref(), Some("acc-2"));
    // Press, two moves, release, on a 1000 x 1000 touch area. Then a tap.
    assert_eq!(
        log.touches,
        [
            (800, 500, 1),
            (600, 500, 3),
            (400, 500, 3),
            (200, 500, 4),
            (250, 750, 1),
            (250, 750, 4)
        ]
    );
    assert!(
        !log.session_stopped,
        "device ended the session after launch"
    );
}

/// The fake device never answers `silent`, so the request times out.
#[tokio::test]
async fn request_timeout() {
    struct Silent;
    impl ratv_companion::message::Outgoing for Silent {
        const ID: &'static str = "silent";
        fn content(&self, e: &mut Encoder<'_>) {
            empty(e);
        }
    }
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let opts = ClientOptions {
        timeout: Duration::from_millis(100),
        ..ClientOptions::default()
    };
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    assert!(matches!(client.request(&Silent).await, Err(Error::Timeout)));
}

#[tokio::test]
async fn disconnect_fails_pending_requests() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_session(device_io, controller, Arc::default()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    device.abort();
    let _ = device.await;
    let err = client.press(HidCommand::Up).await.unwrap_err();
    assert!(matches!(err, Error::Disconnected), "{err}");
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!client.is_connected());
}

#[tokio::test]
async fn verify_with_unknown_credentials_fails() {
    let (mut creds, controller) = pair().await;
    // Different client key: the device rejects the M3 signature.
    creds.client_ltsk = [1u8; 32].into();
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let err = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Hap(ratv_hap::Error::Device(_))),
        "{err}"
    );
}
