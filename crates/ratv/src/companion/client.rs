use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ratv_companion::message::{
    self, Account, App, AttentionState, ButtonState, Event, FetchAccounts, FetchApps,
    FetchAttentionState, FetchSupportedActions, Hid, HidCommand, Interest, LaunchApp, MediaControl,
    Message, MessageType, Outgoing, SessionStart, SessionStop, SiriEndpointReply, SwitchAccount,
    SystemInfo, TextInputStart, TextInputStop, Touch, TouchPhase, TouchStart, TvRemoteSessionStart,
    event,
};
use ratv_companion::now_playing::{FetchNowPlaying, NowPlaying};
use ratv_companion::text_input::{self, TextInputCommand, TextSession};
use ratv_companion::{Connection, FrameType};
use ratv_hap::{Credentials, pair_verify};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{Instrument, Span};

use super::io::{Step, flush, pairing_step};
use crate::ClientOptions;
use crate::link::{Batches, CloseGuard, Link, LinkTx, Queued};
use crate::{Error, log, rng, tcp, touch};

const EVENT_CAPACITY: usize = 32;
/// Value of `Shared::now_playing_known`: no now playing data from the device yet.
const NP_UNKNOWN: u8 = 0;
/// Value of `Shared::now_playing_known`: data from the device.
const NP_KNOWN: u8 = 1;
/// Value of `Shared::now_playing_known`: a now playing event came that could
/// not be read. A later fetch answer then does not make the value known.
const NP_UNREADABLE: u8 = 2;
/// Time for the stream shutdown when the connection ends. A stream from
/// [`Client::connect_stream`] could otherwise keep the task alive while a
/// client exists.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// Encoded messages that are not sent yet. Checked before each push, so the
/// outbox can be larger than this by one message. Releases and the `_siriEndpoint`
/// reply skip it.
const MAX_OUTBOX: usize = 1 << 20;

/// Connected Companion session.
///
/// Requests can be sent from several tasks at the same time. Clones share the
/// session. A background task owns the socket; it stops when the last clone is
/// dropped, after [`close`](Self::close), when the device disconnects, or after
/// silent timeouts ([`ClientOptions::max_silent_timeouts`]). A [`Siri`](super::Siri) holds a
/// clone, so dropping the other clones does not stop the connection.
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    shared: Arc<Shared>,
    link: Link,
    /// Owned by the task, so receivers close when it ends.
    events: broadcast::WeakSender<Event>,
    now_playing: watch::Receiver<NowPlaying>,
    xid: AtomicU32,
    timeout: Duration,
    /// Requests, one after the other, that timed out while nothing was
    /// received.
    silent_timeouts: AtomicU32,
    /// Silent timeouts after which the connection is treated as dead. 0 means never.
    max_silent_timeouts: u32,
    session_id: AtomicU64,
    /// Start of the touch session, for touch timestamps.
    touch_base: Instant,
    /// True while a [`Siri`](super::Siri) exists.
    siri_active: AtomicBool,
    task: JoinHandle<()>,
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
    /// Number of reads with data, to see if the device sends anything.
    reads: AtomicU64,
    /// Last [`Event::ScreenChanged`], see [`to_flag`].
    guide_supported: AtomicU8,
    /// Siri on in the device settings, see [`to_flag`].
    siri_enabled: AtomicU8,
    /// [`NP_KNOWN`] when the device answered the now playing fetch or sent now
    /// playing that could be read, so an empty value means that nothing plays.
    now_playing_known: AtomicU8,
}

