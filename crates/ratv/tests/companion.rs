//! End-to-end tests against a fake Apple TV over an in-memory stream.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratv::companion::{
    self, AttentionState, ButtonState, ClientOptions, Event, HidCommand, MediaControlFlags,
    PairOptions,
};
use ratv::{Credentials, Error, ExposeSecret};
use ratv_codec::opack::{Encoder, Value};
use ratv_companion::message::{Message, MessageType};
use ratv_companion::{Connection, FrameType, pairing};
use ratv_hap::accessory::{PairSetupServer, PairVerifyServer, PairedController};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpListener;

const DEVICE_ID: &[u8] = b"58:55:CA:00:00:01";
const DEVICE_LTSK: [u8; 32] = [0x42; 32];
const PIN: &str = "1234";

#[derive(Default)]
// Test flags and results, not a state machine.
#[allow(clippy::struct_excessive_bools)]
struct Log {
    hid: Vec<(i64, i64)>,
    launched: Vec<String>,
    account: Option<String>,
    volume: Option<f64>,
    /// `_vol` in the volume reply; 0.25 when `None`.
    reply_volume: Option<f64>,
    session_stopped: bool,
    /// `_hidT` events as (x, y, phase).
    touches: Vec<(i64, i64, i64)>,
    /// Siri messages in order: `start`, `stop`, or the packet sizes of an audio event.
    siri: Vec<String>,
    /// `_pttEligible` of each Siri start and stop.
    siri_ptt: Vec<Option<i64>>,
    /// Set before connecting: Siri is off in the device settings.
    siri_off: bool,
    /// Does not answer `FetchCurrentNowPlayingInfoEvent`.
    no_now_playing: bool,
    /// Does not answer `_sessionStop`.
    no_stop_reply: bool,
}

/// Value of an integer key in an OPACK dictionary.
fn int_key<'v>(v: &'v Value<'_>, key: i64) -> Option<&'v Value<'v>> {
    v.as_dict()?
        .iter()
        .find(|(k, _)| k.as_i64() == Some(key))
        .map(|(_, v)| v)
}

