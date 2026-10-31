//! One handle for a device: Companion and MRP together.
//!
//! [`AppleTv::connect`] opens both protocols in parallel and keeps the ones
//! that connect. Each method uses the protocol that supports it:
//!
//! | Feature | Protocol |
//! |---------|----------|
//! | buttons | Companion, else MRP |
//! | play, pause, next, previous, skip, fast forward, rewind | MRP, else Companion |
//! | seek, stop, shuffle, repeat, speed, rating, like, languages, chapters, queue, now playing, artwork, volume state, mute | MRP |
//! | volume | MRP, else Companion |
//! | power, apps, accounts, text input, swipe, tap | Companion |
//!
//! ```no_run
//! # async fn run(device: ratv::discovery::Device, credentials: ratv::Credentials) -> Result<(), ratv::Error> {
//! use ratv::{AppleTv, ConnectOptions, Endpoints, Key};
//!
//! let tv = AppleTv::connect(&Endpoints::from(&device), &credentials, &ConnectOptions::default()).await?;
//! tv.press(Key::Select).await?;
//! tv.pause().await?;
//! if let Some(np) = tv.now_playing() {
//!     println!("{:?}", np.borrow().title);
//! }
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

use crate::companion::{self, Account, App, ButtonState, HidCommand, MediaControl};
use crate::mrp::{
    self, Artwork, Button, Command, LanguageOption, NowPlaying, RepeatMode, ShuffleMode,
};
use crate::{Credentials, Error};

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

/// Options for [`AppleTv::connect`].
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Companion options.
    pub companion: companion::ClientOptions,
    /// MRP options.
    pub mrp: mrp::ClientOptions,
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

/// A connected Apple TV.
#[derive(Debug)]
pub struct AppleTv {
    companion: Option<companion::Client>,
    mrp: Option<mrp::Client>,
}

impl AppleTv {
    /// Connects to all given endpoints in parallel.
    ///
    /// Succeeds if at least one protocol connects. Fails with the first
    /// error if none does.
    pub async fn connect(
        endpoints: &Endpoints,
        credentials: &Credentials,
        options: &ConnectOptions,
    ) -> Result<Self, Error> {
        let companion = async {
            match endpoints.companion {
                Some(addr) => {
                    Some(companion::Client::connect(addr, credentials, &options.companion).await)
                }
                None => None,
            }
        };
        let mrp = async {
            match endpoints.airplay {
                Some(addr) => Some(mrp::Client::connect(addr, credentials, &options.mrp).await),
                None => None,
            }
        };
        let (companion, mrp) = tokio::join!(companion, mrp);
        let mut first_error = None;
        let tv = Self {
            companion: keep("Companion", companion, &mut first_error),
            mrp: keep("MRP", mrp, &mut first_error),
        };
        if tv.companion.is_none() && tv.mrp.is_none() {
            return Err(first_error.unwrap_or(Error::NotConnected("no endpoints")));
        }
        Ok(tv)
    }

    /// Companion client, if connected.
    #[must_use]
    pub const fn companion(&self) -> Option<&companion::Client> {
        self.companion.as_ref()
    }

    /// MRP client, if connected.
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

    /// MRP client and button for keys that Companion does not handle.
    fn mrp_button(&self, key: Key) -> Result<(&mrp::Client, Button), Error> {
        let button = key.button().ok_or(Error::NotConnected("Companion"))?;
        Ok((self.need_mrp()?, button))
    }

    /// What the device plays. Needs MRP.
    #[must_use]
    pub fn now_playing(&self) -> Option<watch::Receiver<NowPlaying>> {
        self.mrp.as_ref().map(mrp::Client::now_playing)
    }

    /// Volume and mute state. Needs MRP.
    #[must_use]
    pub fn volume(&self) -> Option<watch::Receiver<mrp::Volume>> {
        self.mrp.as_ref().map(mrp::Client::volume)
    }

    /// Image of the current item. Needs MRP. See [`mrp::Client::artwork`].
    pub async fn artwork(&self, width: f64, height: f64) -> Result<Option<Artwork>, Error> {
        self.need_mrp()?.artwork(width, height).await
    }

    /// Presses and releases a key.
    pub async fn press(&self, key: Key) -> Result<(), Error> {
        if let (Some(c), Some(hid)) = (&self.companion, key.hid()) {
            return c.press(hid).await;
        }
        let (m, button) = self.mrp_button(key)?;
        m.press(button).await
    }

