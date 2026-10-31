use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ratv_companion::message::{
    self, Account, App, AttentionState, ButtonState, Event, FetchAccounts, FetchApps,
    FetchAttentionState, Hid, HidCommand, Interest, LaunchApp, MediaControl, Message, MessageType,
    Outgoing, SessionStart, SessionStop, SwitchAccount, SystemInfo, TextInputStart, TextInputStop,
    Touch, TouchPhase, TouchStart, TvRemoteSessionStart, event,
};
use ratv_companion::pairing::Flow;
use ratv_companion::text_input::{self, TextInputCommand, TextSession};
use ratv_companion::{Connection, FrameType};
use ratv_hap::{Credentials, pair_verify};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{Instrument, Span};

use super::io::{flush, pairing_step};
use crate::{Error, log, rng, tcp, touch};

const EVENT_CAPACITY: usize = 32;
const COMMAND_CAPACITY: usize = 16;

/// Options for [`Client::connect`].
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Name we report to the device.
    pub name: String,
    /// Model we report, for example `iPhone10,6`.
    pub model: String,
    /// Time to wait for each response.
    pub timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            name: "ratv".into(),
            model: "iPhone10,6".into(),
            timeout: Duration::from_secs(5),
        }
    }
}

enum Command {
    Request {
        xid: u32,
        payload: Vec<u8>,
        reply: oneshot::Sender<Bytes>,
    },
    Event {
        payload: Vec<u8>,
    },
    Close,
}

/// Connected Companion session.
///
/// Requests can be sent from several tasks at the same time. A background task
/// owns the socket; it stops when the client is dropped or the device disconnects.
#[derive(Debug)]
pub struct Client {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
    xid: AtomicU32,
    timeout: Duration,
    session_id: u64,
    /// Start of the touch session, for touch timestamps.
    touch_base: Instant,
    task: JoinHandle<()>,
    span: Span,
}