/// Stores an `Option<bool>` in an atomic: 0 unknown, 1 false, 2 true.
const fn to_flag(v: Option<bool>) -> u8 {
    match v {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}

const fn from_flag(v: u8) -> Option<bool> {
    match v {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct Outbox {
    /// OPACK payloads to send, each after its length as `u32` little endian.
    buf: Vec<u8>,
    /// Counts how often the task took the buffer, and tracks removed messages.
    batches: Batches,
    /// Requests that wait for a response, by xid.
    pending: Vec<(u32, oneshot::Sender<Reply>)>,
    /// Set by `close`.
    close: bool,
    /// Set when requests time out and nothing is received.
    stalled: bool,
}

/// Response payload, or the error the device sent.
type Reply = Result<Bytes, ratv_companion::Error>;

impl Outbox {
    /// Encodes `msg` with its length in front. A `release` skips the size limit.
    fn push<M: Outgoing>(&mut self, msg: &M, xid: u32, release: bool) -> Result<Queued, Error> {
        if !release && self.buf.len() > MAX_OUTBOX {
            return Err(Error::QueueFull);
        }
        let start = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        message::encode(msg, xid, &mut self.buf);
        let len = self.buf.len() - start - 4;
        if len > ratv_companion::MAX_MESSAGE_LEN {
            self.buf.truncate(start);
            return Err(ratv_companion::Error::FrameTooLarge(len).into());
        }
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        self.buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
        Ok(self.batches.queued(start, self.buf.len()))
    }

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
    /// Sets `now_playing_known` to `new` if it is `old`.
    fn set_now_playing(&self, old: u8, new: u8) {
        let flag = &self.now_playing_known;
        let _ = flag.compare_exchange(old, new, Ordering::Release, Ordering::Relaxed);
    }

    /// After the now playing fetch: an answer makes the value known, unless
    /// an event that could not be read came first.
    fn fetch_answered(&self, answered: bool) {
        if answered {
            self.set_now_playing(NP_UNKNOWN, NP_KNOWN);
        }
    }

    /// Locks the outbox. A panic while locked cannot leave it invalid.
    fn outbox(&self) -> MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Client {
    /// Connects to `addr`, runs pair-verify and starts a session.
    ///
    /// The TV closes an open connection that uses the same credentials.
    pub async fn connect(
        addr: SocketAddr,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error> {
        let span = log::session("companion", Some(addr));
        async {
            options
                .connect_with_retry(|step| async move {
                    tracing::debug!("connecting");
                    let stream = tcp::connect(addr, step).await?;
                    Self::start(stream, credentials, options, step, Span::current()).await
                })
                .await
        }
        .instrument(span)
        .await
    }

    /// Same as [`Client::connect`] on an existing stream. `stall_timeout` is
    /// not used, because the stream cannot be opened again.
    pub async fn connect_stream<S>(
        stream: S,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let span = log::session("companion", None);
        Self::start(
            stream,
            credentials,
            options,
            options.connect_step(),
            span.clone(),
        )
        .instrument(span)
        .await
    }

    /// `step` is the time to wait for each connect step.
    async fn start<S>(
        mut stream: S,
        credentials: &Credentials,
        options: &ClientOptions,
        step: Duration,
        span: Span,
    ) -> Result<Self, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut conn = Connection::new();
        let mut buf = Vec::with_capacity(256);
        let mut rng = rng::os();
        let (m1, state) = pair_verify::start(credentials, &mut rng);
        let m2 = pairing_step(&mut stream, &mut conn, Step::VerifyM1, &m1, &mut buf, step).await?;
        let (m3, state) = state.handle_m2(&m2)?;
        let m4 = pairing_step(&mut stream, &mut conn, Step::VerifyM3, &m3, &mut buf, step).await?;
        let secret = state.handle_m4(&m4)?;
        conn.enable_encryption(&secret);
        tracing::debug!("pair-verify done");

        let shared = Arc::new(Shared::default());
        let (link_tx, link) = Link::new();
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (np_tx, now_playing) = watch::channel(NowPlaying::default());
        let task = tokio::spawn(
            run(stream, conn, shared.clone(), link_tx, events.clone(), np_tx)
                .instrument(span.clone()),
        );
        let client = Self {
            inner: Arc::new(Inner {
                shared,
                link,
                events: events.downgrade(),
                now_playing,
                xid: AtomicU32::new(rng::u32() >> 16),
                timeout: options.timeout,
                silent_timeouts: AtomicU32::new(0),
                max_silent_timeouts: options.max_silent_timeouts,
                session_id: AtomicU64::new(0),
                touch_base: Instant::now(),
                siri_active: AtomicBool::new(false),
                task,
                span,
            }),
        };

        let rp_id = options
            .device_id
            .clone()
            .unwrap_or_else(|| rng::stable_device_id(credentials.client_id()));
        let ours = rng::u32();
        let info = SystemInfo {
            name: &options.name,
            model: &options.model,
            client_id: credentials.client_id(),
            rp_id: &rp_id,
            pub_id: &rp_id,
        };
        let session = SessionStart { session_id: ours };
        // Sent in this order without waiting for each reply. Replies are matched by xid.
        let touch_area = TouchStart::default();
        // Each request has its own limit, so an optional one that gets no
        // answer does not fail the connect.
        let (info, touch_start, session, tv_remote, text_input, interest, actions, playing) = tokio::join!(
            within(step, client.request_with(&info, None)),
            within(step, client.request_with(&touch_area, None)),
            within(step, client.request_with(&session, None)),
            within(step, client.request_with(&TvRemoteSessionStart, None)),
            // Makes the device send keyboard focus events.
            within(step, client.request_with(&TextInputStart, None)),
            within(
                step,
                client.send_event(&Interest {
                    register: &[
                        event::MEDIA_CONTROL,
                        event::SYSTEM_STATUS,
                        event::TV_SYSTEM_STATUS,
                        event::SIRI_ENDPOINT,
                        event::SUPPORTED_ACTIONS,
                        event::NOW_PLAYING_INFO,
                    ],
                    deregister: &[],
                })
            ),
            // The device then sends the current state as an event.
            within(step, client.request_with(&FetchSupportedActions, None)),
            // The device then sends what plays as an event.
            within(step, client.request_with(&FetchNowPlaying, None)),
        );
        // The Siri state is optional, so a reply that cannot be read only leaves it unknown.
        let siri = Message::decode(&info?)
            .ok()
            .and_then(|m| message::siri_enabled(&m));
        tracing::debug!(?siri, "Siri enabled");
        // An `Event::SiriStatus` may have come first. It is newer.
        let flag = &client.inner.shared.siri_enabled;
        let _ = flag.compare_exchange(0, to_flag(siri), Ordering::Relaxed, Ordering::Relaxed);
        let device = message::session_start_response(&Message::decode(&session?)?)?;
        client.inner.session_id.store(
            message::combined_session_id(device, ours),
            Ordering::Relaxed,
        );
        // If the device sent the now playing event, it came before this reply.
        client.inner.shared.fetch_answered(playing.is_ok());
        // Older tvOS versions may not support these.
        for (id, result) in [
            ("_touchStart", touch_start),
            ("TVRCSessionStart", tv_remote),
            ("_tiStart", text_input),
            ("FetchSupportedActionsEvent", actions),
            ("FetchCurrentNowPlayingInfoEvent", playing),
        ] {
            if let Err(err) = result {
                tracing::debug!(%err, "{id} failed");
            }
        }
        interest?;
        tracing::debug!("connected");
        Ok(client)
    }

    fn next_xid(&self) -> u32 {
        self.inner.xid.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a request and returns the raw response payload.
    /// Decode it with [`Message::decode`]. Device errors are returned as `Err`.
    ///
    /// Fails with [`Error::Closed`] if the connection was closed before the
    /// request was sent, with [`Error::QueueFull`] if the outbox is full, and
    /// with [`Error::SendTimeout`] if the request was still in the outbox
    /// after the time limit. All three mean that it was not sent, see
    /// [`Error::is_not_sent`].
    /// A request that is too large ([`Error::is_too_large`]) is not sent
    /// either.
    /// Fails with [`Error::NoAnswer`] if the request left the outbox but got
    /// no answer in time; it may still be in the write, or it may have been
    /// sent. Fails with [`Error::Disconnected`] if the connection closed after
    /// the request left the outbox; then it may have been sent.
    pub async fn request<M: Outgoing + Sync>(&self, msg: &M) -> Result<Bytes, Error> {
        self.request_with(msg, Some(self.inner.timeout)).await
    }

    /// Like [`request`](Self::request), with `limit` instead of the timeout.
    /// `None` waits until the reply or the end of the connection.
    async fn request_with<M: Outgoing + Sync>(
        &self,
        msg: &M,
        limit: Option<Duration>,
    ) -> Result<Bytes, Error> {
        self.send_request(msg, limit, false, true).await
    }

    /// Like [`request`](Self::request) for a button or Siri release, which
    /// skips the outbox size limit.
    pub(super) async fn release_request<M: Outgoing + Sync>(
        &self,
        msg: &M,
    ) -> Result<Bytes, Error> {
        self.send_request(msg, Some(self.inner.timeout), true, true)
            .await
    }

    /// Sends a request and waits at most `limit` for the response. `None`
    /// waits without a limit. A `release` skips the outbox size limit. With
    /// `count`, a timeout counts toward
    /// [`ClientOptions::max_silent_timeouts`].
    async fn send_request<M: Outgoing + Sync>(
        &self,
        msg: &M,
        limit: Option<Duration>,
        release: bool,
        count: bool,
    ) -> Result<Bytes, Error> {
        let xid = self.next_xid();
        tracing::trace!(parent: &self.inner.span, id = M::ID, xid, "request");
        let start = Instant::now();
        let (reply, rx) = oneshot::channel();
        let reads = self.inner.shared.reads.load(Ordering::Relaxed);
        let queued = self.queue(msg, xid, Some(reply), release)?;
        let reply = match limit {
            Some(limit) => {
                let Ok(reply) = tokio::time::timeout(limit, rx).await else {
                    let removed = self.inner.shared.outbox().remove(queued);
                    tracing::debug!(parent: &self.inner.span, id = M::ID, xid, removed, "request timed out");
                    if removed {
                        // Never sent, so it does not show that the device stopped answering.
                        return Err(Error::SendTimeout(M::ID));
                    }
                    if count {
                        self.timed_out(reads);
                    }
                    return Err(Error::NoAnswer(M::ID));
                };
                self.inner.silent_timeouts.store(0, Ordering::Relaxed);
                reply
            }
            None => rx.await,
        };
        let Ok(reply) = reply else {
            // The connection closed. A message that is still queued was not sent.
            let removed = self.inner.shared.outbox().remove(queued);
            return Err(if removed {
                Error::Closed
            } else {
                Error::Disconnected
            });
        };
        let elapsed = start.elapsed();
        match reply {
            Ok(bytes) => {
                tracing::trace!(parent: &self.inner.span, id = M::ID, xid, ?elapsed, "response");
                Ok(bytes)
            }
            Err(err) => {
                tracing::debug!(parent: &self.inner.span, id = M::ID, xid, ?elapsed, %err, "request failed");
                Err(err.into())
            }
        }
    }

    /// Ends the connection after requests, one after the other, time out
    /// while the device sends nothing. TCP keepalive does not run while sent
    /// data waits for acknowledgement, so this finds a device that is gone.
    fn timed_out(&self, reads_before: u64) {
        if self.inner.shared.reads.load(Ordering::Relaxed) != reads_before {
            self.inner.silent_timeouts.store(0, Ordering::Relaxed);
            return;
        }
        let max = self.inner.max_silent_timeouts;
        if max == 0 {
            return;
        }
        let n = self.inner.silent_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= max {
            tracing::debug!(parent: &self.inner.span, "device sends nothing");
            self.inner.shared.outbox().stalled = true;
            self.inner.shared.wake.notify_one();
        }
    }

    /// Sends an event (no response). `Ok` only means that the event was
    /// queued. Fails with [`Error::Closed`], [`Error::QueueFull`] or a too
    /// large event ([`Error::is_too_large`]); then it was not sent.
    // Async, so waiting for queue space can be added without an API change.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn send_event<M: Outgoing + Sync>(&self, msg: &M) -> Result<(), Error> {
        self.queue_event(msg, false)
    }

    /// Queues a message without waiting for a response.
    /// A `release` skips the outbox size limit.
    pub(super) fn queue_event<M: Outgoing>(&self, msg: &M, release: bool) -> Result<(), Error> {
        let xid = self.next_xid();
        tracing::trace!(parent: &self.inner.span, id = M::ID, xid, release, "event");
        self.queue(msg, xid, None, release).map(drop)
    }

    /// Queues two events, or neither: if the second one does not fit, the
    /// first one is removed again.
    fn queue_events<M: Outgoing>(&self, first: &M, second: &M) -> Result<(), Error> {
        let (x1, x2) = (self.next_xid(), self.next_xid());
        tracing::trace!(parent: &self.inner.span, id = M::ID, x1, x2, "events");
        {
            let mut out = self.inner.shared.outbox();
            // Checked while locked: the task clears `pending` after the link closes.
            if !self.inner.link.is_open() {
                return Err(Error::Closed);
            }
            let queued = out.push(first, x1, false)?;
            if let Err(err) = out.push(second, x2, false) {
                // The task cannot take the first one while the outbox is locked.
                let removed = out.remove(queued);
                debug_assert!(removed);
                return Err(err);
            }
        }
        self.inner.shared.wake.notify_one();
        Ok(())
    }

    /// Flag that is true while a [`Siri`](super::Siri) exists.
    pub(super) fn siri_active(&self) -> &AtomicBool {
        &self.inner.siri_active
    }

    pub(super) fn span(&self) -> &Span {
        &self.inner.span
    }

    /// Encodes `msg` into the outbox and wakes the background task.
    /// A `release` skips the outbox size limit.
    fn queue<M: Outgoing>(
        &self,
        msg: &M,
        xid: u32,
        reply: Option<oneshot::Sender<Reply>>,
        release: bool,
    ) -> Result<Queued, Error> {
        let queued = {
            let mut out = self.inner.shared.outbox();
            // Checked while locked: the task clears `pending` after the link closes.
            if !self.inner.link.is_open() {
                return Err(Error::Closed);
            }
            let queued = out.push(msg, xid, release)?;
            if let Some(reply) = reply {
                out.pending.retain(|(_, tx)| !tx.is_closed());
                out.pending.push((xid, reply));
            }
            queued
        };
        self.inner.shared.wake.notify_one();
        Ok(queued)
    }

    /// Receives device events (media controls, power state, keyboard focus).
    ///
    /// A receiver that reads too slowly misses old events and gets `RecvError::Lagged`.
    /// The receiver closes when the connection closes.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<Event> {
        crate::subscribe(&self.inner.events)
    }

    /// True if Siri is on in the device settings.
    /// Read when connecting and updated with [`Event::SiriStatus`].
    /// `None` if the device does not say. Keeps the last value after the
    /// connection closes.
    #[must_use]
    pub fn siri_enabled(&self) -> Option<bool> {
        from_flag(self.inner.shared.siri_enabled.load(Ordering::Relaxed))
    }

    /// What the device plays. Faster than MRP, but has less data, and may not
    /// hold an item that was paused before connecting. If the value is not
    /// known after connect (no answer to the now playing fetch, or an event
    /// that could not be read), the first event that can be read counts as a
    /// change, also when the value stays the same. After the connection
    /// closes, the receiver keeps the last value and closes.
    #[must_use]
    pub fn now_playing(&self) -> watch::Receiver<NowPlaying> {
        self.inner.now_playing.clone()
    }

    /// True if [`now_playing`](Self::now_playing) holds data from the device.
    /// An empty value then means that nothing plays.
    #[cfg(feature = "mrp")]
    pub(crate) fn now_playing_known(&self) -> bool {
        self.inner.shared.now_playing_known.load(Ordering::Acquire) == NP_KNOWN
    }

    /// True if the Guide button does something on the current screen.
    /// Read when connecting and updated with [`Event::ScreenChanged`].
    /// `None` if the device did not say. Keeps the last value after the
    /// connection closes.
    #[must_use]
    pub fn guide_supported(&self) -> Option<bool> {
        from_flag(self.inner.shared.guide_supported.load(Ordering::Relaxed))
    }

    /// Combined session id.
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.inner.session_id.load(Ordering::Relaxed)
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

    /// Sets the state of a button.
    pub async fn button(&self, command: HidCommand, state: ButtonState) -> Result<(), Error> {
        self.request(&Hid { command, state }).await.map(drop)
    }

    /// Clicks a button (press and release).
    pub async fn press(&self, command: HidCommand) -> Result<(), Error> {
        // The device starts the screensaver without answering, so do not wait for replies.
        if command == HidCommand::Screensaver {
            let state = ButtonState::Down;
            self.send_event(&Hid { command, state }).await?;
            let state = ButtonState::Up;
            return self.queue_event(&Hid { command, state }, true);
        }
        let mut held = Held {
            client: self,
            command,
            armed: true,
        };
        held.down().await?;
        held.release().await
    }

    /// Clicks a button two times fast. [`HidCommand::Home`] opens the app switcher.
    ///
    /// All events are sent before waiting for answers, so the device sees a double click.
    /// When it fails, some of the 4 messages can still be sent, also with an
    /// error where [`Error::is_not_sent`] is true.
    pub async fn double_tap(&self, command: HidCommand) -> Result<(), Error> {
        let mut held = Held {
            client: self,
            command,
            armed: true,
        };
        // `try_join!` polls in order, and each request is queued on its first poll.
        tokio::try_join!(
            self.button(command, ButtonState::Down),
            self.button(command, ButtonState::Up),
            self.button(command, ButtonState::Down),
            self.button(command, ButtonState::Up),
        )?;
        held.armed = false;
        Ok(())
    }

    /// Holds a button for `duration`. The button is released also if the
    /// returned future is dropped.
    pub async fn hold(&self, command: HidCommand, duration: Duration) -> Result<(), Error> {
        let mut held = Held {
            client: self,
            command,
            armed: true,
        };
        held.down().await?;
        tokio::time::sleep(duration).await;
        held.release().await
    }

    /// Sends a media control command. A `SkipBy` value that is not finite
    /// or a `SetVolume` value of NaN gives [`Error::InvalidArgument`]. A
    /// `SetVolume` value is limited to 0.0 to 1.0, as in
    /// [`set_volume`](Self::set_volume).
    pub async fn media(&self, command: MediaControl) -> Result<(), Error> {
        let command = match command {
            MediaControl::SkipBy(seconds) => {
                MediaControl::SkipBy(crate::finite(seconds, "seconds")?)
            }
            MediaControl::SetVolume(volume) => {
                if volume.is_nan() {
                    return Err(Error::InvalidArgument("volume"));
                }
                // `+ 0.0` turns -0.0 into 0.0.
                MediaControl::SetVolume(volume.clamp(0.0, 1.0) + 0.0)
            }
            other => other,
        };
        self.request(&command).await.map(drop)
    }

    /// Returns the volume from 0.0 to 1.0. A device value outside that range
    /// is limited to it. Fails with [`Error::Companion`] if the reply has no
    /// volume, or a volume that is not finite or too large for f32.
    pub async fn volume(&self) -> Result<f32, Error> {
        let reply = self.request(&MediaControl::GetVolume).await?;
        let volume = message::volume_response(&Message::decode(&reply)?)?;
        // Volume is 0.0 to 1.0, so f32 is exact enough.
        #[allow(clippy::cast_possible_truncation)]
        let volume = volume as f32;
        // A finite f64 can be too large for f32.
        if !volume.is_finite() {
            return Err(ratv_companion::Error::InvalidField("_vol").into());
        }
        // `+ 0.0` turns -0.0 into 0.0.
        Ok(volume.clamp(0.0, 1.0) + 0.0)
    }

    /// Sets the volume from 0.0 to 1.0. NaN gives [`Error::InvalidArgument`];
    /// other values are limited to 0.0 to 1.0.
    pub async fn set_volume(&self, volume: f32) -> Result<(), Error> {
        let volume = crate::volume_level(volume)?;
        self.media(MediaControl::SetVolume(f64::from(volume))).await
    }

    /// Launches an app by bundle id.
    pub async fn launch_app(&self, bundle_id: &str) -> Result<(), Error> {
        self.request(&LaunchApp::BundleId(bundle_id))
            .await
            .map(drop)
    }

    /// Opens a URL in the app that handles it.
    pub async fn open_url(&self, url: &str) -> Result<(), Error> {
        self.request(&LaunchApp::Url(url)).await.map(drop)
    }

    /// Lists installed apps.
    pub async fn apps(&self) -> Result<Vec<App>, Error> {
        let reply = self.request(&FetchApps).await?;
        Ok(message::apps_response(&Message::decode(&reply)?)?)
    }

    /// User accounts that can be switched to.
    pub async fn accounts(&self) -> Result<Vec<Account>, Error> {
        let reply = self.request(&FetchAccounts).await?;
        Ok(message::accounts_response(&Message::decode(&reply)?)?)
    }

    /// Switches to the user account with `id`. Ids are in [`accounts`](Self::accounts).
    pub async fn switch_account(&self, id: &str) -> Result<(), Error> {
        self.request(&SwitchAccount(id)).await.map(drop)
    }

    /// Returns the power state.
    pub async fn power_state(&self) -> Result<AttentionState, Error> {
        let reply = self.request(&FetchAttentionState).await?;
        Ok(message::attention_response(&Message::decode(&reply)?)?)
    }

    /// Text of the focused text field. `None` if no text field has focus.
    pub async fn text(&self) -> Result<Option<String>, Error> {
        Ok(self.text_session().await?.map(|s| s.text))
    }

    /// Replaces the text of the focused text field. Fails with
    /// [`Error::NoTextField`] if no text field has focus. If it fails, for
    /// example with text that is too large ([`Error::is_too_large`]), the old
    /// text stays.
    pub async fn set_text(&self, text: &str) -> Result<(), Error> {
        self.edit_text(text, true).await
    }

    /// Adds text at the cursor of the focused text field. Fails with
    /// [`Error::NoTextField`] if no text field has focus.
    pub async fn append_text(&self, text: &str) -> Result<(), Error> {
        self.edit_text(text, false).await
    }

    /// Removes all text of the focused text field. Fails with
    /// [`Error::NoTextField`] if no text field has focus.
    pub async fn clear_text(&self) -> Result<(), Error> {
        self.edit_text("", true).await
    }

    async fn edit_text(&self, text: &str, clear: bool) -> Result<(), Error> {
        let session = self.text_session().await?.ok_or(Error::NoTextField)?;
        let cleared = clear.then(|| text_input::clear_text(&session.uuid));
        let inserted = (!text.is_empty()).then(|| text_input::insert_text(&session.uuid, text));
        match (&cleared, &inserted) {
            (Some(c), Some(i)) => {
                self.queue_events(&TextInputCommand { data: c }, &TextInputCommand { data: i })
            }
            (Some(data), None) | (None, Some(data)) => {
                self.send_event(&TextInputCommand { data }).await
            }
            (None, None) => Ok(()),
        }
    }

    /// Restarts the text input session to get the current text field.
    async fn text_session(&self) -> Result<Option<TextSession>, Error> {
        let (stop, start) =
            tokio::join!(self.request(&TextInputStop), self.request(&TextInputStart));
        if let Err(err) = stop {
            tracing::debug!(parent: &self.inner.span, %err, "_tiStop failed");
        }
        let reply = start?;
        let msg = Message::decode(&reply)?;
        text_input::session(&msg).map_err(|err| {
            tracing::debug!(parent: &self.inner.span, %err, "unknown text input data");
            #[cfg(feature = "sensitive-logs")]
            tracing::trace!(parent: &self.inner.span, data = ?msg.field("_tiD"), "unknown text input data");
            err.into()
        })
    }

    /// Sends a touch event. `x` and `y` range from 0.0 to 1.0.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn touch(&self, x: f64, y: f64, phase: TouchPhase) -> Result<(), Error> {
        self.send_event(&self.touch_event(x, y, phase)).await
    }

    fn touch_event(&self, x: f64, y: f64, phase: TouchPhase) -> Touch {
        let area = TouchStart::default();
        // Rounded and clamped, so the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scale = |v: f64, size: f64| (v.clamp(0.0, 1.0) * size).round() as u16;
        Touch {
            timestamp_ns: u64::try_from(self.inner.touch_base.elapsed().as_nanos())
                .unwrap_or(u64::MAX),
            finger: 1,
            x: scale(x, area.width),
            y: scale(y, area.height),
            phase,
        }
    }

    /// Swipes from `from` to `to` in about `duration`. Coordinates range from 0.0 to 1.0.
    /// The finger is lifted also if the returned future is dropped.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn swipe(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
    ) -> Result<(), Error> {
        let mut down = Touching {
            client: self,
            at: from,
            armed: true,
        };
        let mut pressed = false;
        let result = touch::swipe(from, to, duration, |x, y, phase| {
            let phase = match phase {
                touch::Phase::Begin => TouchPhase::Press,
                touch::Phase::Move => TouchPhase::Hold,
                touch::Phase::End => TouchPhase::Release,
            };
            let msg = self.touch_event(x, y, phase);
            let result = self.queue_event(&msg, phase == TouchPhase::Release);
            if result.is_ok() {
                pressed = true;
                // Lift where the finger is, not at the end of the swipe.
                down.at = (x, y);
            }
            async move { result }
        })
        .await;
        // No lift without a press.
        down.armed = result.is_err() && pressed;
        result
    }

    /// Taps at `x`, `y` without a click. Coordinates range from 0.0 to 1.0.
    /// The finger is lifted also if the returned future is dropped.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn tap(&self, x: f64, y: f64) -> Result<(), Error> {
        let mut down = Touching {
            client: self,
            at: (x, y),
            armed: true,
        };
        if let Err(err) = self.touch(x, y, TouchPhase::Press).await {
            // No lift without a press.
            down.armed = !err.is_not_sent();
            return Err(err);
        }
        tokio::time::sleep(touch::TAP_HOLD).await;
        down.armed = false;
        self.queue_event(&self.touch_event(x, y, TouchPhase::Release), true)
    }

    /// Wakes the device.
    pub async fn turn_on(&self) -> Result<(), Error> {
        self.press(HidCommand::Wake).await
    }

    /// Puts the device to sleep.
    pub async fn turn_off(&self) -> Result<(), Error> {
        self.press(HidCommand::Sleep).await
    }

    /// Stops the session and closes the connection of all clones. Returns
    /// `Ok` at once when the connection is already closed.
    ///
    /// A device error for the stop request is ignored: the device ends the
    /// session itself when the app changes or it goes to sleep. A stop
    /// request that was not sent or not answered, or a connection that closes
    /// while the stop request waits, is also ignored, because the connection
    /// closes anyway. Its timeout does not count toward
    /// [`ClientOptions::max_silent_timeouts`].
    ///
    /// When the device does not read, this can take about the timeout plus
    /// one or two 10 s write limits. Calls on other clones while this runs
    /// can return `Ok` but not be sent.
    pub async fn close(&self) -> Result<(), Error> {
        if !self.inner.link.is_open() {
            return Ok(());
        }
        let stop = SessionStop {
            session_id: self.session_id(),
        };
        // Closes also when this future is dropped while the stop request waits.
        let close = CloseOnDrop(&self.inner.shared);
        let result = match self
            .send_request(&stop, Some(self.inner.timeout), false, false)
            .await
        {
            // Closed while waiting, or the device does not answer.
            Ok(_) | Err(Error::Disconnected | Error::NoAnswer(_)) => Ok(()),
            // Closed since the check above, or the device does not read.
            Err(err) if err.is_not_sent() => Ok(()),
            Err(Error::Companion(err @ ratv_companion::Error::Device { .. })) => {
                tracing::debug!(parent: &self.inner.span, %err, "session already stopped by device");
                Ok(())
            }
            Err(err) => Err(err),
        };
        drop(close);
        self.inner.link.closed().await;
        result
    }
}

