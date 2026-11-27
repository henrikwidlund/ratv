//! One handle for a device: Companion and MRP together.
//!
//! [`AppleTv::connect`] opens both protocols in parallel, returns when both
//! have connected or failed, and keeps the ones that connect.
//! [`AppleTv::connect_missing`] adds the others later. Each
//! method uses the protocol that supports it:
//!
//! | Feature | Protocol |
//! |---------|----------|
//! | buttons | Companion, else MRP |
//! | play, pause, next, previous, skip, fast forward, rewind | MRP, else Companion |
//! | seek, stop, shuffle, repeat, speed, rating, like, languages, chapters, queue, artwork, volume state, mute | MRP |
//! | now playing | MRP, with Companion merged in, else Companion |
//! | volume | MRP, else Companion |
//! | power, apps, accounts, text input, swipe, tap | Companion |
//! | Siri | Companion, else MRP |
//!
//! ```no_run
//! # async fn run(endpoints: ratv::Endpoints, credentials: ratv::Credentials) -> Result<(), ratv::Error> {
//! use ratv::{AppleTv, ClientOptions, Key};
//!
//! let tv = AppleTv::connect(&endpoints, &credentials, &ClientOptions::default()).await?;
//! tv.press(Key::Select).await?;
//! tv.pause().await?;
//! println!("{:?}", tv.now_playing().borrow().title);
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::net::TcpStream;
use tokio::sync::watch;
use tracing::Instrument;

use crate::companion::{
    self, Account, App, ButtonState, HidCommand, MediaControl, MediaControlFlags,
};
use crate::mrp::{
    self, Artwork, Button, Command, LanguageOption, NowPlaying, PlaybackState, RepeatMode,
    ShuffleMode,
};
use crate::{ClientOptions, Credentials, Error, PairOptions, Siri, log, now_playing};

/// Where the device's services listen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Endpoints {
    /// `_companion-link._tcp` address.
    pub companion: Option<SocketAddr>,
    /// `_airplay._tcp` address, used for MRP.
    pub airplay: Option<SocketAddr>,
}

#[cfg(feature = "discovery")]
impl From<&crate::discovery::Device> for Endpoints {
    fn from(d: &crate::discovery::Device) -> Self {
        use crate::discovery::ServiceKind;
        Self {
            companion: d.socket_addr(ServiceKind::Companion),
            airplay: d.socket_addr(ServiceKind::AirPlay),
        }
    }
}

/// A protocol of [`AppleTv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Protocol {
    /// Companion, at [`Endpoints::companion`].
    Companion,
    /// MRP, at [`Endpoints::airplay`].
    Mrp,
}

/// Starts pairing. The device shows a PIN when this returns.
///
/// Uses Companion if `endpoints` has it, else AirPlay. Credentials from
/// Companion pairing work for both protocols on recent tvOS versions.
pub async fn pair(endpoints: &Endpoints, options: &PairOptions) -> Result<PinPrompt, Error> {
    let prompt = match (endpoints.companion, endpoints.airplay) {
        (Some(addr), _) => Prompt::Companion(companion::pair(addr, options).await?),
        (None, Some(addr)) => Prompt::Mrp(mrp::pair(addr, options).await?),
        (None, None) => return Err(Error::NoEndpoints),
    };
    Ok(PinPrompt(prompt))
}

/// Pairing that waits for the PIN shown on the device, from [`pair`].
#[derive(Debug)]
pub struct PinPrompt(Prompt);

#[derive(Debug)]
enum Prompt {
    Companion(companion::PinPrompt<TcpStream>),
    Mrp(mrp::PinPrompt),
}

impl PinPrompt {
    /// Completes pairing with the PIN shown on the device.
    pub async fn finish(self, pin: &str) -> Result<Credentials, Error> {
        match self.0 {
            Prompt::Companion(p) => p.finish(pin).await,
            Prompt::Mrp(p) => p.finish(pin).await,
        }
    }
}

/// Remote control key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Select,
    Menu,
    Home,
    /// Behavior depends on the app: some go to the Home Screen, some do nothing.
    TopMenu,
    PlayPause,
    VolumeUp,
    VolumeDown,
    Sleep,
    Wake,
    Guide,
    PageUp,
    /// Opens Control Center.
    PageDown,
    Screensaver,
    ChannelUp,
    ChannelDown,
    /// Starts Siri.
    Siri,
}

impl Key {
    const fn hid(self) -> Option<HidCommand> {
        Some(match self {
            Self::Up => HidCommand::Up,
            Self::Down => HidCommand::Down,
            Self::Left => HidCommand::Left,
            Self::Right => HidCommand::Right,
            Self::Select => HidCommand::Select,
            Self::Menu => HidCommand::Menu,
            Self::Home => HidCommand::Home,
            Self::PlayPause => HidCommand::PlayPause,
            Self::VolumeUp => HidCommand::VolumeUp,
            Self::VolumeDown => HidCommand::VolumeDown,
            Self::Sleep => HidCommand::Sleep,
            Self::Wake => HidCommand::Wake,
            Self::Guide => HidCommand::Guide,
            Self::PageUp => HidCommand::PageUp,
            Self::PageDown => HidCommand::PageDown,
            Self::Screensaver => HidCommand::Screensaver,
            Self::ChannelUp => HidCommand::ChannelIncrement,
            Self::ChannelDown => HidCommand::ChannelDecrement,
            Self::Siri => HidCommand::Siri,
            Self::TopMenu => return None,
        })
    }

    const fn button(self) -> Option<Button> {
        Some(match self {
            Self::Up => Button::Up,
            Self::Down => Button::Down,
            Self::Left => Button::Left,
            Self::Right => Button::Right,
            Self::Select => Button::Select,
            Self::Menu => Button::Menu,
            Self::Home => Button::Home,
            Self::TopMenu => Button::TopMenu,
            Self::PlayPause => Button::PlayPause,
            Self::VolumeUp => Button::VolumeUp,
            Self::VolumeDown => Button::VolumeDown,
            Self::Sleep => Button::Suspend,
            Self::Wake => Button::Wakeup,
            Self::Guide
            | Self::PageUp
            | Self::PageDown
            | Self::Screensaver
            | Self::ChannelUp
            | Self::ChannelDown => return None,
            Self::Siri => Button::Siri,
        })
    }
}

