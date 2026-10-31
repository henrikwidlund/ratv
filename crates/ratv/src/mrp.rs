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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
use tokio::sync::{Notify, broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{Instrument, Span};

use ratv_airplay::Channel;
use ratv_airplay::http::Message;
use ratv_airplay::rtsp::{self, Session};
use ratv_codec::opack::Encoder;
use ratv_hap::pair_setup::{self, ClientIdentity, NeedPin};
use tokio::net::TcpStream;

use crate::airplay::{self, Connections, Identity, READ_CHUNK};
use crate::link::{Link, LinkTx};
pub use crate::{ClientOptions, PairOptions};
use crate::{Credentials, Error, log, rng, tcp};

/// Encoded messages that are not sent yet.
const MAX_OUTBOX: usize = 1 << 20;
const MESSAGE_CAPACITY: usize = 64;
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// The device answers each feedback request. Without an answer for this long,
/// the connection is dead.
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Voice packets kept until the device records: 30 s.
const MAX_PENDING: usize = 1500;
/// Siri listening to voice input, from [`Client::start_siri`].
///
/// Audio is kept until the device starts recording, then sent.
/// Siri is released when this is dropped, but call [`finish`](Self::finish)
/// to send the rest of the audio and to know that the release was sent.
#[must_use = "Siri stays active until `finish` is called or this is dropped"]
pub struct Siri {
    client: Client,
    /// Recording count when Siri was pressed.
    start: u32,
    recording: watch::Receiver<u32>,
    /// True after the device started recording.
    started: bool,
    /// Packets kept until the device records.
    pending: Vec<Bytes>,
    #[cfg(feature = "opus")]
    /// Boxed because it is large.
    encoder: Option<Box<ratv_mrp::VoiceEncoder>>,
    /// Sample number of the next packet.
    sample: u32,
    /// True while the button is held.
    held: bool,
}

impl Siri {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms.
    pub async fn send(&mut self, packet: impl Into<Bytes>) -> Result<(), Error> {
        self.pending.push(packet.into());
        self.send_pending().await
    }

    /// Sends signed 16-bit, 16 kHz, mono PCM in chunks of any size.
    /// Do not mix with [`send`](Self::send).
    #[cfg(feature = "opus")]
    pub async fn push_pcm(&mut self, pcm: &[i16]) -> Result<(), Error> {
        let pending = &mut self.pending;
        self.encoder
            .get_or_insert_with(Box::default)
            .push(pcm, |p| pending.push(Bytes::copy_from_slice(p)))?;
        self.send_pending().await
    }

    /// Sends the rest of the audio and releases Siri. The device then answers.
    ///
    /// Waits for the device to record if it has not started yet.
    pub async fn finish(mut self) -> Result<(), Error> {
        #[cfg(feature = "opus")]
        if let Some(encoder) = &mut self.encoder {
            let pending = &mut self.pending;
            encoder.flush(|p| pending.push(Bytes::copy_from_slice(p)))?;
        }
        if !self.pending.is_empty() && !self.is_recording() {
            let (start, client) = (self.start, self.client.clone());
            let wait = self.recording.wait_for(|&n| n != start);
            match tokio::time::timeout(client.inner.timeout, wait).await {
                // `send_pending` sees the change.
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return Err(Error::Disconnected),
                Err(_) => {
                    tracing::debug!(parent: &client.inner.span, "Siri did not start recording");
                    return Err(Error::SiriNotRecording);
                }
            }
        }
        self.send_pending().await?;
        let result = self.client.button(Button::Siri, false).await;
        self.held = false;
        tracing::debug!(parent: &self.client.inner.span, packets = self.sample / messages::VOICE_FRAMES, "Siri released");
        result
    }

    /// True once the device has started recording.
    fn is_recording(&mut self) -> bool {
        if !self.started && *self.recording.borrow() != self.start {
            self.started = true;
            tracing::debug!(parent: &self.client.inner.span, kept = self.pending.len(), "Siri recording");
        }
        self.started
    }

    /// Sends the kept packets if the device records.
    async fn send_pending(&mut self) -> Result<(), Error> {
        if !self.is_recording() {
            if self.pending.len() > MAX_PENDING {
                tracing::debug!(parent: &self.client.inner.span, "Siri did not start recording");
                self.pending.clear();
                return Err(Error::SiriNotRecording);
            }
            return Ok(());
        }
        let mut sent = 0;
        while let Some(packet) = self.pending.get(sent) {
            let msg = messages::voice_input(packet.clone(), self.sample);
            if let Err(err) = self.client.send(&msg).await {
                // Sent packets are not sent again.
                self.pending.drain(..sent);
                return Err(err);
            }
            sent += 1;
            self.sample = self.sample.wrapping_add(messages::VOICE_FRAMES);
        }
        self.pending.clear();
        Ok(())
    }
}

impl Drop for Siri {
    fn drop(&mut self) {
        if self.held {
            tracing::debug!(parent: &self.client.inner.span, "Siri released on drop");
            let _ = self.client.queue(&Button::Siri.event(false), None, None);
        }
        self.client
            .inner
            .siri_active
            .store(false, Ordering::Release);
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

/// AirPlay pairing that waits for the PIN shown on the device.
pub struct PinPrompt {
    span: Span,
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
    let span = log::session("airplay pairing", Some(addr));
    pair_inner(addr, options, span.clone())
        .instrument(span)
        .await
}

async fn pair_inner(
    addr: SocketAddr,
    options: &PairOptions,
    span: Span,
) -> Result<PinPrompt, Error> {
    let stream = tcp::connect(addr, options.timeout).await?;
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
    tracing::debug!("waiting for PIN");
    Ok(PinPrompt {
        span,
        link,
        state: state.handle_m2(&m2.body())?,
        options: options.clone(),
    })
}

impl PinPrompt {
    /// Completes pairing with the PIN shown on the device.
    pub async fn finish(self, pin: &str) -> Result<Credentials, Error> {
        let span = self.span.clone();
        self.finish_inner(pin).instrument(span).await
    }

    async fn finish_inner(self, pin: &str) -> Result<Credentials, Error> {
        let Self {
            span: _,
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
        tracing::debug!("PIN accepted");
        let identity = ClientIdentity {
            id: client_id.as_bytes(),
            metadata: Some(&metadata),
        };
        let (m5, state) = verified.exchange(identity, &mut rng);
        let m6 = link.step(&m5, name).await?;
        let credentials = state.handle_m6(&m6.body())?;
        tracing::debug!("paired");
        Ok(credentials)
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

/// Connected MRP session.
///
/// A background task owns the three AirPlay connections. Clones share the
/// session. Dropping the last clone stops it; [`Client::close`] also ends the
/// session on the device.
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    shared: Arc<Shared>,
    link: Link,
    messages: broadcast::Sender<Envelope>,
    now_playing: watch::Receiver<NowPlaying>,
    volume: watch::Receiver<Volume>,
    /// Number of times the device started recording voice input.
    recording: watch::Receiver<u32>,
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
    span: Span,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Written by clients, read by the background task.
#[derive(Debug, Default)]
struct Shared {
    outbox: Mutex<Outbox>,
    wake: Notify,
}

#[derive(Debug, Default)]
struct Outbox {
    /// Encoded messages to send.
    buf: BytesMut,
    /// Requests that wait for a response, by identifier.
    pending: Vec<(u32, oneshot::Sender<Envelope>)>,
    /// Set by `close`.
    close: Option<oneshot::Sender<()>>,
}

impl Shared {
    /// Locks the outbox. A panic while locked cannot leave it invalid.
    fn outbox(&self) -> MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Formats a request identifier without allocating.
fn id_str(mut n: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        // A digit, so the cast is exact.
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[i..]).expect("ASCII digits")
}

impl Client {
    /// Connects to the AirPlay service at `addr` and starts an MRP session.
    pub async fn connect(
        addr: SocketAddr,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error> {
        let span = log::session("mrp", Some(addr));
        Self::start(addr, credentials, options, span.clone())
            .instrument(span)
            .await
    }

    async fn start(
        addr: SocketAddr,
        credentials: &Credentials,
        options: &ClientOptions,
        span: Span,
    ) -> Result<Self, Error> {
        tracing::debug!("connecting");
        let device_id = options
            .device_id
            .clone()
            .unwrap_or_else(|| rng::stable_device_id(credentials.client_id()));
        let identity = Identity {
            name: &options.name,
            model: &options.model,
            os_version: &options.os_version,
            os_build: &options.os_build,
            device_id: &device_id,
        };
        let mut conns = airplay::connect(addr, credentials, &identity, options.timeout).await?;

        // DEVICE_INFO must be answered before anything else is sent.
        let unique = options
            .unique_identifier
            .clone()
            .unwrap_or_else(|| rng::stable_uuid(credentials.client_id()));
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
        let (recording_tx, recording) = watch::channel(0);
        let mut state = State::new(np_tx, volume_tx, recording_tx);
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

        tracing::debug!(
            model = device.model_id.as_deref(),
            build = device.system_build_version.as_deref(),
            "device info received"
        );
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

        let shared = Arc::new(Shared::default());
        let (link_tx, link) = Link::new();
        let (messages, _) = broadcast::channel(MESSAGE_CAPACITY);
        state.publish();
        let task = tokio::spawn(
            run(conns, shared.clone(), link_tx, messages.clone(), state).instrument(span.clone()),
        );
        tracing::debug!("connected");
        let inner = Inner {
            shared,
            link,
            messages,
            now_playing,
            volume,
            recording,
            device,
            output_uid,
            next_id: AtomicU32::new(6),
            voice_device: tokio::sync::OnceCell::new(),
            siri_active: AtomicBool::new(false),
            timeout: options.timeout,
            task,
            span,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Device info from the connection handshake.
    #[must_use]
    pub fn device_info(&self) -> &proto::DeviceInfoMessage {
        &self.inner.device
    }

    /// What the device plays. Updated by the background task.
    #[must_use]
    pub fn now_playing(&self) -> watch::Receiver<NowPlaying> {
        self.inner.now_playing.clone()
    }

    /// Volume and mute state. Updated by the background task.
    #[must_use]
    pub fn volume(&self) -> watch::Receiver<Volume> {
        self.inner.volume.clone()
    }

    /// All received MRP messages.
    ///
    /// A receiver that reads too slowly misses old messages and gets
    /// `RecvError::Lagged`. For state, use [`now_playing`](Self::now_playing)
    /// and [`volume`](Self::volume): they always have the latest value.
    #[must_use]
    pub fn messages(&self) -> broadcast::Receiver<Envelope> {
        self.inner.messages.subscribe()
    }

    /// True until the connection closes.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.link.is_open()
    }

    /// Waits until the connection closes. Returns the error that closed it,
    /// or `None` after [`close`](Self::close).
    pub async fn closed(&self) -> Option<Arc<Error>> {
        self.inner.link.closed().await
    }

    /// Encodes `msg` into the outbox and wakes the background task.
    /// Registers `reply` for the response to `id`.
    fn queue<P: Payload>(
        &self,
        msg: &P,
        id: Option<u32>,
        reply: Option<oneshot::Sender<Envelope>>,
    ) -> Result<(), Error> {
        let mut buf = [0; 10];
        let id_text = id.map(|n| id_str(n, &mut buf));
        {
            let mut out = self.inner.shared.outbox();
            // Checked while locked: the task clears `pending` after the link closes.
            if !self.inner.link.is_open() {
                return Err(Error::Disconnected);
            }
            if out.buf.len() > MAX_OUTBOX {
                return Err(Error::QueueFull);
            }
            encode(msg, id_text, &mut out.buf);
            if let (Some(id), Some(reply)) = (id, reply) {
                out.pending.retain(|(_, tx)| !tx.is_closed());
                out.pending.push((id, reply));
            }
        }
        self.inner.shared.wake.notify_one();
        Ok(())
    }

    /// Sends a message and waits for the response with the same identifier.
    /// Device errors are returned as `Err`.
    pub async fn request<P: Payload>(&self, msg: &P) -> Result<Envelope, Error> {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(parent: &self.inner.span, kind = P::TYPE, id = n, "request");
        let start = std::time::Instant::now();
        let (tx, rx) = oneshot::channel();
        self.queue(msg, Some(n), Some(tx))?;
        let Ok(env) = tokio::time::timeout(self.inner.timeout, rx).await else {
            tracing::debug!(parent: &self.inner.span, kind = P::TYPE, id = n, "request timed out");
            let name = proto::protocol_message::Type::try_from(P::TYPE)
                .map_or("MRP request", |t| t.as_str_name());
            return Err(Error::NoAnswer(name));
        };
        let env = env.map_err(|_| Error::Disconnected)?;
        let elapsed = start.elapsed();
        if let Some(err) = env.error() {
            tracing::debug!(parent: &self.inner.span, kind = P::TYPE, id = n, ?elapsed, %err, "request failed");
            return Err(err.into());
        }
        tracing::trace!(parent: &self.inner.span, kind = P::TYPE, id = n, ?elapsed, "response");
        Ok(env)
    }

    /// Sends a message without waiting for a response.
    // Async, so waiting for queue space can be added without an API change.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn send<P: Payload>(&self, msg: &P) -> Result<(), Error> {
        tracing::trace!(parent: &self.inner.span, kind = P::TYPE, "send");
        self.queue(msg, None, None)
    }

    /// Holds Siri. Send the audio with [`Siri::send`] or `Siri::push_pcm`
    /// (feature `opus`), then release Siri with [`Siri::finish`].
    ///
    /// Returns right after the press. Audio is kept until the device records,
    /// because audio sent before is lost. Sending fails with
    /// [`Error::SiriNotRecording`] after 30 s of kept audio, and
    /// [`Siri::finish`] fails with it after the timeout.
    /// Registers a voice input device on first use.
    /// Fails with [`Error::SiriActive`] while another [`Siri`] exists.
    pub async fn start_siri(&self) -> Result<Siri, Error> {
        if self.inner.siri_active.swap(true, Ordering::AcqRel) {
            return Err(Error::SiriActive);
        }
        let recording = self.inner.recording.clone();
        let start = *recording.borrow();
        // Releases the button and the flag if a step fails or the future is dropped.
        let mut siri = Siri {
            client: self.clone(),
            start,
            recording,
            started: false,
            pending: Vec::new(),
            #[cfg(feature = "opus")]
            encoder: None,
            sample: 0,
            held: false,
        };
        self.voice_device().await?;
        self.button(Button::Siri, true).await?;
        siri.held = true;
        tracing::debug!(parent: &self.inner.span, "Siri pressed");
        Ok(siri)
    }

    async fn voice_device(&self) -> Result<i32, Error> {
        self.inner.voice_device
            .get_or_try_init(|| async {
                let env = self.request(&messages::register_voice_input()).await?;
                let reply: proto::RegisterVoiceInputDeviceResponseMessage = env.payload()?;
                match reply.error_code.unwrap_or(0) {
                    0 => {
                        let id = reply.device_id.unwrap_or(0);
                        tracing::debug!(parent: &self.inner.span, id, "voice input device registered");
                        Ok(id)
                    }
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

    /// Sends a button down or up event.
    pub async fn button(&self, button: Button, down: bool) -> Result<(), Error> {
        self.send(&button.event(down)).await
    }

    /// Presses and releases a button.
    pub async fn press(&self, button: Button) -> Result<(), Error> {
        self.button(button, true).await?;
        self.button(button, false).await
    }

    /// Holds a button for `duration`. The button is released also if the
    /// returned future is dropped.
    pub async fn hold(&self, button: Button, duration: Duration) -> Result<(), Error> {
        let mut held = Held {
            client: self,
            button,
            armed: true,
        };
        self.button(button, true).await?;
        tokio::time::sleep(duration).await;
        held.armed = false;
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
    pub async fn skip(&self, seconds: f64) -> Result<(), Error> {
        // The protocol field is a float.
        #[allow(clippy::cast_possible_truncation)]
        self.run_command(&messages::skip(seconds as f32)).await
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
    /// A size of -1 keeps the aspect ratio. This is not the same as
    /// [`NowPlaying::artwork_url_sized`], which uses 0. `None` if the item has no image.
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
        let volume = crate::volume_level(volume)?;
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
        let msg = match self.inner.now_playing.borrow().subtitle_language.as_ref() {
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
        let volume = crate::volume_level(volume)?;
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
        self.inner
            .output_uid
            .as_deref()
            .ok_or(Error::Protocol("device has no output device UID"))
    }

    /// Ends the session and closes the connections of all clones.
    pub async fn close(&self) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        {
            let mut out = self.inner.shared.outbox();
            // Checked while locked: the task drops `close` after the link closes.
            if !self.inner.link.is_open() {
                return Ok(());
            }
            out.close = Some(tx);
        }
        self.inner.shared.wake.notify_one();
        let _ = rx.await;
        self.inner.link.closed().await;
        Ok(())
    }
}

/// Releases a button when dropped while armed.
struct Held<'a> {
    client: &'a Client,
    button: Button,
    armed: bool,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.client.queue(&self.button.event(false), None, None);
        }
    }
}

async fn wait_device_info(
    conns: &mut Connections,
    state: &mut State,
) -> Result<proto::DeviceInfoMessage, Error> {
    let mut device = None;
    loop {
        if conns.stream.has_transmit() {
            tcp::write_all(&mut conns.data, &conns.stream.take_transmit()).await?;
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
        state.publish();
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
    recording_tx: watch::Sender<u32>,
    /// Item that details were requested for.
    item_id: Option<String>,
    /// Now playing changed since the last `publish`.
    now_playing_changed: bool,
    /// Volume changed since the last `publish`.
    volume_changed: bool,
}

impl State {
    fn new(
        now_playing_tx: watch::Sender<NowPlaying>,
        volume_tx: watch::Sender<Volume>,
        recording_tx: watch::Sender<u32>,
    ) -> Self {
        Self {
            now_playing: NowPlayingState::new(),
            now_playing_tx,
            volume: VolumeState::new(),
            volume_tx,
            recording_tx,
            item_id: None,
            now_playing_changed: false,
            volume_changed: false,
        }
    }

    /// Updates the state. Call [`publish`](Self::publish) after a batch of messages.
    fn apply(&mut self, env: &Envelope) {
        match self.now_playing.apply(env) {
            Ok(changed) => self.now_playing_changed |= changed,
            Err(err) => tracing::debug!(%err, kind = env.kind(), "invalid now playing message"),
        }
        match self.volume.apply(env) {
            Ok(changed) => self.volume_changed |= changed,
            Err(err) => tracing::debug!(%err, kind = env.kind(), "invalid volume message"),
        }
        if is_recording(env) {
            self.recording_tx.send_modify(|n| *n = n.wrapping_add(1));
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

    /// Publishes what changed since the last call.
    fn publish(&mut self) {
        if std::mem::take(&mut self.now_playing_changed) {
            publish(&self.now_playing_tx, self.now_playing.now_playing());
        }
        if std::mem::take(&mut self.volume_changed) {
            publish(&self.volume_tx, self.volume.volume().clone());
        }
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
    shared: Arc<Shared>,
    link: LinkTx,
    messages: broadcast::Sender<Envelope>,
    state: State,
) {
    let reason = match run_inner(conns, &shared, &messages, state).await {
        Ok(()) => {
            tracing::debug!("connection closed");
            None
        }
        Err(err) => {
            tracing::debug!(%err, "connection ended");
            Some(Arc::new(err))
        }
    };
    link.close(reason);
    // Waiting requests fail and `close` returns right away.
    let mut out = shared.outbox();
    out.pending.clear();
    out.close = None;
}

async fn run_inner(
    conns: Connections,
    shared: &Shared,
    messages: &broadcast::Sender<Envelope>,
    mut state: State,
) -> Result<(), Error> {
    let Connections {
        mut control,
        mut channel,
        mut session,
        mut events,
        mut event_channel,
        mut data,
        mut stream,
    } = conns;
    let mut details = BytesMut::new();
    let mut received = Vec::new();
    let start = tokio::time::Instant::now() + FEEDBACK_INTERVAL;
    let mut feedback = tokio::time::interval_at(start, FEEDBACK_INTERVAL);
    let mut last_control = tokio::time::Instant::now();

    loop {
        // Pushed items often lack languages, chapters and next items.
        if state.item_changed() {
            details.clear();
            encode(
                &messages::playback_queue_details(QUEUE_LENGTH),
                Some("details"),
                &mut details,
            );
            stream.send(&details);
        }
        if stream.has_transmit() {
            tcp::write_all(&mut data, &stream.take_transmit()).await?;
        }
        tokio::select! {
            () = shared.wake.notified() => {
                let (chunk, close) = {
                    let mut out = shared.outbox();
                    (out.buf.split(), out.close.take())
                };
                // Sent before TEARDOWN, so a release queued before `close` arrives.
                stream.send(&chunk);
                if let Some(done) = close {
                    if stream.has_transmit() {
                        tcp::write_all(&mut data, &stream.take_transmit()).await?;
                    }
                    airplay::teardown(&mut control, &mut channel, &mut session).await;
                    let _ = done.send(());
                    return Ok(());
                }
            },
            _ = feedback.tick() => {
                if last_control.elapsed() > FEEDBACK_TIMEOUT {
                    tracing::debug!("no answer to feedback");
                    return Err(Error::NoAnswer("feedback"));
                }
                tracing::trace!("feedback");
                session.feedback(&mut channel);
                airplay::flush(&mut control, &mut channel).await?;
            }
            n = control.read_buf(channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                last_control = tokio::time::Instant::now();
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
                        let env = env?;
                        state.apply(&env);
                        received.push(env);
                    }
                }
                // Once per read, so a burst of updates builds the state once.
                // Before the replies, so a caller sees the new state.
                state.publish();
                // `drain` keeps the capacity for the next read.
                #[allow(clippy::iter_with_drain)]
                for env in received.drain(..) {
                    dispatch(env, shared, messages);
                }
            }
        }
    }
}

fn dispatch(env: Envelope, shared: &Shared, messages: &broadcast::Sender<Envelope>) {
    tracing::trace!(kind = env.kind(), id = ?env.identifier(), "received");
    #[cfg(feature = "sensitive-logs")]
    tracing::trace!(?env, "received");
    if let Some(id) = env.identifier().and_then(|id| id.parse::<u32>().ok()) {
        let reply = {
            let mut out = shared.outbox();
            out.pending
                .iter()
                .position(|(p, _)| *p == id)
                .map(|i| out.pending.swap_remove(i).1)
        };
        if let Some(reply) = reply {
            let _ = reply.send(env.clone());
        }
    }
    let _ = messages.send(env);
}
