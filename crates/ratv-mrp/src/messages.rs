//! Builders for the messages a remote control sends.
//!
//! Connection setup order: [`device_info`] (request), [`connected`],
//! [`client_updates_config`], then `GET_KEYBOARD_SESSION_MESSAGE` with
//! [`encode_type`](crate::encode_type).

use std::sync::LazyLock;

use bytes::Bytes;
use prost::Message as _;

use crate::proto::{
    self, AudioBuffer, AudioDataBlock, AudioFormatSettings, AudioStreamPacketDescription,
    AudioTime, ClientUpdatesConfigMessage, Command, CommandOptions, DeviceInfoMessage,
    GetVolumeControlCapabilitiesMessage, GetVolumeMessage, GetVolumeMutedMessage, LanguageOption,
    ModifyOutputContextRequestMessage, PlaybackQueueRequestMessage,
    RegisterVoiceInputDeviceMessage, SendCommandMessage, SendVoiceInputMessage,
    SetConnectionStateMessage, SetVolumeMessage, SetVolumeMutedMessage, TextInputMessage,
    VoiceInputDeviceDescriptor,
};

/// Client identity sent in [`device_info`].
#[derive(Debug, Clone, Copy)]
pub struct ClientInfo<'a> {
    /// Stable unique id, usually a UUID string.
    pub unique_identifier: &'a str,
    /// Name shown on the device.
    pub name: &'a str,
    /// iOS build version, for example `18M60`.
    pub system_build_version: &'a str,
}

/// First message of a connection. The device answers with its own device info.
#[must_use]
pub fn device_info(client: &ClientInfo<'_>) -> DeviceInfoMessage {
    DeviceInfoMessage {
        unique_identifier: Some(client.unique_identifier.to_owned()),
        name: client.name.to_owned(),
        localized_model_name: Some("iPhone".to_owned()),
        system_build_version: Some(client.system_build_version.to_owned()),
        application_bundle_identifier: Some("com.apple.TVRemote".to_owned()),
        application_bundle_version: Some("344.28".to_owned()),
        protocol_version: Some(1),
        last_supported_message_type: Some(108),
        supports_system_pairing: Some(true),
        allows_pairing: Some(true),
        system_media_application: Some("com.apple.TVMusic".to_owned()),
        supports_acl: Some(true),
        supports_shared_queue: Some(true),
        supports_extended_motion: Some(true),
        shared_queue_version: Some(2),
        device_class: Some(proto::device_class::Enum::IPhone as i32),
        logical_device_count: Some(1),
        ..Default::default()
    }
}

/// Tells the device the connection is ready.
#[must_use]
pub const fn connected() -> SetConnectionStateMessage {
    SetConnectionStateMessage {
        state: Some(proto::set_connection_state_message::ConnectionState::Connected as i32),
    }
}

/// Subscribes to now playing, artwork, volume, keyboard and output device updates.
#[must_use]
pub const fn client_updates_config() -> ClientUpdatesConfigMessage {
    ClientUpdatesConfigMessage {
        now_playing_updates: Some(true),
        artwork_updates: Some(true),
        volume_updates: Some(true),
        keyboard_updates: Some(true),
        output_device_updates: Some(true),
        system_endpoint_updates: Some(false),
        subscribed_player_paths: Vec::new(),
    }
}

/// Playback command without options.
#[must_use]
pub const fn command(command: Command) -> SendCommandMessage {
    command_with(command, None)
}

const fn command_with(command: Command, options: Option<CommandOptions>) -> SendCommandMessage {
    SendCommandMessage {
        command: Some(command as i32),
        options,
        player_path: None,
    }
}

/// Seeks to `position` seconds.
#[must_use]
pub fn seek(position: f64) -> SendCommandMessage {
    command_with(
        Command::SeekToPlaybackPosition,
        Some(CommandOptions {
            playback_position: Some(position),
            ..Default::default()
        }),
    )
}

/// Skips `seconds` forward, or backward if negative.
#[must_use]
pub fn skip(seconds: f32) -> SendCommandMessage {
    let cmd = if seconds < 0.0 {
        Command::SkipBackward
    } else {
        Command::SkipForward
    };
    command_with(
        cmd,
        Some(CommandOptions {
            skip_interval: Some(seconds.abs()),
            ..Default::default()
        }),
    )
}