/// Checks a `_siA` event and returns its packet sizes, like `3+2`.
fn siri_audio(msg: &Message<'_>) -> String {
    let c = msg.content().unwrap();
    let data = int_key(c, 3).and_then(Value::as_bytes).unwrap();
    assert_eq!(int_key(c, 4).and_then(Value::as_f64), Some(1.0));
    let mut offset = 0;
    let mut sizes = Vec::new();
    for p in int_key(c, 5).and_then(Value::as_array).unwrap() {
        let size = int_key(p, 1).and_then(Value::as_i64).unwrap();
        assert_eq!(int_key(p, 2).and_then(Value::as_i64), Some(offset));
        offset += size;
        sizes.push(size.to_string());
    }
    assert_eq!(usize::try_from(offset).unwrap(), data.len());
    sizes.join("+")
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
async fn device_pair_setup(s: DuplexStream) -> PairedController {
    device_pair_setup_with_noise(s, false).await
}

/// Like [`device_pair_setup`]. With `noise`, sends other frames before each reply.
async fn device_pair_setup_with_noise(mut s: DuplexStream, noise: bool) -> PairedController {
    let mut conn = Connection::new();
    let mut server = PairSetupServer::new(DEVICE_ID, &DEVICE_LTSK, PIN, [9; 16], &[0x77; 32]);
    while server.controller().is_none() {
        let Some(frame) = read_frame(&mut s, &mut conn).await else {
            break;
        };
        let tlv = pairing::decode(&frame.payload).unwrap();
        let reply = server.handle(&tlv).unwrap();
        if noise {
            send(&mut s, &mut conn, FrameType::NO_OP, &[]).await;
            send(&mut s, &mut conn, FrameType::U_OPACK, &[0xE0]).await;
        }
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

/// Archived Siri capability as in `_siriInfo`: status 1 is on, 2 is off.
fn siri_capability(status: i64) -> Vec<u8> {
    use ratv_codec::bplist::Value as P;
    let dict = |entries: Vec<(&'static str, P<'static>)>| -> P<'static> {
        P::Dict(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    };
    dict(vec![
        ("$top", dict(vec![("root", P::Uid(1))])),
        (
            "$objects",
            P::Array(vec![
                P::String("$null".into()),
                dict(vec![("SVDAssistantEnabledCapabilityBacking", P::Uid(2))]),
                dict(vec![(
                    "primitivesMap_AssistantEnabledCapability::supportStatus",
                    P::Uid(3),
                )]),
                dict(vec![("supportStatus", P::Int(status))]),
            ]),
        ),
    ])
    .encode()
}

/// `NowPlayingInfo` event with a keyed archive, as the device sends it.
fn now_playing_event(title: &str, state: i64) -> Vec<u8> {
    use ratv_codec::bplist::Value as P;
    let dict = |entries: Vec<(&'static str, P<'static>)>| -> P<'static> {
        P::Dict(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    };
    let archive = dict(vec![
        ("$top", dict(vec![("root", P::Uid(1))])),
        (
            "$objects",
            P::Array(vec![
                P::String("$null".into()),
                dict(vec![
                    ("playbackState", P::Uid(2)),
                    ("playbackRate", P::Uid(3)),
                    ("metadata", P::Uid(4)),
                    ("imageData", P::Uid(5)),
                ]),
                P::Int(state),
                P::Real(if state == 1 { 1.0 } else { 0.0 }),
                dict(vec![
                    ("title", P::Uid(6)),
                    ("timeOffset", P::Real(12.0)),
                    ("timestamp", P::Real(100.0)),
                ]),
                P::Data(b"jpeg".to_vec().into()),
                P::String(title.to_owned().into()),
            ]),
        ),
    ])
    .encode();
    let mut ev = Vec::new();
    Encoder::new(&mut ev).dict(4, |e| {
        e.str("_i").str("NowPlayingInfo");
        e.str("_x").int(5);
        e.str("_t").int(MessageType::Event as i64);
        e.str("_c").dict(1, |e| {
            e.str("NowPlayingInfoKey").data(&archive);
        });
    });
    ev
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
        // Tag 7 is the HAP error item: verification failed.
        let tlv = ratv_codec::tlv8::Tlv8::decode(&reply).unwrap();
        if tlv.get(0x07).is_some() {
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
                let reg = msg.field("_regEvents").and_then(Value::as_array).unwrap();
                assert!(reg.iter().any(|v| v.as_str() == Some("_siriEndpoint")));
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
            "_systemInfo" => {
                let status = if log.lock().unwrap().siri_off { 2 } else { 1 };
                let capability = siri_capability(status);
                Some(response(xid, &id, |e| {
                    e.dict(1, |e| {
                        e.str("_siriInfo").dict(1, |e| {
                            e.str("deviceCapabilitiesV2").array(1, |e| {
                                e.data(&capability);
                            });
                        });
                    });
                }))
            }
            "_tiStop" | "_touchStart" => Some(response(xid, &id, empty)),
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
                let mut log = log.lock().unwrap();
                log.session_stopped = true;
                (!log.no_stop_reply).then(|| response(xid, &id, empty))
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
                5 => {
                    let vol = log.lock().unwrap().reply_volume.unwrap_or(0.25);
                    Some(response(xid, &id, |e| {
                        e.dict(1, |e| {
                            e.str("_vol").float(vol);
                        });
                    }))
                }
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
            "FetchSupportedActionsEvent" => {
                // Like a real device, the event comes before the response.
                let mut ev = Vec::new();
                Encoder::new(&mut ev).dict(4, |e| {
                    e.str("_i").str("SupportedActions");
                    e.str("_x").int(3);
                    e.str("_t").int(MessageType::Event as i64);
                    e.str("_c").dict(1, |e| {
                        e.str("GuideSupportedKey").bool(true);
                    });
                });
                send(&mut s, &mut conn, FrameType::E_OPACK, &ev).await;
                Some(response(xid, &id, empty))
            }
            "FetchCurrentNowPlayingInfoEvent" if log.lock().unwrap().no_now_playing => None,
            "FetchCurrentNowPlayingInfoEvent" => {
                let ev = now_playing_event("Nocturne", 1);
                send(&mut s, &mut conn, FrameType::E_OPACK, &ev).await;
                Some(response(xid, &id, empty))
            }
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
            "_siriStart" | "_siriStop" => {
                assert_eq!(int("_vtDurMs"), Some(0));
                let mut log = log.lock().unwrap();
                log.siri.push(id.trim_start_matches("_siri").to_lowercase());
                log.siri_ptt
                    .push(msg.value().get("_pttEligible").and_then(Value::as_i64));
                Some(response(xid, &id, empty))
            }
            "_siA" => {
                let audio = siri_audio(&msg);
                // A packet of 99 bytes stands for the end of speech.
                let end = audio.starts_with("99");
                log.lock().unwrap().siri.push(audio);
                end.then(|| {
                    let mut ev = Vec::new();
                    Encoder::new(&mut ev).dict(4, |e| {
                        e.str("_i").str("_siriEndpoint");
                        e.str("_x").int(2);
                        e.str("_t").int(MessageType::Request as i64);
                        e.str("_c").dict(0, |_| {});
                    });
                    ev
                })
            }
            "_siriEndpoint" => {
                assert_eq!(msg.kind(), Some(MessageType::Response));
                assert_eq!(xid, 2);
                log.lock().unwrap().siri.push("endpoint reply".into());
                None
            }
            // Test only: the device sends `SiriStatus` with Siri off.
            "turnSiriOff" => {
                let capability = siri_capability(2);
                let mut ev = Vec::new();
                Encoder::new(&mut ev).dict(4, |e| {
                    e.str("_i").str("SiriStatus");
                    e.str("_x").int(4);
                    e.str("_t").int(MessageType::Event as i64);
                    e.str("_c").dict(1, |e| {
                        e.str("deviceCapabilitiesV2").array(1, |e| {
                            e.data(&capability);
                        });
                    });
                });
                send(&mut s, &mut conn, FrameType::E_OPACK, &ev).await;
                Some(response(xid, &id, empty))
            }
            "silent" | "filler" => None,
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
    let mut options = ClientOptions::default();
    options.timeout = Duration::from_secs(2);
    options
}

#[tokio::test]
async fn pairing_creates_credentials() {
    let (creds, controller) = pair().await;
    assert_eq!(creds.accessory_id(), DEVICE_ID);
    assert_eq!(creds.client_id(), controller.id);
    let meta = ratv_codec::opack::decode(controller.metadata.as_deref().unwrap()).unwrap();
    assert_eq!(meta.get("name").and_then(Value::as_str), Some("ratv"));
    // Credentials are the same after conversion to text and back.
    assert_eq!(
        creds
            .to_secret_string()
            .expose_secret()
            .parse::<Credentials>()
            .unwrap(),
        creds
    );
}

#[tokio::test]
async fn pairing_skips_other_frames() {
    let (client, device) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_pair_setup_with_noise(device, true));
    let prompt = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap();
    let creds = prompt.finish(PIN).await.unwrap();
    assert_eq!(creds.client_id(), device.await.unwrap().id);
}

#[tokio::test]
async fn invalid_pairing_reply_shows_frame() {
    let (client, mut device) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut conn = Connection::new();
        read_frame(&mut device, &mut conn).await.unwrap();
        send(&mut device, &mut conn, FrameType::PAIR_SETUP_NEXT, &[]).await;
        read_frame(&mut device, &mut conn).await;
    });
    let err = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "pair-setup M2: invalid pairing reply (unexpected end of input): frame type 4, 0 bytes, starts with "
    );
}

