// Fake AirPlay device with MRP. Included with `include!` by the tests that use it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

// Each flag is a separate fake device setting.
#[allow(clippy::struct_excessive_bools)]
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
    /// The device cannot change the volume.
    no_volume_control: bool,
    /// Where the current item is missing.
    no_item: NoItem,
    /// Queue requests for one item without artwork.
    item_requests: usize,
    /// Sent on the data stream after the next read.
    push: Option<Vec<u8>>,
    /// From the next command on, the data stream is not read while this is set.
    stop_reading: bool,
    /// Closes the connection while it does not read.
    hang_up: bool,
    /// `SkipBackward` fails with a send error.
    fail_skip: bool,
    /// The command list is not in the first state. It comes just before the
    /// answer to `SkipForward`, `Stop` or `BeginRewind`, which fails with
    /// handler status 404.
    late_commands: bool,
    /// Commands that fail with handler status 404, without a command list.
    /// Checked before `late_commands`.
    not_supported: Vec<i32>,
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
    let cseq = req.cseq().map_or_default(|c| c.to_string());
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

/// Where the fake device leaves out the current item.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum NoItem {
    #[default]
    None,
    /// The player is paused in the background: the state is sent without an item.
    State,
    /// As `State`, and queue requests without artwork get no item either.
    Reply,
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
                // A small receive buffer, so writes wait soon when the device does not read.
                let socket = tokio::net::TcpSocket::new_v4().unwrap();
                socket.set_recv_buffer_size(16 * 1024).unwrap();
                socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let listener = socket.listen(8).unwrap();
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

/// Sends one request right after it connects, before RECORD is answered.
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
        let (mut close, mut command) = (false, false);
        while let Some(data) = ds.poll().unwrap() {
            for env in frames(data) {
                let env = env.unwrap();
                close |= drop && env.is::<proto::ClientUpdatesConfigMessage>();
                command |= env.is::<proto::SendCommandMessage>();
                if let Some(out) = handle(&env, &log) {
                    ds.send(&out);
                }
            }
        }
        let pushed = log.lock().unwrap().push.take();
        if let Some(out) = pushed {
            ds.send(&out);
        }
        if ds.has_transmit() {
            s.write_all(&ds.take_transmit()).await.unwrap();
        }
        if close {
            return;
        }
        while command && log.lock().unwrap().stop_reading {
            if log.lock().unwrap().hang_up {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
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
                is_volume_control_available: Some(!log.lock().unwrap().no_volume_control),
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
        let l = log.lock().unwrap();
        let (no_item, late_commands) = (l.no_item != NoItem::None, l.late_commands);
        drop(l);
        return Some(initial_state(no_item, late_commands));
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
        return command_reply(env, id, log);
    }
    if env.is::<proto::PlaybackQueueRequestMessage>() {
        let r: proto::PlaybackQueueRequestMessage = env.payload().unwrap();
        let mut l = log.lock().unwrap();
        if r.length == Some(1) && r.artwork_width.is_none() {
            l.item_requests += 1;
        }
        let artwork = r.return_content_item_assets_in_user_completion == Some(true);
        if l.no_item == NoItem::Reply && !artwork {
            let state = proto::SetStateMessage {
                playback_queue: Some(proto::PlaybackQueue::default()),
                ..Default::default()
            };
            return Some(mrp(&state, id, 0));
        }
        drop(l);
        // Only artwork requests get an image, like on a real device.
        #[allow(clippy::cast_possible_truncation)]
        let item = if artwork {
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

/// Answer to a `SendCommandMessage`.
fn command_reply(env: &Envelope, id: Option<&str>, log: &Arc<Mutex<Log>>) -> Option<Vec<u8>> {
    let c: proto::SendCommandMessage = env.payload().unwrap();
    let cmd = c.command?;
    let mut l = log.lock().unwrap();
    l.commands.push(cmd);
    if let Some(o) = c.options {
        l.seeks.extend(o.playback_position);
        l.language_options.extend(o.language_option);
    }
    let (late_commands, fail_skip) = (l.late_commands, l.fail_skip);
    let not_supported = l.not_supported.contains(&cmd);
    drop(l);
    if not_supported {
        let result = proto::SendCommandResultMessage {
            handler_return_status: Some(404),
            ..Default::default()
        };
        return Some(mrp(&result, id, 0));
    }
    let late = [Command::SkipForward, Command::Stop, Command::BeginRewind];
    if late_commands && late.iter().any(|&c| c as i32 == cmd) {
        let state = proto::SetStateMessage {
            player_path: Some(proto::PlayerPath {
                client: Some(music_client()),
                ..Default::default()
            }),
            supported_commands: Some(supported_commands()),
            ..Default::default()
        };
        let result = proto::SendCommandResultMessage {
            handler_return_status: Some(404),
            ..Default::default()
        };
        let mut out = mrp(&state, None, 0);
        out.extend(mrp(&result, id, 0));
        return Some(out);
    }
    if cmd == Command::NextTrack as i32 {
        // Device-level error code on the envelope.
        return Some(mrp(&proto::SendCommandResultMessage::default(), id, 6));
    }
    let fail = cmd == Command::Stop as i32 || (cmd == Command::SkipBackward as i32 && fail_skip);
    let send_error = i32::from(fail);
    let result = proto::SendCommandResultMessage {
        send_error: Some(send_error),
        ..Default::default()
    };
    Some(mrp(&result, id, 0))
}

/// Now playing client and state, sent after `CLIENT_UPDATES_CONFIG`.
/// With `no_item`, a paused state without an item. With `late_commands`,
/// without the command list.
fn initial_state(no_item: bool, late_commands: bool) -> Vec<u8> {
    let client = music_client();
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
        supported_commands: (!late_commands).then(supported_commands),
        ..Default::default()
    };
    if no_item {
        let paused = proto::SetStateMessage {
            playback_state: Some(PlaybackState::Paused as i32),
            playback_queue: None,
            now_playing_info: None,
            ..state
        };
        out.extend(mrp(&paused, None, 0));
        return out;
    }
    out.extend(mrp(&state, None, 0));
    out
}

fn music_client() -> proto::NowPlayingClient {
    proto::NowPlayingClient {
        bundle_identifier: Some("com.apple.TVMusic".into()),
        display_name: Some("Music".into()),
        ..Default::default()
    }
}

/// Commands of the app. These are not listed, so `AppleTv` uses another way:
/// `PreviousChapter`, `Play`, `TogglePlayPause`, `Stop`, `SkipForward`,
/// `BeginFastForward`.
fn supported_commands() -> proto::SupportedCommands {
    proto::SupportedCommands {
        supported_commands: [
            Command::NextChapter,
            Command::Pause,
            Command::SeekToPlaybackPosition,
            Command::SkipBackward,
        ]
        .into_iter()
        .map(|c| proto::CommandInfo {
            command: Some(c as i32),
            enabled: Some(true),
            ..Default::default()
        })
        // Scan rates only, as in the Apple TV app.
        .chain([proto::CommandInfo {
            command: Some(Command::ChangePlaybackRate as i32),
            enabled: Some(true),
            supported_rates: vec![-8.0, 8.0],
            ..Default::default()
        }])
        .collect(),
    }
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
