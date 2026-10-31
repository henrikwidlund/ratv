//! End-to-end tests against a fake AirPlay device on localhost.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use ed25519_dalek::SigningKey;
use prost::Message as _;
use ratv::mrp::{self, Button, Client, ClientOptions, Command, PairOptions, PlaybackState};
use ratv::{Credentials, Error};
use ratv_airplay::Channel;
use ratv_airplay::datastream::DataStream;
use ratv_airplay::http::Message;
use ratv_airplay::keys::{self, Side};
use ratv_codec::bplist::{self, Value};
use ratv_hap::SharedSecret;
use ratv_hap::accessory::{PairSetupServer, PairVerifyServer, PairedController};
use ratv_mrp::{Envelope, Payload, frames, proto};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const DEVICE_ID: &[u8] = b"58:55:CA:00:00:02";
const DEVICE_LTSK: [u8; 32] = [0x42; 32];
const CLIENT_SEED: [u8; 32] = [0x11; 32];
const CLIENT_ID: &[u8] = b"client-1";

#[derive(Default, Debug)]
struct Log {
    buttons: Vec<(u32, u32, bool)>,
    commands: Vec<i32>,
    /// Positions of seek commands.
    seeks: Vec<f64>,
    /// `language_option` of language commands.
    language_options: Vec<bytes::Bytes>,
    /// Voice registrations.
    voice_devices: usize,
    /// Sample time and size of each voice packet.
    voice: Vec<(f64, usize)>,
    volume: Option<f32>,
    event_acks: usize,
    feedback: usize,
    teardown: bool,
    /// Siri does not start recording.
    no_recording: bool,
}

fn credentials() -> Credentials {
    Credentials::new(
        SigningKey::from_bytes(&DEVICE_LTSK)
            .verifying_key()
            .to_bytes(),
        CLIENT_SEED.into(),
        DEVICE_ID.to_vec(),
        CLIENT_ID.to_vec(),
    )
}

async fn read_msg(s: &mut TcpStream, ch: &mut Channel) -> Option<Message> {
    loop {
        if let Some(m) = ch.poll_message().unwrap() {
            return Some(m);
        }
        if s.read_buf(ch.receive_buffer(4096)).await.ok()? == 0 {
            return None;
        }
    }
}

async fn reply(s: &mut TcpStream, ch: &mut Channel, req: &Message, body: &[u8]) {
    let cseq = req.cseq().map(|c| c.to_string()).unwrap_or_default();
    ch.send_response(req.protocol(), 200, "OK", &[("CSeq", &cseq)], body);
    s.write_all(&ch.take_transmit()).await.unwrap();
}

/// Encodes a framed `ProtocolMessage` with a payload and header fields.
fn mrp<P: Payload>(payload: &P, id: Option<&str>, error_code: i32) -> Vec<u8> {
    let mut pm = proto::ProtocolMessage {
        r#type: Some(P::TYPE),
        identifier: id.map(str::to_owned),
        error_code: (error_code != 0).then_some(error_code),
        ..Default::default()
    };
    let mut buf = BytesMut::new();
    ratv_mrp::encode(payload, None, &mut buf);
    // Adds the payload field to `pm`.
    pm.merge_length_delimited(&buf[..]).unwrap();
    pm.encode_length_delimited_to_vec()
}

/// Step where the fake device fails.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// The data stream port is closed.
    DataPort,
    /// `DEVICE_INFO` gets no answer.
    DeviceInfo,
    /// The data stream closes after the connect handshake.
    DropData,
    /// Feedback requests get no answer.
    NoFeedback,
}

struct Device {
    port: u16,
    log: Arc<Mutex<Log>>,
}

async fn start_device() -> Device {
    start_faulty_device(Fault::None).await
}

async fn start_faulty_device(fault: Fault) -> Device {
    let control = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = control.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Log::default()));
    tokio::spawn(run_device(control, log.clone(), fault));
    Device { port, log }
}