#[tokio::test]
async fn pairing_close_shows_skipped_frames() {
    let (client, mut device) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut conn = Connection::new();
        read_frame(&mut device, &mut conn).await.unwrap();
        send(&mut device, &mut conn, FrameType::NO_OP, &[]).await;
        send(&mut device, &mut conn, FrameType::U_OPACK, &[0xE0]).await;
    });
    let err = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "pair-setup M2: connection closed (skipped frames: type 1, 0 bytes; type 7, 1 bytes)"
    );
}

#[tokio::test(start_paused = true)]
async fn pairing_timeout_names_step() {
    let (client, mut device) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut conn = Connection::new();
        read_frame(&mut device, &mut conn).await.unwrap();
        read_frame(&mut device, &mut conn).await;
    });
    let err = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "pair-setup M2: timed out");
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
    assert!((client.volume().await.unwrap() - 0.25).abs() < f32::EPSILON);
    client.set_volume(1.5).await.unwrap();
    // Limited like `set_volume`, and -0.0 becomes 0.0.
    for (sent, expected) in [(-0.0, 0.0), (2.0, 1.0)] {
        client
            .media(companion::MediaControl::SetVolume(sent))
            .await
            .unwrap();
        settle(&client).await;
        let volume = log.lock().unwrap().volume.unwrap();
        assert_eq!(volume, expected);
        assert!(volume.is_sign_positive());
    }
    client.media(companion::MediaControl::Play).await.unwrap();
    let err = client
        .media(companion::MediaControl::SkipBy(f64::NAN))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument("seconds")), "{err}");
    let err = client
        .media(companion::MediaControl::SetVolume(f64::NAN))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument("volume")), "{err}");
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
    let start = Instant::now();
    client.tap(0.25, 0.75).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));

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
    // Press, three moves ending at the target, release there, on a 1000 x 1000
    // touch area. Then a tap.
    assert_eq!(
        log.touches,
        [
            (800, 500, 1),
            (600, 500, 3),
            (400, 500, 3),
            (200, 500, 3),
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

/// Message that the fake device never answers.
struct Silent;

impl ratv_companion::message::Outgoing for Silent {
    const ID: &str = "silent";
    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// The fake device never answers `silent`, so the request times out.
/// After two such timeouts with nothing received, the connection closes.
#[tokio::test]
async fn request_timeout() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let mut opts = ClientOptions::default();
    opts.timeout = Duration::from_millis(100);
    opts.connect_timeout = Some(Duration::from_secs(2));
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    let err = client.request(&Silent).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    assert!(client.is_connected());
    let _ = client.request(&Silent).await;
    let reason = tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason.as_deref(), Some(Error::NoAnswer("requests"))),
        "{reason:?}"
    );
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
    // `Closed` if the client saw the end before the press.
    assert!(matches!(err, Error::Disconnected | Error::Closed), "{err}");
    let reason = tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(reason.is_some());
    assert!(!client.is_connected());
}

#[tokio::test]
async fn clones_share_session() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let clone = client.clone();
    tokio::spawn(async move { clone.press(HidCommand::Up).await })
        .await
        .unwrap()
        .unwrap();
    client.close().await.unwrap();
    // Closed with `close`, so there is no error.
    assert!(client.closed().await.is_none());
    assert!(!client.is_connected());
    let err = client.press(HidCommand::Up).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    assert!(err.is_not_sent());
    client.close().await.unwrap();
}

/// Stream whose writes wait while it is blocked, or fail.
struct Gate {
    io: DuplexStream,
    blocked: Blocked,
}

