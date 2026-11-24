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

use std::collections::VecDeque;
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
    Fields, NowPlayingState, Payload, VolumeState, encode, encode_type, frames, messages, proto,
};
use tokio::io::AsyncReadExt;
use tokio::sync::{Notify, broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{Instrument, Span};

use ratv_airplay::Channel;
use ratv_airplay::http::Message;
use ratv_airplay::rtsp::{self, Session};
use ratv_codec::opack::Encoder;
use ratv_hap::pair_setup::{self, ClientIdentity, NeedPin};
use tokio::net::TcpStream;

use crate::airplay::{self, Connections, Identity, READ_CHUNK};
use crate::link::{Batches, CloseGuard, Link, LinkTx, Queued};
pub use crate::{ClientOptions, PairOptions};
use crate::{Credentials, Error, log, rng, tcp};

/// Encoded messages that are not sent yet. Checked before each push, so the
/// outbox can be larger than this by one message. Button and Siri releases skip it.
const MAX_OUTBOX: usize = 1 << 20;
const MESSAGE_CAPACITY: usize = 64;
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// The device answers each feedback request. When the first request sent
/// after the last control data is this old, the connection is dead.
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Voice packets kept until the device records: 30 s.
const MAX_PENDING: usize = 1500;
/// Siri listening to voice input, from [`Client::start_siri`].
///
/// Audio is kept until the device starts recording, then sent.
/// Siri is released when this is dropped, but call [`finish`](Self::finish)
/// to send the rest of the audio and to know whether the release failed.
/// It holds a [`Client`] clone, so dropping the other clones does not stop the connection.
#[must_use = "Siri stays active until `finish` is called or this is dropped"]
pub struct Siri {
    client: Client,
    /// Recording count when Siri was pressed.
    start: u32,
    recording: watch::Receiver<u32>,
    /// True after the device started recording.
    started: bool,
    /// Packets kept until the device records.
    pending: VecDeque<Bytes>,
    #[cfg(feature = "opus")]
    /// Boxed because it is large.
    encoder: Option<Box<ratv_mrp::VoiceEncoder>>,
    /// Sample number of the next packet.
    sample: u32,
    /// True while the button is held.
    held: bool,
    /// When [`Client::start_siri`] was called.
    begin: Instant,
    /// Audio bytes sent.
    bytes: usize,
    /// Times since `begin` when the first and the last packet were sent.
    sent: Option<(Duration, Duration)>,
}

impl Siri {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms. A packet over 65535 bytes gives
    /// [`Error::InvalidArgument`], as over Companion. On a closed
    /// connection, fails with [`Error::Closed`]. After 30 s of audio kept
    /// while the device does not record, fails with
    /// [`Error::SiriNotRecording`].
    /// On [`Error::QueueFull`] the packet is kept and sent with the next
    /// call; do not send it again.
    pub async fn send(&mut self, packet: impl Into<Bytes>) -> Result<(), Error> {
        let packet = packet.into();
        if packet.len() > usize::from(u16::MAX) {
            return Err(Error::InvalidArgument("packet size"));
        }
        self.check_open()?;
        self.pending.push_back(packet);
        self.send_pending().await
    }

    /// Sends signed 16-bit, 16 kHz, mono PCM in chunks of any size.
    /// Do not mix with [`send`](Self::send). Fails with [`Error::Closed`] and
    /// [`Error::SiriNotRecording`] as [`send`](Self::send) does.
    /// On [`Error::QueueFull`] the audio is kept and sent with the next call;
    /// do not push it again.
    #[cfg(feature = "opus")]
    pub async fn push_pcm(&mut self, pcm: &[i16]) -> Result<(), Error> {
        self.check_open()?;
        let pending = &mut self.pending;
        self.encoder
            .get_or_insert_with(Box::default)
            .push(pcm, |p| pending.push_back(Bytes::copy_from_slice(p)))?;
        self.send_pending().await
    }

    /// Sends the rest of the audio and releases Siri. The device then answers.
    ///
    /// Waits for the device to record if it has not started yet.
    /// Fails with [`Error::SiriNoAudio`] if no audio was sent, for example
    /// because none was given, with [`Error::SiriNotRecording`] if the device
    /// does not record within the timeout, and with [`Error::Closed`] on a
    /// closed connection.
    pub async fn finish(mut self) -> Result<(), Error> {
        #[cfg(feature = "opus")]
        if let Some(encoder) = &mut self.encoder {
            let pending = &mut self.pending;
            encoder.flush(|p| pending.push_back(Bytes::copy_from_slice(p)))?;
        }
        if self.pending.is_empty() && self.sample == 0 {
            let result = self.client.release(Button::Siri).await;
            self.held = false;
            tracing::debug!(parent: &self.client.inner.span, elapsed = ?self.begin.elapsed(), "Siri released without audio");
            return result.and(Err(Error::SiriNoAudio));
        }
        if !self.pending.is_empty() && !self.is_recording() {
            let (start, client) = (self.start, self.client.clone());
            let wait = self.recording.wait_for(|&n| n != start);
            match tokio::time::timeout(client.inner.timeout, wait).await {
                // `send_pending` sees the change.
                Ok(Ok(_)) => {}
                // The rest of the audio and the release were not sent.
                Ok(Err(_)) => return Err(Error::Closed),
                Err(_) => {
                    tracing::debug!(parent: &client.inner.span, elapsed = ?self.begin.elapsed(), "Siri did not start recording");
                    return Err(Error::SiriNotRecording);
                }
            }
        }
        self.send_pending().await?;
        let result = self.client.release(Button::Siri).await;
        self.held = false;
        tracing::debug!(
            parent: &self.client.inner.span,
            elapsed = ?self.begin.elapsed(),
            packets = self.sample / messages::VOICE_FRAMES,
            last_sample = self.sample.saturating_sub(messages::VOICE_FRAMES),
            bytes = self.bytes,
            first_sent = ?self.sent.map(|s| s.0),
            last_sent = ?self.sent.map(|s| s.1),
            "Siri released"
        );
        result
    }

    /// True once the device has started recording.
    fn is_recording(&mut self) -> bool {
        if !self.started && *self.recording.borrow() != self.start {
            self.started = true;
            tracing::debug!(parent: &self.client.inner.span, elapsed = ?self.begin.elapsed(), kept = self.pending.len(), "Siri recording");
        }
        self.started
    }

    /// Fails with `Closed` on a closed connection. Nothing is kept then.
    fn check_open(&mut self) -> Result<(), Error> {
        if self.client.is_connected() {
            return Ok(());
        }
        self.pending.clear();
        Err(Error::Closed)
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
        while let Some(packet) = self.pending.front() {
            let len = packet.len();
            let msg = messages::voice_input(packet.clone(), self.sample);
            self.client.send(&msg).await?;
            let elapsed = self.begin.elapsed();
            if self.sample == 0 {
                tracing::debug!(parent: &self.client.inner.span, ?elapsed, audio = len, fields = %VoiceFields(&msg), "first voice packet");
            }
            self.sent = Some((self.sent.map_or(elapsed, |s| s.0), elapsed));
            self.bytes += len;
            // Sent packets are not sent again.
            self.pending.pop_front();
            self.sample = self.sample.wrapping_add(messages::VOICE_FRAMES);
        }
        Ok(())
    }
}

impl Drop for Siri {
    fn drop(&mut self) {
        if self.held {
            tracing::debug!(parent: &self.client.inner.span, "Siri released on drop");
            let _ = self
                .client
                .queue(&Button::Siri.event(false), None, None, true);
        }
        let shared = &self.client.inner.shared;
        // Cleared first, so the next Siri never logs the time of this one.
        *shared
            .siri_begin
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        shared.siri_active.store(false, Ordering::Release);
    }
}

/// Name of a message type, for logs.
fn type_name(kind: i32) -> &'static str {
    proto::protocol_message::Type::try_from(kind).map_or("unknown", |t| t.as_str_name())
}