/// Sets the playback rate, 1.0 is normal speed.
#[must_use]
pub fn playback_rate(rate: f32) -> SendCommandMessage {
    command_with(
        Command::ChangePlaybackRate,
        Some(CommandOptions {
            playback_rate: Some(rate),
            ..Default::default()
        }),
    )
}

/// Rates the current item. See `NowPlaying::rating_range`.
#[must_use]
pub fn rate_track(rating: f32) -> SendCommandMessage {
    command_with(
        Command::RateTrack,
        Some(CommandOptions {
            rating: Some(rating),
            ..Default::default()
        }),
    )
}

/// Likes the current item, or removes the like if `liked` is false.
#[must_use]
pub fn like_track(liked: bool) -> SendCommandMessage {
    feedback(Command::LikeTrack, liked)
}

/// Dislikes the current item, or removes the dislike if `disliked` is false.
#[must_use]
pub fn dislike_track(disliked: bool) -> SendCommandMessage {
    feedback(Command::DislikeTrack, disliked)
}

fn feedback(command: Command, active: bool) -> SendCommandMessage {
    // `negative` turns the command off.
    let options = (!active).then(|| CommandOptions {
        negative: Some(true),
        ..Default::default()
    });
    command_with(command, options)
}

/// Turns on an audio or subtitle language from `ContentItem::available_language_options`.
#[must_use]
pub fn enable_language(option: &LanguageOption) -> SendCommandMessage {
    language(Command::EnableLanguageOption, option)
}

/// Turns off an audio or subtitle language.
#[must_use]
pub fn disable_language(option: &LanguageOption) -> SendCommandMessage {
    language(Command::DisableLanguageOption, option)
}

/// Turns off subtitles when the current subtitle is not known.
/// The device may show another subtitle instead; prefer [`disable_language`] with the current one.
#[must_use]
pub fn disable_subtitles() -> SendCommandMessage {
    let option = LanguageOption {
        r#type: Some(proto::language_option::Type::Legible as i32),
        ..Default::default()
    };
    language(Command::DisableLanguageOption, &option)
}

fn language(command: Command, option: &LanguageOption) -> SendCommandMessage {
    command_with(
        command,
        Some(CommandOptions {
            language_option: Some(archive_language(option).into()),
            ..Default::default()
        }),
    )
}