/// Client chosen for a call: Companion with `A`, or MRP with `B`.
enum Route<'a, A, B> {
    Companion(&'a companion::Client, A),
    Mrp(&'a mrp::Client, B),
}

/// Seconds to skip instead of fast forward or rewind, if the app does not say.
const SCRUB_SKIP: f64 = 10.0;
/// Time after connect in which playback calls wait for the app's command list.
const STATE_WAIT: Duration = Duration::from_secs(1);
/// Apps that list fast forward and rewind, but go back to the start position when it ends.
const SKIP_ONLY_APPS: &[&str] = &["com.disney.disneyplus"];

/// True if the app has no handler for the command.
const fn is_unsupported(result: &Result<(), Error>) -> bool {
    matches!(
        result,
        Err(Error::CommandFailed {
            handler_status: 404,
            ..
        })
    )
}

/// What the current app says about a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Support {
    Yes,
    No,
    /// MRP is not open, or the app sent no command list.
    Unknown,
}

impl Support {
    fn of(np: &NowPlaying, command: Command) -> Self {
        if np.commands.is_empty() {
            Self::Unknown
        } else if np.supports(command) {
            Self::Yes
        } else {
            Self::No
        }
    }
}

/// What the last `begin_*` call did, per direction (rewind, fast forward).
#[derive(Debug, Default)]
struct Scrubs([AtomicU8; 2]);

impl Scrubs {
    const UNKNOWN: u8 = 0;
    const SKIPPED: u8 = 1;
    const STARTED: u8 = 2;

    fn set(&self, forward: bool, state: u8) {
        self.0[usize::from(forward)].store(state, Ordering::Relaxed);
    }

    /// Returns the state and sets it back to unknown.
    fn take(&self, forward: bool) -> u8 {
        self.0[usize::from(forward)].swap(Self::UNKNOWN, Ordering::Relaxed)
    }
}

/// A connected Apple TV.
///
/// Clones share the connections. If one protocol closes, calls that the
/// other protocol supports use the other one.
///
/// Playback calls check [`NowPlaying::commands`]. If the app does not list a
/// command, they use another way to do the same, for example a key press.
/// In the first second after connect, they first wait until the list arrives.
#[derive(Debug, Clone)]
pub struct AppleTv {
    companion: Option<companion::Client>,
    mrp: Option<mrp::Client>,
    connected_at: Instant,
    scrubs: Arc<Scrubs>,
    /// Now playing: merged when Companion connected, else from MRP. After
    /// Companion closes, MRP data only.
    now_playing: watch::Receiver<NowPlaying>,
    /// Task that merges MRP and Companion now playing.
    merge: Option<Arc<now_playing::MergeTask>>,
    /// Connect errors of the protocols that are not connected.
    companion_error: Option<Arc<Error>>,
    mrp_error: Option<Arc<Error>>,
}

