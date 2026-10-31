//! Tries MRP voice input: registers a voice input device, holds Siri over
//! Companion and streams audio over MRP. Prints what the device sends.
//!
//! ```text
//! say -o voice.wav --data-format=LEI16@16000 "What's the weather tomorrow"
//! afconvert -f caff -d opus@16000 -c 1 voice.wav voice.caf
//! cargo run --example voice_probe -- <credentials> <ip> voice.caf
//! ```
//!
//! A `.caf` file must hold 16 kHz mono Opus with 20 ms packets, as `afconvert` writes it.
//! A `.wav` file must hold 16-bit, 16 kHz, mono PCM.
//! Options, comma separated: `nosiri` (do not hold Siri), `mrpsiri` (hold Siri with an
//! MRP button event instead of Companion), `nostate` (do not send
//! `SET_RECORDING_STATE`), `noformat` (no format in each audio block).

#[path = "common/caf.rs"]
mod caf;
#[path = "common/wav.rs"]
mod wav;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use ratv::discovery::{MDNS_PORT, ScanOptions, scan};
use ratv::{AppleTv, ConnectOptions, Endpoints, Key};
use ratv_codec::bplist::Value;
use ratv_mrp::Envelope;
use ratv_mrp::proto::{
    AudioBuffer, AudioDataBlock, AudioFormatSettings, AudioStreamPacketDescription, AudioTime,
    GetVoiceInputDevicesMessage, GetVoiceInputDevicesResponseMessage,
    RegisterVoiceInputDeviceMessage, RegisterVoiceInputDeviceResponseMessage,
    SendButtonEventMessage, SendVoiceInputMessage, SetRecordingStateMessage,
    VoiceInputDeviceDescriptor, set_recording_state_message::RecordingState,
};

const SAMPLE_RATE: u32 = 16_000;
/// Samples per voice input message (20 ms).
const FRAMES: u32 = 320;
/// `lpcm` as a four character code.
const LINEAR_PCM: i64 = 0x6c70_636d;
/// `opus` as a four character code.
const OPUS: i64 = 0x6f70_7573;

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Pcm,
    Opus,
}

/// `AVAudioFormat` settings as a binary plist. Opus uses the keys of Apple's Remote app.
fn format_settings(format: Format) -> AudioFormatSettings {
    let rate = (
        "AVSampleRateKey".into(),
        Value::Real(f64::from(SAMPLE_RATE)),
    );
    let channels = ("AVNumberOfChannelsKey".into(), Value::Int(1));
    let settings = match format {
        Format::Opus => vec![rate, ("AVFormatIDKey".into(), Value::Int(OPUS)), channels],
        Format::Pcm => vec![
            ("AVFormatIDKey".into(), Value::Int(LINEAR_PCM)),
            rate,
            channels,
            ("AVLinearPCMBitDepthKey".into(), Value::Int(16)),
            ("AVLinearPCMIsFloatKey".into(), Value::Bool(false)),
            ("AVLinearPCMIsBigEndianKey".into(), Value::Bool(false)),
            ("AVLinearPCMIsNonInterleaved".into(), Value::Bool(false)),
        ],
    };
    AudioFormatSettings {
        format_settings_plist_data: Some(Value::Dict(settings).encode().into()),
    }
}

/// One audio block of 20 ms that starts at sample `start`.
fn voice_input(
    packet: &[u8],
    format: Format,
    start: u32,
    with_format: bool,
) -> SendVoiceInputMessage {
    let len = i64::try_from(packet.len()).unwrap_or(i64::MAX);
    let (capacity, max_size, count, descriptions) = match format {
        // One sample is one packet of 2 bytes.
        Format::Pcm => (len / 2, 2, len / 2, Vec::new()),
        Format::Opus => (
            1,
            len,
            1,
            vec![AudioStreamPacketDescription {
                start_offset: Some(0),
                variable_frames_in_packet: Some(0),
                data_byte_size: u32::try_from(packet.len()).ok(),
            }],
        ),
    };
    SendVoiceInputMessage {
        data_block: Some(AudioDataBlock {
            buffer: Some(AudioBuffer {
                format_settings: with_format.then(|| format_settings(format)),
                packet_capacity: Some(capacity),
                maximum_packet_size: Some(max_size),
                packet_count: Some(count),
                contents: Some(packet.to_vec().into()),
                packet_descriptions: descriptions,
            }),
            time: Some(AudioTime {
                timestamp: Some(f64::from(start)),
                sample_rate: Some(f64::from(SAMPLE_RATE)),
            }),
            gain: Some(1.0),
        }),
    }
}

/// Prints voice related messages, and the kind of all others.
fn print(env: &Envelope) {
    if env.is::<SetRecordingStateMessage>() {
        let msg: Result<SetRecordingStateMessage, _> = env.payload();
        println!("<- SET_RECORDING_STATE {msg:?}");
    } else if env.is::<RegisterVoiceInputDeviceResponseMessage>() {
        let msg: Result<RegisterVoiceInputDeviceResponseMessage, _> = env.payload();
        println!("<- REGISTER_VOICE_INPUT_DEVICE_RESPONSE {msg:?}");
    } else if env.is::<GetVoiceInputDevicesResponseMessage>() {
        let msg: Result<GetVoiceInputDevicesResponseMessage, _> = env.payload();
        println!("<- GET_VOICE_INPUT_DEVICES_RESPONSE {msg:?}");
    } else {
        println!("<- kind {} id {:?}", env.kind(), env.identifier());
    }
}

