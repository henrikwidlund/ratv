//! Companion messages.
//!
//! Every message is an OPACK dictionary:
//! `_i` identifier, `_x` transaction id, `_t` [`MessageType`], `_c` content.
//! Responses use the `_x` of their request. Errors use `_ec`, `_ed` and `_em`.

use ratv_codec::bplist;
use ratv_codec::opack::{self, Encoder, Value};

use crate::Error;

/// Service type used for remote control sessions.
pub const SERVICE_TV_REMOTE: &str = "com.apple.tvremoteservices";

/// Value of the `_t` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Event; no response expected.
    Event = 1,
    /// Request; device sends a [`MessageType::Response`].
    Request = 2,
    /// Response to a request.
    Response = 3,
}

/// A message that can be sent to the device.
pub trait Outgoing {
    /// Message identifier (`_i`).
    const ID: &'static str;
    /// Message type.
    const TYPE: MessageType = MessageType::Request;
    /// Writes the `_c` value.
    fn content(&self, e: &mut Encoder<'_>);

    /// Number of fields that [`header`](Self::header) writes.
    fn header_len(&self) -> usize {
        0
    }

    /// Writes more fields next to `_i`, `_x`, `_t` and `_c`.
    fn header(&self, _e: &mut Encoder<'_>) {}
}

/// Encodes `msg` with transaction id `xid` and appends it to `out`.
pub fn encode<M: Outgoing>(msg: &M, xid: u32, out: &mut Vec<u8>) {
    Encoder::new(out).dict(4 + msg.header_len(), |e| {
        e.str("_i").str(M::ID);
        e.str("_x").int(xid.into());
        e.str("_t").int(M::TYPE as i64);
        e.str("_c");
        msg.content(e);
        msg.header(e);
    });
}

fn empty(e: &mut Encoder<'_>) {
    e.dict(0, |_| {});
}

/// A received message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message<'a>(Value<'a>);

impl<'a> Message<'a> {
    /// Decodes a frame payload.
    pub fn decode(payload: &'a [u8]) -> Result<Self, Error> {
        let value = opack::decode(payload)?;
        if value.as_dict().is_none() {
            return Err(Error::InvalidField("_i"));
        }
        Ok(Self(value))
    }

    /// Message identifier (`_i`).
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.0.get("_i").and_then(Value::as_str)
    }

    /// Message type (`_t`).
    #[must_use]
    pub fn kind(&self) -> Option<MessageType> {
        match self.0.get("_t").and_then(Value::as_i64)? {
            1 => Some(MessageType::Event),
            2 => Some(MessageType::Request),
            3 => Some(MessageType::Response),
            _ => None,
        }
    }

    /// Transaction id (`_x`).
    #[must_use]
    pub fn xid(&self) -> Option<u32> {
        self.0
            .get("_x")
            .and_then(Value::as_i64)
            .and_then(|x| u32::try_from(x).ok())
    }

    /// Content (`_c`).
    #[must_use]
    pub fn content(&self) -> Option<&Value<'a>> {
        self.0.get("_c")
    }

    /// Field of the content dictionary.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value<'a>> {
        self.content()?.get(key)
    }

    /// Whole message value.
    #[must_use]
    pub const fn value(&self) -> &Value<'a> {
        &self.0
    }

    /// Returns the device error carried by this message, if any.
    #[must_use]
    pub fn error(&self) -> Option<Error> {
        let code = self.0.get("_ec").and_then(Value::as_i64);
        let message = self.0.get("_em").and_then(Value::as_str);
        if code.is_none() && message.is_none() {
            return None;
        }
        Some(Error::Device {
            code: code.unwrap_or(0),
            domain: self
                .0
                .get("_ed")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            message: message.unwrap_or_default().to_owned(),
        })
    }

    /// Returns `Err` if the message carries a device error.
    pub fn into_result(self) -> Result<Self, Error> {
        self.error().map_or(Ok(self), Err)
    }

    /// Copies borrowed data so the message outlives the payload.
    #[must_use]
    pub fn into_owned(self) -> Message<'static> {
        Message(self.0.into_owned())
    }
}

/// Requests device information. Must be sent before [`SessionStart`].
#[derive(Debug, Clone, Copy)]
pub struct SystemInfo<'a> {
    /// Our display name.
    pub name: &'a str,
    /// Our model, for example `iPhone10,6`.
    pub model: &'a str,
    /// Our pairing identifier.
    pub client_id: &'a [u8],
    /// Our Bluetooth-style address, `AA:BB:CC:DD:EE:FF`.
    pub rp_id: &'a str,
    /// Our public id, same format as `rp_id`.
    pub pub_id: &'a str,
}

impl Outgoing for SystemInfo<'_> {
    const ID: &'static str = "_systemInfo";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(10, |e| {
            e.str("_bf").int(0);
            e.str("_cf").int(512);
            e.str("_clFl").int(128);
            e.str("_i").str(self.rp_id);
            e.str("_idsID").data(self.client_id);
            e.str("_pubID").str(self.pub_id);
            e.str("_sf").int(256);
            e.str("_sv").str("170.18");
            e.str("model").str(self.model);
            e.str("name").str(self.name);
        });
    }
}