impl tokio::io::AsyncRead for Gate {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Gate {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        {
            let mut blocked = self.blocked.lock().unwrap();
            if blocked.2 {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            if blocked.0 {
                blocked.1 = Some(cx.waker().clone());
                return std::task::Poll::Pending;
            }
        }
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Writes wait, waker of the waiting write, writes fail.
type Blocked = Arc<Mutex<(bool, Option<std::task::Waker>, bool)>>;

/// Connects through a [`Gate`] with a timeout of 100 ms. Silent timeouts
/// never close the connection.
async fn connect_gated() -> (companion::Client, Arc<Mutex<Log>>, Blocked) {
    // With the default of 2, two silent timeouts would close the connection.
    connect_gated_with(0).await
}

/// Connects through a [`Gate`] with a timeout of 100 ms.
async fn connect_gated_with(
    max_silent_timeouts: u32,
) -> (companion::Client, Arc<Mutex<Log>>, Blocked) {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let blocked = Blocked::default();
    let gate = Gate {
        io: client_io,
        blocked: blocked.clone(),
    };
    let mut opts = options();
    opts.timeout = Duration::from_millis(100);
    opts.connect_timeout = Some(Duration::from_secs(2));
    opts.max_silent_timeouts = max_silent_timeouts;
    let client = companion::Client::connect_stream(gate, &creds, &opts)
        .await
        .unwrap();
    (client, log, blocked)
}

/// Blocks writes and waits until the background task waits in a write.
/// Messages queued after this stay in the outbox.
async fn block_writes(client: &companion::Client, blocked: &Blocked) {
    blocked.lock().unwrap().0 = true;
    client.send_event(&Silent).await.unwrap();
    while blocked.lock().unwrap().1.is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Makes the waiting write and all later writes fail.
fn fail_writes(blocked: &Blocked) {
    let waker = {
        let mut blocked = blocked.lock().unwrap();
        blocked.2 = true;
        blocked.1.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

fn unblock_writes(blocked: &Blocked) {
    let waker = {
        let mut blocked = blocked.lock().unwrap();
        blocked.0 = false;
        blocked.1.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[tokio::test]
async fn request_not_written_is_not_sent() {
    let (client, log, blocked) = connect_gated().await;
    blocked.lock().unwrap().0 = true;
    // The background task takes it and then waits in the write.
    let err = client
        .button(HidCommand::Select, ButtonState::Down)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NoAnswer("_hidC")), "{err}");
    assert!(!err.is_not_sent());
    // Stays in the outbox, so it is removed.
    let err = client
        .button(HidCommand::Menu, ButtonState::Down)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    assert!(err.is_not_sent());
    assert!(client.is_connected());

    assert!(blocked.lock().unwrap().1.is_some());
    unblock_writes(&blocked);
    client
        .button(HidCommand::Up, ButtonState::Down)
        .await
        .unwrap();
    let hid = log.lock().unwrap().hid.clone();
    assert_eq!(hid, [(6, 1), (1, 1)]);
}

/// A request that is still in the outbox when the connection closes was not
/// sent.
#[tokio::test]
async fn request_queued_at_close_is_not_sent() {
    let (client, _log, blocked) = connect_gated().await;
    block_writes(&client, &blocked).await;
    let request = client.request(&Silent);
    let fail = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        fail_writes(&blocked);
    };
    let (result, ()) = tokio::join!(request, fail);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    assert!(err.is_not_sent());
    assert!(!client.is_connected());
}

/// A button release that was not sent in time is sent by the drop guard.
#[tokio::test]
async fn hold_release_not_written_is_sent_later() {
    let (client, log, blocked) = connect_gated().await;
    let hold = client.hold(HidCommand::Select, Duration::from_millis(200));
    let block = async {
        while log.lock().unwrap().hid.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        block_writes(&client, &blocked).await;
    };
    let (result, ()) = tokio::join!(hold, block);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    assert_eq!(log.lock().unwrap().hid, [(6, 1), (6, 2)]);
}

/// A Siri release that was not sent in time is sent by the drop.
#[tokio::test]
async fn siri_release_not_written_is_sent_later() {
    let (client, log, blocked) = connect_gated().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    for size in 1..=3 {
        siri.send(&vec![0; size]).await.unwrap();
    }
    block_writes(&client, &blocked).await;
    let err = siri.finish().await.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_siriStop")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    assert_eq!(log.lock().unwrap().siri, ["start", "1+2+3", "stop"]);
}

/// Requests that were not sent in time do not count as silent timeouts, so
/// the release of a hold is still sent with the default limit.
#[tokio::test]
async fn removed_requests_do_not_close() {
    let (client, log, blocked) =
        connect_gated_with(ClientOptions::default().max_silent_timeouts).await;
    let hold = client.hold(HidCommand::Select, Duration::from_millis(300));
    let other = async {
        while log.lock().unwrap().hid.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        block_writes(&client, &blocked).await;
        let err = client.request(&Silent).await.unwrap_err();
        assert!(matches!(err, Error::SendTimeout("silent")), "{err}");
    };
    let (result, ()) = tokio::join!(hold, other);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    assert!(client.is_connected());
    assert_eq!(log.lock().unwrap().hid, [(6, 1), (6, 2)]);
}

/// A press or Siri start that was not sent in time sends no release.
#[tokio::test]
async fn press_not_written_sends_no_release() {
    let (client, log, blocked) = connect_gated().await;
    block_writes(&client, &blocked).await;
    let err = client.press(HidCommand::Select).await.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    let err = client
        .hold(HidCommand::Menu, Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    let options = companion::SiriOptions::default();
    let err = client.start_siri(&options).await.err().unwrap();
    assert!(matches!(err, Error::SendTimeout("_siriStart")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    let log = log.lock().unwrap();
    assert!(log.hid.is_empty(), "{:?}", log.hid);
    assert!(log.siri.is_empty(), "{:?}", log.siri);
}

/// Message with `len` bytes of data, which the fake device ignores.
struct Filler(usize);

impl ratv_companion::message::Outgoing for Filler {
    const ID: &str = "filler";
    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(1, |e| {
            e.str("_d").data(&vec![0; self.0]);
        });
    }
}

/// Releases skip the outbox limit. A press that does not fit sends nothing.
#[tokio::test]
async fn release_queued_when_outbox_full() {
    let (client, log, blocked) = connect_gated().await;
    let hold = client.hold(HidCommand::Select, Duration::from_millis(50));
    tokio::pin!(hold);
    // Sends the press. The hold is not polled again here.
    while log.lock().unwrap().hid.is_empty() {
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut hold)
                .await
                .is_err()
        );
    }
    block_writes(&client, &blocked).await;
    let mut queued = 0;
    let err = loop {
        match client.send_event(&Filler(256 * 1024)).await {
            Ok(()) => queued += 1,
            Err(err) => break err,
        }
        assert!(queued < 8, "outbox not full");
    };
    assert!(matches!(err, Error::QueueFull), "{err}");
    let err = client.press(HidCommand::Menu).await.unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    // No lift without a touch.
    let err = client.tap(0.5, 0.5).await.unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    let err = client
        .swipe((0.2, 0.5), (0.8, 0.5), Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    // The release request is removed, then the guard queues it again.
    let err = hold.await.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    unblock_writes(&blocked);
    // Not `settle`: its request does not fit while the outbox is full.
    let start = Instant::now();
    while log.lock().unwrap().hid.len() < 2 {
        assert!(start.elapsed() < Duration::from_secs(5), "no release");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The press that did not fit would come before the release.
    assert_eq!(log.lock().unwrap().hid, [(6, 1), (6, 2)]);
    assert_eq!(log.lock().unwrap().touches, []);
}

/// `close` ends also when the stop request was not sent in time.
#[tokio::test]
async fn close_with_stop_not_written() {
    let (client, log, blocked) = connect_gated().await;
    block_writes(&client, &blocked).await;
    let close = tokio::spawn({
        let client = client.clone();
        async move { client.close().await }
    });
    // Unblocks after the stop request was removed, so the task then sees
    // `close` and an empty outbox.
    tokio::time::sleep(Duration::from_millis(500)).await;
    unblock_writes(&blocked);
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!client.is_connected());
    assert!(!log.lock().unwrap().session_stopped);
}

/// A close after a silent timeout, while the write waits, is a clean close.
#[tokio::test]
async fn close_while_stalled() {
    let (client, _log, blocked) = connect_gated_with(1).await;
    blocked.lock().unwrap().0 = true;
    // Taken by the task, so it counts as a silent timeout.
    let err = client.request(&Silent).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    let close = tokio::spawn({
        let client = client.clone();
        async move { client.close().await }
    });
    // The stop request (100 ms limit) must end before the write does.
    tokio::time::sleep(Duration::from_millis(500)).await;
    unblock_writes(&blocked);
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reason = client.closed().await;
    assert!(reason.is_none(), "{reason:?}");
}

/// A stop request without an answer is a clean close.
#[tokio::test]
async fn close_without_stop_reply() {
    let (client, log, _blocked) = connect_gated_with(1).await;
    log.lock().unwrap().no_stop_reply = true;
    tokio::time::timeout(Duration::from_secs(2), client.close())
        .await
        .unwrap()
        .unwrap();
    assert!(log.lock().unwrap().session_stopped);
    let reason = client.closed().await;
    assert!(reason.is_none(), "{reason:?}");
}

/// Event receivers close with the connection, also ones made after it.
#[tokio::test]
async fn events_close_with_connection() {
    let (client, _log) = connect().await;
    let mut before = client.events();
    client.close().await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(1), before.recv())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        err,
        tokio::sync::broadcast::error::RecvError::Closed
    ));
    let mut after = client.events();
    let after = tokio::time::timeout(Duration::from_secs(1), after.recv()).await;
    assert!(after.unwrap().is_err());
}

/// A close dropped while the stop request waits still closes.
#[tokio::test]
async fn close_dropped_still_closes() {
    let (client, log, _blocked) = connect_gated_with(1).await;
    log.lock().unwrap().no_stop_reply = true;
    let dropped = tokio::time::timeout(Duration::from_millis(20), client.close()).await;
    assert!(dropped.is_err());
    tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(!client.is_connected());
}

/// With `max_silent_timeouts` 1, the first timeout without data closes.
#[tokio::test]
async fn one_silent_timeout_closes() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let mut opts = options();
    opts.timeout = Duration::from_millis(100);
    opts.connect_timeout = Some(Duration::from_secs(2));
    opts.max_silent_timeouts = 1;
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    let err = client.request(&Silent).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    let reason = tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason.as_deref(), Some(Error::NoAnswer("requests"))),
        "{reason:?}"
    );
}

