//! Media Remote Protocol over AirPlay 2: playback, now playing, buttons and volume.
//!
//! The AirPlay port is the one of the `_airplay._tcp` service (usually 7000).
//! Companion credentials work for AirPlay on recent tvOS versions.
//! Otherwise get credentials with [`pair`].
//!
//! ```no_run
//! # async fn run(credentials: ratv::Credentials) -> Result<(), ratv::Error> {
//! use ratv::mrp::{Button, Client, ClientOptions, Command};
//!
//! let addr = "192.168.1.20:7000".parse().unwrap();
//! let client = Client::connect(addr, &credentials, &ClientOptions::default()).await?;
//! let mut now_playing = client.now_playing();
//! client.press(Button::Select).await?;
//! client.command(Command::Pause).await?;
//! while now_playing.changed().await.is_ok() {
//!     println!("{:?}", now_playing.borrow().title);
//! }
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
pub use ratv_mrp::hid::Button;
pub use ratv_mrp::proto::{AudioFormat, Command, LanguageOption};
pub use ratv_mrp::proto::{repeat_mode::Enum as RepeatMode, shuffle_mode::Enum as ShuffleMode};
pub use ratv_mrp::{
    Artwork, Chapter, Envelope, LanguageType, MediaSubType, MediaType, NowPlaying, OutputDevice,
    PlaybackState, QueueItem, Volume, cocoa_time, four_cc,
};
use ratv_mrp::{
    NowPlayingState, Payload, VolumeState, encode, encode_type, frames, messages, proto,
};
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use ratv_airplay::Channel;
use ratv_airplay::http::Message;
use ratv_airplay::rtsp::{self, Session};
use ratv_codec::opack::Encoder;
use ratv_hap::pair_setup::{self, ClientIdentity, NeedPin};
use tokio::net::TcpStream;

use crate::airplay::{self, Connections, Identity, READ_CHUNK};
use crate::{Credentials, Error, rng};

const COMMAND_CAPACITY: usize = 16;
const MESSAGE_CAPACITY: usize = 64;
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// Siri listening to voice input, from [`Client::start_siri`].
///
/// Siri is released when this is dropped, but call [`finish`](Self::finish)
/// to know that the release was sent.
#[must_use = "Siri stays active until `finish` is called or this is dropped"]
pub struct Siri<'a> {
    client: &'a Client,
    /// Sample number of the next packet.
    sample: u32,
    /// True while the button is held.
    held: bool,
}

impl Siri<'_> {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms.
    pub async fn send(&mut self, packet: impl Into<Bytes>) -> Result<(), Error> {
        let msg = messages::voice_input(packet.into(), self.sample);
        self.client.send(&msg).await?;
        self.sample = self.sample.wrapping_add(messages::VOICE_FRAMES);
        Ok(())
    }

    /// Releases Siri. The device then answers.
    pub async fn finish(mut self) -> Result<(), Error> {
        let result = self.client.button(Button::Siri, false).await;
        self.held = false;
        result
    }
}

impl Drop for Siri<'_> {
    fn drop(&mut self) {
        if self.held {
            self.client.send_now(&Button::Siri.event(false));
        }
        self.client.siri_active.store(false, Ordering::Release);
    }
}

/// True for `SetRecordingStateMessage` with `Recording`.
fn is_recording(env: &Envelope) -> bool {
    use proto::set_recording_state_message::RecordingState;

    env.is::<proto::SetRecordingStateMessage>()
        && env
            .payload::<proto::SetRecordingStateMessage>()
            .is_ok_and(|m| m.state == Some(RecordingState::Recording as i32))
}

/// Language tag for automatic subtitles.
const AUTO_LANGUAGE: &str = "__AUTO__";
/// Current item plus next items to request for [`NowPlaying::up_next`].
const QUEUE_LENGTH: i32 = 10;