/// The device expects the option in an `NSKeyedArchiver` binary plist:
/// a `_MRLanguageOptionProtobuf` object with the protobuf in `data`.
fn archive_language(option: &LanguageOption) -> Vec<u8> {
    use ratv_codec::bplist::Value;

    fn class(names: [&'static str; 3]) -> Value<'static> {
        Value::Dict(vec![
            (
                "$classes".into(),
                Value::Array(names.map(|n| Value::String(n.into())).to_vec()),
            ),
            ("$classname".into(), Value::String(names[0].into())),
        ])
    }
    let objects = vec![
        Value::String("$null".into()),
        Value::Dict(vec![
            ("$class".into(), Value::Uid(4)),
            ("data".into(), Value::Uid(2)),
        ]),
        Value::Dict(vec![
            ("$class".into(), Value::Uid(3)),
            ("NS.data".into(), Value::Data(option.encode_to_vec().into())),
        ]),
        class(["NSMutableData", "NSData", "NSObject"]),
        class(["_MRLanguageOptionProtobuf", "PBCodable", "NSObject"]),
    ];
    Value::Dict(vec![
        ("$version".into(), Value::Int(100_000)),
        ("$archiver".into(), Value::String("NSKeyedArchiver".into())),
        (
            "$top".into(),
            Value::Dict(vec![("root".into(), Value::Uid(1))]),
        ),
        ("$objects".into(), Value::Array(objects)),
    ])
    .encode()
}

/// Plays the queue item `offset` items after the current one. 1 is the next item.
///
/// Music rejects `content_item_id`, so the offset is used.
#[must_use]
pub fn play_queue_offset(offset: i32) -> SendCommandMessage {
    command_with(
        Command::PlayItemInPlaybackQueue,
        Some(CommandOptions {
            playback_queue_offset: Some(offset),
            ..Default::default()
        }),
    )
}

/// Sets the shuffle mode.
#[must_use]
pub fn shuffle(mode: proto::shuffle_mode::Enum) -> SendCommandMessage {
    command_with(
        Command::ChangeShuffleMode,
        Some(CommandOptions {
            shuffle_mode: Some(mode as i32),
            ..Default::default()
        }),
    )
}

/// Sets the repeat mode.
#[must_use]
pub fn repeat(mode: proto::repeat_mode::Enum) -> SendCommandMessage {
    command_with(
        Command::ChangeRepeatMode,
        Some(CommandOptions {
            repeat_mode: Some(mode as i32),
            ..Default::default()
        }),
    )
}

/// Sets the volume, 0.0 to 1.0, of an output device.
///
/// Use [`output_device_uid`](crate::output_device_uid) for the connected device.
#[must_use]
pub fn set_volume(volume: f32, output_device_uid: &str) -> SetVolumeMessage {
    SetVolumeMessage {
        volume: Some(volume.clamp(0.0, 1.0)),
        output_device_uid: Some(output_device_uid.to_owned()),
        details: None,
    }
}

/// Asks for the volume of an output device. The reply is `GetVolumeResultMessage`.
#[must_use]
pub fn get_volume(output_device_uid: &str) -> GetVolumeMessage {
    GetVolumeMessage {
        output_device_uid: Some(output_device_uid.to_owned()),
    }
}

/// Asks how the volume of an output device can be controlled.
/// The reply is `GetVolumeControlCapabilitiesResultMessage`.
#[must_use]
pub fn get_volume_control_capabilities(
    output_device_uid: &str,
) -> GetVolumeControlCapabilitiesMessage {
    GetVolumeControlCapabilitiesMessage {
        output_device_uid: Some(output_device_uid.to_owned()),
    }
}

/// Asks if an output device is muted. The reply is `GetVolumeMutedResultMessage`.
#[must_use]
pub fn get_volume_muted(output_device_uid: &str) -> GetVolumeMutedMessage {
    GetVolumeMutedMessage {
        output_device_uid: Some(output_device_uid.to_owned()),
    }
}

/// Adds AirPlay output devices to the group of the connected device.
#[must_use]
pub fn add_output_devices(uids: &[&str]) -> ModifyOutputContextRequestMessage {
    let mut msg = modify_output_context();
    msg.adding_devices = owned(uids);
    msg.cluster_aware_adding_devices = owned(uids);
    msg
}

/// Removes AirPlay output devices from the group of the connected device.
#[must_use]
pub fn remove_output_devices(uids: &[&str]) -> ModifyOutputContextRequestMessage {
    let mut msg = modify_output_context();
    msg.removing_devices = owned(uids);
    msg.cluster_aware_removing_devices = owned(uids);
    msg
}

/// Replaces the output devices of the connected device.
#[must_use]
pub fn set_output_devices(uids: &[&str]) -> ModifyOutputContextRequestMessage {
    let mut msg = modify_output_context();
    msg.setting_devices = owned(uids);
    msg.cluster_aware_setting_devices = owned(uids);
    msg
}

fn modify_output_context() -> ModifyOutputContextRequestMessage {
    ModifyOutputContextRequestMessage {
        r#type: Some(
            proto::modify_output_context_request_message::Type::SharedAudioPresentation as i32,
        ),
        ..Default::default()
    }
}

fn owned(uids: &[&str]) -> Vec<String> {
    uids.iter().map(|&u| u.to_owned()).collect()
}

/// Mutes or unmutes an output device.
#[must_use]
pub fn set_volume_muted(muted: bool, output_device_uid: &str) -> SetVolumeMutedMessage {
    SetVolumeMutedMessage {
        is_muted: Some(muted),
        output_device_uid: Some(output_device_uid.to_owned()),
        details: None,
    }
}

/// Changes the text of the focused text field.
#[must_use]
pub fn text_input(text: &str, action: proto::action_type::Enum) -> TextInputMessage {
    TextInputMessage {
        timestamp: None,
        text: Some(text.to_owned()),
        action_type: Some(action as i32),
    }
}

/// Requests metadata and artwork of the current item.
#[must_use]
pub fn playback_queue_request(
    artwork_width: f64,
    artwork_height: f64,
) -> PlaybackQueueRequestMessage {
    PlaybackQueueRequestMessage {
        location: Some(0),
        length: Some(1),
        include_metadata: Some(true),
        artwork_width: Some(artwork_width),
        artwork_height: Some(artwork_height),
        return_content_item_assets_in_user_completion: Some(true),
        ..Default::default()
    }
}

/// Sample rate of voice input.
pub const VOICE_SAMPLE_RATE: u32 = 16_000;
/// Samples in one voice input packet (20 ms).
pub const VOICE_FRAMES: u32 = 320;

/// Voice input format: Opus, 16 kHz, mono, as `AVAudioFormat` settings in a binary plist.
/// The keys are the ones Apple's Remote app sends.
static VOICE_FORMAT: LazyLock<Bytes> = LazyLock::new(|| {
    use ratv_codec::bplist::Value;

    // `opus` as a four character code.
    const OPUS: i64 = 0x6f70_7573;
    Value::Dict(vec![
        (
            "AVSampleRateKey".into(),
            Value::Real(f64::from(VOICE_SAMPLE_RATE)),
        ),
        ("AVFormatIDKey".into(), Value::Int(OPUS)),
        ("AVNumberOfChannelsKey".into(), Value::Int(1)),
    ])
    .encode()
    .into()
});

fn voice_format() -> AudioFormatSettings {
    AudioFormatSettings {
        format_settings_plist_data: Some(VOICE_FORMAT.clone()),
    }
}

/// Registers a voice input device for Siri. The reply is
/// `RegisterVoiceInputDeviceResponseMessage`.
///
/// While [`Button::Siri`](crate::hid::Button::Siri) is held, the device sends
/// `SetRecordingStateMessage` with `Recording`; then send [`voice_input`] packets.
#[must_use]
pub fn register_voice_input() -> RegisterVoiceInputDeviceMessage {
    RegisterVoiceInputDeviceMessage {
        device_descriptor: Some(VoiceInputDeviceDescriptor {
            supported_formats: vec![voice_format()],
            default_format: Some(voice_format()),
        }),
    }
}

/// One Opus packet of 20 ms that starts at sample `start`.
#[must_use]
pub fn voice_input(packet: Bytes, start: u32) -> SendVoiceInputMessage {
    let len = u32::try_from(packet.len()).unwrap_or(u32::MAX);
    SendVoiceInputMessage {
        data_block: Some(AudioDataBlock {
            buffer: Some(AudioBuffer {
                format_settings: Some(voice_format()),
                packet_capacity: Some(1),
                maximum_packet_size: Some(len.into()),
                packet_count: Some(1),
                contents: Some(packet),
                packet_descriptions: vec![AudioStreamPacketDescription {
                    start_offset: Some(0),
                    variable_frames_in_packet: Some(0),
                    data_byte_size: Some(len),
                }],
            }),
            time: Some(AudioTime {
                timestamp: Some(f64::from(start)),
                sample_rate: Some(f64::from(VOICE_SAMPLE_RATE)),
            }),
            gain: Some(1.0),
        }),
    }
}

/// Requests the current item and up to `length - 1` next items, with
/// language options, chapters and description. No artwork.
#[must_use]
pub fn playback_queue_details(length: i32) -> PlaybackQueueRequestMessage {
    PlaybackQueueRequestMessage {
        location: Some(0),
        length: Some(length),
        include_metadata: Some(true),
        include_sections: Some(true),
        include_info: Some(true),
        include_language_options: Some(true),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders() {
        let info = device_info(&ClientInfo {
            unique_identifier: "id",
            name: "ratv",
            system_build_version: "18M60",
        });
        assert_eq!(info.name, "ratv");
        assert_eq!(info.device_class, Some(1));
        assert_eq!(connected().state, Some(2));
        assert_eq!(client_updates_config().system_endpoint_updates, Some(false));
        assert_eq!(command(Command::Play).command, Some(1));

        let r = playback_rate(1.5);
        assert_eq!(r.command, Some(Command::ChangePlaybackRate as i32));
        assert_eq!(r.options.unwrap().playback_rate, Some(1.5));
        let r = rate_track(4.0);
        assert_eq!(r.command, Some(Command::RateTrack as i32));
        assert_eq!(r.options.unwrap().rating, Some(4.0));
        assert_eq!(like_track(true).options, None);
        let l = like_track(false);
        assert_eq!(l.command, Some(Command::LikeTrack as i32));
        assert_eq!(l.options.unwrap().negative, Some(true));
        assert_eq!(
            dislike_track(false).command,
            Some(Command::DislikeTrack as i32)
        );
        let s = seek(12.5);
        assert_eq!(s.command, Some(45));
        assert_eq!(s.options.unwrap().playback_position, Some(12.5));

        let s = skip(-10.0);
        assert_eq!(s.command, Some(19));
        assert_eq!(s.options.unwrap().skip_interval, Some(10.0));
        assert_eq!(skip(5.0).command, Some(18));

        assert_eq!(
            shuffle(proto::shuffle_mode::Enum::Songs)
                .options
                .unwrap()
                .shuffle_mode,
            Some(3)
        );
        assert_eq!(
            repeat(proto::repeat_mode::Enum::All)
                .options
                .unwrap()
                .repeat_mode,
            Some(3)
        );
        assert_eq!(set_volume(1.5, "uid").volume, Some(1.0));
        assert_eq!(get_volume("uid").output_device_uid.as_deref(), Some("uid"));
        assert_eq!(
            get_volume_muted("uid").output_device_uid.as_deref(),
            Some("uid")
        );
        assert_eq!(set_volume_muted(true, "uid").is_muted, Some(true));
        let add = add_output_devices(&["a", "b"]);
        assert_eq!(add.adding_devices, ["a", "b"]);
        assert_eq!(add.cluster_aware_adding_devices, ["a", "b"]);
        assert_eq!(add.r#type, Some(1));
        assert_eq!(remove_output_devices(&["a"]).removing_devices, ["a"]);
        assert_eq!(
            set_output_devices(&["a"]).cluster_aware_setting_devices,
            ["a"]
        );
        assert_eq!(
            text_input("hi", proto::action_type::Enum::Set).action_type,
            Some(2)
        );
        assert_eq!(playback_queue_request(400.0, 400.0).length, Some(1));
        let details = playback_queue_details(10);
        assert_eq!(details.length, Some(10));
        assert_eq!(details.include_language_options, Some(true));
        assert_eq!(details.artwork_width, None);

        let option = LanguageOption {
            language_tag: Some("sv".into()),
            ..Default::default()
        };
        let enable = enable_language(&option);
        assert_eq!(enable.command, Some(Command::EnableLanguageOption as i32));
        let bytes = enable.options.unwrap().language_option.unwrap();
        let archive = ratv_codec::bplist::decode(&bytes).unwrap();
        let objects = archive.get("$objects").unwrap().as_array().unwrap();
        let data = objects[2].get("NS.data").unwrap().as_bytes().unwrap();
        assert_eq!(LanguageOption::decode(data).unwrap(), option);
        let class = objects[4].get("$classname").unwrap().as_str();
        assert_eq!(class, Some("_MRLanguageOptionProtobuf"));
        assert_eq!(
            disable_language(&option).command,
            Some(Command::DisableLanguageOption as i32)
        );
        let off = disable_subtitles();
        let bytes = off.options.unwrap().language_option.unwrap();
        let archive = ratv_codec::bplist::decode(&bytes).unwrap();
        let objects = archive.get("$objects").unwrap().as_array().unwrap();
        let data = objects[2].get("NS.data").unwrap().as_bytes().unwrap();
        assert_eq!(LanguageOption::decode(data).unwrap().r#type, Some(1));
        let play = play_queue_offset(2);
        assert_eq!(play.command, Some(Command::PlayItemInPlaybackQueue as i32));
        assert_eq!(play.options.unwrap().playback_queue_offset, Some(2));
    }

    #[test]
    fn voice() {
        let reg = register_voice_input();
        let format = reg.device_descriptor.unwrap().default_format.unwrap();
        let plist = format.format_settings_plist_data.unwrap();
        let plist = ratv_codec::bplist::decode(&plist).unwrap();
        assert_eq!(
            plist.get("AVFormatIDKey").unwrap().as_i64(),
            Some(0x6f70_7573)
        );
        let voice = voice_input(Bytes::from_static(b"opus"), 640);
        let block = voice.data_block.unwrap();
        assert_eq!(block.time.unwrap().timestamp, Some(640.0));
        let buffer = block.buffer.unwrap();
        assert_eq!(buffer.contents.as_deref(), Some(&b"opus"[..]));
        assert_eq!(buffer.packet_descriptions[0].data_byte_size, Some(4));
    }
}