async fn run_device(listener: TcpListener, log: Arc<Mutex<Log>>, fault: Fault) {
    let (mut s, _) = listener.accept().await.unwrap();
    let mut ch = Channel::new();
    let controller_ltpk = SigningKey::from_bytes(&CLIENT_SEED)
        .verifying_key()
        .to_bytes();
    let mut verify = PairVerifyServer::new(
        DEVICE_ID,
        &DEVICE_LTSK,
        CLIENT_ID,
        controller_ltpk,
        [0x55; 32],
    );
    let mut secret: Option<SharedSecret> = None;

    while let Some(req) = read_msg(&mut s, &mut ch).await {
        match (req.method().unwrap(), req.uri().unwrap()) {
            ("POST", "/pair-verify") => {
                let out = verify.handle(&req.body()).unwrap();
                reply(&mut s, &mut ch, &req, &out).await;
                if let Some(sec) = verify.shared_secret() {
                    secret = Some(sec.clone());
                    ch.enable_encryption(keys::control(sec, Side::Device));
                }
            }
            ("SETUP", _) => {
                let body = bplist::decode(&req.body()).unwrap().into_owned();
                let sec = secret.clone().unwrap();
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = i64::from(listener.local_addr().unwrap().port());
                if body.get("isRemoteControlOnly").is_some() {
                    let resp = Value::Dict(vec![("eventPort".into(), Value::Int(port))]).encode();
                    tokio::spawn(event_channel(listener, sec, log.clone()));
                    reply(&mut s, &mut ch, &req, &resp).await;
                } else {
                    let stream = &body.get("streams").and_then(Value::as_array).unwrap()[0];
                    let seed = stream
                        .get("seed")
                        .and_then(Value::as_i64)
                        .unwrap()
                        .cast_unsigned();
                    let resp = Value::Dict(vec![(
                        "streams".into(),
                        Value::Array(vec![Value::Dict(vec![
                            ("dataPort".into(), Value::Int(port)),
                            ("streamID".into(), Value::Int(1)),
                        ])]),
                    )])
                    .encode();
                    match fault {
                        Fault::DataPort => drop(listener),
                        Fault::DeviceInfo => {
                            tokio::spawn(async move {
                                let (mut s, _) = listener.accept().await.unwrap();
                                while s.read(&mut [0; 256]).await.is_ok_and(|n| n > 0) {}
                            });
                        }
                        Fault::None | Fault::DropData | Fault::NoFeedback => {
                            let drop = matches!(fault, Fault::DropData);
                            tokio::spawn(data_stream(listener, sec, seed, log.clone(), drop));
                        }
                    }
                    reply(&mut s, &mut ch, &req, &resp).await;
                }
            }
            ("RECORD", _) => reply(&mut s, &mut ch, &req, b"").await,
            ("POST", "/feedback") => {
                log.lock().unwrap().feedback += 1;
                if !matches!(fault, Fault::NoFeedback) {
                    reply(&mut s, &mut ch, &req, b"").await;
                }
            }
            ("TEARDOWN", _) => {
                log.lock().unwrap().teardown = true;
                reply(&mut s, &mut ch, &req, b"").await;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Sends one request right away, before RECORD is answered.
async fn event_channel(listener: TcpListener, secret: SharedSecret, log: Arc<Mutex<Log>>) {
    let (mut s, _) = listener.accept().await.unwrap();
    let mut ch = Channel::new();
    ch.enable_encryption(keys::events(&secret, Side::Device));
    ch.send_request("POST", "/command", "RTSP/1.0", &[("CSeq", "7")], b"x");
    s.write_all(&ch.take_transmit()).await.unwrap();
    while let Some(m) = read_msg(&mut s, &mut ch).await {
        assert_eq!(m.status(), Some(200));
        assert_eq!(m.cseq(), Some(7));
        log.lock().unwrap().event_acks += 1;
    }
}

/// With `drop`, closes after the answer to `CLIENT_UPDATES_CONFIG`.
async fn data_stream(
    listener: TcpListener,
    secret: SharedSecret,
    seed: u64,
    log: Arc<Mutex<Log>>,
    drop: bool,
) {
    let (mut s, _) = listener.accept().await.unwrap();
    let mut ds = DataStream::new(keys::data_stream(&secret, seed, Side::Device), 500);
    loop {
        // Windows reports a closed connection as a reset.
        let Ok(n) = s.read_buf(ds.receive_buffer(4096)).await else {
            return;
        };
        if n == 0 {
            return;
        }
        let mut close = false;
        while let Some(data) = ds.poll().unwrap() {
            for env in frames(data) {
                let env = env.unwrap();
                close |= drop && env.is::<proto::ClientUpdatesConfigMessage>();
                if let Some(out) = handle(&env, &log) {
                    ds.send(&out);
                }
            }
        }
        if ds.has_transmit() {
            s.write_all(&ds.take_transmit()).await.unwrap();
        }
        if close {
            return;
        }
    }
}

fn handle(env: &Envelope, log: &Arc<Mutex<Log>>) -> Option<Vec<u8>> {
    let id = env.identifier();
    if env.is::<proto::DeviceInfoMessage>() {
        let info = proto::DeviceInfoMessage {
            name: "Fake TV".into(),
            device_uid: Some("UID-1".into()),
            ..Default::default()
        };
        // An output device update in the same frame, like a real device sends.
        let devices = proto::UpdateOutputDeviceMessage {
            output_devices: vec![proto::AvOutputDeviceDescriptor {
                unique_identifier: Some("UID-1".into()),
                is_volume_control_available: Some(true),
                volume_capabilities: Some(3),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut out = mrp(&info, id, 0);
        out.extend(mrp(&devices, None, 0));
        return Some(out);
    }
    if env.is::<proto::ClientUpdatesConfigMessage>() {
        return Some(initial_state());
    }
    if env.is::<proto::SendButtonEventMessage>() {
        let b: proto::SendButtonEventMessage = env.payload().unwrap();
        let (page, usage, down) = (b.usage_page?, b.usage?, b.button_down?);
        let mut log = log.lock().unwrap();
        log.buttons.push((page, usage, down));
        // Siri: the device starts and stops recording.
        return ((page, usage) == (0x0c, 0x04) && !log.no_recording).then(|| recording(down));
    }
    if env.is::<proto::SendCommandMessage>() {
        let c: proto::SendCommandMessage = env.payload().unwrap();
        let cmd = c.command?;
        let mut l = log.lock().unwrap();
        l.commands.push(cmd);
        if let Some(o) = c.options {
            l.seeks.extend(o.playback_position);
            l.language_options.extend(o.language_option);
        }
        drop(l);
        if cmd == Command::NextTrack as i32 {
            // Device-level error code on the envelope.
            return Some(mrp(&proto::SendCommandResultMessage::default(), id, 6));
        }
        let send_error = i32::from(cmd == Command::Stop as i32);
        let result = proto::SendCommandResultMessage {
            send_error: Some(send_error),
            ..Default::default()
        };
        return Some(mrp(&result, id, 0));
    }
    if env.is::<proto::PlaybackQueueRequestMessage>() {
        let r: proto::PlaybackQueueRequestMessage = env.payload().unwrap();
        // Only artwork requests get an image, like on a real device.
        #[allow(clippy::cast_possible_truncation)]
        let item = if r.return_content_item_assets_in_user_completion == Some(true) {
            proto::ContentItem {
                identifier: Some("video-1".into()),
                artwork_data: Some(bytes::Bytes::from_static(b"\x89PNG")),
                artwork_data_width: r.artwork_width.map(|w| w as i32),
                metadata: Some(proto::ContentItemMetadata {
                    artwork_mime_type: Some("image/png".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        } else {
            video_item()
        };
        let state = proto::SetStateMessage {
            playback_queue: Some(proto::PlaybackQueue {
                location: Some(0),
                content_items: vec![item],
                ..Default::default()
            }),
            ..Default::default()
        };
        return Some(mrp(&state, id, 0));
    }
    handle_voice(env, id, log).or_else(|| handle_volume(env, id, log))
}

/// Now playing client and state, sent after `CLIENT_UPDATES_CONFIG`.
fn initial_state() -> Vec<u8> {
    let client = proto::NowPlayingClient {
        bundle_identifier: Some("com.apple.TVMusic".into()),
        display_name: Some("Music".into()),
        ..Default::default()
    };
    let mut out = mrp(
        &proto::SetNowPlayingClientMessage {
            client: Some(client.clone()),
        },
        None,
        0,
    );
    let state = proto::SetStateMessage {
        player_path: Some(proto::PlayerPath {
            client: Some(client),
            ..Default::default()
        }),
        playback_state: Some(PlaybackState::Playing as i32),
        now_playing_info: Some(proto::NowPlayingInfo {
            title: Some("Song".into()),
            playback_rate: Some(1.0),
            elapsed_time: Some(100.0),
            timestamp: Some(mrp::cocoa_time(std::time::SystemTime::now())),
            ..Default::default()
        }),
        playback_queue: Some(proto::PlaybackQueue {
            location: Some(0),
            content_items: vec![video_item()],
            ..Default::default()
        }),
        // `PreviousChapter` is not supported, so `AppleTv` seeks instead.
        supported_commands: Some(proto::SupportedCommands {
            supported_commands: vec![proto::CommandInfo {
                command: Some(Command::NextChapter as i32),
                enabled: Some(true),
                ..Default::default()
            }],
        }),
        ..Default::default()
    };
    out.extend(mrp(&state, None, 0));
    out
}

/// Item with chapters at 0, 60, 120 and 180 s, and Swedish subtitles on.
fn video_item() -> proto::ContentItem {
    let chapter = |start: f64| proto::ContentItem {
        metadata: Some(proto::ContentItemMetadata {
            start_time: Some(start),
            ..Default::default()
        }),
        ..Default::default()
    };
    let subtitles = proto::LanguageOption {
        r#type: Some(mrp::LanguageType::Legible as i32),
        language_tag: Some("sv".into()),
        identifier: Some("sub-sv".into()),
        ..Default::default()
    };
    proto::ContentItem {
        identifier: Some("video-1".into()),
        metadata: Some(proto::ContentItemMetadata {
            duration: Some(240.0),
            ..Default::default()
        }),
        sections: [0.0, 60.0, 120.0, 180.0].map(chapter).into(),
        available_language_options: vec![proto::LanguageOptionGroup {
            language_options: vec![subtitles.clone()],
            ..Default::default()
        }],
        current_language_options: vec![subtitles],
        ..Default::default()
    }
}

fn recording(down: bool) -> Vec<u8> {
    use proto::set_recording_state_message::RecordingState;

    let state = if down {
        RecordingState::Recording
    } else {
        RecordingState::NotRecording
    };
    let msg = proto::SetRecordingStateMessage {
        state: Some(state as i32),
    };
    mrp(&msg, None, 0)
}

/// Voice device registration and voice input.
fn handle_voice(env: &Envelope, id: Option<&str>, log: &Arc<Mutex<Log>>) -> Option<Vec<u8>> {
    if env.is::<proto::RegisterVoiceInputDeviceMessage>() {
        log.lock().unwrap().voice_devices += 1;
        let reply = proto::RegisterVoiceInputDeviceResponseMessage {
            device_id: Some(7),
            error_code: Some(0),
        };
        return Some(mrp(&reply, id, 0));
    }
    if env.is::<proto::SendVoiceInputMessage>() {
        let m: proto::SendVoiceInputMessage = env.payload().unwrap();
        let block = m.data_block?;
        let time = block.time?.timestamp?;
        let size = block.buffer?.contents?.len();
        log.lock().unwrap().voice.push((time, size));
    }
    None
}

/// Volume messages: set, get and mute.
fn handle_volume(env: &Envelope, id: Option<&str>, log: &Arc<Mutex<Log>>) -> Option<Vec<u8>> {
    if env.is::<proto::SetVolumeMessage>() {
        let v: proto::SetVolumeMessage = env.payload().unwrap();
        if v.output_device_uid.as_deref() == Some("UID-1") {
            log.lock().unwrap().volume = v.volume;
            return None;
        }
        // Another output device: report its new volume.
        let changed = proto::VolumeDidChangeMessage {
            volume: v.volume,
            output_device_uid: v.output_device_uid,
            ..Default::default()
        };
        return Some(mrp(&changed, None, 0));
    }
    if env.is::<proto::ModifyOutputContextRequestMessage>() {
        // The device reports the new group in a device info update.
        let r: proto::ModifyOutputContextRequestMessage = env.payload().unwrap();
        let grouped = r
            .adding_devices
            .iter()
            .map(|uid| proto::DeviceInfoMessage {
                device_uid: Some(uid.clone()),
                name: format!("Speaker {uid}"),
                ..Default::default()
            })
            .collect();
        let info = proto::DeviceInfoMessage {
            name: "Fake TV".into(),
            device_uid: Some("UID-1".into()),
            grouped_devices: grouped,
            ..Default::default()
        };
        return Some(mrp(&info, None, 0));
    }
    if env.is::<proto::GetVolumeMessage>() {
        let r: proto::GetVolumeMessage = env.payload().unwrap();
        assert_eq!(r.output_device_uid.as_deref(), Some("UID-1"));
        let result = proto::GetVolumeResultMessage { volume: Some(0.3) };
        return Some(mrp(&result, id, 0));
    }
    if env.is::<proto::GetVolumeMutedMessage>() {
        let result = proto::GetVolumeMutedResultMessage {
            is_muted: Some(false),
        };
        return Some(mrp(&result, id, 0));
    }
    if env.is::<proto::SetVolumeMutedMessage>() {
        let m: proto::SetVolumeMutedMessage = env.payload().unwrap();
        assert_eq!(m.output_device_uid.as_deref(), Some("UID-1"));
        let changed = proto::VolumeMutedDidChangeMessage {
            is_muted: m.is_muted,
            output_device_uid: m.output_device_uid,
        };
        return Some(mrp(&changed, None, 0));
    }
    None
}

fn options() -> ClientOptions {
    let mut options = ClientOptions::default();
    options.timeout = Duration::from_secs(2);
    options
}

#[tokio::test]
async fn session() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    assert!(client.is_connected());
    assert_eq!(client.device_info().name, "Fake TV");

    let mut np = client.now_playing();
    tokio::time::timeout(Duration::from_secs(2), np.wait_for(|n| n.title.is_some()))
        .await
        .unwrap()
        .unwrap();
    {
        let n = np.borrow();
        assert_eq!(n.title.as_deref(), Some("Song"));
        assert_eq!(n.state, Some(PlaybackState::Playing));
        assert_eq!(n.app.as_deref(), Some("com.apple.TVMusic"));
        assert_eq!(n.app_name.as_deref(), Some("Music"));
    }

    let mut messages = client.messages();
    client.press(Button::Select).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client.seek(30.0).await.unwrap();
    match client.command(Command::Stop).await.unwrap_err() {
        Error::CommandFailed { send_error, .. } => assert_eq!(send_error, 1),
        other => panic!("unexpected {other}"),
    }
    match client.command(Command::NextTrack).await.unwrap_err() {
        Error::Mrp(ratv_mrp::Error::Device { code, .. }) => assert_eq!(code, 6),
        other => panic!("unexpected {other}"),
    }
    // Other messages, such as the reply to the details request, can come first.
    while !messages
        .recv()
        .await
        .unwrap()
        .is::<proto::SendCommandResultMessage>()
    {}
    let art = client.artwork(300.0, -1.0).await.unwrap().unwrap();
    assert_eq!(&art.data[..], b"\x89PNG");
    assert_eq!(art.mime_type.as_deref(), Some("image/png"));
    assert_eq!(art.width, Some(300));
    client.set_volume(0.5).await.unwrap();

    // Sent right after DEVICE_INFO, so it is known when connect returns.
    assert_eq!(client.volume().borrow().capabilities, Some(3));
    // Initial volume comes from the replies to the connect requests.
    let mut volume = client.volume();
    let wait = volume.wait_for(|v| v.level.is_some() && v.muted.is_some());
    let v = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap()
        .clone();
    assert_eq!((v.level, v.muted), (Some(0.3), Some(false)));
    client.set_muted(true).await.unwrap();
    let wait = volume.wait_for(|v| v.muted == Some(true));
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();

    // Output devices: add a speaker, then change its volume.
    client.add_outputs(&["HP-1"]).await.unwrap();
    let wait = volume.wait_for(|v| v.outputs.iter().any(|o| o.uid == "HP-1"));
    let outputs = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap()
        .outputs
        .clone();
    assert_eq!(outputs[0].name, "Speaker HP-1");
    client.set_output_volume("HP-1", 0.4).await.unwrap();
    let wait = volume.wait_for(|v| v.outputs[0].volume == Some(0.4));
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    // The connected device keeps its own volume.
    assert_eq!(volume.borrow().level, Some(0.3));
    client
        .hold(Button::Home, Duration::from_millis(1))
        .await
        .unwrap();

    // Wait for one feedback request (sent every 2 seconds).
    tokio::time::sleep(Duration::from_millis(2300)).await;
    client.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(&log.buttons[..2], &[(1, 0x89, true), (1, 0x89, false)]);
    assert_eq!(
        &log.buttons[2..],
        &[(0x0c, 0x40, true), (0x0c, 0x40, false)]
    );
    assert_eq!(
        log.commands,
        [
            Command::Pause as i32,
            Command::SeekToPlaybackPosition as i32,
            Command::Stop as i32,
            Command::NextTrack as i32
        ]
    );
    assert_eq!(log.volume, Some(0.5));
    assert_eq!(log.event_acks, 1);
    assert!(log.feedback >= 1);
    assert!(log.teardown);
}

#[tokio::test]
async fn siri() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();

    let mut siri = client.start_siri().await.unwrap();
    assert!(matches!(client.start_siri().await, Err(Error::SiriActive)));
    siri.send(&b"one"[..]).await.unwrap();
    siri.send(&b"three"[..]).await.unwrap();
    siri.finish().await.unwrap();
    // Dropping releases Siri too. The device is registered once.
    drop(client.start_siri().await.unwrap());
    // A start can finish or be canceled here. Both release Siri and allow a new start.
    if let Ok(result) = tokio::time::timeout(Duration::ZERO, client.start_siri()).await {
        drop(result.unwrap());
    }
    drop(client.start_siri().await.unwrap());
    // Lets the device read the last release before the connection closes.
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice_devices, 1);
    assert_eq!(log.voice, [(0.0, 3), (320.0, 5)]);
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, [siri; 4].concat());
}

#[tokio::test]
async fn closed_by_device() {
    let device = start_faulty_device(Fault::DropData).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason.as_deref(), Some(Error::Disconnected | Error::Io(_))),
        "{reason:?}"
    );
    assert!(!client.is_connected());
    let err = client.press(Button::Select).await.unwrap_err();
    assert!(matches!(err, Error::Disconnected), "{err}");
    // Nothing to close any more.
    client.close().await.unwrap();
}

#[tokio::test]
async fn closed_without_feedback_answer() {
    let device = start_faulty_device(Fault::NoFeedback).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    // Time moves forward by itself while all tasks wait.
    tokio::time::pause();
    let reason = client.closed().await;
    assert!(
        matches!(reason.as_deref(), Some(Error::NoAnswer("feedback"))),
        "{reason:?}"
    );
    assert!(device.log.lock().unwrap().feedback >= 4);
}

#[tokio::test]
async fn siri_in_task() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    // `Siri` owns a clone of the client, so it can move to another task.
    let mut siri = client.start_siri().await.unwrap();
    tokio::spawn(async move {
        siri.send(&b"one"[..]).await?;
        siri.finish().await
    })
    .await
    .unwrap()
    .unwrap();
    client.close().await.unwrap();
    assert!(client.closed().await.is_none());
    let err = client.press(Button::Select).await.unwrap_err();
    assert!(matches!(err, Error::Disconnected), "{err}");
    // Lets the device read TEARDOWN.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice, [(0.0, 3)]);
    // The release is sent before TEARDOWN.
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, siri);
    assert!(log.teardown);
}

#[tokio::test]
async fn siri_not_recording() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut opts = options();
    opts.timeout = Duration::from_millis(200);
    let client = Client::connect(addr, &credentials(), &opts).await.unwrap();
    device.log.lock().unwrap().no_recording = true;

    // Kept audio has a limit.
    let mut siri = client.start_siri().await.unwrap();
    let mut result = Ok(());
    for _ in 0..2000 {
        result = siri.send(&b"x"[..]).await;
        if result.is_err() {
            break;
        }
    }
    assert!(matches!(result, Err(Error::SiriNotRecording)));
    drop(siri);

    // `finish` waits for the timeout.
    let mut siri = client.start_siri().await.unwrap();
    siri.send(&b"x"[..]).await.unwrap();
    assert!(matches!(siri.finish().await, Err(Error::SiriNotRecording)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice, []);
    // Both sessions release Siri.
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, [siri; 2].concat());
}

#[cfg(feature = "opus")]
#[tokio::test]
async fn siri_pcm() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();

    let mut siri = client.start_siri().await.unwrap();
    // 100 + 600 samples: two frames now, the rest padded by `finish`.
    siri.push_pcm(&[100; 100]).await.unwrap();
    siri.push_pcm(&[-100; 600]).await.unwrap();
    siri.finish().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();

    let log = device.log.lock().unwrap();
    let times: Vec<f64> = log.voice.iter().map(|&(t, _)| t).collect();
    assert_eq!(times, [0.0, 320.0, 640.0]);
    assert!(log.voice.iter().all(|&(_, size)| size > 0));
}

#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_chapters_and_subtitles() {
    use ratv::{AppleTv, Endpoints};

    let device = start_device().await;
    let endpoints = Endpoints {
        companion: None,
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let opts = options();
    let tv = AppleTv::connect(&endpoints, &credentials(), &opts)
        .await
        .unwrap();
    let mut np = tv.now_playing().unwrap();
    let ready = np.wait_for(|n| n.chapters.len() == 4 && n.subtitle_language.is_some());
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    drop(np);

    // At about 100 s: the app supports `NextChapter`, but not `PreviousChapter`.
    tv.next_chapter().await.unwrap();
    tv.previous_chapter().await.unwrap();
    // Turns off the current track, not only the type.
    tv.disable_subtitles().await.unwrap();
    tv.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(
        log.commands,
        [
            Command::NextChapter as i32,
            Command::SeekToPlaybackPosition as i32,
            Command::DisableLanguageOption as i32
        ]
    );
    assert_eq!(log.seeks, [60.0]);
    let archive = &log.language_options[0];
    assert!(archive.windows(6).any(|w| w == b"sub-sv"));
}

/// Waits until the fake device got TEARDOWN.
async fn wait_teardown(log: &Mutex<Log>) {
    for _ in 0..100 {
        if log.lock().unwrap().teardown {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no TEARDOWN");
}

#[tokio::test]
async fn failed_setup_sends_teardown() {
    let device = start_faulty_device(Fault::DataPort).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let result = Client::connect(addr, &credentials(), &options()).await;
    // Windows retries a refused connect for about 2 s, so the timeout can come first.
    assert!(matches!(result, Err(Error::Io(_) | Error::Timeout)));
    wait_teardown(&device.log).await;
}

#[tokio::test]
async fn missing_device_info_sends_teardown() {
    let device = start_faulty_device(Fault::DeviceInfo).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut options = options();
    options.timeout = Duration::from_millis(200);
    let result = Client::connect(addr, &credentials(), &options).await;
    assert!(matches!(result, Err(Error::Timeout)));
    wait_teardown(&device.log).await;
}

#[tokio::test]
async fn wrong_credentials_fail() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let good = credentials();
    let creds = Credentials::new(
        [1; 32],
        [0; 32].into(),
        good.accessory_id().to_vec(),
        good.client_id().to_vec(),
    );
    assert!(Client::connect(addr, &creds, &options()).await.is_err());
}

#[tokio::test]
async fn connection_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    // Windows retries a refused connect for about 2 s, so the timeout can come first.
    assert!(matches!(
        Client::connect(addr, &credentials(), &options()).await,
        Err(Error::Io(_) | Error::Timeout)
    ));
}

/// Companion is refused, so `AppleTv` uses only MRP.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_falls_back_to_mrp() {
    use ratv::{AppleTv, Endpoints, Key};

    let device = start_device().await;
    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let companion = refused.local_addr().unwrap();
    drop(refused);
    let endpoints = Endpoints {
        companion: Some(companion),
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let opts = options();
    let tv = AppleTv::connect(&endpoints, &credentials(), &opts)
        .await
        .unwrap();
    assert!(tv.companion().is_none());
    assert!(tv.mrp().is_some());
    assert!(tv.now_playing().is_some());

    tv.press(Key::Select).await.unwrap();
    tv.press(Key::TopMenu).await.unwrap();
    tv.pause().await.unwrap();
    tv.seek(10.0).await.unwrap();
    tv.set_volume(0.25).await.unwrap();
    assert!(matches!(
        tv.turn_on().await,
        Err(Error::NotConnected("Companion"))
    ));
    assert!(matches!(
        tv.launch_app("x").await,
        Err(Error::NotConnected("Companion"))
    ));
    assert!(tv.is_connected());
    tv.clone().close().await.unwrap();
    assert!(tv.closed().await.is_none());
    assert!(!tv.is_connected());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(log.buttons[0], (1, 0x89, true));
    assert_eq!(log.buttons[2], (0x0c, 0x60, true));
    assert_eq!(
        log.commands,
        [
            Command::Pause as i32,
            Command::SeekToPlaybackPosition as i32
        ]
    );
    assert_eq!(log.volume, Some(0.25));
    assert!(log.teardown);
}

/// Both endpoints fail: the first error is returned.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_fails_when_nothing_connects() {
    use ratv::{AppleTv, Endpoints};

    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = refused.local_addr().unwrap();
    drop(refused);
    let endpoints = Endpoints {
        companion: Some(addr),
        airplay: Some(addr),
    };
    let err = AppleTv::connect(&endpoints, &credentials(), &ClientOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)));
}