/// Options for [`Client::connect`].
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Name we report to the device.
    pub name: String,
    /// Model we report, for example `iPhone14,3`.
    pub model: String,
    /// iOS version we report.
    pub os_version: String,
    /// iOS build we report.
    pub os_build: String,
    /// Stable id, `AA:BB:CC:DD:EE:FF`. Random if `None`.
    pub device_id: Option<String>,
    /// Stable unique id for `DEVICE_INFO`, usually a UUID. Random if `None`.
    pub unique_identifier: Option<String>,
    /// Time to wait for each response.
    pub timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            name: "ratv".into(),
            model: "iPhone14,3".into(),
            os_version: "17.0".into(),
            os_build: "21A329".into(),
            device_id: None,
            unique_identifier: None,
            timeout: Duration::from_secs(5),
        }
    }
}

/// Options for [`pair`].
#[derive(Debug, Clone)]
pub struct PairOptions {
    /// Name shown with the PIN and in the device settings.
    pub name: String,
    /// Pairing identifier. A random UUID is used when `None`.
    pub client_id: Option<String>,
    /// Time to wait for each response.
    pub timeout: Duration,
}

impl Default for PairOptions {
    fn default() -> Self {
        Self {
            name: "ratv".into(),
            client_id: None,
            timeout: Duration::from_secs(5),
        }
    }
}

/// AirPlay pairing that waits for the PIN shown on the device.
pub struct PinPrompt {
    link: PairLink,
    state: NeedPin,
    options: PairOptions,
}

impl core::fmt::Debug for PinPrompt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PinPrompt")
            .field("name", &self.options.name)
            .finish_non_exhaustive()
    }
}

/// Asks the AirPlay service at `addr` to show a PIN and starts pair-setup.
/// Complete it with [`PinPrompt::finish`].
pub async fn pair(addr: SocketAddr, options: &PairOptions) -> Result<PinPrompt, Error> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let local = stream.local_addr()?.ip().to_string();
    let mut link = PairLink {
        stream,
        channel: Channel::new(),
        session: Session::new(&local, rng::u32(), rng::u64(), rng::u32()),
        timeout: options.timeout,
    };
    let name = Some(options.name.as_str());
    link.session.pair_pin_start(&mut link.channel, name);
    link.receive().await?;
    let (m1, state) = pair_setup::start(false);
    let m2 = link.step(&m1, name).await?;
    Ok(PinPrompt {
        link,
        state: state.handle_m2(&m2.body())?,
        options: options.clone(),
    })
}

impl PinPrompt {
    /// Completes pairing with the PIN shown on the device.
    pub async fn finish(self, pin: &str) -> Result<Credentials, Error> {
        let Self {
            mut link,
            state,
            options,
        } = self;
        let name = Some(options.name.as_str());
        let client_id = options.client_id.clone().unwrap_or_else(rng::uuid);
        // pyatv sends only the name here; Companion sends more device info.
        let mut metadata = Vec::with_capacity(16 + options.name.len());
        Encoder::new(&mut metadata).dict(1, |e| {
            e.str("name").str(&options.name);
        });
        let mut rng = rng::os();
        let (m3, state) = state.submit_pin(pin, &mut rng)?;
        let m4 = link.step(&m3, name).await?;
        let verified = state.handle_m4(&m4.body())?;
        let identity = ClientIdentity {
            id: client_id.as_bytes(),
            metadata: Some(&metadata),
        };
        let (m5, state) = verified.exchange(identity, &mut rng);
        let m6 = link.step(&m5, name).await?;
        Ok(state.handle_m6(&m6.body())?)
    }
}

/// HTTP connection used for pairing.
struct PairLink {
    stream: TcpStream,
    channel: Channel,
    session: Session,
    timeout: Duration,
}

impl PairLink {
    async fn step(&mut self, tlv: &[u8], client_name: Option<&str>) -> Result<Message, Error> {
        self.session.pair_setup(&mut self.channel, tlv, client_name);
        self.receive().await
    }

    async fn receive(&mut self) -> Result<Message, Error> {
        let resp = tokio::time::timeout(
            self.timeout,
            airplay::exchange(&mut self.stream, &mut self.channel),
        )
        .await??;
        rtsp::check(&resp)?;
        Ok(resp)
    }
}