#[tokio::test]
async fn zero_silent_timeouts_keep_connection() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let mut opts = options();
    opts.timeout = Duration::from_millis(50);
    opts.connect_timeout = Some(Duration::from_secs(2));
    opts.max_silent_timeouts = 0;
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    for _ in 0..3 {
        let err = client.request(&Silent).await.unwrap_err();
        assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(client.is_connected());
    client.press(HidCommand::Up).await.unwrap();
}

#[tokio::test]
async fn verify_with_unknown_credentials_fails() {
    let (creds, controller) = pair().await;
    // Different client key: the device rejects the M3 signature.
    let creds = Credentials::new(
        *creds.accessory_ltpk(),
        [1u8; 32].into(),
        creds.accessory_id().to_vec(),
        creds.client_id().to_vec(),
    );
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let err = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Hap(ratv_hap::Error::Device(_))),
        "{err}"
    );
    assert!(err.is_auth(), "{err}");
    assert!(!err.is_other_device(), "{err}");
}

#[tokio::test]
async fn verify_with_other_device_fails() {
    let (creds, controller) = pair().await;
    let creds = Credentials::new(
        *creds.accessory_ltpk(),
        [1u8; 32].into(),
        b"58:55:CA:00:00:02".to_vec(),
        creds.client_id().to_vec(),
    );
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let err = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap_err();
    assert!(err.is_other_device(), "{err}");
    assert!(!err.is_auth(), "{err}");
}