/// Tells the task to close the connection when dropped.
struct CloseOnDrop<'a>(&'a Shared);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.outbox().close = true;
        self.0.wake.notify_one();
    }
}

/// Releases a button when dropped while armed.
struct Held<'a> {
    client: &'a Client,
    command: HidCommand,
    armed: bool,
}

impl Held<'_> {
    /// Sends the press. If it was not sent, no release is sent either.
    async fn down(&mut self) -> Result<(), Error> {
        let result = self.client.button(self.command, ButtonState::Down).await;
        if result.as_ref().is_err_and(Error::is_not_sent) {
            self.armed = false;
        }
        result
    }

    /// Sends the release. If it was not sent in time, the drop queues it again
    /// without waiting for the reply, so it is not removed from the outbox.
    async fn release(&mut self) -> Result<(), Error> {
        self.armed = false;
        let up = Hid {
            command: self.command,
            state: ButtonState::Up,
        };
        let result = self.client.release_request(&up).await;
        self.armed = matches!(result, Err(Error::SendTimeout(_)));
        result.map(drop)
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.armed {
            let up = Hid {
                command: self.command,
                state: ButtonState::Up,
            };
            let _ = self.client.queue_event(&up, true);
        }
    }
}

/// Lifts the finger at `at` when dropped while armed.
struct Touching<'a> {
    client: &'a Client,
    /// The last point sent.
    at: (f64, f64),
    armed: bool,
}