/// Starts a remote control session. The response has the device session id in `_sid`.
#[derive(Debug, Clone, Copy)]
pub struct SessionStart {
    /// Our random session id.
    pub session_id: u32,
}

impl Outgoing for SessionStart {
    const ID: &str = "_sessionStart";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(2, |e| {
            e.str("_srvT").str(SERVICE_TV_REMOTE);
            e.str("_sid").int(self.session_id.into());
        });
    }
}

/// Reads the device session id from a [`SessionStart`] response.
pub fn session_start_response(msg: &Message<'_>) -> Result<u32, Error> {
    msg.field("_sid")
        .and_then(Value::as_i64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(Error::InvalidField("_sid"))
}

/// Combined session id: `device << 32 | ours`.
#[must_use]
pub fn combined_session_id(device: u32, ours: u32) -> u64 {
    (u64::from(device) << 32) | u64::from(ours)
}

/// Stops a session.
#[derive(Debug, Clone, Copy)]
pub struct SessionStop {
    /// Combined session id, see [`combined_session_id`].
    pub session_id: u64,
}

impl Outgoing for SessionStop {
    const ID: &str = "_sessionStop";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(2, |e| {
            e.str("_srvT").str(SERVICE_TV_REMOTE);
            e.str("_sid").int(self.session_id.cast_signed());
        });
    }
}

/// Registers a TV Remote session. tvOS 26 answers `Fetch*` requests only after it.
#[derive(Debug, Clone, Copy)]
pub struct TvRemoteSessionStart;

impl Outgoing for TvRemoteSessionStart {
    const ID: &str = "TVRCSessionStart";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(1, |e| {
            e.str("ProtocolVersionKey").str("1.2");
        });
    }
}

/// HID button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[allow(missing_docs)]
pub enum HidCommand {
    Up = 1,
    Down = 2,
    Left = 3,
    Right = 4,
    Menu = 5,
    Select = 6,
    Home = 7,
    VolumeUp = 8,
    VolumeDown = 9,
    Siri = 10,
    Screensaver = 11,
    Sleep = 12,
    Wake = 13,
    PlayPause = 14,
    ChannelIncrement = 15,
    ChannelDecrement = 16,
    Guide = 17,
    PageUp = 18,
    PageDown = 19,
}

/// Button state for [`Hid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ButtonState {
    /// Pressed.
    Down = 1,
    /// Released.
    Up = 2,
}

/// Presses or releases a button. A click is `Down` followed by `Up`.
#[derive(Debug, Clone, Copy)]
pub struct Hid {
    /// Button.
    pub command: HidCommand,
    /// New state.
    pub state: ButtonState,
}

impl Outgoing for Hid {
    const ID: &str = "_hidC";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(2, |e| {
            e.str("_hBtS").int(self.state as i64);
            e.str("_hidC").int(self.command as i64);
        });
    }
}

/// Media control command (`_mcc`).
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(missing_docs)]
pub enum MediaControl {
    Play,
    Pause,
    NextTrack,
    PreviousTrack,
    /// Response has the volume in `_vol`, see [`volume_response`].
    GetVolume,
    /// Volume from 0.0 to 1.0.
    SetVolume(f64),
    /// Skip by seconds; negative skips backwards.
    SkipBy(f64),
    FastForwardBegin,
    FastForwardEnd,
    RewindBegin,
    RewindEnd,
    GetCaptionSettings,
}

impl MediaControl {
    const fn code(self) -> i64 {
        match self {
            Self::Play => 1,
            Self::Pause => 2,
            Self::NextTrack => 3,
            Self::PreviousTrack => 4,
            Self::GetVolume => 5,
            Self::SetVolume(_) => 6,
            Self::SkipBy(_) => 7,
            Self::FastForwardBegin => 8,
            Self::FastForwardEnd => 9,
            Self::RewindBegin => 10,
            Self::RewindEnd => 11,
            Self::GetCaptionSettings => 12,
        }
    }
}

impl Outgoing for MediaControl {
    const ID: &str = "_mcc";

    fn content(&self, e: &mut Encoder<'_>) {
        let arg = match *self {
            Self::SetVolume(v) => Some(("_vol", v)),
            Self::SkipBy(s) => Some(("_skpS", s)),
            _ => None,
        };
        e.dict(1 + usize::from(arg.is_some()), |e| {
            e.str("_mcc").int(self.code());
            if let Some((k, v)) = arg {
                e.str(k).float(v);
            }
        });
    }
}

/// Reads the volume (0.0 to 1.0) from a [`MediaControl::GetVolume`] response.
pub fn volume_response(msg: &Message<'_>) -> Result<f64, Error> {
    msg.field("_vol")
        .and_then(Value::as_f64)
        .ok_or(Error::InvalidField("_vol"))
}

/// Supported media controls from the `_iMC` event (`_mcF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaControlFlags(pub u32);

#[allow(missing_docs)]
impl MediaControlFlags {
    pub const PLAY: u32 = 0x0001;
    pub const PAUSE: u32 = 0x0002;
    pub const NEXT_TRACK: u32 = 0x0004;
    pub const PREVIOUS_TRACK: u32 = 0x0008;
    pub const FAST_FORWARD: u32 = 0x0010;
    pub const REWIND: u32 = 0x0020;
    pub const VOLUME: u32 = 0x0100;
    pub const SKIP_FORWARD: u32 = 0x0200;
    pub const SKIP_BACKWARD: u32 = 0x0400;