impl AppleTv {
    /// Connects to all given endpoints in parallel.
    ///
    /// Succeeds if at least one protocol connects. Fails with the Companion
    /// error, else the MRP error, if none does, or with
    /// [`Error::NoEndpoints`] if no endpoint is given. On success, see
    /// [`connect_error`](Self::connect_error) for the protocol that failed.
    pub async fn connect(
        endpoints: &Endpoints,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Self, Error> {
        async {
            let (companion, mrp) =
                connect_clients(endpoints, credentials, options, true, true).await;
            let companion = split("Companion", companion);
            let mrp = split("MRP", mrp);
            let tv = Self::assemble(companion, mrp, Instant::now(), Arc::default())?;
            tv.log_connect();
            Ok(tv)
        }
        .instrument(log::session("apple tv", None))
        .await
    }

    /// Connects the protocols that are not open (see [`is_open`](Self::is_open)),
    /// and returns a new handle with them and the open protocols of `self`.
    ///
    /// The open protocols stay connected. `self` and its clones do not get
    /// the new protocols, so replace them with the returned handle:
    ///
    /// - Do not [`close`](Self::close) `self`. It shares the open protocols
    ///   with the new handle, so they would close too.
    /// - Wait on [`closed`](Self::closed) of the new handle, so that a close
    ///   of an added protocol is seen.
    /// - Get [`now_playing`](Self::now_playing) and [`volume`](Self::volume)
    ///   again from the new handle. Receivers from `self` do not get data from
    ///   the added protocols. They stay open while their sender lives: an MRP
    ///   receiver until MRP closes, a merged one until all clones of `self`
    ///   are dropped or all its protocols have closed.
    ///
    /// A closed protocol that does not connect again is not in the new handle.
    ///
    /// Returns `None` if all protocols are open. Fails with the Companion
    /// error, else the MRP error, if no missing protocol connects, and with
    /// [`Error::NoEndpoints`] if no missing protocol has an endpoint. On
    /// failure, a protocol of `self` that closed is still in it, so
    /// [`closed`](Self::closed) returns at once. A protocol that never
    /// connected gives no event. Wait before trying again. On success,
    /// [`connect_error`](Self::connect_error) has the errors of the protocols
    /// that are still missing.
    ///
    /// ```no_run
    /// # async fn run(mut tv: ratv::AppleTv, endpoints: ratv::Endpoints, credentials: ratv::Credentials) -> Result<(), ratv::Error> {
    /// use ratv::{ClientOptions, Error, Protocol};
    ///
    /// if !tv.is_open(Protocol::Companion) || !tv.is_open(Protocol::Mrp) {
    ///     let options = ClientOptions::default();
    ///     let result = tv.connect_missing(&endpoints, &credentials, &options).await;
    ///     // The device does not offer the missing protocol: keep `tv`. If a
    ///     // protocol closed, `closed()` returns at once, so wait before the
    ///     // next try.
    ///     if let Err(Error::NoEndpoints) = result {
    ///         return Ok(());
    ///     }
    ///     if let Some(new) = result? {
    ///         // Not `tv.close()`: the old handle shares the open protocol.
    ///         tv = new;
    ///         // Get these again, and stop the tasks that use the old ones.
    ///         let now_playing = tv.now_playing();
    ///         let volume = tv.volume();
    ///         let closed = tv.closed();
    /// #       drop((now_playing, volume, closed));
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect_missing(
        &self,
        endpoints: &Endpoints,
        credentials: &Credentials,
        options: &ClientOptions,
    ) -> Result<Option<Self>, Error> {
        let (companion, mrp) = (self.open_companion(), self.open_mrp());
        if companion.is_some() && mrp.is_some() {
            return Ok(None);
        }
        self.connect_missing_inner(endpoints, credentials, options, companion, mrp)
            .instrument(log::session("apple tv", None))
            .await
    }

    async fn connect_missing_inner(
        &self,
        endpoints: &Endpoints,
        credentials: &Credentials,
        options: &ClientOptions,
        companion: Option<&companion::Client>,
        mrp: Option<&mrp::Client>,
    ) -> Result<Option<Self>, Error> {
        let (new_companion, new_mrp) = connect_clients(
            endpoints,
            credentials,
            options,
            companion.is_none(),
            mrp.is_none(),
        )
        .await;
        let (new_companion, companion_error) = split("Companion", new_companion);
        let (new_mrp, mrp_error) = split("MRP", new_mrp);
        if new_companion.is_none() && new_mrp.is_none() {
            return Err(first_error(companion_error, mrp_error));
        }
        // A new MRP connection has no scrub state yet, and playback calls
        // wait for its command list.
        let (connected_at, scrubs) = if new_mrp.is_some() {
            (Instant::now(), Arc::default())
        } else {
            (self.connected_at, self.scrubs.clone())
        };
        let tv = Self::assemble(
            (
                new_companion.or_else(|| companion.cloned()),
                companion_error,
            ),
            (new_mrp.or_else(|| mrp.cloned()), mrp_error),
            connected_at,
            scrubs,
        )?;
        tv.log_connect();
        Ok(Some(tv))
    }

    /// Builds the handle from the clients and connect errors. Fails if no
    /// protocol is connected.
    fn assemble(
        (companion, companion_error): (Option<companion::Client>, Option<Arc<Error>>),
        (mrp, mrp_error): (Option<mrp::Client>, Option<Arc<Error>>),
        connected_at: Instant,
        scrubs: Arc<Scrubs>,
    ) -> Result<Self, Error> {
        let (now_playing, merge) = match (&companion, &mrp) {
            (Some(c), m) => {
                let (rx, task) = now_playing::spawn(
                    m.as_ref().map(mrp::Client::now_playing),
                    c.now_playing(),
                    c.now_playing_known(),
                );
                (rx, Some(Arc::new(task)))
            }
            (None, Some(m)) => (m.now_playing(), None),
            (None, None) => return Err(first_error(companion_error, mrp_error)),
        };
        Ok(Self {
            companion,
            mrp,
            connected_at,
            scrubs,
            now_playing,
            merge,
            companion_error,
            mrp_error,
        })
    }

    fn log_connect(&self) {
        let (companion, mrp) = (self.companion.is_some(), self.mrp.is_some());
        if let Some(err) = self.companion_error.as_ref().or(self.mrp_error.as_ref()) {
            tracing::warn!(%err, companion, mrp, "one protocol failed to connect");
        } else {
            tracing::debug!(companion, mrp, "connected");
        }
    }

    /// True if `protocol` is connected and has not closed.
    #[must_use]
    pub fn is_open(&self, protocol: Protocol) -> bool {
        match protocol {
            Protocol::Companion => self.open_companion().is_some(),
            Protocol::Mrp => self.open_mrp().is_some(),
        }
    }

    fn open_companion(&self) -> Option<&companion::Client> {
        self.companion.as_ref().filter(|c| c.is_connected())
    }

    fn open_mrp(&self) -> Option<&mrp::Client> {
        self.mrp.as_ref().filter(|m| m.is_connected())
    }

    /// Why `protocol` did not connect in [`connect`](Self::connect) or
    /// [`connect_missing`](Self::connect_missing).
    ///
    /// `None` if it connected or had no endpoint. Use [`Error::is_refused`]
    /// to see if the port may have changed.
    #[must_use]
    pub fn connect_error(&self, protocol: Protocol) -> Option<&Error> {
        match protocol {
            Protocol::Companion => self.companion_error.as_deref(),
            Protocol::Mrp => self.mrp_error.as_deref(),
        }
    }

    /// Companion client, if it connected. It may have closed since:
    /// check with [`is_open`](Self::is_open).
    #[must_use]
    pub const fn companion(&self) -> Option<&companion::Client> {
        self.companion.as_ref()
    }

    /// MRP client, if it connected. It may have closed since:
    /// check with [`is_open`](Self::is_open).
    #[must_use]
    pub const fn mrp(&self) -> Option<&mrp::Client> {
        self.mrp.as_ref()
    }

    fn need_companion(&self) -> Result<&companion::Client, Error> {
        self.companion
            .as_ref()
            .ok_or(Error::NotConnected("Companion"))
    }

    fn need_mrp(&self) -> Result<&mrp::Client, Error> {
        self.mrp.as_ref().ok_or(Error::NotConnected("MRP"))
    }

    /// Picks the client for a call that Companion does with `a` and MRP with `b`.
    ///
    /// An open connection is used before a closed one. If both are open or both
    /// are closed, MRP is used when `prefer_mrp` is true. A closed client
    /// returns [`Error::Closed`].
    fn route<A, B>(&self, prefer_mrp: bool, a: Option<A>, b: Option<B>) -> Option<Route<'_, A, B>> {
        let c = self.companion.as_ref().zip(a);
        let m = self.mrp.as_ref().zip(b);
        let use_mrp = match (&c, &m) {
            (None, None) => return None,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some((c, _)), Some((m, _))) => {
                let (c_open, m_open) = (c.is_connected(), m.is_connected());
                if c_open == m_open { prefer_mrp } else { m_open }
            }
        };
        if use_mrp {
            m.map(|(m, b)| Route::Mrp(m, b))
        } else {
            c.map(|(c, a)| Route::Companion(c, a))
        }
    }

    /// Route for a key: Companion first.
    fn key_route(&self, key: Key) -> Result<Route<'_, HidCommand, Button>, Error> {
        self.route(false, key.hid(), key.button()).ok_or_else(|| {
            if key.button().is_none() {
                Error::NotConnected("Companion")
            } else {
                Error::NotConnected("MRP")
            }
        })
    }

    /// What the device plays.
    ///
    /// MRP data. With Companion, state and position change about 0.5 s
    /// sooner, and [`NowPlaying::artwork_data`] and
    /// [`NowPlaying::captions_enabled`] are set when both name the same item.
    /// Without MRP, only the
    /// Companion data: title, state, rate, position, duration, artwork and
    /// subtitle state, and the app only for the Apple TV app. With both, the
    /// app is cleared after Home when MRP has no title and Companion says
    /// that nothing plays. A first value that is not empty counts as a
    /// change. When MRP closes, only the Companion data stays; when Companion
    /// closes, only the MRP data. When all its protocols have closed, the
    /// receiver keeps the last value and closes. A merged receiver also
    /// closes when all clones of this handle are dropped. Keep each
    /// `borrow()` short, and do not borrow another now playing receiver
    /// while you hold one: the merge reads them while it writes this one.
    #[must_use]
    pub fn now_playing(&self) -> watch::Receiver<NowPlaying> {
        self.now_playing.clone()
    }

    /// Volume and mute state. Needs MRP. After MRP closes, it keeps the last
    /// value and closes.
    #[must_use]
    pub fn volume(&self) -> Option<watch::Receiver<mrp::Volume>> {
        self.mrp.as_ref().map(mrp::Client::volume)
    }

    /// Image of the current item. Needs MRP. See [`mrp::Client::artwork`].
    pub async fn artwork(&self, width: u32, height: u32) -> Result<Option<Artwork>, Error> {
        self.need_mrp()?.artwork(width, height).await
    }

    /// Presses and releases a key.
    /// Over MRP the device does not answer, so `Ok` only means that the message was queued.
    pub async fn press(&self, key: Key) -> Result<(), Error> {
        match self.key_route(key)? {
            Route::Companion(c, hid) => c.press(hid).await,
            Route::Mrp(m, button) => m.press(button).await,
        }
    }

    /// Presses and releases a key two times fast.
    /// Over MRP the device does not answer, so `Ok` only means that the message was queued.
    /// When it fails, some of its 4 messages can still be sent, also with an
    /// error where [`Error::is_not_sent`] is true.
    pub async fn double_tap(&self, key: Key) -> Result<(), Error> {
        match self.key_route(key)? {
            Route::Companion(c, hid) => c.double_tap(hid).await,
            Route::Mrp(m, button) => m.double_tap(button).await,
        }
    }

    /// Opens the app switcher by double tapping [`Key::Home`].
    pub async fn app_switcher(&self) -> Result<(), Error> {
        self.double_tap(Key::Home).await
    }

    /// Opens the context menu of the focused item by holding [`Key::Select`] for 1 s.
    pub async fn context_menu(&self) -> Result<(), Error> {
        self.hold(Key::Select, Duration::from_secs(1)).await
    }

    /// Opens Control Center with [`Key::PageDown`]. Needs Companion.
    pub async fn control_center(&self) -> Result<(), Error> {
        self.press(Key::PageDown).await
    }

    /// Holds a key for `duration`. The key is released also if the
    /// returned future is dropped.
    /// Over MRP the device does not answer, so `Ok` only means that the message was queued.
    pub async fn hold(&self, key: Key, duration: Duration) -> Result<(), Error> {
        match self.key_route(key)? {
            Route::Companion(c, hid) => c.hold(hid, duration).await,
            Route::Mrp(m, button) => m.hold(button, duration).await,
        }
    }

    /// Sends a key down or up event.
    /// Over MRP the device does not answer, so `Ok` only means that the message was queued.
    pub async fn key(&self, key: Key, down: bool) -> Result<(), Error> {
        match self.key_route(key)? {
            Route::Companion(c, hid) => {
                let state = if down {
                    ButtonState::Down
                } else {
                    ButtonState::Up
                };
                c.button(hid, state).await
            }
            Route::Mrp(m, button) => m.button(button, down).await,
        }
    }

    /// Route for a playback call: MRP first, unless the app does not support it.
    fn media_route<A>(
        &self,
        support: Support,
        fallback: Option<A>,
    ) -> Result<Route<'_, A, ()>, Error> {
        self.route(support != Support::No, fallback, Some(()))
            .ok_or(Error::NotConnected("MRP"))
    }

    /// Reads what the device plays, if MRP is open.
    fn with_now_playing<R>(&self, f: impl FnOnce(&NowPlaying) -> R) -> Option<R> {
        let m = self.open_mrp()?;
        Some(f(&m.now_playing().borrow()))
    }

    /// Right after connect, waits until the app's command list arrives, at most
    /// until [`STATE_WAIT`] after connect. Apps may ignore commands sent before it.
    async fn wait_for_state(&self) {
        let Some(m) = self.open_mrp() else {
            return;
        };
        let Some(left) = STATE_WAIT.checked_sub(self.connected_at.elapsed()) else {
            return;
        };
        let mut np = m.now_playing();
        let _ = tokio::time::timeout(left, np.wait_for(|n| !n.commands.is_empty())).await;
    }

    fn support(&self, command: Command) -> Support {
        self.with_now_playing(|np| Support::of(np, command))
            .unwrap_or(Support::Unknown)
    }

    /// True if playing, false if paused or stopped, `None` if not known.
    /// Uses the merged state, which changes sooner than the MRP state.
    fn playing(&self) -> Option<bool> {
        let state = if self.merge.is_some() {
            self.now_playing.borrow().state
        } else {
            self.with_now_playing(|np| np.state).flatten()
        };
        match state? {
            PlaybackState::Playing => Some(true),
            PlaybackState::Paused | PlaybackState::Stopped => Some(false),
            _ => None,
        }
    }

    /// Sends `command`, or the Companion `fallback` if the app does not list `check`.
    async fn playback(
        &self,
        check: Command,
        command: Command,
        fallback: Option<MediaControl>,
    ) -> Result<(), Error> {
        self.wait_for_state().await;
        let support = self.support(check);
        match self.media_route(support, fallback)? {
            Route::Companion(c, f) => c.media(f).await,
            Route::Mrp(m, ()) => {
                let result = m.command(command).await;
                // Right after connect the command list may not be known yet.
                if support == Support::Unknown
                    && is_unsupported(&result)
                    && let Some(f) = fallback
                    && let Some(c) = self.open_companion()
                {
                    return c.media(f).await;
                }
                result
            }
        }
    }

    /// Toggles with the MRP command if the app lists it, else with the play/pause key.
    async fn toggle(&self) -> Result<(), Error> {
        if self.support(Command::TogglePlayPause) == Support::Yes {
            self.need_mrp()?.command(Command::TogglePlayPause).await
        } else {
            self.press(Key::PlayPause).await
        }
    }

    /// Starts playback. When the app does not list Play and the device
    /// already plays, this sends nothing.
    pub async fn play(&self) -> Result<(), Error> {
        self.wait_for_state().await;
        if self.support(Command::Play) == Support::No {
            return match self.playing() {
                Some(true) => Ok(()),
                _ => self.toggle().await,
            };
        }
        self.playback(Command::Play, Command::Play, Some(MediaControl::Play))
            .await
    }

    /// Pauses playback. When the app does not list Pause and the device is
    /// already paused or stopped, this sends nothing.
    pub async fn pause(&self) -> Result<(), Error> {
        self.wait_for_state().await;
        if self.support(Command::Pause) == Support::No {
            return match self.playing() {
                Some(false) => Ok(()),
                _ => self.toggle().await,
            };
        }
        self.playback(Command::Pause, Command::Pause, Some(MediaControl::Pause))
            .await
    }

    /// Toggles play and pause.
    pub async fn play_pause(&self) -> Result<(), Error> {
        self.wait_for_state().await;
        let support = self.support(Command::TogglePlayPause);
        if support == Support::No {
            let command = match self.playing() {
                Some(true) => Command::Pause,
                _ => Command::Play,
            };
            if self.support(command) == Support::Yes {
                return self.need_mrp()?.command(command).await;
            }
            return self.press(Key::PlayPause).await;
        }
        match self.media_route(support, Some(HidCommand::PlayPause))? {
            Route::Companion(c, hid) => c.press(hid).await,
            Route::Mrp(m, ()) => {
                let result = m.command(Command::TogglePlayPause).await;
                // Right after connect the command list may not be known yet,
                // as in `playback`.
                if support == Support::Unknown
                    && is_unsupported(&result)
                    && let Some(c) = self.open_companion()
                {
                    return c.press(HidCommand::PlayPause).await;
                }
                result
            }
        }
    }

    /// Next track.
    pub async fn next(&self) -> Result<(), Error> {
        let c = Command::NextTrack;
        self.playback(c, c, Some(MediaControl::NextTrack)).await
    }

    /// Previous track.
    pub async fn previous(&self) -> Result<(), Error> {
        let c = Command::PreviousTrack;
        self.playback(c, c, Some(MediaControl::PreviousTrack)).await
    }

    /// Stops playback. Pauses if the app does not support stop. Needs MRP.
    pub async fn stop(&self) -> Result<(), Error> {
        self.wait_for_state().await;
        let support = self.support(Command::Stop);
        if support == Support::No {
            return self.pause().await;
        }
        let result = self.playback(Command::Stop, Command::Stop, None).await;
        // The command list may have arrived while waiting for the answer.
        if support == Support::Unknown
            && is_unsupported(&result)
            && self.support(Command::Stop) == Support::No
        {
            return self.pause().await;
        }
        result
    }

    /// Seeks to `position` seconds. Needs MRP. A position that is not finite
    /// gives [`Error::InvalidArgument`].
    pub async fn seek(&self, position: f64) -> Result<(), Error> {
        crate::finite(position, "position")?;
        self.need_mrp()?.seek(position).await
    }

    /// Skips `seconds` forward, or backward if negative.
    /// Seeks instead if the app supports seeking but not skipping.
    /// A value that is not finite, or too large for a 32-bit float, gives
    /// [`Error::InvalidArgument`].
    pub async fn skip(&self, seconds: f64) -> Result<(), Error> {
        // MRP sends a 32-bit float. Checked here, so all ways behave the same.
        #[allow(clippy::cast_possible_truncation)]
        crate::finite(f64::from(seconds as f32), "seconds")?;
        self.wait_for_state().await;
        let check = if seconds < 0.0 {
            Command::SkipBackward
        } else {
            Command::SkipForward
        };
        let support = self.support(check);
        if let Some(to) = self.seek_instead(support, seconds) {
            return self.seek(to).await;
        }
        match self.media_route(support, Some(MediaControl::SkipBy(seconds)))? {
            Route::Companion(c, f) => c.media(f).await,
            Route::Mrp(m, ()) => {
                let result = m.skip(seconds).await;
                // Right after connect the command list may not be known yet,
                // as in `playback`. It may have arrived while waiting for the
                // answer, so check for seeking again first.
                if support == Support::Unknown && is_unsupported(&result) {
                    if let Some(to) = self.seek_instead(self.support(check), seconds) {
                        return self.seek(to).await;
                    }
                    if let Some(c) = self.open_companion() {
                        return c.media(MediaControl::SkipBy(seconds)).await;
                    }
                }
                result
            }
        }
    }

    /// Target position when the app does not support the skip (`support`)
    /// but supports seeking, and the position is known.
    fn seek_instead(&self, support: Support, seconds: f64) -> Option<f64> {
        if support != Support::No || self.support(Command::SeekToPlaybackPosition) != Support::Yes {
            return None;
        }
        let pos = self
            .with_now_playing(|np| np.position(mrp::cocoa_time(SystemTime::now())))
            .flatten()?;
        Some((pos + seconds).max(0.0))
    }

    /// Starts fast forward. Stop it with [`end_fast_forward`](Self::end_fast_forward).
    ///
    /// Skips forward instead if the app supports skipping but not fast forward,
    /// or if fast forward is known not to work in the app. The skip uses
    /// [`NowPlaying::skip_forward_interval`], else 10 s. Do not call the
    /// begin and end of one direction at the same time.
    pub async fn begin_fast_forward(&self) -> Result<(), Error> {
        self.begin_scrub(
            Command::BeginFastForward,
            MediaControl::FastForwardBegin,
            true,
        )
        .await
    }

    /// Stops fast forward. Does nothing if
    /// [`begin_fast_forward`](Self::begin_fast_forward) skipped instead, also
    /// when the connection closed since.
    pub async fn end_fast_forward(&self) -> Result<(), Error> {
        self.end_scrub(
            Command::BeginFastForward,
            Command::EndFastForward,
            MediaControl::FastForwardEnd,
            true,
        )
        .await
    }

    /// Starts rewind. Stop it with [`end_rewind`](Self::end_rewind).
    ///
    /// Skips back instead, like [`begin_fast_forward`](Self::begin_fast_forward).
    pub async fn begin_rewind(&self) -> Result<(), Error> {
        self.begin_scrub(Command::BeginRewind, MediaControl::RewindBegin, false)
            .await
    }

    /// Stops rewind. Does nothing if [`begin_rewind`](Self::begin_rewind)
    /// skipped instead, also when the connection closed since.
    pub async fn end_rewind(&self) -> Result<(), Error> {
        self.end_scrub(
            Command::BeginRewind,
            Command::EndRewind,
            MediaControl::RewindEnd,
            false,
        )
        .await
    }

    /// Seconds to skip instead of `begin`, or `None` to use `begin`.
    fn skip_instead(&self, begin: Command, forward: bool) -> Option<f64> {
        self.with_now_playing(|np| {
            let skip = if forward {
                Command::SkipForward
            } else {
                Command::SkipBackward
            };
            let known_bad = np
                .app
                .as_deref()
                .is_some_and(|a| SKIP_ONLY_APPS.contains(&a));
            if Support::of(np, skip) != Support::Yes
                || (Support::of(np, begin) != Support::No && !known_bad)
            {
                return None;
            }
            let interval = if forward {
                np.skip_forward_interval
            } else {
                np.skip_backward_interval
            };
            // `skip` takes only values that fit in an f32.
            let seconds = interval
                .filter(|s| *s > 0.0 && *s <= f64::from(f32::MAX))
                .unwrap_or(SCRUB_SKIP);
            Some(if forward { seconds } else { -seconds })
        })
        .flatten()
    }

    async fn begin_scrub(
        &self,
        begin: Command,
        fallback: MediaControl,
        forward: bool,
    ) -> Result<(), Error> {
        // Set before the first wait, so a canceled call leaves no old state.
        // Each state is set only after its command worked.
        self.scrubs.set(forward, Scrubs::UNKNOWN);
        self.wait_for_state().await;
        if let Some(s) = self.skip_instead(begin, forward) {
            return self.scrub_skip(forward, s).await;
        }
        let support = self.support(begin);
        let result = match self.media_route(support, Some(fallback))? {
            Route::Companion(c, f) => c.media(f).await,
            Route::Mrp(m, ()) => {
                let result = m.command(begin).await;
                if !is_unsupported(&result) {
                    result
                } else if let Some(s) = self.skip_instead(begin, forward) {
                    // The command list may have arrived while waiting for the answer.
                    return self.scrub_skip(forward, s).await;
                } else if support == Support::Unknown
                    && let Some(c) = self.open_companion()
                {
                    // Right after connect the command list may not be known
                    // yet, as in `playback`.
                    c.media(fallback).await
                } else {
                    result
                }
            }
        };
        if result.is_ok() {
            self.scrubs.set(forward, Scrubs::STARTED);
        }
        result
    }

    /// Skips instead of a scrub. After a failed skip the state stays
    /// unknown, so the next end checks again. Then the next end does not
    /// return `Ok` when the connection is closed.
    async fn scrub_skip(&self, forward: bool, seconds: f64) -> Result<(), Error> {
        let result = self.skip(seconds).await;
        if result.is_ok() {
            self.scrubs.set(forward, Scrubs::SKIPPED);
        }
        result
    }

    /// Does the opposite of the last begin. If it is not known, checks `begin`
    /// again.
    async fn end_scrub(
        &self,
        begin: Command,
        end: Command,
        fallback: MediaControl,
        forward: bool,
    ) -> Result<(), Error> {
        match self.scrubs.take(forward) {
            Scrubs::SKIPPED => return Ok(()),
            Scrubs::STARTED => {}
            _ => {
                self.wait_for_state().await;
                if self.skip_instead(begin, forward).is_some() {
                    return Ok(());
                }
            }
        }
        self.playback(begin, end, Some(fallback)).await
    }

    /// Sets the shuffle mode. Needs MRP.
    pub async fn set_shuffle(&self, mode: ShuffleMode) -> Result<(), Error> {
        self.need_mrp()?.set_shuffle(mode).await
    }

    /// Sets the repeat mode. Needs MRP.
    pub async fn set_repeat(&self, mode: RepeatMode) -> Result<(), Error> {
        self.need_mrp()?.set_repeat(mode).await
    }

    /// Changes to the next shuffle mode. Needs MRP.
    pub async fn cycle_shuffle(&self) -> Result<(), Error> {
        let c = Command::AdvanceShuffleMode;
        self.playback(c, c, None).await
    }

    /// Changes to the next repeat mode. Needs MRP.
    pub async fn cycle_repeat(&self) -> Result<(), Error> {
        let c = Command::AdvanceRepeatMode;
        self.playback(c, c, None).await
    }

    /// Next chapter. Needs MRP. See [`NowPlaying::chapters`], which is filled shortly
    /// after the item starts.
    /// Fails with [`Error::NoChapter`] if the app does not support the
    /// command and no position or no target chapter start is known, and with [`Error::Closed`] when
    /// MRP is closed.
    pub async fn next_chapter(&self) -> Result<(), Error> {
        self.chapter(Command::NextChapter, NowPlaying::next_chapter_start)
            .await
    }

    /// Start of the current chapter, or the previous chapter in the first 3 s. Needs MRP.
    /// Fails with [`Error::NoChapter`] if the app does not support the
    /// command and no position or no target chapter start is known, and with [`Error::Closed`] when
    /// MRP is closed.
    pub async fn previous_chapter(&self) -> Result<(), Error> {
        self.chapter(Command::PreviousChapter, |np, pos| {
            // Before the first chapter start, go to the start of the item.
            np.previous_chapter_start(pos)
                .or_else(|| (!np.chapters.is_empty()).then_some(0.0))
        })
        .await
    }

    /// Sends `command` if the app supports it, else seeks to the start from `target`.
    async fn chapter(
        &self,
        command: Command,
        target: impl Fn(&NowPlaying, f64) -> Option<f64>,
    ) -> Result<(), Error> {
        self.wait_for_state().await;
        let m = self.need_mrp()?;
        // The last value of a closed connection would give `NoChapter`.
        if !m.is_connected() {
            return Err(Error::Closed);
        }
        let start = {
            let np = m.now_playing();
            let np = np.borrow();
            if np.supports(command) {
                None
            } else {
                // Video players often send chapters but do not support the command.
                let pos = np.position(mrp::cocoa_time(SystemTime::now()));
                Some(pos.and_then(|p| target(&np, p)).ok_or(Error::NoChapter)?)
            }
        };
        match start {
            Some(s) => m.seek(s).await,
            None => m.command(command).await,
        }
    }

    /// Turns on an audio or subtitle language. Needs MRP.
    /// See [`NowPlaying::audio_languages`] and [`NowPlaying::subtitle_languages`].
    pub async fn enable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.need_mrp()?.enable_language(option).await
    }

    /// Turns off a language. Needs MRP. To turn off subtitles, use
    /// [`disable_subtitles`](Self::disable_subtitles), which also works when subtitles are automatic.
    pub async fn disable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.need_mrp()?.disable_language(option).await
    }

    /// Holds Siri. Send the audio, then call [`Siri::finish`].
    ///
    /// Uses Companion, as the iPhone remote does, else MRP. Siri must be on
    /// in the device settings, see [`siri_enabled`](Self::siri_enabled). To choose, use
    /// [`companion::Client::start_siri`] or [`mrp::Client::start_siri`].
    /// Fails with [`Error::SiriActive`] while another [`Siri`] on the same
    /// protocol exists, over
    /// Companion with [`Error::SiriDisabled`] if Siri is off, and with
    /// [`Error::Closed`] if the protocol it uses is closed. Over MRP, audio is
    /// kept until the device records, see [`Error::SiriNotRecording`].
    pub async fn start_siri(&self) -> Result<Siri, Error> {
        match self.route(false, Some(()), Some(())) {
            Some(Route::Companion(c, ())) => c
                .start_siri(&companion::SiriOptions::default())
                .await
                .map(Siri::Companion),
            Some(Route::Mrp(m, ())) => m.start_siri().await.map(Siri::Mrp),
            None => Err(Error::NotConnected("Companion")),
        }
    }

    /// Turns off subtitles. Needs MRP.
    pub async fn disable_subtitles(&self) -> Result<(), Error> {
        self.need_mrp()?.disable_subtitles().await
    }

    /// Plays `NowPlaying::up_next[index]`. Needs MRP.
    pub async fn play_queue_item(&self, index: usize) -> Result<(), Error> {
        self.need_mrp()?.play_queue_item(index).await
    }

    /// Sets the volume, 0.0 to 1.0. Fails with [`Error::VolumeNotAvailable`] if MRP
    /// reports that the device cannot change it. NaN gives
    /// [`Error::InvalidArgument`]; other values are limited to 0.0 to 1.0.
    /// Over MRP the device does not answer, so `Ok` only means that the message was queued.
    pub async fn set_volume(&self, volume: f32) -> Result<(), Error> {
        crate::volume_level(volume)?;
        self.check_volume().await?;
        match self
            .route(true, Some(()), Some(()))
            .ok_or(Error::NotConnected("MRP"))?
        {
            Route::Companion(c, ()) => c.set_volume(volume).await,
            Route::Mrp(m, ()) => m.set_volume(volume).await,
        }
    }

    /// Sets the playback rate, 1.0 is normal speed. Needs MRP.
    /// Rates the app lists are in [`NowPlaying::supported_rates`], normal speeds in
    /// [`NowPlaying::playback_speeds`]. Apps handle other rates in their own way:
    /// the Apple TV app changes its scan speed, and Apple Podcasts skips to the
    /// next episode. So a rate that is not finite, or that
    /// [`NowPlaying::accepts_rate`] does not accept, gives
    /// [`Error::InvalidArgument`] and is not sent. A rate close to a listed
    /// rate sends the listed rate, see [`NowPlaying::listed_rate`]. To get
    /// back to normal speed after a scan, use [`play`](Self::play). Without a
    /// command list from the app, no rate is accepted.
    pub async fn set_playback_rate(&self, rate: f32) -> Result<(), Error> {
        crate::finite(f64::from(rate), "rate")?;
        self.wait_for_state().await;
        self.need_mrp()?.set_playback_rate(rate).await
    }

    /// Rates the current item. Needs MRP. See [`NowPlaying::rating_range`].
    /// A rating that is not finite gives [`Error::InvalidArgument`].
    pub async fn set_rating(&self, rating: f32) -> Result<(), Error> {
        crate::finite(f64::from(rating), "rating")?;
        self.need_mrp()?.set_rating(rating).await
    }

    /// Likes the current item, or removes the like. Needs MRP.
    pub async fn set_liked(&self, liked: bool) -> Result<(), Error> {
        self.need_mrp()?.set_liked(liked).await
    }

    /// Dislikes the current item, or removes the dislike. Needs MRP.
    pub async fn set_disliked(&self, disliked: bool) -> Result<(), Error> {
        self.need_mrp()?.set_disliked(disliked).await
    }

    /// Mutes or unmutes the device. Needs MRP. Fails with [`Error::VolumeNotAvailable`]
    /// if the device cannot change the volume.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn set_muted(&self, muted: bool) -> Result<(), Error> {
        self.check_volume().await?;
        self.need_mrp()?.set_muted(muted).await
    }

    /// Fails if MRP reports that the device cannot change the volume. Right after
    /// connect, waits for the volume state like [`wait_for_state`](Self::wait_for_state).
    async fn check_volume(&self) -> Result<(), Error> {
        let Some(m) = self.open_mrp() else {
            return Ok(());
        };
        let mut volume = m.volume();
        if let Some(left) = STATE_WAIT.checked_sub(self.connected_at.elapsed()) {
            let _ = tokio::time::timeout(left, volume.wait_for(|v| v.available.is_some())).await;
        }
        // MRP can close while this waits. Then `route` picks Companion, or MRP
        // gives `Closed`.
        if m.is_connected() && volume.borrow().available == Some(false) {
            return Err(Error::VolumeNotAvailable);
        }
        Ok(())
    }

    /// Wakes the device. Needs Companion. See [`companion::Client::turn_on`](crate::companion::Client::turn_on).
    pub async fn turn_on(&self) -> Result<(), Error> {
        self.need_companion()?.turn_on().await
    }

    /// Puts the device to sleep. Needs Companion.
    pub async fn turn_off(&self) -> Result<(), Error> {
        self.need_companion()?.turn_off().await
    }

    /// Launches an app by bundle id. Needs Companion.
    pub async fn launch_app(&self, bundle_id: &str) -> Result<(), Error> {
        self.need_companion()?.launch_app(bundle_id).await
    }

    /// Opens a URL. Needs Companion.
    pub async fn open_url(&self, url: &str) -> Result<(), Error> {
        self.need_companion()?.open_url(url).await
    }

    /// Installed apps. Needs Companion; not answered by all tvOS versions.
    pub async fn apps(&self) -> Result<Vec<App>, Error> {
        self.need_companion()?.apps().await
    }

    /// User accounts that can be switched to. Needs Companion.
    pub async fn accounts(&self) -> Result<Vec<Account>, Error> {
        self.need_companion()?.accounts().await
    }

    /// Switches to the user account with `id`. Needs Companion.
    pub async fn switch_account(&self, id: &str) -> Result<(), Error> {
        self.need_companion()?.switch_account(id).await
    }

    /// Swipes from `from` to `to` in about `duration`. Coordinates range from 0.0 to 1.0.
    /// Needs Companion.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn swipe(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
    ) -> Result<(), Error> {
        self.need_companion()?.swipe(from, to, duration).await
    }

    /// Taps at `x`, `y` without a click. Coordinates range from 0.0 to 1.0. Needs Companion.
    /// The device does not answer, so `Ok` only means that the message was queued.
    pub async fn tap(&self, x: f64, y: f64) -> Result<(), Error> {
        self.need_companion()?.tap(x, y).await
    }

    /// Text of the focused text field. Needs Companion.
    pub async fn text(&self) -> Result<Option<String>, Error> {
        self.need_companion()?.text().await
    }

    /// Replaces the text of the focused text field. Needs Companion. Fails with
    /// [`Error::NoTextField`] if no text field has focus. If it fails, for
    /// example with text that is too large ([`Error::is_too_large`]), the old
    /// text stays.
    pub async fn set_text(&self, text: &str) -> Result<(), Error> {
        self.need_companion()?.set_text(text).await
    }

    /// Adds text at the cursor of the focused text field. Needs Companion. Fails with
    /// [`Error::NoTextField`] if no text field has focus.
    pub async fn append_text(&self, text: &str) -> Result<(), Error> {
        self.need_companion()?.append_text(text).await
    }

    /// Removes all text of the focused text field. Needs Companion. Fails with
    /// [`Error::NoTextField`] if no text field has focus.
    pub async fn clear_text(&self) -> Result<(), Error> {
        self.need_companion()?.clear_text().await
    }

    /// Closes all connections. When the device does not read, this can take
    /// as long as the slower client close, see
    /// [`companion::Client::close`](crate::companion::Client::close) and
    /// [`mrp::Client::close`](crate::mrp::Client::close).
    pub async fn close(&self) -> Result<(), Error> {
        let (a, b) = tokio::join!(
            async {
                match &self.companion {
                    Some(c) => c.close().await,
                    None => Ok(()),
                }
            },
            async {
                match &self.mrp {
                    Some(m) => m.close().await,
                    None => Ok(()),
                }
            }
        );
        a.and(b)
    }

    /// True if Siri is on in the device settings. Only Companion sends this.
    /// `None` without an open Companion connection or if the device does not
    /// say. [`start_siri`](Self::start_siri) can still use MRP.
    #[must_use]
    pub fn siri_enabled(&self) -> Option<bool> {
        self.open_companion()?.siri_enabled()
    }

    /// True if the Guide button does something on the current screen. Only
    /// Companion sends this. `None` without an open Companion connection or if
    /// the device did not say.
    #[must_use]
    pub fn guide_supported(&self) -> Option<bool> {
        self.open_companion()?.guide_supported()
    }

    /// Media controls that the app in front supports. Not zero while an app
    /// shows a player, also when it is paused. Only Companion sends this.
    /// `None` without an open Companion connection or if the device did not
    /// say.
    #[must_use]
    pub fn media_controls(&self) -> Option<MediaControlFlags> {
        self.open_companion()?.media_controls()
    }

    /// True until one of the connected protocols closes.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.companion
            .as_ref()
            .is_none_or(companion::Client::is_connected)
            && self.mrp.as_ref().is_none_or(mrp::Client::is_connected)
    }

    /// Waits until one of the connected protocols closes. Returns the error
    /// that closed it, or `None` after [`close`](Self::close). After `close`,
    /// a write error or [`Error::NoAnswer`] can still be returned when the
    /// device did not read, [`Error::Disconnected`] when the device closed
    /// the connection first, or the error of a protocol that closed before
    /// `close`. Returns at once when a protocol is already closed.
    ///
    /// The other protocol stays connected. To get all protocols back, call
    /// [`connect_missing`](Self::connect_missing), or call
    /// [`close`](Self::close) or drop all clones, then connect again. If
    /// `connect_missing` fails, this handle still has the closed protocol, so
    /// wait before trying again.
    pub async fn closed(&self) -> Option<Arc<Error>> {
        let companion = async {
            match &self.companion {
                Some(c) => c.closed().await,
                None => std::future::pending().await,
            }
        };
        let mrp = async {
            match &self.mrp {
                Some(m) => m.closed().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            reason = companion => reason,
            reason = mrp => reason,
        }
    }
}