/// Waits at most `limit` for `f`.
async fn within<T>(limit: Duration, f: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    tokio::time::timeout(limit, f).await?
}

impl Drop for Touching<'_> {
    fn drop(&mut self) {
        if self.armed {
            let (x, y) = self.at;
            let up = self.client.touch_event(x, y, TouchPhase::Release);
            let _ = self.client.queue_event(&up, true);
        }
    }
}

async fn run<S>(
    mut stream: S,
    mut conn: Connection,
    shared: Arc<Shared>,
    link: LinkTx,
    events: broadcast::Sender<Event>,
    now_playing: watch::Sender<NowPlaying>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Waiting requests fail at once.
    let mut guard = CloseGuard::new(link, || shared.outbox().pending.clear());
    // Swapped with the outbox buffer, so both keep their capacity.
    let mut batch = Vec::new();
    let result: Result<(), Error> = async {
        loop {
            tokio::select! {
                () = shared.wake.notified() => {
                    let (close, stalled) = {
                        let mut out = shared.outbox();
                        std::mem::swap(&mut out.buf, &mut batch);
                        out.batches.take();
                        (out.close, out.stalled)
                    };
                    // After `close`, the rest is sent and the close is clean.
                    if stalled && !close {
                        return Err(Error::NoAnswer("requests"));
                    }
                    let mut rest = batch.as_slice();
                    while let Some((len, tail)) = rest.split_first_chunk::<4>() {
                        let (payload, tail) = tail.split_at(u32::from_le_bytes(*len) as usize);
                        conn.send(FrameType::E_OPACK, payload);
                        rest = tail;
                    }
                    batch.clear();
                    flush(&mut stream, &mut conn).await?;
                    if close {
                        return Ok(());
                    }
                }
                n = stream.read_buf(conn.receive_buffer(4096)) => {
                    if n? == 0 {
                        return Err(Error::Disconnected);
                    }
                    shared.reads.fetch_add(1, Ordering::Relaxed);
                    while let Some(frame) = conn.poll_frame()? {
                        dispatch(frame.ty, frame.payload, &shared, &events, &now_playing);
                    }
                }
            }
        }
    }
    .await;
    guard.reason = match result {
        Ok(()) => {
            tracing::debug!("connection closed");
            None
        }
        Err(err) => {
            tracing::debug!(%err, "connection ended");
            Some(Arc::new(err))
        }
    };
    drop(guard);
    // Receivers see the close also when the shutdown waits.
    drop((now_playing, events));
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, stream.shutdown()).await;
}