    /// Returns true if all bits in `flag` are set.
    #[must_use]
    pub const fn contains(self, flag: u32) -> bool {
        self.0 & flag == flag
    }
}

/// Launches an app by bundle id or opens a URL.
#[derive(Debug, Clone, Copy)]
pub enum LaunchApp<'a> {
    /// Bundle id, for example `com.netflix.Netflix`.
    BundleId(&'a str),
    /// URL handled by an installed app.
    Url(&'a str),
}

impl Outgoing for LaunchApp<'_> {
    const ID: &'static str = "_launchApp";

    fn content(&self, e: &mut Encoder<'_>) {
        let (k, v) = match *self {
            Self::BundleId(id) => ("_bundleID", id),
            Self::Url(url) => ("_urlS", url),
        };
        e.dict(1, |e| {
            e.str(k).str(v);
        });
    }
}

/// Requests the list of launchable apps. See [`apps_response`].
#[derive(Debug, Clone, Copy)]
pub struct FetchApps;

impl Outgoing for FetchApps {
    const ID: &str = "FetchLaunchableApplicationsEvent";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// An installed app.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct App {
    /// Bundle id.
    pub bundle_id: String,
    /// Display name.
    pub name: String,
}

/// Reads the app list from a [`FetchApps`] response.
pub fn apps_response(msg: &Message<'_>) -> Result<Vec<App>, Error> {
    Ok(string_pairs(msg)?
        .map(|(id, name)| App {
            bundle_id: id.to_owned(),
            name: name.to_owned(),
        })
        .collect())
}

/// Requests the user accounts. See [`accounts_response`].
#[derive(Debug, Clone, Copy)]
pub struct FetchAccounts;

impl Outgoing for FetchAccounts {
    const ID: &str = "FetchUserAccountsEvent";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Switches to the user account with this id.
#[derive(Debug, Clone, Copy)]
pub struct SwitchAccount<'a>(pub &'a str);

impl Outgoing for SwitchAccount<'_> {
    const ID: &'static str = "SwitchUserAccountEvent";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(1, |e| {
            e.str("SwitchAccountID").str(self.0);
        });
    }
}

/// A user account on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Account {
    /// Id for [`SwitchAccount`].
    pub id: String,
    /// Display name.
    pub name: String,
}

/// Reads the account list from a [`FetchAccounts`] response.
pub fn accounts_response(msg: &Message<'_>) -> Result<Vec<Account>, Error> {
    Ok(string_pairs(msg)?
        .map(|(id, name)| Account {
            id: id.to_owned(),
            name: name.to_owned(),
        })
        .collect())
}

/// String keys and values of the response content. Other entries are skipped.
fn string_pairs<'m>(
    msg: &'m Message<'_>,
) -> Result<impl Iterator<Item = (&'m str, &'m str)>, Error> {
    let entries = msg
        .content()
        .and_then(Value::as_dict)
        .ok_or(Error::InvalidField("_c"))?;
    Ok(entries
        .iter()
        .filter_map(|(k, v)| Some((k.as_str()?, v.as_str()?))))
}

/// Requests the power state. See [`attention_response`].
#[derive(Debug, Clone, Copy)]
pub struct FetchAttentionState;

impl Outgoing for FetchAttentionState {
    const ID: &str = "FetchAttentionState";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Device power/attention state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
#[non_exhaustive]
pub enum AttentionState {
    Unknown,
    Asleep,
    Screensaver,
    Awake,
    Idle,
}

impl AttentionState {
    const fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Asleep,
            2 => Self::Screensaver,
            3 => Self::Awake,
            4 => Self::Idle,
            _ => Self::Unknown,
        }
    }

    /// Returns true unless the device is asleep or the state is unknown.
    #[must_use]
    pub const fn is_on(self) -> bool {
        matches!(self, Self::Screensaver | Self::Awake | Self::Idle)
    }
}

/// Reads the state from a [`FetchAttentionState`] response.
pub fn attention_response(msg: &Message<'_>) -> Result<AttentionState, Error> {
    msg.field("state")
        .and_then(Value::as_i64)
        .map(AttentionState::from_code)
        .ok_or(Error::InvalidField("state"))
}

/// Reads from a [`SystemInfo`] response if Siri is on in the device settings.
/// `None` if the response does not say.
#[must_use]
pub fn siri_enabled(msg: &Message<'_>) -> Option<bool> {
    siri_info_enabled(msg.field("_siriInfo")?)
}

/// Reads if Siri is on from Siri info, as in `_siriInfo` or [`event::SIRI_STATUS`].
fn siri_info_enabled(info: &Value<'_>) -> Option<bool> {
    info.get("deviceCapabilitiesV2")?
        .as_array()?
        .iter()
        .filter_map(Value::as_bytes)
        .find_map(assistant_enabled)
}