/// Logs a message received while Siri is active, to find messages that tvOS sends for voice input.
/// Shows the payload fields; bytes that are not text or messages are shown as length only.
fn log_siri_message(env: &Envelope, elapsed: Duration) {
    tracing::debug!(
        kind = env.kind(),
        name = type_name(env.kind()),
        ?elapsed,
        error = ?env.error(),
        fields = %env.payload_bytes().map_or(Fields(&[]), |b| Fields(b)),
        "received during Siri"
    );
}

/// Shows the fields of a payload, for logs. Encodes only when shown.
///
/// Do not use `tracing::enabled!` to skip the work: it is false when the
/// events go to the `log` crate.
struct PayloadFields<'a, P>(&'a P);

impl<P: Payload> core::fmt::Display for PayloadFields<'_, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Fields(&self.0.encode_to_vec()).fmt(f)
    }
}

/// Shows the fields of a voice packet without the audio, for logs.
struct VoiceFields<'a>(&'a proto::SendVoiceInputMessage);

impl core::fmt::Display for VoiceFields<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut msg = self.0.clone();
        if let Some(b) = msg.data_block.as_mut().and_then(|d| d.buffer.as_mut()) {
            b.contents = None;
        }
        PayloadFields(&msg).fmt(f)
    }
}

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
    let local = stream.local_addr()?.ip();
    let mut link = PairLink {
        stream,
        channel: Channel::new(),
        session: Session::new(local, rng::u32(), rng::u64(), rng::u32()),
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
/// session on the device. The task also stops when the device disconnects or
/// does not answer feedback requests. A [`Siri`] holds a clone, so
/// dropping the other clones does not stop the connection.
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    shared: Arc<Shared>,
    link: Link,
    /// Owned by the task, so receivers close when it ends.
    messages: broadcast::WeakSender<Envelope>,
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
    /// True while a [`Siri`] exists.
    siri_active: AtomicBool,
    /// When the active [`Siri`] began.
    siri_begin: Mutex<Option<Instant>>,
}