async fn connect(credentials: &str, ip: IpAddr) -> Result<AppleTv, Box<dyn std::error::Error>> {
    let credentials = credentials.parse().map_err(|_| "invalid credentials")?;
    let options = ScanOptions {
        targets: vec![SocketAddr::new(ip, MDNS_PORT)],
        ..ScanOptions::default()
    };
    let devices = scan(&options).await?;
    let device = devices.first().ok_or("device not found")?;
    let tv = AppleTv::connect(
        &Endpoints::from(device),
        &credentials,
        &ConnectOptions::default(),
    )
    .await?;
    Ok(tv)
}

/// Presses or releases Siri over Companion, or over MRP with the `mrpsiri` option.
async fn siri(tv: &AppleTv, options: &str, down: bool) -> Result<(), Box<dyn std::error::Error>> {
    let has = |name: &str| options.split(',').any(|o| o == name);
    let action = if down { "down" } else { "up" };
    if has("nosiri") {
        println!("   skip Siri {action}");
    } else if has("mrpsiri") {
        // Consumer page, usage 4 (microphone), as in a comment in pyatv.
        println!("-> Siri {action} (MRP button 12/0x04)");
        let event = SendButtonEventMessage {
            usage_page: Some(12),
            usage: Some(0x04),
            button_down: Some(down),
        };
        tv.mrp().ok_or("MRP is not connected")?.send(&event).await?;
    } else {
        println!("-> Siri {action} (Companion)");
        tv.key(Key::Siri, down).await?;
    }
    Ok(())
}

/// Sends `SET_RECORDING_STATE` unless `skip` is set.
async fn set_recording(
    mrp: &ratv::mrp::Client,
    skip: bool,
    state: RecordingState,
) -> Result<(), ratv::Error> {
    if skip {
        println!("   skip SET_RECORDING_STATE");
        return Ok(());
    }
    println!("-> SET_RECORDING_STATE {state:?}");
    mrp.send(&SetRecordingStateMessage {
        state: Some(state as i32),
    })
    .await?;
    // A disconnect caused by this message shows before the next step.
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("   connected {}", mrp.is_connected());
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (credentials, ip, path, options) = match args.as_slice() {
        [c, i, p] => (c, i, p, ""),
        [c, i, p, o] => (c, i, p, o.as_str()),
        _ => return Err("usage: voice_probe <credentials> <ip> <file> [options]".into()),
    };
    let has = |name: &str| options.split(',').any(|o| o == name);
    let file = std::fs::read(path)?;
    let is_caf = std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("caf"));
    let (format, packets) = if is_caf {
        (Format::Opus, caf::packets(&file)?)
    } else {
        (
            Format::Pcm,
            wav::pcm(&file)?.chunks(FRAMES as usize * 2).collect(),
        )
    };
    let bytes: usize = packets.iter().map(|p| p.len()).sum();
    println!("{} blocks of 20 ms, {bytes} bytes", packets.len());

    let tv = connect(credentials, ip.parse()?).await?;
    let mrp = tv.mrp().ok_or("MRP is not connected")?;
    let mut messages = mrp.messages();
    let printer = tokio::spawn(async move {
        while let Ok(env) = messages.recv().await {
            print(&env);
        }
    });

    println!("-> GET_VOICE_INPUT_DEVICES");
    if let Err(err) = mrp.request(&GetVoiceInputDevicesMessage {}).await {
        println!("   failed: {err}");
    }

    let register = RegisterVoiceInputDeviceMessage {
        device_descriptor: Some(VoiceInputDeviceDescriptor {
            supported_formats: vec![format_settings(format)],
            default_format: Some(format_settings(format)),
        }),
    };
    println!("-> REGISTER_VOICE_INPUT_DEVICE");
    match mrp.request(&register).await {
        Ok(env) => match env.payload::<RegisterVoiceInputDeviceResponseMessage>() {
            Ok(r) => println!("   device {:?}, error {:?}", r.device_id, r.error_code),
            Err(err) => println!("   reply kind {}: {err}", env.kind()),
        },
        Err(err) => println!("   failed: {err}"),
    }

    siri(&tv, options, true).await?;
    // Give the device time to ask for recording.
    tokio::time::sleep(Duration::from_millis(500)).await;
    set_recording(mrp, has("nostate"), RecordingState::Recording).await?;

    println!("-> {} SEND_VOICE_INPUT messages", packets.len());
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    for (i, packet) in packets.iter().enumerate() {
        interval.tick().await;
        let start = u32::try_from(i)? * FRAMES;
        let msg = voice_input(packet, format, start, !has("noformat"));
        if let Err(err) = mrp.send(&msg).await {
            println!("   failed at message {i} of {}: {err}", packets.len());
            return Ok(());
        }
    }
    // Sends are queued; wait to see if the device closes the connection.
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("   all sent, connected {}", mrp.is_connected());

    set_recording(mrp, has("nostate"), RecordingState::NotRecording).await?;
    siri(&tv, options, false).await?;

    println!("listening 8 s for the answer");
    tokio::time::sleep(Duration::from_secs(8)).await;
    printer.abort();
    tv.close().await?;
    Ok(())
}