/// Reads `AssistantEnabledCapability` from one archived Siri capability.
fn assistant_enabled(data: &[u8]) -> Option<bool> {
    let archive = bplist::decode(data).ok()?;
    let objects = archive.get("$objects")?.as_array()?;
    let resolve = |v: &bplist::Value<'_>| match *v {
        bplist::Value::Uid(i) => objects.get(usize::try_from(i).ok()?),
        _ => None,
    };
    let root = resolve(archive.get("$top")?.get("root")?)?;
    let backing = resolve(root.get("SVDAssistantEnabledCapabilityBacking")?)?;
    let status = resolve(backing.get("primitivesMap_AssistantEnabledCapability::supportStatus")?)?;
    match status.get("supportStatus")?.as_i64()? {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Makes the device send [`event::SUPPORTED_ACTIONS`] now. The response is empty.
#[derive(Debug, Clone, Copy)]
pub struct FetchSupportedActions;

impl Outgoing for FetchSupportedActions {
    const ID: &str = "FetchSupportedActionsEvent";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Event names for [`Interest`].
#[allow(missing_docs)]
pub mod event {
    pub const MEDIA_CONTROL: &str = "_iMC";
    pub const SYSTEM_STATUS: &str = "SystemStatus";
    pub const TV_SYSTEM_STATUS: &str = "TVSystemStatus";
    pub const TEXT_INPUT_STARTED: &str = "_tiStarted";
    pub const TEXT_INPUT_STOPPED: &str = "_tiStopped";
    /// Sent after `TVRCSessionStart` without registering interest.
    pub const SUPPORTED_ACTIONS: &str = "SupportedActions";
    /// What the device plays, see [`now_playing`](crate::now_playing).
    pub const NOW_PLAYING_INFO: &str = "NowPlayingInfo";
    /// Siri info, as in the `_systemInfo` response. Sent without registering interest.
    pub const SIRI_STATUS: &str = "SiriStatus";
    /// Siri stopped recording. Sent as a request; reply with [`SiriEndpointReply`](super::SiriEndpointReply).
    pub const SIRI_ENDPOINT: &str = "_siriEndpoint";
}

/// Registers or removes interest in events. This is an event, not a request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Interest<'a> {
    /// Events to receive.
    pub register: &'a [&'a str],
    /// Events to stop receiving.
    pub deregister: &'a [&'a str],
}

impl Outgoing for Interest<'_> {
    const ID: &'static str = "_interest";
    const TYPE: MessageType = MessageType::Event;

    fn content(&self, e: &mut Encoder<'_>) {
        let lists = [
            ("_regEvents", self.register),
            ("_deregEvents", self.deregister),
        ];
        let count = lists.iter().filter(|(_, l)| !l.is_empty()).count();
        e.dict(count, |e| {
            for (k, list) in lists.into_iter().filter(|(_, l)| !l.is_empty()) {
                e.str(k).array(list.len(), |e| {
                    for name in list {
                        e.str(name);
                    }
                });
            }
        });
    }
}

/// Starts a touch session. Coordinates for [`Touch`] range from 0 to `width`/`height`.
#[derive(Debug, Clone, Copy)]
pub struct TouchStart {
    /// Virtual touch area width.
    pub width: f64,
    /// Virtual touch area height.
    pub height: f64,
}

impl Default for TouchStart {
    fn default() -> Self {
        Self {
            width: 1000.0,
            height: 1000.0,
        }
    }
}

impl Outgoing for TouchStart {
    const ID: &str = "_touchStart";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(3, |e| {
            e.str("_height").float(self.height);
            e.str("_tFl").int(0);
            e.str("_width").float(self.width);
        });
    }
}

/// Ends a touch session.
#[derive(Debug, Clone, Copy)]
pub struct TouchStop;

impl Outgoing for TouchStop {
    const ID: &str = "_touchStop";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Touch phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(missing_docs)]
pub enum TouchPhase {
    Press = 1,
    Hold = 3,
    Release = 4,
    Click = 5,
}

/// A touch event. This is an event, not a request.
#[derive(Debug, Clone, Copy)]
pub struct Touch {
    /// Time in nanoseconds since the touch session started.
    pub timestamp_ns: u64,
    /// Finger index.
    pub finger: u8,
    /// X coordinate. Sent as an integer, like pyatv.
    pub x: u16,
    /// Y coordinate.
    pub y: u16,
    /// Phase.
    pub phase: TouchPhase,
}

impl Outgoing for Touch {
    const ID: &str = "_hidT";
    const TYPE: MessageType = MessageType::Event;

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(5, |e| {
            e.str("_ns").int(self.timestamp_ns.cast_signed());
            e.str("_tFg").int(self.finger.into());
            e.str("_cx").int(self.x.into());
            e.str("_tPh").int(self.phase as i64);
            e.str("_cy").int(self.y.into());
        });
    }
}

/// Subscribes to keyboard focus changes. The response tells if a text field is focused.
#[derive(Debug, Clone, Copy)]
pub struct TextInputStart;