#[derive(Debug, Default)]
struct Outbox {
    /// Encoded messages to send.
    buf: BytesMut,
    /// Counts how often the task took the buffer, and tracks removed messages.
    batches: Batches,
    /// Requests that wait for a response, by identifier.
    pending: Vec<(u32, oneshot::Sender<Envelope>)>,
    /// Set by `close`.
    close: Option<oneshot::Sender<()>>,
}

impl Outbox {
    /// Removes a message if the task has not taken it. True if removed.
    fn remove(&mut self, queued: Queued) -> bool {
        let len = self.batches.remove(queued, &mut self.buf);
        if let Some(len) = len {
            self.buf.truncate(len);
        }
        len.is_some()
    }
}

impl Shared {
    /// Locks the outbox. A panic while locked cannot leave it invalid.
    fn outbox(&self) -> MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Time since the active [`Siri`] began, or `None` without one.
    fn siri_elapsed(&self) -> Option<Duration> {
        if !self.siri_active.load(Ordering::Acquire) {
            return None;
        }
        let begin = *self
            .siri_begin
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        begin.map(|b| b.elapsed())
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
        options
            .connect_with_retry(|step| Self::start(addr, credentials, options, step, span.clone()))
            .instrument(span.clone())
            .await
    }

    /// `step` is the time to wait for each connect step.
    async fn start(
        addr: SocketAddr,
        credentials: &Credentials,
        options: &ClientOptions,
        step: Duration,
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
        let mut conns = airplay::connect(addr, credentials, &identity, step).await?;

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
        let device = tokio::time::timeout(step, wait_device_info(&mut conns, &mut state))
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
            messages: messages.downgrade(),
            now_playing,
            volume,
            recording,
            device,
            output_uid,
            next_id: AtomicU32::new(6),
            voice_device: tokio::sync::OnceCell::new(),
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

    /// What the device plays. Updated by the background task. After the
    /// connection closes, the receiver keeps the last value and closes.
    #[must_use]
    pub fn now_playing(&self) -> watch::Receiver<NowPlaying> {
        self.inner.now_playing.clone()
    }

    /// Volume and mute state. Updated by the background task. After the
    /// connection closes, the receiver keeps the last value and closes.
    #[must_use]
    pub fn volume(&self) -> watch::Receiver<Volume> {
        self.inner.volume.clone()
    }

    /// All received MRP messages.
    ///
    /// A receiver that reads too slowly misses old messages and gets
    /// `RecvError::Lagged`. For state, use [`now_playing`](Self::now_playing)
    /// and [`volume`](Self::volume): they always have the latest value.
    /// The receiver closes when the connection closes.
    #[must_use]
    pub fn messages(&self) -> broadcast::Receiver<Envelope> {
        crate::subscribe(&self.inner.messages)
    }

    /// True until the connection closes.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.link.is_open()
    }