#[tokio::test]
async fn connect_timeout_limits_pair_verify() {
    let (creds, _) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok(Ok((s, _))) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            open.push(s);
        }
        open.len()
    });
    let mut options = options();
    options.timeout = Duration::from_secs(30);
    options.connect_timeout = Some(Duration::from_millis(100));
    let start = Instant::now();
    let err = companion::Client::connect(addr, &creds, &options)
        .await
        .unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(err.to_string(), "pair-verify M2: timed out");
    assert_eq!(accepted.await.unwrap(), 1);
}

#[tokio::test]
async fn stalled_connect_starts_again() {
    let (creds, controller) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // The first connection gets no answer.
        let (_silent, _) = listener.accept().await.unwrap();
        let (mut tcp, _) = listener.accept().await.unwrap();
        let (mut client_io, device_io) = tokio::io::duplex(8192);
        tokio::spawn(device_session(device_io, controller, Arc::default()));
        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
    });
    let mut options = options();
    options.stall_timeout = Some(Duration::from_millis(200));
    let start = Instant::now();
    companion::Client::connect(addr, &creds, &options)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test]
async fn stalled_connect_starts_again_only_once() {
    let (creds, _) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok(Ok((s, _))) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            open.push(s);
        }
        open.len()
    });
    let mut options = options();
    options.timeout = Duration::from_millis(300);
    options.stall_timeout = Some(Duration::from_millis(100));
    let err = companion::Client::connect(addr, &creds, &options)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "pair-verify M2: timed out");
    assert_eq!(accepted.await.unwrap(), 2);
}

#[tokio::test]
async fn hold_released_when_canceled() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let hold = client.hold(HidCommand::Select, Duration::from_secs(10));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), hold)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let hid = log.lock().unwrap().hid.clone();
    // Down, then up from the dropped future.
    assert_eq!(hid.len(), 2, "{hid:?}");
    assert_eq!((hid[0].0, hid[0].1, hid[1].1), (hid[1].0, 1, 2));
}

#[tokio::test]
async fn double_tap_sends_two_clicks() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    client.double_tap(HidCommand::Home).await.unwrap();
    let hid = log.lock().unwrap().hid.clone();
    assert_eq!(hid, [(7, 1), (7, 2), (7, 1), (7, 2)]);
}

#[tokio::test]
async fn too_large_message_fails_and_connection_stays() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let url = "a".repeat(ratv_companion::MAX_MESSAGE_LEN);
    match client.open_url(&url).await.unwrap_err() {
        Error::Companion(ratv_companion::Error::FrameTooLarge(len)) => {
            assert!(len > ratv_companion::MAX_MESSAGE_LEN);
        }
        other => panic!("unexpected {other}"),
    }
    client.double_tap(HidCommand::Home).await.unwrap();
    assert_eq!(log.lock().unwrap().hid.len(), 4);
}

#[tokio::test]
async fn swipe_lifted_when_canceled() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let swipe = client.swipe((0.5, 0.5), (0.5, 0.1), Duration::from_secs(10));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), swipe)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let touches = log.lock().unwrap().touches.clone();
    let phases: Vec<_> = touches.iter().map(|t| t.2).collect();
    // Press (1), moves (3), then release (4) from the dropped future.
    assert_eq!(phases.first(), Some(&1), "{touches:?}");
    assert_eq!(phases.last(), Some(&4), "{touches:?}");
    // Lifted at the last point sent, not at the end of the swipe.
    let [.., last, up] = touches[..] else {
        panic!("{touches:?}");
    };
    assert_eq!((up.0, up.1), (last.0, last.1), "{touches:?}");
}

/// A swipe whose move does not fit in the outbox lifts the finger at the
/// last point sent.
#[tokio::test]
async fn swipe_lifted_when_move_not_queued() {
    let (client, log, blocked) = connect_gated().await;
    let swipe = client.swipe((0.5, 0.5), (0.5, 0.1), Duration::from_secs(10));
    tokio::pin!(swipe);
    while log.lock().unwrap().touches.is_empty() {
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut swipe)
                .await
                .is_err()
        );
    }
    block_writes(&client, &blocked).await;
    let mut queued = 0;
    while client.send_event(&Filler(256 * 1024)).await.is_ok() {
        queued += 1;
        assert!(queued < 8, "outbox not full");
    }
    let err = tokio::time::timeout(Duration::from_secs(2), swipe)
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    unblock_writes(&blocked);
    let start = Instant::now();
    while log.lock().unwrap().touches.last().map(|t| t.2) != Some(4) {
        assert!(start.elapsed() < Duration::from_secs(5), "no release");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let touches = log.lock().unwrap().touches.clone();
    let [.., last, up] = touches[..] else {
        panic!("{touches:?}");
    };
    assert_eq!((up.0, up.1), (last.0, last.1), "{touches:?}");
}

async fn connect() -> (companion::Client, Arc<Mutex<Log>>) {
    connect_with(Log::default()).await
}

async fn connect_with(log: Log) -> (companion::Client, Arc<Mutex<Log>>) {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(log));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    (client, log)
}

/// Waits until the device has read all messages that were sent before.
async fn settle(client: &companion::Client) {
    client.volume().await.unwrap();
}