/// Connects the protocols that are wanted and have an endpoint, in parallel.
async fn connect_clients(
    endpoints: &Endpoints,
    credentials: &Credentials,
    options: &ClientOptions,
    want_companion: bool,
    want_mrp: bool,
) -> (
    Option<Result<companion::Client, Error>>,
    Option<Result<mrp::Client, Error>>,
) {
    let companion = async {
        match endpoints.companion.filter(|_| want_companion) {
            Some(addr) => Some(companion::Client::connect(addr, credentials, options).await),
            None => None,
        }
    };
    let mrp = async {
        match endpoints.airplay.filter(|_| want_mrp) {
            Some(addr) => Some(mrp::Client::connect(addr, credentials, options).await),
            None => None,
        }
    };
    tokio::join!(companion, mrp)
}

/// The Companion error, else the MRP error, else [`Error::NoEndpoints`].
fn first_error(companion: Option<Arc<Error>>, mrp: Option<Arc<Error>>) -> Error {
    companion
        .or(mrp)
        .and_then(Arc::into_inner)
        .unwrap_or(Error::NoEndpoints)
}

/// Splits a connect result into the client and the error, and logs the error.
fn split<T>(name: &str, result: Option<Result<T, Error>>) -> (Option<T>, Option<Arc<Error>>) {
    match result {
        Some(Ok(client)) => (Some(client), None),
        Some(Err(err)) => {
            tracing::debug!(%err, protocol = name, "connection failed");
            (None, Some(Arc::new(err)))
        }
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_are_kept_per_direction_until_taken() {
        let scrubs = Scrubs::default();
        scrubs.set(true, Scrubs::STARTED);
        scrubs.set(false, Scrubs::SKIPPED);
        assert_eq!(scrubs.take(true), Scrubs::STARTED);
        assert_eq!(scrubs.take(true), Scrubs::UNKNOWN);
        assert_eq!(scrubs.take(false), Scrubs::SKIPPED);
    }

    #[test]
    fn key_mapping() {
        assert_eq!(Key::Select.hid(), Some(HidCommand::Select));
        assert_eq!(Key::TopMenu.hid(), None);
        assert_eq!(Key::Select.button(), Some(Button::Select));
        assert_eq!(Key::Sleep.button(), Some(Button::Suspend));
        assert_eq!(Key::Guide.hid(), Some(HidCommand::Guide));
        assert_eq!(Key::PageDown.button(), None);
        for key in [
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Menu,
            Key::Home,
            Key::PlayPause,
            Key::VolumeUp,
            Key::VolumeDown,
            Key::Wake,
        ] {
            assert!(key.hid().is_some());
            assert!(key.button().is_some());
        }
    }

    #[test]
    fn support() {
        let mut np = NowPlaying::default();
        assert_eq!(Support::of(&np, Command::Play), Support::Unknown);
        np.commands = vec![Command::Pause];
        assert_eq!(Support::of(&np, Command::Pause), Support::Yes);
        assert_eq!(Support::of(&np, Command::Play), Support::No);
    }

    #[tokio::test]
    async fn no_endpoints() {
        let err = AppleTv::connect(
            &Endpoints::default(),
            &test_credentials(),
            &ClientOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::NoEndpoints));
    }

    #[tokio::test]
    async fn pair_needs_endpoints() {
        let err = pair(&Endpoints::default(), &PairOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoEndpoints));
    }

    fn test_credentials() -> Credentials {
        Credentials::new([0; 32], [0; 32].into(), Vec::new(), Vec::new())
    }
}