fn dispatch(
    ty: FrameType,
    payload: Bytes,
    shared: &Shared,
    events: &broadcast::Sender<Event>,
    now_playing: &watch::Sender<NowPlaying>,
) {
    if ty != FrameType::E_OPACK {
        tracing::trace!(ty = ty.0, "ignoring frame");
        return;
    }
    let msg = match Message::decode(&payload) {
        Ok(msg) => msg,
        Err(err) => {
            tracing::debug!(%err, len = payload.len(), "invalid OPACK payload");
            #[cfg(feature = "sensitive-logs")]
            tracing::trace!(?payload, "invalid OPACK payload");
            return;
        }
    };
    tracing::trace!(id = ?msg.id(), kind = ?msg.kind(), xid = ?msg.xid(), len = payload.len(), "received");
    #[cfg(feature = "sensitive-logs")]
    tracing::trace!(?msg, "received");
    match msg.kind() {
        Some(MessageType::Event) if msg.id() == Some(event::NOW_PLAYING_INFO) => {
            // The first event that can be read is always a change, also when
            // it is empty, so the merge task learns that the device sent data.
            let first = shared.now_playing_known.load(Ordering::Relaxed) != NP_KNOWN;
            let mut result = Ok(false);
            now_playing.send_if_modified(|np| {
                result = np.apply(&msg);
                result.as_ref().is_ok_and(|&changed| changed || first)
            });
            match result {
                // After the send: who sees the flag also sees the value.
                Ok(_) if first => shared.now_playing_known.store(NP_KNOWN, Ordering::Release),
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(%err, "now playing event not read");
                    shared.set_now_playing(NP_UNKNOWN, NP_UNREADABLE);
                }
            }
        }
        Some(MessageType::Event) => {
            if let Some(event) = Event::parse(&msg) {
                match event {
                    Event::ScreenChanged { guide_supported } => shared
                        .guide_supported
                        .store(to_flag(Some(guide_supported)), Ordering::Relaxed),
                    Event::SiriStatus { enabled } => shared
                        .siri_enabled
                        .store(to_flag(Some(enabled)), Ordering::Relaxed),
                    _ => {}
                }
                let _ = events.send(event);
            } else {
                // Debug, not trace: unknown events help to find tvOS changes.
                tracing::debug!(id = ?msg.id(), "ignoring device event");
            }
        }
        Some(MessageType::Response) | None => {
            let Some(xid) = msg.xid() else { return };
            let reply = {
                let mut out = shared.outbox();
                out.pending
                    .iter()
                    .position(|(x, _)| *x == xid)
                    .map(|i| out.pending.swap_remove(i).1)
            };
            if let Some(reply) = reply {
                let _ = reply.send(msg.error().map_or(Ok(payload), Err));
            } else {
                tracing::debug!(xid, "response without request");
            }
        }
        Some(MessageType::Request) => {
            if let Some(event @ Event::SiriEndpoint) = Event::parse(&msg) {
                // The iPhone replies with an empty response. Skips the outbox
                // limit, as a release does.
                if let Some(xid) = msg.xid()
                    && let Err(err) = shared.outbox().push(&SiriEndpointReply, xid, true)
                {
                    tracing::debug!(%err, "_siriEndpoint reply failed");
                }
                shared.wake.notify_one();
                let _ = events.send(event);
            } else {
                // Not answered. Debug, not trace: the device may wait for an answer.
                tracing::debug!(id = ?msg.id(), xid = ?msg.xid(), "ignoring device request");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratv_codec::bplist::Value as P;
    use ratv_codec::opack::Encoder;

    use super::*;

    /// `NowPlayingInfo` event with `data` as the archive.
    fn now_playing_event(data: &[u8]) -> Bytes {
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(3, |e| {
            e.str("_i").str(event::NOW_PLAYING_INFO);
            e.str("_t").int(MessageType::Event as i64);
            e.str("_c").dict(1, |e| {
                e.str("NowPlayingInfoKey").data(data);
            });
        });
        out.into()
    }

    /// Archive whose root has no fields.
    fn empty_archive() -> Vec<u8> {
        P::Dict(vec![
            ("$top".into(), P::Dict(vec![("root".into(), P::Uid(1))])),
            (
                "$objects".into(),
                P::Array(vec![P::String("$null".into()), P::Dict(Vec::new())]),
            ),
        ])
        .encode()
    }

    #[test]
    fn now_playing_known_from_unknown() {
        // A fetch that got no answer keeps the value unknown; an answer makes
        // it known.
        let shared = Shared::default();
        shared.fetch_answered(false);
        assert_eq!(shared.now_playing_known.load(Ordering::Relaxed), NP_UNKNOWN);
        shared.fetch_answered(true);
        assert_eq!(shared.now_playing_known.load(Ordering::Relaxed), NP_KNOWN);

        // A readable event makes it known and is a change.
        let shared = Shared::default();
        let (events, _) = broadcast::channel(1);
        let (tx, rx) = watch::channel(NowPlaying::default());
        let event = now_playing_event(&empty_archive());
        dispatch(FrameType::E_OPACK, event, &shared, &events, &tx);
        assert_eq!(shared.now_playing_known.load(Ordering::Relaxed), NP_KNOWN);
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn now_playing_known_only_when_read() {
        let shared = Shared::default();
        let (events, _) = broadcast::channel(1);
        let (tx, mut rx) = watch::channel(NowPlaying::default());
        let state = || shared.now_playing_known.load(Ordering::Relaxed);

        // Cannot be read: not known, no change.
        dispatch(
            FrameType::E_OPACK,
            now_playing_event(b"x"),
            &shared,
            &events,
            &tx,
        );
        assert_eq!(state(), NP_UNREADABLE);
        assert!(!rx.has_changed().unwrap());
        // A fetch answer after it does not make the value known.
        shared.fetch_answered(true);
        assert_eq!(state(), NP_UNREADABLE);

        // The first event that can be read is a change, also when empty.
        let empty = empty_archive();
        dispatch(
            FrameType::E_OPACK,
            now_playing_event(&empty),
            &shared,
            &events,
            &tx,
        );
        assert_eq!(state(), NP_KNOWN);
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();

        // The same event again is no change. One that cannot be read keeps
        // the value known.
        dispatch(
            FrameType::E_OPACK,
            now_playing_event(&empty),
            &shared,
            &events,
            &tx,
        );
        assert!(!rx.has_changed().unwrap());
        dispatch(
            FrameType::E_OPACK,
            now_playing_event(b"x"),
            &shared,
            &events,
            &tx,
        );
        assert_eq!(state(), NP_KNOWN);
        assert!(!rx.has_changed().unwrap());
    }
}