    /// Waits until the connection closes. Returns the error that closed it,
    /// or `None` after [`close`](Self::close). After `close`, a write error
    /// or [`Error::NoAnswer`] can still be returned when the device did not
    /// read, or [`Error::Disconnected`] when the device closed the
    /// connection first. Returns at once when the connection is already
    /// closed.
    pub async fn closed(&self) -> Option<Arc<Error>> {
        self.inner.link.closed().await
    }

    /// Encodes `msg` into the outbox and wakes the background task.
    /// Registers `reply` for the response to `id`. A `release` skips the
    /// outbox size limit.
    fn queue<P: Payload>(
        &self,
        msg: &P,
        id: Option<u32>,
        reply: Option<oneshot::Sender<Envelope>>,
        release: bool,
    ) -> Result<Queued, Error> {
        let mut buf = [0; 10];
        let id_text = id.map(|n| id_str(n, &mut buf));
        // Voice packets are logged by `Siri`.
        if P::TYPE != proto::SendVoiceInputMessage::TYPE
            && let Some(elapsed) = self.inner.shared.siri_elapsed()
        {
            tracing::debug!(parent: &self.inner.span, kind = P::TYPE, name = type_name(P::TYPE), ?elapsed, fields = %PayloadFields(msg), "sent during Siri");
        }
        let queued = {
            let mut out = self.inner.shared.outbox();
            // Checked while locked: the task clears `pending` after the link closes.
            if !self.inner.link.is_open() {
                return Err(Error::Closed);
            }
            let start = out.buf.len();
            if !release && start > MAX_OUTBOX {
                return Err(Error::QueueFull);
            }
            encode(msg, id_text, &mut out.buf);
            if let (Some(id), Some(reply)) = (id, reply) {
                out.pending.retain(|(_, tx)| !tx.is_closed());
                out.pending.push((id, reply));
            }
            out.batches.queued(start, out.buf.len())
        };
        self.inner.shared.wake.notify_one();
        Ok(queued)
    }

    /// Sends a message and waits for the response with the same identifier.
    /// Device errors are returned as `Err`.
    ///
    /// Fails with [`Error::Closed`] if the connection was closed before the
    /// request was sent, with [`Error::QueueFull`] if the outbox is full, and
    /// with [`Error::SendTimeout`] if the request was still in the outbox
    /// after the time limit.
    /// All three mean that it was not sent, see [`Error::is_not_sent`].
    /// Fails with [`Error::NoAnswer`] if the request left the outbox but got
    /// no answer in time; it may still be in the write, or it may have been
    /// sent. Fails with [`Error::Disconnected`] if the connection closed after
    /// the request left the outbox; then it may have been sent.
    pub async fn request<P: Payload>(&self, msg: &P) -> Result<Envelope, Error> {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(parent: &self.inner.span, kind = P::TYPE, id = n, "request");
        let start = std::time::Instant::now();
        let (tx, rx) = oneshot::channel();
        let queued = self.queue(msg, Some(n), Some(tx), false)?;
        let Ok(env) = tokio::time::timeout(self.inner.timeout, rx).await else {
            let removed = self.inner.shared.outbox().remove(queued);
            tracing::debug!(parent: &self.inner.span, kind = P::TYPE, id = n, removed, "request timed out");
            let name = proto::protocol_message::Type::try_from(P::TYPE)
                .map_or("MRP request", |t| t.as_str_name());
            return Err(if removed {
                Error::SendTimeout(name)
            } else {
                Error::NoAnswer(name)
            });
        };
        let Ok(env) = env else {
            // The connection closed. A message that is still queued was not sent.
            let removed = self.inner.shared.outbox().remove(queued);
            return Err(if removed {
                Error::Closed
            } else {
                Error::Disconnected
            });
        };
        let elapsed = start.elapsed();
        if let Some(err) = env.error() {
            tracing::debug!(parent: &self.inner.span, kind = P::TYPE, id = n, ?elapsed, %err, "request failed");
            return Err(err.into());
        }
        tracing::trace!(parent: &self.inner.span, kind = P::TYPE, id = n, ?elapsed, "response");
        Ok(env)
    }