/// Fake AirPlay pair-setup. Returns the paired controller and the client names it saw.
async fn pair_device(listener: TcpListener) -> (Option<PairedController>, Vec<String>) {
    let (mut s, _) = listener.accept().await.unwrap();
    let mut ch = Channel::new();
    let mut server = PairSetupServer::new(DEVICE_ID, &DEVICE_LTSK, "1234", [9; 16], &[0x77; 32]);
    let mut names = Vec::new();
    while server.controller().is_none() {
        let Some(req) = read_msg(&mut s, &mut ch).await else {
            break;
        };
        names.extend(req.header("X-Apple-Client-Name").map(str::to_owned));
        match (req.method().unwrap(), req.uri().unwrap()) {
            ("POST", "/pair-pin-start") => reply(&mut s, &mut ch, &req, b"").await,
            ("POST", "/pair-setup") => {
                let out = server.handle(&req.body()).unwrap();
                reply(&mut s, &mut ch, &req, &out).await;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    (server.controller().cloned(), names)
}

#[tokio::test]
async fn pairing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let device = tokio::spawn(pair_device(listener));
    let mut options = PairOptions::default();
    options.name = "Test Remote".into();
    options.client_id = Some("client-9".into());
    let creds = mrp::pair(addr, &options)
        .await
        .unwrap()
        .finish("1234")
        .await
        .unwrap();
    assert_eq!(creds.accessory_id(), DEVICE_ID);
    assert_eq!(creds.client_id(), b"client-9");
    assert_eq!(
        *creds.accessory_ltpk(),
        SigningKey::from_bytes(&DEVICE_LTSK)
            .verifying_key()
            .to_bytes()
    );

    let (controller, names) = device.await.unwrap();
    let controller = controller.unwrap();
    assert_eq!(controller.id, b"client-9");
    // pin-start and the three pair-setup requests carry the name.
    assert_eq!(names, ["Test Remote"; 4]);
    let metadata = controller.metadata.unwrap();
    let meta = ratv_codec::opack::decode(&metadata).unwrap();
    assert_eq!(
        meta.get("name").and_then(ratv_codec::opack::Value::as_str),
        Some("Test Remote")
    );
}

#[tokio::test]
async fn pairing_with_wrong_pin_fails() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pair_device(listener));
    let prompt = mrp::pair(addr, &PairOptions::default()).await.unwrap();
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
async fn hold_released_when_cancelled() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let hold = client.hold(Button::Home, Duration::from_secs(10));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), hold)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Down, then up from the dropped future.
    assert_eq!(
        device.log.lock().unwrap().buttons,
        [(0x0c, 0x40, true), (0x0c, 0x40, false)]
    );
}