#[tokio::test]
async fn siri() {
    let (client, log) = connect().await;
    let options = companion::SiriOptions::default();
    let mut siri = client.start_siri(&options).await.unwrap();
    assert!(matches!(
        client.start_siri(&options).await,
        Err(Error::SiriActive)
    ));
    let start = Instant::now();
    for size in 1..=12 {
        siri.send(&vec![0; size]).await.unwrap();
    }
    siri.finish().await.unwrap();
    // Events of 100 ms, no faster than real time: the third is sent after 200 ms.
    assert!(start.elapsed() >= Duration::from_millis(200));
    // Dropping releases Siri too.
    drop(client.start_siri(&options).await.unwrap());
    settle(&client).await;

    let log = log.lock().unwrap();
    assert_eq!(
        log.siri,
        [
            "start",
            "1+2+3+4+5",
            "6+7+8+9+10",
            "11+12",
            "stop",
            "start",
            "stop"
        ]
    );
    assert_eq!(log.siri_ptt, [None; 4]);
}

#[tokio::test]
async fn state_read_when_connecting() {
    let (client, _) = connect().await;
    assert_eq!(client.siri_enabled(), Some(true));
    assert_eq!(client.guide_supported(), Some(true));
}

#[tokio::test]
async fn now_playing_read_when_connecting() {
    let (client, _) = connect().await;
    let np = client.now_playing().borrow().clone();
    assert_eq!(np.title.as_deref(), Some("Nocturne"));
    assert_eq!(np.state, Some(companion::PlaybackState::Playing));
    assert_eq!(np.artwork.as_deref(), Some(&b"jpeg"[..]));
}

/// An optional request that gets no answer does not start the connect again.
#[tokio::test]
async fn connect_without_optional_answer() {
    let (creds, controller) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Log {
        no_now_playing: true,
        ..Log::default()
    }));
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::Relaxed);
            let (mut client_io, device_io) = tokio::io::duplex(8192);
            tokio::spawn(device_session(device_io, controller.clone(), log.clone()));
            tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
            });
        }
    });
    let mut opts = options();
    opts.stall_timeout = Some(Duration::from_millis(300));
    let client = companion::Client::connect(addr, &creds, &opts)
        .await
        .unwrap();
    assert!(client.is_connected());
    assert_eq!(client.now_playing().borrow().title, None);
    assert_eq!(accepted.load(Ordering::Relaxed), 1);
}

/// A `Siri` kept over a close gives `Closed`, not `SiriActive`.
#[tokio::test]
async fn siri_kept_over_close() {
    let (client, _log) = connect().await;
    let kept = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    client.close().await.unwrap();
    let err = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Closed), "{err}");
    let mut kept = kept;
    let err = kept.send(&[1, 2, 3]).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
}

#[tokio::test]
async fn siri_off_fails_before_start() {
    let (client, log) = connect_with(Log {
        siri_off: true,
        ..Log::default()
    })
    .await;
    assert_eq!(client.siri_enabled(), Some(false));
    assert!(matches!(
        client.start_siri(&companion::SiriOptions::default()).await,
        Err(Error::SiriDisabled)
    ));
    settle(&client).await;
    assert_eq!(log.lock().unwrap().siri, Vec::<String>::new());
    // Not `SiriDisabled` from the last value, so `is_closed` is true.
    client.close().await.unwrap();
    let err = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Closed), "{err}");
}

#[tokio::test]
async fn siri_status_event_updates_state() {
    struct TurnSiriOff;
    impl ratv_companion::message::Outgoing for TurnSiriOff {
        const ID: &str = "turnSiriOff";
        fn content(&self, e: &mut Encoder<'_>) {
            empty(e);
        }
    }
    let (client, _) = connect().await;
    let mut events = client.events();
    client.request(&TurnSiriOff).await.unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        Event::SiriStatus { enabled: false }
    );
    assert_eq!(client.siri_enabled(), Some(false));
    assert!(matches!(
        client.start_siri(&companion::SiriOptions::default()).await,
        Err(Error::SiriDisabled)
    ));
}

#[tokio::test]
async fn siri_endpoint() {
    let (client, log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    for size in [99, 1, 1, 1, 1] {
        siri.send(&vec![0; size]).await.unwrap();
    }
    assert!(!siri.ended());
    // The device answers the first event with `_siriEndpoint`. At most 5 s.
    for _ in 0..250 {
        if siri.ended() {
            break;
        }
        siri.send(&[2]).await.unwrap();
    }
    assert!(siri.ended());
    siri.send(&[3]).await.unwrap();
    siri.finish().await.unwrap();
    settle(&client).await;
    assert_eq!(
        log.lock().unwrap().siri,
        ["start", "99+1+1+1+1", "endpoint reply", "stop"]
    );
}

/// Device volumes outside 0.0 to 1.0 are limited; a value too large for f32
/// is an error.
#[tokio::test]
async fn volume_limited() {
    let (client, log) = connect().await;
    for (device, expected) in [(1.5, 1.0), (-0.2, 0.0), (-0.0, 0.0)] {
        log.lock().unwrap().reply_volume = Some(device);
        let volume = client.volume().await.unwrap();
        assert_eq!(volume, expected);
        // Never -0.0.
        assert!(volume.is_sign_positive());
    }
    // Finite, but too large for f32.
    log.lock().unwrap().reply_volume = Some(1e300);
    let err = client.volume().await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Companion(ratv_companion::Error::InvalidField("_vol"))
        ),
        "{err}"
    );
}

