// Fake Companion device. Included with `include!` by the tests that use it.

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

// Same as the fake MRP device, so one set of credentials works for both.
const DEVICE_ID: &[u8] = b"58:55:CA:00:00:02";
const DEVICE_LTSK: [u8; 32] = [0x42; 32];
const PIN: &str = "1234";

#[derive(Default)]
// Test flags and results, not a state machine.
#[allow(clippy::struct_excessive_bools)]
struct Log {
    hid: Vec<(i64, i64)>,
    /// `_mcc` codes of media commands, without volume reads and changes.
    media: Vec<i64>,
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
    /// Sends a now playing event that cannot be read before the answer to
    /// `FetchCurrentNowPlayingInfoEvent`, and an empty one before the answer
    /// to `FetchUserAccountsEvent`.
    late_now_playing: bool,
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

/// `NowPlayingInfo` event with `archive` as its data.
fn now_playing_event_with(archive: &[u8]) -> Vec<u8> {
    let mut ev = Vec::new();
    Encoder::new(&mut ev).dict(4, |e| {
        e.str("_i").str("NowPlayingInfo");
        e.str("_x").int(5);
        e.str("_t").int(MessageType::Event as i64);
        e.str("_c").dict(1, |e| {
            e.str("NowPlayingInfoKey").data(archive);
        });
    });
    ev
}

/// `NowPlayingInfo` event whose archive root has no fields.
fn empty_now_playing_event() -> Vec<u8> {
    use ratv_codec::bplist::Value as P;
    let archive = P::Dict(
        [
            ("$top".into(), P::Dict([("root".into(), P::Uid(1))].into())),
            (
                "$objects".into(),
                P::Array(vec![P::String("$null".into()), P::Dict(Vec::new())]),
            ),
        ]
        .into(),
    )
    .encode();
    now_playing_event_with(&archive)
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
                code => {
                    log.lock().unwrap().media.push(code);
                    Some(response(xid, &id, empty))
                }
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
            "FetchCurrentNowPlayingInfoEvent" if log.lock().unwrap().late_now_playing => {
                let ev = now_playing_event_with(b"not a plist");
                send(&mut s, &mut conn, FrameType::E_OPACK, &ev).await;
                Some(response(xid, &id, empty))
            }
            "FetchCurrentNowPlayingInfoEvent" => {
                let ev = now_playing_event("Nocturne", 1);
                send(&mut s, &mut conn, FrameType::E_OPACK, &ev).await;
                Some(response(xid, &id, empty))
            }
            "FetchUserAccountsEvent" => {
                if log.lock().unwrap().late_now_playing {
                    send(
                        &mut s,
                        &mut conn,
                        FrameType::E_OPACK,
                        &empty_now_playing_event(),
                    )
                    .await;
                }
                Some(response(xid, &id, |e| {
                    e.dict(2, |e| {
                        e.str("acc-1").str("Anna");
                        e.str("acc-2").str("Ben");
                    });
                }))
            }
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