    /// Sends a message without waiting for a response. `Ok` only means that
    /// the message was queued. Fails with [`Error::Closed`] or
    /// [`Error::QueueFull`]; then it was not sent.
    // Async, so waiting for queue space can be added without an API change.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn send<P: Payload>(&self, msg: &P) -> Result<(), Error> {
        tracing::trace!(parent: &self.inner.span, kind = P::TYPE, "send");
        self.queue(msg, None, None, false).map(drop)
    }

    /// Sends a button up event. Skips the outbox size limit, so a held
    /// button is released also when the outbox is full.
    // Async like `send`.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn release(&self, button: Button) -> Result<(), Error> {
        let msg = button.event(false);
        tracing::trace!(parent: &self.inner.span, kind = proto::SendButtonEventMessage::TYPE, "release");
        self.queue(&msg, None, None, true).map(drop)
    }

    /// Holds Siri. Send the audio with [`Siri::send`] or `Siri::push_pcm`
    /// (feature `opus`), then release Siri with [`Siri::finish`].
    ///
    /// Returns right after the press. Audio is kept until the device records,
    /// because audio sent before is lost. Sending fails with
    /// [`Error::SiriNotRecording`] after 30 s of kept audio, and
    /// [`Siri::finish`] fails with it after the timeout. On a closed
    /// connection, this call and sending fail with [`Error::Closed`].
    /// The device does not record when Siri is off in its settings.
    /// Registers a voice input device on first use.
    /// Fails with [`Error::SiriActive`] while another [`Siri`] exists.
    pub async fn start_siri(&self) -> Result<Siri, Error> {
        // A `Siri` kept over a close would give `SiriActive`.
        if !self.is_connected() {
            return Err(Error::Closed);
        }
        if self.inner.shared.siri_active.swap(true, Ordering::AcqRel) {
            return Err(Error::SiriActive);
        }
        let begin = Instant::now();
        *self
            .inner
            .shared
            .siri_begin
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(begin);
        let recording = self.inner.recording.clone();
        let start = *recording.borrow();
        // Releases the button and the flag if a step fails or the future is dropped.
        let mut siri = Siri {
            client: self.clone(),
            start,
            recording,
            started: false,
            pending: VecDeque::new(),
            #[cfg(feature = "opus")]
            encoder: None,
            sample: 0,
            held: false,
            begin,
            bytes: 0,
            sent: None,
        };
        let device = self.voice_device().await?;
        self.button(Button::Siri, true).await?;
        siri.held = true;
        tracing::debug!(parent: &self.inner.span, elapsed = ?begin.elapsed(), device, "Siri pressed");
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
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn button(&self, button: Button, down: bool) -> Result<(), Error> {
        self.send(&button.event(down)).await
    }

    /// Presses and releases a button.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn press(&self, button: Button) -> Result<(), Error> {
        // Releases the button if the future is dropped. `send` does not wait
        // today, but may wait in a later version.
        let mut held = Held {
            client: self,
            button,
            armed: true,
        };
        held.down().await?;
        held.release().await
    }

    /// Presses and releases a button two times fast.
    /// The device does not answer, so `Ok` only means that the message was queued.
    /// When it fails, the first click can still be sent, also with an error
    /// where [`Error::is_not_sent`] is true.
    pub async fn double_tap(&self, button: Button) -> Result<(), Error> {
        self.press(button).await?;
        self.press(button).await
    }

    /// Holds a button for `duration`. The button is released also if the
    /// returned future is dropped.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn hold(&self, button: Button, duration: Duration) -> Result<(), Error> {
        let mut held = Held {
            client: self,
            button,
            armed: true,
        };
        held.down().await?;
        tokio::time::sleep(duration).await;
        held.release().await
    }

    /// Sends a playback command, for example [`Command::Play`].
    pub async fn command(&self, command: Command) -> Result<(), Error> {
        self.run_command(&messages::command(command)).await
    }

    /// Seeks to `position` seconds. A position that is not finite gives
    /// [`Error::InvalidArgument`].
    pub async fn seek(&self, position: f64) -> Result<(), Error> {
        let position = crate::finite(position, "position")?;
        self.run_command(&messages::seek(position)).await
    }

    /// Skips `seconds` forward, or backward if negative. A value that is not
    /// finite as `f32` gives [`Error::InvalidArgument`].
    pub async fn skip(&self, seconds: f64) -> Result<(), Error> {
        // The protocol field is a float.
        #[allow(clippy::cast_possible_truncation)]
        let seconds = seconds as f32;
        crate::finite(f64::from(seconds), "seconds")?;
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

    /// Requests the image of the current item in about this size in pixels.
    /// A size of 0 keeps the aspect ratio, as in [`NowPlaying::artwork_url_sized`].
    /// `None` if the item has no image.
    ///
    /// Right after connect the device may not answer with an image yet.
    /// Wait until [`NowPlaying::artwork_id`] is set, and fetch again when it changes.
    pub async fn artwork(&self, width: u32, height: u32) -> Result<Option<Artwork>, Error> {
        // The device keeps the aspect ratio for -1.
        let size = |v: u32| if v == 0 { -1.0 } else { f64::from(v) };
        let env = self
            .request(&messages::playback_queue_request(size(width), size(height)))
            .await?;
        Ok(ratv_mrp::artwork(&env)?)
    }

    /// Sets the volume of the device, 0.0 to 1.0. Fails with
    /// [`Error::VolumeNotAvailable`] if [`Volume::available`] is false. NaN
    /// gives [`Error::InvalidArgument`]; other values are limited to 0.0 to
    /// 1.0. On a closed connection, fails with [`Error::Closed`].
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn set_volume(&self, volume: f32) -> Result<(), Error> {
        let volume = crate::volume_level(volume)?;
        self.check_volume()?;
        self.send(&messages::set_volume(volume, self.output_uid()?))
            .await
    }

    /// Fails if the device cannot change the volume, see [`Volume::available`].
    fn check_volume(&self) -> Result<(), Error> {
        // The last value of a closed connection would give `VolumeNotAvailable`.
        if !self.is_connected() {
            return Err(Error::Closed);
        }
        if self.inner.volume.borrow().available == Some(false) {
            return Err(Error::VolumeNotAvailable);
        }
        Ok(())
    }

    /// Mutes or unmutes the device. Fails with [`Error::VolumeNotAvailable`]
    /// if [`Volume::available`] is false, and with [`Error::Closed`] on a
    /// closed connection.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn set_muted(&self, muted: bool) -> Result<(), Error> {
        self.check_volume()?;
        self.send(&messages::set_volume_muted(muted, self.output_uid()?))
            .await
    }

    /// Sets the playback rate, 1.0 is normal speed.
    /// Rates the app accepts are in [`NowPlaying::supported_rates`], normal speeds in
    /// [`NowPlaying::playback_speeds`].
    /// A rate that is not finite gives [`Error::InvalidArgument`].
    pub async fn set_playback_rate(&self, rate: f32) -> Result<(), Error> {
        crate::finite(f64::from(rate), "rate")?;
        self.run_command(&messages::playback_rate(rate)).await
    }

    /// Rates the current item. The range is in [`NowPlaying::rating_range`].
    /// A rating that is not finite gives [`Error::InvalidArgument`].
    pub async fn set_rating(&self, rating: f32) -> Result<(), Error> {
        crate::finite(f64::from(rating), "rating")?;
        self.run_command(&messages::rate_track(rating)).await
    }

    /// Likes the current item, or removes the like. See [`NowPlaying::liked`].
    pub async fn set_liked(&self, liked: bool) -> Result<(), Error> {
        self.run_command(&messages::like_track(liked)).await
    }

    /// Dislikes the current item, or removes the dislike. See [`NowPlaying::disliked`].
    pub async fn set_disliked(&self, disliked: bool) -> Result<(), Error> {
        self.run_command(&messages::dislike_track(disliked)).await
    }

    /// Turns on an audio or subtitle language from [`NowPlaying::audio_languages`]
    /// or [`NowPlaying::subtitle_languages`].
    pub async fn enable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.run_command(&messages::enable_language(option)).await
    }

    /// Turns off a language. To turn off subtitles, use [`disable_subtitles`](Self::disable_subtitles),
    /// which also works when subtitles are automatic.
    pub async fn disable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.run_command(&messages::disable_language(option)).await
    }

    /// Turns off subtitles.
    pub async fn disable_subtitles(&self) -> Result<(), Error> {
        // With only the type, the device can show another subtitle track instead.
        let msg = {
            let np = self.inner.now_playing.borrow();
            match np.subtitle_language.as_ref() {
                Some(o) if !np.subtitles_auto() => messages::disable_language(o),
                _ => messages::disable_subtitles(),
            }
        };
        self.run_command(&msg).await
    }

    /// Plays `NowPlaying::up_next[index]`.
    pub async fn play_queue_item(&self, index: usize) -> Result<(), Error> {
        let offset = i32::try_from(index).map_or(i32::MAX, |i| i.saturating_add(1));
        self.run_command(&messages::play_queue_offset(offset)).await
    }

    /// Sets the volume, 0.0 to 1.0, of one output device. UIDs are in [`Volume::outputs`].
    /// NaN gives [`Error::InvalidArgument`]; other values are limited to 0.0 to 1.0.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn set_output_volume(&self, uid: &str, volume: f32) -> Result<(), Error> {
        let volume = crate::volume_level(volume)?;
        self.send(&messages::set_volume(volume, uid)).await
    }

    /// Adds AirPlay output devices, for example `HomePods`, to the group.
    /// The new list arrives in [`Volume::outputs`].
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn add_outputs(&self, uids: &[&str]) -> Result<(), Error> {
        self.send(&messages::add_output_devices(uids)).await
    }

    /// Removes AirPlay output devices from the group.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn remove_outputs(&self, uids: &[&str]) -> Result<(), Error> {
        self.send(&messages::remove_output_devices(uids)).await
    }

    /// Replaces the output devices of the group.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn set_outputs(&self, uids: &[&str]) -> Result<(), Error> {
        self.send(&messages::set_output_devices(uids)).await
    }

    fn output_uid(&self) -> Result<&str, Error> {
        self.inner
            .output_uid
            .as_deref()
            .ok_or(Error::Protocol("device has no output device UID"))
    }

    /// Ends the session and closes the connections of all clones. Returns
    /// `Ok` at once when the connection is already closed.
    ///
    /// When the device does not read, this can take about one or two 10 s
    /// write limits plus 1 s for TEARDOWN. Calls on other clones while this
    /// runs can return `Ok` but not be sent.
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