/// After the device stopped recording, sending is not an error, also on a
/// closed connection.
#[tokio::test]
async fn siri_ended_then_closed() {
    let (client, _log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    for size in [99, 1, 1, 1, 1] {
        siri.send(&vec![0; size]).await.unwrap();
    }
    // The device answers the first event with `_siriEndpoint`. At most 5 s.
    for _ in 0..250 {
        if siri.ended() {
            break;
        }
        siri.send(&[2]).await.unwrap();
    }
    assert!(siri.ended());
    // The packet size is checked first.
    let err = siri.send(&vec![0; 65_536]).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    client.close().await.unwrap();
    siri.send(&[3]).await.unwrap();
    let err = siri.send(&vec![0; 65_536]).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
}

#[tokio::test]
async fn siri_without_audio() {
    let (client, log) = connect().await;
    let siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    assert!(matches!(siri.finish().await, Err(Error::SiriNoAudio)));
    settle(&client).await;
    assert_eq!(log.lock().unwrap().siri, ["start", "stop"]);
}

#[tokio::test]
async fn siri_ptt_flags() {
    let (client, log) = connect().await;
    let mut options = companion::SiriOptions::default();
    options.ptt_flags = true;
    let mut siri = client.start_siri(&options).await.unwrap();
    siri.send(&[1, 2]).await.unwrap();
    siri.finish().await.unwrap();
    assert_eq!(log.lock().unwrap().siri_ptt, [Some(1); 2]);
}

#[tokio::test]
async fn siri_packet_too_large() {
    let (client, _log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    let err = siri.send(&vec![0; 70_000]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument("packet size")),
        "{err}"
    );
}

#[cfg(feature = "opus")]
#[tokio::test]
async fn siri_pcm() {
    let (client, log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    // 100 + 600 samples: two packets now, the rest padded by `finish`.
    siri.push_pcm(&[100; 100]).await.unwrap();
    siri.push_pcm(&[-100; 600]).await.unwrap();
    siri.finish().await.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(log.siri.len(), 3);
    assert_eq!(log.siri[1].split('+').count(), 3);
}

#[cfg(feature = "mrp")]
#[tokio::test]
async fn apple_tv_siri_uses_companion() {
    use ratv::{AppleTv, Endpoints, Siri};

    let (creds, controller) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Log::default()));
    let device_log = log.clone();
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let (mut client_io, device_io) = tokio::io::duplex(8192);
        tokio::spawn(device_session(device_io, controller, device_log));
        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
    });
    let endpoints = Endpoints {
        companion: Some(addr),
        airplay: None,
    };
    let tv = AppleTv::connect(&endpoints, &creds, &options())
        .await
        .unwrap();
    // Without MRP, now playing comes from Companion.
    let np = tv.now_playing().borrow().clone();
    assert_eq!(np.title.as_deref(), Some("Nocturne"));
    assert_eq!(np.state, Some(ratv::mrp::PlaybackState::Playing));
    assert_eq!(np.elapsed, Some(12.0));
    assert_eq!(np.artwork_data.as_deref(), Some(&b"jpeg"[..]));
    let mut siri = tv.start_siri().await.unwrap();
    assert!(matches!(siri, Siri::Companion(_)));
    assert!(!siri.ended());
    siri.send(vec![1, 2, 3]).await.unwrap();
    siri.finish().await.unwrap();
    assert_eq!(log.lock().unwrap().siri, ["start", "3", "stop"]);
    assert_eq!(tv.siri_enabled(), Some(true));
    assert_eq!(tv.guide_supported(), Some(true));
    // The argument is checked before the protocol.
    for err in [
        tv.seek(f64::NAN).await.unwrap_err(),
        tv.set_playback_rate(f32::NAN).await.unwrap_err(),
        tv.set_rating(f32::INFINITY).await.unwrap_err(),
    ] {
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    }
    // Not the last value of a closed connection.
    tv.close().await.unwrap();
    assert_eq!(tv.siri_enabled(), None);
    assert_eq!(tv.guide_supported(), None);
}

/// Accepts TCP connections and runs a device session on each.
#[cfg(feature = "mrp")]
async fn listen(controller: PairedController, log: Arc<Mutex<Log>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let (mut client_io, device_io) = tokio::io::duplex(8192);
            tokio::spawn(device_session(device_io, controller.clone(), log.clone()));
            tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
            });
        }
    });
    addr
}

/// A closed Companion connects again, and the new handle has the merged
/// now playing again.
#[cfg(feature = "mrp")]
#[tokio::test]
async fn apple_tv_connects_missing_companion() {
    use ratv::{AppleTv, Endpoints, Protocol};

    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let endpoints = Endpoints {
        companion: Some(listen(controller, log).await),
        airplay: None,
    };
    let tv = AppleTv::connect(&endpoints, &creds, &options())
        .await
        .unwrap();
    assert!(tv.is_open(Protocol::Companion));
    assert!(!tv.is_open(Protocol::Mrp));
    assert!(tv.connect_error(Protocol::Mrp).is_none());

    tv.companion().unwrap().close().await.unwrap();
    assert!(!tv.is_open(Protocol::Companion));
    let new = tv
        .connect_missing(&endpoints, &creds, &options())
        .await
        .unwrap()
        .unwrap();
    assert!(new.is_open(Protocol::Companion));
    assert!(!tv.is_open(Protocol::Companion));
    let np = new.now_playing().borrow().clone();
    assert_eq!(np.title.as_deref(), Some("Nocturne"));
    new.turn_on().await.unwrap();
    new.close().await.unwrap();
}