impl Outgoing for TextInputStart {
    const ID: &str = "_tiStart";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Ends the keyboard subscription.
#[derive(Debug, Clone, Copy)]
pub struct TextInputStop;

impl Outgoing for TextInputStop {
    const ID: &str = "_tiStop";

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Starts Siri, like holding the Siri button. Then send [`SiriAudio`]
/// events and [`SiriStop`]. The response is empty.
#[derive(Debug, Clone, Copy, Default)]
pub struct SiriStart {
    /// Also send `_isVT = 0` and `_pttEligible = 1`.
    /// The iPhone gives these as options; it is not known if they are sent.
    pub ptt_flags: bool,
}

impl Outgoing for SiriStart {
    const ID: &str = "_siriStart";

    fn content(&self, e: &mut Encoder<'_>) {
        siri_content(e);
    }

    fn header_len(&self) -> usize {
        siri_header_len(self.ptt_flags)
    }

    fn header(&self, e: &mut Encoder<'_>) {
        siri_header(self.ptt_flags, e);
    }
}

/// Ends Siri, like releasing the Siri button. Siri then answers.
/// The response is empty.
#[derive(Debug, Clone, Copy, Default)]
pub struct SiriStop {
    /// See [`SiriStart::ptt_flags`].
    pub ptt_flags: bool,
}

impl Outgoing for SiriStop {
    const ID: &str = "_siriStop";

    fn content(&self, e: &mut Encoder<'_>) {
        siri_content(e);
    }

    fn header_len(&self) -> usize {
        siri_header_len(self.ptt_flags)
    }

    fn header(&self, e: &mut Encoder<'_>) {
        siri_header(self.ptt_flags, e);
    }
}

/// Content of [`SiriStart`] and [`SiriStop`]: no voice trigger ("Hey Siri").
fn siri_content(e: &mut Encoder<'_>) {
    e.dict(2, |e| {
        e.str("_vtDurMs").int(0);
        e.str("_vtTwoShotSec").int(0);
    });
}

const fn siri_header_len(ptt_flags: bool) -> usize {
    if ptt_flags { 2 } else { 0 }
}

/// Not a voice trigger, push to talk.
fn siri_header(ptt_flags: bool, e: &mut Encoder<'_>) {
    if ptt_flags {
        e.str("_isVT").int(0);
        e.str("_pttEligible").int(1);
    }
}

/// Opus packets for Siri: 16 kHz, mono, 20 ms each. This is an event.
///
/// The iPhone sends 5 packets (100 ms) in each event.
#[derive(Debug, Clone, Copy)]
pub struct SiriAudio<'a> {
    /// The packets, one after the other.
    pub data: &'a [u8],
    /// Size of each packet in `data`, in order. The sizes add up to `data.len()`.
    pub sizes: &'a [u16],
}

impl Outgoing for SiriAudio<'_> {
    const ID: &str = "_siA";
    const TYPE: MessageType = MessageType::Event;

    fn content(&self, e: &mut Encoder<'_>) {
        debug_assert_eq!(
            self.sizes.iter().map(|&s| usize::from(s)).sum::<usize>(),
            self.data.len()
        );
        e.dict(3, |e| {
            e.int(3).data(self.data);
            // Linear gain.
            e.int(4).float(1.0);
            e.int(5).array(self.sizes.len(), |e| {
                let mut offset = 0;
                for &size in self.sizes {
                    e.dict(2, |e| {
                        e.int(1).int(size.into());
                        e.int(2).int(offset);
                    });
                    offset += i64::from(size);
                }
            });
        });
    }
}

/// Empty reply to [`Event::SiriEndpoint`]. Use the xid of the request.
#[derive(Debug, Clone, Copy, Default)]
pub struct SiriEndpointReply;

impl Outgoing for SiriEndpointReply {
    const ID: &'static str = event::SIRI_ENDPOINT;
    const TYPE: MessageType = MessageType::Response;

    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// Known events from the device.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// Supported media controls changed.
    MediaControl(MediaControlFlags),
    /// Power state changed.
    SystemStatus(AttentionState),
    /// A text field got focus.
    TextInputStarted,
    /// Text field lost focus.
    TextInputStopped,
    /// The screen in front changed, for example to the Home Screen or to a video player.
    /// Not sent for pause or for moving in menus. Does not tell which app is in front.
    ScreenChanged {
        /// True if the Guide button does something on the new screen.
        guide_supported: bool,
    },
    /// Siri was turned on or off, or the user account changed.
    SiriStatus {
        /// True if Siri is on in the device settings.
        enabled: bool,
    },
    /// Siri stopped recording. The iPhone then stops sending audio.
    /// Sent as a request; reply with [`SiriEndpointReply`].
    SiriEndpoint,
}