    /// Holds a key for `duration`.
    pub async fn hold(&self, key: Key, duration: Duration) -> Result<(), Error> {
        if let (Some(c), Some(hid)) = (&self.companion, key.hid()) {
            return c.hold(hid, duration).await;
        }
        let (m, button) = self.mrp_button(key)?;
        m.hold(button, duration).await
    }

    /// Sends a key down or up event.
    pub async fn key(&self, key: Key, down: bool) -> Result<(), Error> {
        if let (Some(c), Some(hid)) = (&self.companion, key.hid()) {
            let state = if down {
                ButtonState::Down
            } else {
                ButtonState::Up
            };
            return c.button(hid, state).await;
        }
        let (m, button) = self.mrp_button(key)?;
        m.button(button, down).await
    }

    async fn playback(
        &self,
        command: Command,
        fallback: Option<MediaControl>,
    ) -> Result<(), Error> {
        match (&self.mrp, &self.companion, fallback) {
            (Some(m), _, _) => m.command(command).await,
            (None, Some(c), Some(f)) => c.media(f).await,
            (None, _, _) => Err(Error::NotConnected("MRP")),
        }
    }

    /// Starts playback.
    pub async fn play(&self) -> Result<(), Error> {
        self.playback(Command::Play, Some(MediaControl::Play)).await
    }

    /// Pauses playback.
    pub async fn pause(&self) -> Result<(), Error> {
        self.playback(Command::Pause, Some(MediaControl::Pause))
            .await
    }

    /// Toggles play and pause.
    pub async fn play_pause(&self) -> Result<(), Error> {
        match &self.mrp {
            Some(m) => m.command(Command::TogglePlayPause).await,
            None => self.need_companion()?.press(HidCommand::PlayPause).await,
        }
    }

    /// Next track.
    pub async fn next(&self) -> Result<(), Error> {
        self.playback(Command::NextTrack, Some(MediaControl::NextTrack))
            .await
    }

    /// Previous track.
    pub async fn previous(&self) -> Result<(), Error> {
        self.playback(Command::PreviousTrack, Some(MediaControl::PreviousTrack))
            .await
    }

    /// Stops playback. Needs MRP.
    pub async fn stop(&self) -> Result<(), Error> {
        self.playback(Command::Stop, None).await
    }

    /// Seeks to `position` seconds. Needs MRP.
    pub async fn seek(&self, position: f64) -> Result<(), Error> {
        self.need_mrp()?.seek(position).await
    }

    /// Skips `seconds` forward, or backward if negative.
    pub async fn skip(&self, seconds: f32) -> Result<(), Error> {
        match &self.mrp {
            Some(m) => m.skip(seconds).await,
            None => {
                self.need_companion()?
                    .media(MediaControl::SkipBy(f64::from(seconds)))
                    .await
            }
        }
    }

    /// Starts fast forward. Stop it with [`end_fast_forward`](Self::end_fast_forward).
    pub async fn begin_fast_forward(&self) -> Result<(), Error> {
        self.playback(
            Command::BeginFastForward,
            Some(MediaControl::FastForwardBegin),
        )
        .await
    }

    /// Stops fast forward.
    pub async fn end_fast_forward(&self) -> Result<(), Error> {
        self.playback(Command::EndFastForward, Some(MediaControl::FastForwardEnd))
            .await
    }

    /// Starts rewind. Stop it with [`end_rewind`](Self::end_rewind).
    pub async fn begin_rewind(&self) -> Result<(), Error> {
        self.playback(Command::BeginRewind, Some(MediaControl::RewindBegin))
            .await
    }

    /// Stops rewind.
    pub async fn end_rewind(&self) -> Result<(), Error> {
        self.playback(Command::EndRewind, Some(MediaControl::RewindEnd))
            .await
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
        self.playback(Command::AdvanceShuffleMode, None).await
    }

    /// Changes to the next repeat mode. Needs MRP.
    pub async fn cycle_repeat(&self) -> Result<(), Error> {
        self.playback(Command::AdvanceRepeatMode, None).await
    }

    /// Next chapter. Needs MRP. See [`NowPlaying::chapters`], which is filled shortly
    /// after the item starts.
    pub async fn next_chapter(&self) -> Result<(), Error> {
        self.chapter(Command::NextChapter, NowPlaying::next_chapter_start)
            .await
    }

    /// Start of the current chapter, or the previous chapter in the first 3 s. Needs MRP.
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
        let m = self.need_mrp()?;
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