enum Request {
    Send {
        payload: BytesMut,
        reply: Option<(String, oneshot::Sender<Envelope>)>,
    },
    Close(oneshot::Sender<()>),
}

/// Connected MRP session.
///
/// A background task owns the three AirPlay connections. Dropping the client
/// stops it; [`Client::close`] also ends the session on the device.
#[derive(Debug)]
pub struct Client {
    requests: mpsc::Sender<Request>,
    messages: broadcast::Sender<Envelope>,
    now_playing: watch::Receiver<NowPlaying>,
    volume: watch::Receiver<Volume>,
    device: proto::DeviceInfoMessage,
    /// Output device UID for volume messages.
    output_uid: Option<String>,
    next_id: AtomicU32,
    timeout: Duration,
    task: JoinHandle<()>,
    /// Voice input device id, registered on first use of Siri.
    voice_device: tokio::sync::OnceCell<i32>,
    /// True while a [`Siri`] exists.
    siri_active: AtomicBool,
}

impl Client {
    /// Connects to the AirPlay service at `addr` and starts an MRP session.
    pub async fn connect(
        addr: SocketAddr,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error> {
        let device_id = options
            .device_id
            .clone()
            .unwrap_or_else(|| rng::mac_string(rng::bytes()));
        let identity = Identity {
            name: &options.name,
            model: &options.model,
            os_version: &options.os_version,
            os_build: &options.os_build,
            device_id: &device_id,
        };
        let mut conns = airplay::connect(addr, credentials, &identity, options.timeout).await?;

        // DEVICE_INFO must be answered before anything else is sent.
        let unique = options.unique_identifier.clone().unwrap_or_else(rng::uuid);
        let mut out = BytesMut::new();
        let info = messages::device_info(&messages::ClientInfo {
            unique_identifier: &unique,
            name: &options.name,
            system_build_version: &options.os_build,
        });
        encode(&info, Some("0"), &mut out);
        conns.stream.send(&out);
        let (np_tx, now_playing) = watch::channel(NowPlaying::default());
        let (volume_tx, volume) = watch::channel(Volume::default());
        let mut state = State {
            now_playing: NowPlayingState::new(),
            now_playing_tx: np_tx,
            volume: VolumeState::new(),
            volume_tx,
            item_id: None,
        };
        let device =
            tokio::time::timeout(options.timeout, wait_device_info(&mut conns, &mut state))
                .await
                .map_err(Error::from)
                .and_then(|r| r);
        let device = match device {
            Ok(d) => d,
            Err(err) => {
                airplay::teardown(&mut conns.control, &mut conns.channel, &mut conns.session).await;
                return Err(err);
            }
        };

        state.volume.set_device(&device);
        let output_uid = state.volume.output_device_uid().map(str::to_owned);

        // The device ignores CLIENT_UPDATES_CONFIG without an identifier.
        out.clear();
        encode(&messages::connected(), None, &mut out);
        encode(&messages::client_updates_config(), Some("1"), &mut out);
        encode_type(
            proto::protocol_message::Type::GetKeyboardSessionMessage,
            Some("2"),
            &mut out,
        );
        // Initial volume. The replies update the volume state.
        if let Some(uid) = &output_uid {
            encode(&messages::get_volume(uid), Some("3"), &mut out);
            encode(&messages::get_volume_muted(uid), Some("4"), &mut out);
            encode(
                &messages::get_volume_control_capabilities(uid),
                Some("5"),
                &mut out,
            );
        }
        conns.stream.send(&out);

        let (requests, rx) = mpsc::channel(COMMAND_CAPACITY);
        let (messages, _) = broadcast::channel(MESSAGE_CAPACITY);
        state.publish();
        let task = tokio::spawn(run(conns, rx, messages.clone(), state));
        Ok(Self {
            requests,
            messages,
            now_playing,
            volume,
            device,
            output_uid,
            next_id: AtomicU32::new(6),
            voice_device: tokio::sync::OnceCell::new(),
            siri_active: AtomicBool::new(false),
            timeout: options.timeout,
            task,
        })
    }

    /// Device info from the connection handshake.
    #[must_use]
    pub const fn device_info(&self) -> &proto::DeviceInfoMessage {
        &self.device
    }

    /// What the device plays. Updated by the background task.
    #[must_use]
    pub fn now_playing(&self) -> watch::Receiver<NowPlaying> {
        self.now_playing.clone()
    }

    /// Volume and mute state. Updated by the background task.
    #[must_use]
    pub fn volume(&self) -> watch::Receiver<Volume> {
        self.volume.clone()
    }

    /// All received MRP messages.
    #[must_use]
    pub fn messages(&self) -> broadcast::Receiver<Envelope> {
        self.messages.subscribe()
    }

    /// True while the background task runs.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        !self.task.is_finished()
    }