impl Client {
    /// Connects to `addr`, runs pair-verify and starts a session.
    pub async fn connect(
        addr: SocketAddr,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error> {
        let span = log::session("companion", Some(addr));
        async {
            tracing::debug!("connecting");
            let stream = tcp::connect(addr, options.timeout).await?;
            Self::start(stream, credentials, options, Span::current()).await
        }
        .instrument(span)
        .await
    }

    /// Same as [`Client::connect`] on an existing stream.
    pub async fn connect_stream<S>(
        stream: S,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let span = log::session("companion", None);
        Self::start(stream, credentials, options, span.clone())
            .instrument(span)
            .await
    }

    async fn start<S>(
        mut stream: S,
        credentials: &Credentials,
        options: &ClientOptions,
        span: Span,
    ) -> Result<Self, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut conn = Connection::new();
        let mut buf = Vec::with_capacity(256);
        let mut rng = rng::os();
        let (m1, state) = pair_verify::start(credentials, &mut rng);
        let m2 = tokio::time::timeout(
            options.timeout,
            pairing_step(&mut stream, &mut conn, Flow::Verify, true, &m1, &mut buf),
        )
        .await??;
        let (m3, state) = state.handle_m2(&m2)?;
        let m4 = tokio::time::timeout(
            options.timeout,
            pairing_step(&mut stream, &mut conn, Flow::Verify, false, &m3, &mut buf),
        )
        .await??;
        let secret = state.handle_m4(&m4)?;
        conn.enable_encryption(&secret);
        tracing::debug!("pair-verify done");

        let (commands, rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let task = tokio::spawn(run(stream, conn, rx, events.clone()).instrument(span.clone()));
        let mut client = Self {
            commands,
            events,
            xid: AtomicU32::new(rng::u32() >> 16),
            timeout: options.timeout,
            session_id: 0,
            touch_base: Instant::now(),
            task,
            span,
        };

        let rp_id = rng::mac_string(rp_id(&credentials.client_id));
        let ours = rng::u32();
        let info = SystemInfo {
            name: &options.name,
            model: &options.model,
            client_id: &credentials.client_id,
            rp_id: &rp_id,
            pub_id: &rp_id,
        };
        let session = SessionStart { session_id: ours };
        // Sent in this order without waiting for each reply. Replies are matched by xid.
        let touch_area = TouchStart::default();
        let (info, touch_start, session, tv_remote, text_input, interest) = tokio::join!(
            client.request(&info),
            client.request(&touch_area),
            client.request(&session),
            client.request(&TvRemoteSessionStart),
            // Makes the device send keyboard focus events.
            client.request(&TextInputStart),
            client.send_event(&Interest {
                register: &[
                    event::MEDIA_CONTROL,
                    event::SYSTEM_STATUS,
                    event::TV_SYSTEM_STATUS,
                ],
                deregister: &[],
            }),
        );
        info?;
        let device = message::session_start_response(&Message::decode(&session?)?)?;
        client.session_id = message::combined_session_id(device, ours);
        // Older tvOS versions may not support it.
        if let Err(err) = touch_start {
            tracing::debug!(%err, "_touchStart failed");
        }
        if let Err(err) = tv_remote {
            tracing::debug!(%err, "TVRCSessionStart failed");
        }
        if let Err(err) = text_input {
            tracing::debug!(%err, "_tiStart failed");
        }
        interest?;
        tracing::debug!("connected");
        Ok(client)
    }

    fn next_xid(&self) -> u32 {
        self.xid.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a request and returns the raw response payload.
    /// Decode it with [`Message::decode`]. Device errors are returned as `Err`.
    pub async fn request<M: Outgoing + Sync>(&self, msg: &M) -> Result<Bytes, Error> {
        let xid = self.next_xid();
        let mut payload = Vec::with_capacity(64);
        message::encode(msg, xid, &mut payload);
        tracing::trace!(parent: &self.span, id = M::ID, xid, "request");
        let start = Instant::now();
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Request {
                xid,
                payload,
                reply,
            })
            .await
            .map_err(|_| Error::Disconnected)?;
        let Ok(bytes) = tokio::time::timeout(self.timeout, rx).await else {
            tracing::debug!(parent: &self.span, id = M::ID, xid, "request timed out");
            return Err(Error::Timeout);
        };
        let bytes = bytes.map_err(|_| Error::Disconnected)?;
        let elapsed = start.elapsed();
        if let Some(err) = Message::decode(&bytes)?.error() {
            tracing::debug!(parent: &self.span, id = M::ID, xid, ?elapsed, %err, "request failed");
            return Err(err.into());
        }
        tracing::trace!(parent: &self.span, id = M::ID, xid, ?elapsed, "response");
        Ok(bytes)
    }

    /// Sends an event (no response).
    pub async fn send_event<M: Outgoing + Sync>(&self, msg: &M) -> Result<(), Error> {
        let mut payload = Vec::with_capacity(64);
        let xid = self.next_xid();
        message::encode(msg, xid, &mut payload);
        tracing::trace!(parent: &self.span, id = M::ID, xid, "event");
        self.commands
            .send(Command::Event { payload })
            .await
            .map_err(|_| Error::Disconnected)
    }

    /// Receives device events (media controls, power state, keyboard focus).
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Combined session id.
    #[must_use]
    pub const fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Returns true while the connection is open.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        !self.task.is_finished()
    }

    /// Sets the state of a button.
    pub async fn button(&self, command: HidCommand, state: ButtonState) -> Result<(), Error> {
        self.request(&Hid { command, state }).await.map(drop)
    }

    /// Clicks a button (press and release).
    pub async fn press(&self, command: HidCommand) -> Result<(), Error> {
        // The device starts the screensaver without answering, so do not wait for replies.
        if command == HidCommand::Screensaver {
            for state in [ButtonState::Down, ButtonState::Up] {
                self.send_event(&Hid { command, state }).await?;
            }
            return Ok(());
        }
        self.button(command, ButtonState::Down).await?;
        self.button(command, ButtonState::Up).await
    }