    /// Turns off a language, for example [`NowPlaying::subtitle_language`]. Needs MRP.
    pub async fn disable_language(&self, option: &LanguageOption) -> Result<(), Error> {
        self.need_mrp()?.disable_language(option).await
    }

    /// Holds Siri. Needs MRP. See [`mrp::Client::start_siri`].
    pub async fn start_siri(&self) -> Result<mrp::Siri<'_>, Error> {
        self.need_mrp()?.start_siri().await
    }

    /// Turns off subtitles. Needs MRP.
    pub async fn disable_subtitles(&self) -> Result<(), Error> {
        self.need_mrp()?.disable_subtitles().await
    }

    /// Plays `NowPlaying::up_next[index]`. Needs MRP.
    pub async fn play_queue_item(&self, index: usize) -> Result<(), Error> {
        self.need_mrp()?.play_queue_item(index).await
    }

    /// Sets the volume, 0.0 to 1.0.
    pub async fn set_volume(&self, volume: f32) -> Result<(), Error> {
        match &self.mrp {
            Some(m) => m.set_volume(volume).await,
            None => self.need_companion()?.set_volume(f64::from(volume)).await,
        }
    }

    /// Sets the playback rate, 1.0 is normal speed. Needs MRP.
    /// See [`NowPlaying::supported_rates`].
    pub async fn set_playback_rate(&self, rate: f32) -> Result<(), Error> {
        self.need_mrp()?.set_playback_rate(rate).await
    }

    /// Rates the current item. Needs MRP. See [`NowPlaying::rating_range`].
    pub async fn rate(&self, rating: f32) -> Result<(), Error> {
        self.need_mrp()?.rate(rating).await
    }

    /// Likes the current item, or removes the like. Needs MRP.
    pub async fn like(&self, liked: bool) -> Result<(), Error> {
        self.need_mrp()?.like(liked).await
    }

    /// Dislikes the current item, or removes the dislike. Needs MRP.
    pub async fn dislike(&self, disliked: bool) -> Result<(), Error> {
        self.need_mrp()?.dislike(disliked).await
    }

    /// Mutes or unmutes the device. Needs MRP.
    pub async fn set_muted(&self, muted: bool) -> Result<(), Error> {
        self.need_mrp()?.set_muted(muted).await
    }

    /// Wakes the device. Needs Companion.
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
    pub async fn swipe(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
    ) -> Result<(), Error> {
        self.need_companion()?.swipe(from, to, duration).await
    }

    /// Taps at `x`, `y` without a click. Coordinates range from 0.0 to 1.0. Needs Companion.
    pub async fn tap(&self, x: f64, y: f64) -> Result<(), Error> {
        self.need_companion()?.tap(x, y).await
    }

    /// Text of the focused text field. Needs Companion.
    pub async fn text(&self) -> Result<Option<String>, Error> {
        self.need_companion()?.text().await
    }

    /// Replaces the text of the focused text field. Needs Companion.
    pub async fn set_text(&self, text: &str) -> Result<(), Error> {
        self.need_companion()?.set_text(text).await
    }

    /// Adds text at the cursor of the focused text field. Needs Companion.
    pub async fn append_text(&self, text: &str) -> Result<(), Error> {
        self.need_companion()?.append_text(text).await
    }

    /// Removes all text of the focused text field. Needs Companion.
    pub async fn clear_text(&self) -> Result<(), Error> {
        self.need_companion()?.clear_text().await
    }

    /// Closes all connections.
    pub async fn close(self) -> Result<(), Error> {
        let (a, b) = tokio::join!(
            async {
                match self.companion {
                    Some(c) => c.close().await,
                    None => Ok(()),
                }
            },
            async {
                match self.mrp {
                    Some(m) => m.close().await,
                    None => Ok(()),
                }
            }
        );
        a.and(b)
    }
}

/// Returns the client, or logs the error and stores the first one.
fn keep<T>(
    name: &str,
    result: Option<Result<T, Error>>,
    first_error: &mut Option<Error>,
) -> Option<T> {
    match result? {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::warn!(%err, "{name} connection failed");
            first_error.get_or_insert(err);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn no_endpoints() {
        let err = AppleTv::connect(
            &Endpoints::default(),
            &test_credentials(),
            &ConnectOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::NotConnected("no endpoints")));
    }

    fn test_credentials() -> Credentials {
        Credentials {
            accessory_ltpk: [0; 32],
            client_ltsk: [0; 32].into(),
            accessory_id: Vec::new(),
            client_id: Vec::new(),
        }
    }
}