    /// Sends a message and waits for the response with the same identifier.
    /// Device errors are returned as `Err`.
    pub async fn request<P: Payload>(&self, msg: &P) -> Result<Envelope, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let mut payload = BytesMut::new();
        encode(msg, Some(&id), &mut payload);
        let (tx, rx) = oneshot::channel();
        self.requests
            .send(Request::Send {
                payload,
                reply: Some((id, tx)),
            })
            .await
            .map_err(|_| Error::Disconnected)?;
        let env = tokio::time::timeout(self.timeout, rx)
            .await?
            .map_err(|_| Error::Disconnected)?;
        if let Some(err) = env.error() {
            return Err(err.into());
        }
        Ok(env)
    }

    /// Sends a message without waiting for a response.
    pub async fn send<P: Payload>(&self, msg: &P) -> Result<(), Error> {
        let mut payload = BytesMut::new();
        encode(msg, None, &mut payload);
        self.requests
            .send(Request::Send {
                payload,
                reply: None,
            })
            .await
            .map_err(|_| Error::Disconnected)
    }

    /// Holds Siri and waits until the device listens. Then send the audio with
    /// [`Siri::send`] and release Siri with [`Siri::finish`].
    ///
    /// Registers a voice input device on first use.
    /// Fails with [`Error::SiriActive`] while another [`Siri`] exists.
    pub async fn start_siri(&self) -> Result<Siri<'_>, Error> {
        if self.siri_active.swap(true, Ordering::AcqRel) {
            return Err(Error::SiriActive);
        }
        // Releases the button and the flag if a step fails or the future is dropped.
        let mut siri = Siri {
            client: self,
            sample: 0,
            held: false,
        };
        self.voice_device().await?;
        let mut messages = self.messages();
        self.button(Button::Siri, true).await?;
        siri.held = true;
        // Audio sent before the device asks for it is lost.
        let recording = async {
            loop {
                match messages.recv().await {
                    Ok(env) if is_recording(&env) => return Ok(()),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Err(Error::Disconnected),
                }
            }
        };
        tokio::time::timeout(self.timeout, recording).await??;
        Ok(siri)
    }

    async fn voice_device(&self) -> Result<i32, Error> {
        self.voice_device
            .get_or_try_init(|| async {
                let env = self.request(&messages::register_voice_input()).await?;
                let reply: proto::RegisterVoiceInputDeviceResponseMessage = env.payload()?;
                match reply.error_code.unwrap_or(0) {
                    0 => Ok(reply.device_id.unwrap_or(0)),
                    code => Err(ratv_mrp::Error::Device {
                        code,
                        description: "voice input registration failed".into(),
                    }
                    .into()),
                }
            })
            .await
            .copied()
    }

    /// Queues a message without waiting. If the queue is full, a task sends it.
    fn send_now<P: Payload>(&self, msg: &P) {
        let mut payload = BytesMut::new();
        encode(msg, None, &mut payload);
        let request = Request::Send {
            payload,
            reply: None,
        };
        if let Err(mpsc::error::TrySendError::Full(request)) = self.requests.try_send(request)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let requests = self.requests.clone();
            runtime.spawn(async move {
                let _ = requests.send(request).await;
            });
        }
    }

    /// Sends a button down or up event.
    pub async fn button(&self, button: Button, down: bool) -> Result<(), Error> {
        self.send(&button.event(down)).await
    }

    /// Presses and releases a button.
    pub async fn press(&self, button: Button) -> Result<(), Error> {
        self.button(button, true).await?;
        self.button(button, false).await
    }

    /// Holds a button for `duration`.
    pub async fn hold(&self, button: Button, duration: Duration) -> Result<(), Error> {
        self.button(button, true).await?;
        tokio::time::sleep(duration).await;
        self.button(button, false).await
    }

    /// Sends a playback command, for example [`Command::Play`].
    pub async fn command(&self, command: Command) -> Result<(), Error> {
        self.run_command(&messages::command(command)).await
    }

    /// Seeks to `position` seconds.
    pub async fn seek(&self, position: f64) -> Result<(), Error> {
        self.run_command(&messages::seek(position)).await
    }

    /// Skips `seconds` forward, or backward if negative.
    pub async fn skip(&self, seconds: f32) -> Result<(), Error> {
        self.run_command(&messages::skip(seconds)).await
    }

    /// Sets the shuffle mode.
    pub async fn set_shuffle(&self, mode: ShuffleMode) -> Result<(), Error> {
        self.run_command(&messages::shuffle(mode)).await
    }

    /// Sets the repeat mode.
    pub async fn set_repeat(&self, mode: RepeatMode) -> Result<(), Error> {
        self.run_command(&messages::repeat(mode)).await
    }

    async fn run_command(&self, msg: &proto::SendCommandMessage) -> Result<(), Error> {
        let env = self.request(msg).await?;
        let Ok(result) = env.payload::<proto::SendCommandResultMessage>() else {
            return Ok(());
        };
        let send_error = result.send_error.unwrap_or(0);
        let handler_status = result.handler_return_status.unwrap_or(0);
        if send_error == 0 && handler_status == 0 {
            Ok(())
        } else {
            Err(Error::CommandFailed {
                send_error,
                handler_status,
            })
        }
    }

    /// Requests the image of the current item in about this size.
    /// A size of -1 keeps the aspect ratio. `None` if the item has no image.
    ///
    /// Right after connect the device may not answer with an image yet.
    /// Wait until [`NowPlaying::artwork_id`] is set, and fetch again when it changes.
    pub async fn artwork(&self, width: f64, height: f64) -> Result<Option<Artwork>, Error> {
        let env = self
            .request(&messages::playback_queue_request(width, height))
            .await?;
        Ok(ratv_mrp::artwork(&env)?)
    }

    /// Sets the volume of the device, 0.0 to 1.0.
    pub async fn set_volume(&self, volume: f32) -> Result<(), Error> {
        self.send(&messages::set_volume(volume, self.output_uid()?))
            .await
    }

    /// Mutes or unmutes the device.
    pub async fn set_muted(&self, muted: bool) -> Result<(), Error> {
        self.send(&messages::set_volume_muted(muted, self.output_uid()?))
            .await
    }

    /// Sets the playback rate, 1.0 is normal speed.
    /// Rates the app accepts are in [`NowPlaying::supported_rates`].
    pub async fn set_playback_rate(&self, rate: f32) -> Result<(), Error> {
        self.run_command(&messages::playback_rate(rate)).await
    }

    /// Rates the current item. The range is in [`NowPlaying::rating_range`].
    pub async fn rate(&self, rating: f32) -> Result<(), Error> {
        self.run_command(&messages::rate_track(rating)).await
    }

    /// Likes the current item, or removes the like. See [`NowPlaying::liked`].
    pub async fn like(&self, liked: bool) -> Result<(), Error> {
        self.run_command(&messages::like_track(liked)).await
    }

    /// Dislikes the current item, or removes the dislike. See [`NowPlaying::disliked`].
    pub async fn dislike(&self, disliked: bool) -> Result<(), Error> {
        self.run_command(&messages::dislike_track(disliked)).await
    }

    /// Turns on an audio or subtitle language from [`NowPlaying::audio_languages`]
    /// or [`NowPlaying::subtitle_languages`].
    pub async fn enable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.run_command(&messages::enable_language(option)).await
    }

    /// Turns off a language. Use it with [`NowPlaying::subtitle_language`] to turn off subtitles.
    pub async fn disable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.run_command(&messages::disable_language(option)).await
    }

    /// Turns off subtitles.
    pub async fn disable_subtitles(&self) -> Result<(), Error> {
        // With only the type, the device can show another subtitle track instead.
        let msg = match self.now_playing.borrow().subtitle_language.as_ref() {
            Some(o) if o.language_tag() != AUTO_LANGUAGE => messages::disable_language(o),
            _ => messages::disable_subtitles(),
        };
        self.run_command(&msg).await
    }

    /// Plays `NowPlaying::up_next[index]`.
    pub async fn play_queue_item(&self, index: usize) -> Result<(), Error> {
        let offset = i32::try_from(index).map_or(i32::MAX, |i| i.saturating_add(1));
        self.run_command(&messages::play_queue_offset(offset)).await
    }

    /// Sets the volume, 0.0 to 1.0, of one output device. UIDs are in [`Volume::outputs`].
    pub async fn set_output_volume(&self, uid: &str, volume: f32) -> Result<(), Error> {
        self.send(&messages::set_volume(volume, uid)).await
    }

    /// Adds AirPlay output devices, for example `HomePods`, to the group.
    /// The new list arrives in [`Volume::outputs`].
    pub async fn add_outputs(&self, uids: &[&str]) -> Result<(), Error> {
        self.send(&messages::add_output_devices(uids)).await
    }

    /// Removes AirPlay output devices from the group.
    pub async fn remove_outputs(&self, uids: &[&str]) -> Result<(), Error> {
        self.send(&messages::remove_output_devices(uids)).await
    }

    /// Replaces the output devices of the group.
    pub async fn set_outputs(&self, uids: &[&str]) -> Result<(), Error> {
        self.send(&messages::set_output_devices(uids)).await
    }

    fn output_uid(&self) -> Result<&str, Error> {
        self.output_uid
            .as_deref()
            .ok_or(Error::Protocol("device has no output device UID"))
    }

    /// Ends the session and closes the connections.
    pub async fn close(mut self) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        if self.requests.send(Request::Close(tx)).await.is_ok() {
            let _ = rx.await;
            let _ = (&mut self.task).await;
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_device_info(
    conns: &mut Connections,
    state: &mut State,
) -> Result<proto::DeviceInfoMessage, Error> {
    use tokio::io::AsyncWriteExt;
    let mut device = None;
    loop {
        if conns.stream.has_transmit() {
            conns.data.write_all(&conns.stream.take_transmit()).await?;
        }
        // Messages after DEVICE_INFO in the same read are applied too, so none are lost.
        while let Some(data) = conns.stream.poll()? {
            for env in frames(data) {
                let env = env?;
                if device.is_none() && env.is::<proto::DeviceInfoMessage>() {
                    device = Some(env.payload()?);
                }
                state.apply(&env);
            }
        }
        if let Some(device) = device {
            return Ok(device);
        }
        if conns.stream.has_transmit() {
            continue;
        }
        tokio::select! {
            n = conns.data.read_buf(conns.stream.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
            }
            n = conns.events.read_buf(conns.event_channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                airplay::acknowledge_events(&mut conns.events, &mut conns.event_channel).await?;
            }
        }
    }
}

/// State tracked from received messages, and the channels that publish it.
struct State {
    now_playing: NowPlayingState,
    now_playing_tx: watch::Sender<NowPlaying>,
    volume: VolumeState,
    volume_tx: watch::Sender<Volume>,
    /// Item that details were requested for.
    item_id: Option<String>,
}

impl State {
    /// Updates the state and publishes changes.
    fn apply(&mut self, env: &Envelope) {
        match self.now_playing.apply(env) {
            Ok(true) => publish(&self.now_playing_tx, self.now_playing.now_playing()),
            Ok(false) => {}
            Err(err) => tracing::debug!(%err, kind = env.kind(), "invalid now playing message"),
        }
        match self.volume.apply(env) {
            Ok(true) => publish(&self.volume_tx, self.volume.volume().clone()),
            Ok(false) => {}
            Err(err) => tracing::debug!(%err, kind = env.kind(), "invalid volume message"),
        }
    }

    /// True once each time the current item changes.
    fn item_changed(&mut self) -> bool {
        let id = self.now_playing.item_id();
        if id == self.item_id.as_deref() {
            return false;
        }
        self.item_id = id.map(str::to_owned);
        id.is_some()
    }

    /// Publishes the current state.
    fn publish(&self) {
        publish(&self.now_playing_tx, self.now_playing.now_playing());
        publish(&self.volume_tx, self.volume.volume().clone());
    }
}

/// Sends `value` if it differs from the current one.
fn publish<T: PartialEq>(tx: &watch::Sender<T>, value: T) {
    tx.send_if_modified(|cur| {
        let changed = *cur != value;
        if changed {
            *cur = value;
        }
        changed
    });
}

async fn run(
    conns: Connections,
    requests: mpsc::Receiver<Request>,
    messages: broadcast::Sender<Envelope>,
    state: State,
) {
    if let Err(err) = run_inner(conns, requests, &messages, state).await {
        tracing::debug!(%err, "MRP connection ended");
    }
}

async fn run_inner(
    conns: Connections,
    mut requests: mpsc::Receiver<Request>,
    messages: &broadcast::Sender<Envelope>,
    mut state: State,
) -> Result<(), Error> {
    use tokio::io::AsyncWriteExt;

    let Connections {
        mut control,
        mut channel,
        mut session,
        mut events,
        mut event_channel,
        mut data,
        mut stream,
    } = conns;
    let mut pending: Vec<(String, oneshot::Sender<Envelope>)> = Vec::new();
    let start = tokio::time::Instant::now() + FEEDBACK_INTERVAL;
    let mut feedback = tokio::time::interval_at(start, FEEDBACK_INTERVAL);

    loop {
        // Pushed items often lack languages, chapters and next items.
        if state.item_changed() {
            let mut out = BytesMut::new();
            encode(
                &messages::playback_queue_details(QUEUE_LENGTH),
                Some("details"),
                &mut out,
            );
            stream.send(&out);
        }
        if stream.has_transmit() {
            data.write_all(&stream.take_transmit()).await?;
        }
        tokio::select! {
            req = requests.recv() => match req {
                Some(Request::Send { payload, reply }) => {
                    if let Some(r) = reply {
                        pending.retain(|(_, tx)| !tx.is_closed());
                        pending.push(r);
                    }
                    stream.send(&payload);
                }
                Some(Request::Close(done)) => {
                    airplay::teardown(&mut control, &mut channel, &mut session).await;
                    let _ = done.send(());
                    return Ok(());
                }
                None => return Ok(()),
            },
            _ = feedback.tick() => {
                session.feedback(&mut channel);
                airplay::flush(&mut control, &mut channel).await?;
            }
            n = control.read_buf(channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                while let Some(m) = channel.poll_message()? {
                    if m.status() != Some(200) {
                        tracing::debug!(status = ?m.status(), "control response");
                    }
                }
            }
            n = events.read_buf(event_channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                airplay::acknowledge_events(&mut events, &mut event_channel).await?;
            }
            n = data.read_buf(stream.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                while let Some(bytes) = stream.poll()? {
                    for env in frames(bytes) {
                        dispatch(env?, &mut pending, messages, &mut state);
                    }
                }
            }
        }
    }
}

fn dispatch(
    env: Envelope,
    pending: &mut Vec<(String, oneshot::Sender<Envelope>)>,
    messages: &broadcast::Sender<Envelope>,
    state: &mut State,
) {
    tracing::trace!(kind = env.kind(), id = ?env.identifier(), "received");
    state.apply(&env);
    if let Some(id) = env.identifier()
        && let Some(i) = pending.iter().position(|(p, _)| p == id)
    {
        let (_, reply) = pending.swap_remove(i);
        let _ = reply.send(env.clone());
    }
    let _ = messages.send(env);
}