impl Event {
    /// Parses a known event. Returns `None` for other messages.
    #[must_use]
    pub fn parse(msg: &Message<'_>) -> Option<Self> {
        if msg.kind() == Some(MessageType::Request) && msg.id() == Some(event::SIRI_ENDPOINT) {
            return Some(Self::SiriEndpoint);
        }
        if msg.kind() != Some(MessageType::Event) {
            return None;
        }
        match msg.id()? {
            event::MEDIA_CONTROL => {
                let flags = msg.field("_mcF").and_then(Value::as_u64).unwrap_or(0);
                Some(Self::MediaControl(MediaControlFlags(flags as u32)))
            }
            event::SYSTEM_STATUS | event::TV_SYSTEM_STATUS => Some(Self::SystemStatus(
                AttentionState::from_code(msg.field("state").and_then(Value::as_i64).unwrap_or(0)),
            )),
            event::SIRI_STATUS => Some(Self::SiriStatus {
                enabled: siri_info_enabled(msg.content()?)?,
            }),
            event::TEXT_INPUT_STARTED => Some(Self::TextInputStarted),
            event::TEXT_INPUT_STOPPED => Some(Self::TextInputStopped),
            event::SUPPORTED_ACTIONS => Some(Self::ScreenChanged {
                guide_supported: msg
                    .field("GuideSupportedKey")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<M: Outgoing>(msg: &M) -> Message<'static> {
        let mut out = Vec::new();
        encode(msg, 7, &mut out);
        let m = Message::decode(&out).unwrap().into_owned();
        assert_eq!(m.id(), Some(M::ID));
        assert_eq!(m.xid(), Some(7));
        assert_eq!(m.kind(), Some(M::TYPE));
        m
    }

    fn int(m: &Message<'_>, k: &str) -> Option<i64> {
        m.field(k).and_then(Value::as_i64)
    }

    fn float(m: &Message<'_>, k: &str) -> Option<f64> {
        m.field(k).and_then(Value::as_f64)
    }

    fn string<'m>(m: &'m Message<'_>, k: &str) -> Option<&'m str> {
        m.field(k).and_then(Value::as_str)
    }

    #[test]
    fn system_info_matches_pyatv_layout() {
        let m = roundtrip(&SystemInfo {
            name: "BunATV Remote",
            model: "iPhone10,6",
            client_id: b"4874b16b-4b77-4998-b9d5-091ef8cf486a",
            rp_id: "E1:48:1F:71:F7:8E",
            pub_id: "02:E3:A0:D9:D3:40",
        });
        assert_eq!(int(&m, "_cf"), Some(512));
        assert_eq!(string(&m, "_i"), Some("E1:48:1F:71:F7:8E"));
        assert_eq!(
            m.field("_idsID").and_then(Value::as_bytes),
            Some(&b"4874b16b-4b77-4998-b9d5-091ef8cf486a"[..])
        );
        assert_eq!(string(&m, "name"), Some("BunATV Remote"));
    }

    #[test]
    fn siri_messages() {
        for m in [
            roundtrip(&SiriStart::default()),
            roundtrip(&SiriStop::default()),
        ] {
            assert_eq!(int(&m, "_vtDurMs"), Some(0));
            assert_eq!(int(&m, "_vtTwoShotSec"), Some(0));
            assert_eq!(m.value().get("_pttEligible"), None);
        }
        let flags = |m: Message<'_>| {
            let v = m.value();
            (
                v.get("_isVT").and_then(Value::as_i64),
                v.get("_pttEligible").and_then(Value::as_i64),
            )
        };
        assert_eq!(
            flags(roundtrip(&SiriStart { ptt_flags: true })),
            (Some(0), Some(1))
        );
        assert_eq!(
            flags(roundtrip(&SiriStop { ptt_flags: true })),
            (Some(0), Some(1))
        );
        let m = roundtrip(&SiriAudio {
            data: &[1, 2, 3, 4, 5],
            sizes: &[3, 2],
        });
        // Integer keys, as the iPhone sends them.
        let field = |v: &Value<'static>, k: i64| {
            v.as_dict()
                .and_then(|d| d.iter().find(|(key, _)| key.as_i64() == Some(k)))
                .map(|(_, v)| v.clone())
        };
        let c = m.content().unwrap();
        assert_eq!(
            field(c, 3).and_then(|v| v.as_bytes().map(<[u8]>::to_vec)),
            Some(vec![1, 2, 3, 4, 5])
        );
        assert_eq!(field(c, 4).and_then(|v| v.as_f64()), Some(1.0));
        let packets = field(c, 5).unwrap();
        let packets: Vec<(i64, i64)> = packets
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                let get = |k| field(p, k).and_then(|v| v.as_i64()).unwrap();
                (get(1), get(2))
            })
            .collect();
        assert_eq!(packets, [(3, 0), (2, 3)]);
    }

    #[test]
    fn tv_remote_session_start() {
        let m = roundtrip(&TvRemoteSessionStart);
        assert_eq!(string(&m, "ProtocolVersionKey"), Some("1.2"));
    }

    #[test]
    fn session_messages() {
        let m = roundtrip(&SessionStart {
            session_id: 0xDEAD_BEEF,
        });
        assert_eq!(int(&m, "_sid"), Some(0xDEAD_BEEF));
        assert_eq!(string(&m, "_srvT"), Some(SERVICE_TV_REMOTE));
        assert_eq!(session_start_response(&m).unwrap(), 0xDEAD_BEEF);
        let sid = combined_session_id(1, 2);
        assert_eq!(sid, 0x1_0000_0002);
        let m = roundtrip(&SessionStop {
            session_id: u64::MAX,
        });
        assert_eq!(m.field("_sid").and_then(Value::as_u64), Some(u64::MAX));
        assert!(session_start_response(&m).is_err());
    }

    #[test]
    fn hid() {
        let m = roundtrip(&Hid {
            command: HidCommand::Select,
            state: ButtonState::Down,
        });
        assert_eq!(int(&m, "_hidC"), Some(6));
        assert_eq!(int(&m, "_hBtS"), Some(1));
    }

    #[test]
    fn media_control() {
        let all = [
            MediaControl::Play,
            MediaControl::Pause,
            MediaControl::NextTrack,
            MediaControl::PreviousTrack,
            MediaControl::GetVolume,
            MediaControl::SetVolume(0.5),
            MediaControl::SkipBy(-10.0),
            MediaControl::FastForwardBegin,
            MediaControl::FastForwardEnd,
            MediaControl::RewindBegin,
            MediaControl::RewindEnd,
            MediaControl::GetCaptionSettings,
        ];
        for (code, cmd) in (1..).zip(all.iter()) {
            let m = roundtrip(cmd);
            assert_eq!(int(&m, "_mcc"), Some(code));
        }
        let m = roundtrip(&MediaControl::SetVolume(0.5));
        assert!((volume_response(&m).unwrap() - 0.5).abs() < f64::EPSILON);
        assert_eq!(
            float(&roundtrip(&MediaControl::SkipBy(-10.0)), "_skpS"),
            Some(-10.0)
        );
        assert!(volume_response(&roundtrip(&MediaControl::Play)).is_err());
    }

    #[test]
    fn launch_and_apps() {
        let m = roundtrip(&LaunchApp::BundleId("com.netflix.Netflix"));
        assert_eq!(string(&m, "_bundleID"), Some("com.netflix.Netflix"));
        let m = roundtrip(&LaunchApp::Url("https://example.com"));
        assert_eq!(string(&m, "_urlS"), Some("https://example.com"));
        roundtrip(&FetchApps);

        let mut out = Vec::new();
        Encoder::new(&mut out).dict(3, |e| {
            e.str("_i").str(FetchApps::ID);
            e.str("_t").int(3);
            e.str("_c").dict(2, |e| {
                e.str("com.apple.TVMusic").str("Music");
                e.str("com.netflix.Netflix").str("Netflix");
            });
        });
        let apps = apps_response(&Message::decode(&out).unwrap()).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(
            apps[1],
            App {
                bundle_id: "com.netflix.Netflix".into(),
                name: "Netflix".into()
            }
        );
        assert_eq!(apps_response(&roundtrip(&TouchStop)).unwrap(), []);
    }

    #[test]
    fn accounts() {
        roundtrip(&FetchAccounts);
        let m = roundtrip(&SwitchAccount("acc-2"));
        assert_eq!(string(&m, "SwitchAccountID"), Some("acc-2"));

        let mut out = Vec::new();
        Encoder::new(&mut out).dict(3, |e| {
            e.str("_i").str(FetchAccounts::ID);
            e.str("_t").int(3);
            e.str("_c").dict(2, |e| {
                e.str("acc-1").str("Anna");
                e.str("acc-2").str("Ben");
            });
        });
        let accounts = accounts_response(&Message::decode(&out).unwrap()).unwrap();
        assert_eq!(
            accounts[1],
            Account {
                id: "acc-2".into(),
                name: "Ben".into()
            }
        );
    }

    /// Siri capability archive as the device sends it.
    fn siri_capability(status: i64) -> Vec<u8> {
        use bplist::Value as P;
        let dict = |entries: Vec<(&'static str, P<'static>)>| -> P<'static> {
            P::Dict(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
        };
        dict(vec![
            ("$archiver", P::String("NSKeyedArchiver".into())),
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

    fn system_info_response(capabilities: &[&[u8]]) -> Message<'static> {
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(2, |e| {
            e.str("_t").int(3);
            e.str("_c").dict(1, |e| {
                e.str("_siriInfo").dict(1, |e| {
                    e.str("deviceCapabilitiesV2")
                        .array(capabilities.len(), |e| {
                            for c in capabilities {
                                e.data(c);
                            }
                        });
                });
            });
        });
        Message::decode(&out).unwrap().into_owned()
    }

    #[test]
    fn siri_status() {
        let on = siri_capability(1);
        let off = siri_capability(2);
        assert_eq!(
            siri_enabled(&system_info_response(&[b"x", &on])),
            Some(true)
        );
        assert_eq!(siri_enabled(&system_info_response(&[&off])), Some(false));
        assert_eq!(
            siri_enabled(&system_info_response(&[&siri_capability(0)])),
            None
        );
        assert_eq!(siri_enabled(&system_info_response(&[])), None);
        assert_eq!(siri_enabled(&roundtrip(&FetchSupportedActions)), None);

        let raw = event("SiriStatus", |e| {
            e.dict(1, |e| {
                e.str("deviceCapabilitiesV2").array(1, |e| {
                    e.data(&off);
                });
            });
        });
        assert_eq!(
            Event::parse(&Message::decode(&raw).unwrap()),
            Some(Event::SiriStatus { enabled: false })
        );
        let raw = event("SiriStatus", empty);
        assert_eq!(Event::parse(&Message::decode(&raw).unwrap()), None);
    }

    #[test]
    fn attention() {
        roundtrip(&FetchAttentionState);
        for (code, state) in [
            (0, AttentionState::Unknown),
            (1, AttentionState::Asleep),
            (2, AttentionState::Screensaver),
            (3, AttentionState::Awake),
            (4, AttentionState::Idle),
        ] {
            let mut out = Vec::new();
            Encoder::new(&mut out).dict(2, |e| {
                e.str("_t").int(3);
                e.str("_c").dict(1, |e| {
                    e.str("state").int(code);
                });
            });
            assert_eq!(
                attention_response(&Message::decode(&out).unwrap()).unwrap(),
                state
            );
            assert_eq!(state.is_on(), code >= 2);
        }
        assert!(attention_response(&roundtrip(&FetchAttentionState)).is_err());
    }

    #[test]
    fn interest() {
        let m = roundtrip(&Interest {
            register: &[event::MEDIA_CONTROL, event::SYSTEM_STATUS],
            deregister: &[],
        });
        let reg = m.field("_regEvents").and_then(Value::as_array).unwrap();
        assert_eq!(reg.len(), 2);
        assert!(m.field("_deregEvents").is_none());
        let m = roundtrip(&Interest::default());
        assert_eq!(
            m.content().and_then(Value::as_dict).map(<[_]>::len),
            Some(0)
        );
    }

    #[test]
    fn touch() {
        let m = roundtrip(&TouchStart::default());
        assert_eq!(float(&m, "_width"), Some(1000.0));
        let m = roundtrip(&Touch {
            timestamp_ns: 5,
            finger: 1,
            x: 10,
            y: 20,
            phase: TouchPhase::Release,
        });
        assert_eq!(int(&m, "_tPh"), Some(4));
        assert_eq!(int(&m, "_cy"), Some(20));
        roundtrip(&TextInputStart);
        roundtrip(&TextInputStop);
    }

    fn event(id: &str, content: impl FnOnce(&mut Encoder<'_>)) -> Vec<u8> {
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(3, |e| {
            e.str("_i").str(id);
            e.str("_t").int(1);
            e.str("_c");
            content(e);
        });
        out
    }

    #[test]
    fn events() {
        let raw = event("_iMC", |e| {
            e.dict(1, |e| {
                e.str("_mcF").int(0x103);
            });
        });
        let Some(Event::MediaControl(flags)) = Event::parse(&Message::decode(&raw).unwrap()) else {
            panic!("expected media control event");
        };
        assert!(flags.contains(MediaControlFlags::PLAY | MediaControlFlags::VOLUME));
        assert!(!flags.contains(MediaControlFlags::REWIND));

        let raw = event("TVSystemStatus", |e| {
            e.dict(1, |e| {
                e.str("state").int(1);
            });
        });
        assert_eq!(
            Event::parse(&Message::decode(&raw).unwrap()),
            Some(Event::SystemStatus(AttentionState::Asleep))
        );
        let raw = event("_tiStarted", empty);
        assert_eq!(
            Event::parse(&Message::decode(&raw).unwrap()),
            Some(Event::TextInputStarted)
        );
        let raw = event("_tiStopped", empty);
        assert_eq!(
            Event::parse(&Message::decode(&raw).unwrap()),
            Some(Event::TextInputStopped)
        );
        let raw = event("_siriEndpoint", empty);
        assert_eq!(Event::parse(&Message::decode(&raw).unwrap()), None);
        let mut raw = Vec::new();
        encode(&SiriEndpointReply, 9, &mut raw);
        let reply = Message::decode(&raw).unwrap();
        assert_eq!(reply.kind(), Some(MessageType::Response));
        assert_eq!(reply.xid(), Some(9));
        let mut raw = Vec::new();
        Encoder::new(&mut raw).dict(4, |e| {
            e.str("_i").str("_siriEndpoint");
            e.str("_x").int(5);
            e.str("_t").int(MessageType::Request as i64);
            e.str("_c").dict(0, |_| {});
        });
        assert_eq!(
            Event::parse(&Message::decode(&raw).unwrap()),
            Some(Event::SiriEndpoint)
        );
        let raw = event("SupportedActions", |e| {
            e.dict(1, |e| {
                e.str("GuideSupportedKey").bool(false);
            });
        });
        assert_eq!(
            Event::parse(&Message::decode(&raw).unwrap()),
            Some(Event::ScreenChanged {
                guide_supported: false
            })
        );
        let raw = event("other", empty);
        assert_eq!(Event::parse(&Message::decode(&raw).unwrap()), None);
        // Requests are not events.
        assert_eq!(Event::parse(&roundtrip(&TouchStop)), None);
    }

    #[test]
    fn errors() {
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(4, |e| {
            e.str("_x").int(1);
            e.str("_ec").int(-6714);
            e.str("_ed").str("RPErrorDomain");
            e.str("_em").str("Not supported");
        });
        let m = Message::decode(&out).unwrap();
        assert_eq!(m.kind(), None);
        assert_eq!(
            m.clone().into_result().unwrap_err(),
            Error::Device {
                code: -6714,
                domain: "RPErrorDomain".into(),
                message: "Not supported".into()
            }
        );
        assert!(m.value().get("_ec").is_some());
        assert!(roundtrip(&TouchStop).into_result().is_ok());
        assert!(matches!(
            Message::decode(&[0x09]),
            Err(Error::InvalidField("_i"))
        ));
    }
}