impl Held<'_> {
    /// Sends the press. If it was not sent, no release is sent either.
    async fn down(&mut self) -> Result<(), Error> {
        let result = self.client.button(self.button, true).await;
        if result.as_ref().is_err_and(Error::is_not_sent) {
            self.armed = false;
        }
        result
    }

    /// Sends the release and disarms the guard.
    async fn release(&mut self) -> Result<(), Error> {
        let result = self.client.release(self.button).await;
        self.armed = false;
        result
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .client
                .queue(&self.button.event(false), None, None, true);
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
    /// Bundle id and player id that the current item was requested for.
    item_requested: Option<(String, String)>,
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
            item_requested: None,
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
        if env.is::<proto::SetRecordingStateMessage>() {
            use proto::set_recording_state_message::RecordingState;

            let state = env
                .payload::<proto::SetRecordingStateMessage>()
                .ok()
                .and_then(|m| m.state);
            tracing::debug!(?state, "recording state");
            if state == Some(RecordingState::Recording as i32) {
                self.recording_tx.send_modify(|n| *n = n.wrapping_add(1));
            }
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

    /// True when the active player has a state but no item, and it is not
    /// the player that the item was requested for last. A reply without an
    /// item does not make it true again. Once the player has an item, or after
    /// another player was active, it can be asked again later.
    fn item_missing(&mut self) -> bool {
        let Some(ratv_mrp::PlayerRef {
            bundle_id,
            player_id,
            ..
        }) = self.now_playing.player_without_item()
        else {
            self.item_requested = None;
            return false;
        };
        if self
            .item_requested
            .as_ref()
            .is_some_and(|(b, p)| b == bundle_id && p == player_id)
        {
            return false;
        }
        self.item_requested = Some((bundle_id.to_owned(), player_id.to_owned()));
        true
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
    mut state: State,
) {
    // Waiting requests fail and `close` returns at once. A local, so it is
    // dropped before `state`: the link closes before the value receivers
    // close.
    let mut guard = CloseGuard::new(link, || {
        let mut out = shared.outbox();
        out.pending.clear();
        out.close = None;
    });
    guard.reason = match run_inner(conns, &shared, &messages, &mut state).await {
        Ok(()) => {
            tracing::debug!("connection closed");
            None
        }
        Err(err) => {
            tracing::debug!(%err, "connection ended");
            Some(Arc::new(err))
        }
    };
}

/// Reads the control data that tokio has seen as ready, without waiting
/// for more. True if any was read.
fn read_waiting(control: &TcpStream, channel: &mut Channel) -> Result<bool, Error> {
    let mut read = false;
    loop {
        match control.try_read_buf(channel.receive_buffer(READ_CHUNK)) {
            Ok(0) => return Err(Error::Disconnected),
            Ok(_) => read = true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.into()),
        }
    }
    if read {
        control_replies(channel)?;
    }
    Ok(read)
}

/// Reads the control replies. They are only logged when not OK.
fn control_replies(channel: &mut Channel) -> Result<(), Error> {
    while let Some(m) = channel.poll_message()? {
        if m.status() != Some(200) {
            tracing::debug!(status = ?m.status(), "control response");
        }
    }
    Ok(())
}

/// Timer for feedback requests. No burst of them after a long write.
fn feedback_timer(start: Instant) -> tokio::time::Interval {
    let mut timer = tokio::time::interval_at(start, FEEDBACK_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timer
}

async fn run_inner(
    conns: Connections,
    shared: &Shared,
    messages: &broadcast::Sender<Envelope>,
    state: &mut State,
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
    let start = Instant::now() + FEEDBACK_INTERVAL;
    let mut feedback = feedback_timer(start);
    // Time of the first feedback request sent after the last control data.
    let mut unanswered: Option<Instant> = None;

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
        // A new connection gets no item for a paused player in the background.
        // Also asked later, when the active player has no item.
        if state.item_missing() {
            details.clear();
            encode(&messages::playback_queue_item(), Some("item"), &mut details);
            stream.send(&details);
        }
        if stream.has_transmit() {
            tcp::write_all(&mut data, &stream.take_transmit()).await?;
        }
        tokio::select! {
            n = control.read_buf(channel.receive_buffer(READ_CHUNK)) => {
                if n? == 0 {
                    return Err(Error::Disconnected);
                }
                unanswered = None;
                control_replies(&mut channel)?;
            }
            _ = feedback.tick() => {
                // After a long write, replies can wait unread.
                if read_waiting(&control, &mut channel)? {
                    unanswered = None;
                }
                if unanswered.is_some_and(|t| t.elapsed() > FEEDBACK_TIMEOUT) {
                    tracing::debug!("no answer to feedback");
                    return Err(Error::NoAnswer("feedback"));
                }
                tracing::trace!("feedback");
                session.feedback(&mut channel);
                unanswered.get_or_insert_with(Instant::now);
                airplay::flush(&mut control, &mut channel).await?;
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
                let siri = shared.siri_elapsed();
                while let Some(bytes) = stream.poll()? {
                    for env in frames(bytes) {
                        let env = env?;
                        if let Some(elapsed) = siri {
                            log_siri_message(&env, elapsed);
                        }
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
            () = shared.wake.notified() => {
                let (chunk, close) = {
                    let mut out = shared.outbox();
                    out.batches.take();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// After a long write, one feedback request is sent, not a burst.
    #[tokio::test(start_paused = true)]
    async fn feedback_after_long_write() {
        let mut timer = feedback_timer(Instant::now());
        timer.tick().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        timer.tick().await;
        let next = tokio::time::timeout(Duration::from_millis(1), timer.tick()).await;
        assert!(next.is_err());
    }
}