    /// Holds a button for `duration`.
    pub async fn hold(&self, command: HidCommand, duration: Duration) -> Result<(), Error> {
        self.button(command, ButtonState::Down).await?;
        tokio::time::sleep(duration).await;
        self.button(command, ButtonState::Up).await
    }

    /// Sends a media control command.
    pub async fn media(&self, command: MediaControl) -> Result<(), Error> {
        self.request(&command).await.map(drop)
    }

    /// Returns the volume from 0.0 to 1.0.
    pub async fn volume(&self) -> Result<f64, Error> {
        let reply = self.request(&MediaControl::GetVolume).await?;
        Ok(message::volume_response(&Message::decode(&reply)?)?)
    }

    /// Sets the volume from 0.0 to 1.0.
    pub async fn set_volume(&self, volume: f64) -> Result<(), Error> {
        self.media(MediaControl::SetVolume(volume.clamp(0.0, 1.0)))
            .await
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

    /// Replaces the text of the focused text field.
    pub async fn set_text(&self, text: &str) -> Result<(), Error> {
        self.edit_text(text, true).await
    }

    /// Adds text at the cursor of the focused text field.
    pub async fn append_text(&self, text: &str) -> Result<(), Error> {
        self.edit_text(text, false).await
    }

    /// Removes all text of the focused text field.
    pub async fn clear_text(&self) -> Result<(), Error> {
        self.edit_text("", true).await
    }

    async fn edit_text(&self, text: &str, clear: bool) -> Result<(), Error> {
        let session = self.text_session().await?.ok_or(Error::NoTextField)?;
        if clear {
            let data = text_input::clear_text(&session.uuid);
            self.send_event(&TextInputCommand { data: &data }).await?;
        }
        if !text.is_empty() {
            let data = text_input::insert_text(&session.uuid, text);
            self.send_event(&TextInputCommand { data: &data }).await?;
        }
        Ok(())
    }

    /// Restarts the text input session to get the current text field.
    async fn text_session(&self) -> Result<Option<TextSession>, Error> {
        let (stop, start) =
            tokio::join!(self.request(&TextInputStop), self.request(&TextInputStart));
        if let Err(err) = stop {
            tracing::debug!(parent: &self.span, %err, "_tiStop failed");
        }
        let reply = start?;
        let msg = Message::decode(&reply)?;
        text_input::session(&msg).map_err(|err| {
            tracing::debug!(parent: &self.span, %err, "unknown text input data");
            #[cfg(feature = "sensitive-logs")]
            tracing::trace!(parent: &self.span, data = ?msg.field("_tiD"), "unknown text input data");
            err.into()
        })
    }

    /// Sends a touch event. `x` and `y` range from 0.0 to 1.0.
    pub async fn touch(&self, x: f64, y: f64, phase: TouchPhase) -> Result<(), Error> {
        let area = TouchStart::default();
        // Rounded and clamped, so the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scale = |v: f64, size: f64| (v.clamp(0.0, 1.0) * size).round() as u16;
        self.send_event(&Touch {
            timestamp_ns: u64::try_from(self.touch_base.elapsed().as_nanos()).unwrap_or(u64::MAX),
            finger: 1,
            x: scale(x, area.width),
            y: scale(y, area.height),
            phase,
        })
        .await
    }

    /// Swipes from `from` to `to` in about `duration`. Coordinates range from 0.0 to 1.0.
    pub async fn swipe(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
    ) -> Result<(), Error> {
        touch::swipe(from, to, duration, |x, y, phase| {
            let phase = match phase {
                touch::Phase::Begin => TouchPhase::Press,
                touch::Phase::Move => TouchPhase::Hold,
                touch::Phase::End => TouchPhase::Release,
            };
            self.touch(x, y, phase)
        })
        .await
    }

    /// Taps at `x`, `y` without a click. Coordinates range from 0.0 to 1.0.
    pub async fn tap(&self, x: f64, y: f64) -> Result<(), Error> {
        self.touch(x, y, TouchPhase::Press).await?;
        tokio::time::sleep(touch::STEP).await;
        self.touch(x, y, TouchPhase::Release).await
    }

    /// Wakes the device.
    pub async fn turn_on(&self) -> Result<(), Error> {
        self.press(HidCommand::Wake).await
    }

    /// Puts the device to sleep.
    pub async fn turn_off(&self) -> Result<(), Error> {
        self.press(HidCommand::Sleep).await
    }

    /// Stops the session and closes the connection.
    ///
    /// A device error for the stop request is ignored: the device ends the
    /// session itself when the app changes or it goes to sleep.
    pub async fn close(mut self) -> Result<(), Error> {
        let result = match self
            .request(&SessionStop {
                session_id: self.session_id,
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(Error::Companion(err @ ratv_companion::Error::Device { .. })) => {
                tracing::debug!(parent: &self.span, %err, "session already stopped by device");
                Ok(())
            }
            Err(err) => Err(err),
        };
        if self.commands.send(Command::Close).await.is_ok() {
            let _ = (&mut self.task).await;
        }
        result
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run<S>(
    mut stream: S,
    mut conn: Connection,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<Event>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pending: Vec<(u32, oneshot::Sender<Bytes>)> = Vec::new();
    let result: Result<(), Error> = async {
        loop {
            tokio::select! {
                cmd = commands.recv() => {
                    let payload = match cmd {
                        None | Some(Command::Close) => return Ok(()),
                        Some(Command::Request { xid, payload, reply }) => {
                            pending.retain(|(_, tx)| !tx.is_closed());
                            pending.push((xid, reply));
                            payload
                        }
                        Some(Command::Event { payload }) => payload,
                    };
                    conn.send(FrameType::E_OPACK, &payload);
                    flush(&mut stream, &mut conn).await?;
                }
                n = stream.read_buf(conn.receive_buffer(4096)) => {
                    if n? == 0 {
                        return Err(Error::Disconnected);
                    }
                    while let Some(frame) = conn.poll_frame()? {
                        dispatch(frame.ty, frame.payload, &mut pending, &events);
                    }
                }
            }
        }
    }
    .await;
    match result {
        Ok(()) => tracing::debug!("connection closed"),
        Err(err) => tracing::debug!(%err, "connection ended"),
    }
    let _ = stream.shutdown().await;
}

fn dispatch(
    ty: FrameType,
    payload: Bytes,
    pending: &mut Vec<(u32, oneshot::Sender<Bytes>)>,
    events: &broadcast::Sender<Event>,
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
        Some(MessageType::Event) => {
            if let Some(event) = Event::parse(&msg) {
                let _ = events.send(event);
            }
        }
        Some(MessageType::Response) | None => {
            let Some(xid) = msg.xid() else { return };
            if let Some(i) = pending.iter().position(|(x, _)| *x == xid) {
                let (_, reply) = pending.swap_remove(i);
                let _ = reply.send(payload);
            } else {
                tracing::trace!(xid, "response without request");
            }
        }
        Some(MessageType::Request) => tracing::trace!(id = ?msg.id(), "ignoring device request"),
    }
}

/// Device id for `_systemInfo`, stable per pairing.
/// Some tvOS versions close the connection if it changes between connects.
fn rp_id(client_id: &[u8]) -> [u8; 6] {
    // FNV-1a, stable across Rust versions.
    let hash = client_id.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let mut id = [0; 6];
    id.copy_from_slice(&hash.to_le_bytes()[..6]);
    // Locally administered, unicast.
    id[0] = (id[0] | 0x02) & !0x01;
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp_id_is_stable_per_client() {
        let a = rp_id(b"E5B59DDB-9930-4ACC-BC01-BDB185F96216");
        assert_eq!(a, rp_id(b"E5B59DDB-9930-4ACC-BC01-BDB185F96216"));
        assert_ne!(a, rp_id(b"1404D88A-8D8A-49C1-82A4-5112791D0018"));
        assert_eq!(a[0] & 0x03, 0x02);
    }

    #[test]
    fn default_options() {
        let o = ClientOptions::default();
        assert_eq!(o.timeout, Duration::from_secs(5));
        assert_eq!(o.model, "iPhone10,6");
    }
}
