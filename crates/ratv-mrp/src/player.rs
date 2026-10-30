use std::time::{Duration, SystemTime};

use prost::Message as _;

use crate::proto::{
    self, Command, CommandInfo, ContentItem, ContentItemMetadata, LanguageOption, NowPlayingClient,
    NowPlayingInfo, NowPlayingPlayer, PlaybackQueue, PlayerPath, RemoveClientMessage,
    RemovePlayerMessage, SetDefaultSupportedCommandsMessage, SetNowPlayingClientMessage,
    SetNowPlayingPlayerMessage, SetStateMessage, UpdateClientMessage, UpdateContentItemMessage,
    UpdatePlayerMessage,
};
use crate::{Envelope, Error};

/// Player used when a message has no player.
pub const DEFAULT_PLAYER: &str = "MediaRemote-DefaultPlayer";

/// Seconds from 1970-01-01 to 2001-01-01.
const COCOA_EPOCH: Duration = Duration::from_secs(978_307_200);

/// Playback state.
pub type PlaybackState = proto::playback_state::Enum;

/// Audio or video.
pub type MediaType = proto::content_item_metadata::MediaType;

/// Kind of audio or video, for example music or podcast.
pub type MediaSubType = proto::content_item_metadata::MediaSubType;

/// Audio (`Audible`) or subtitle (`Legible`) language.
pub type LanguageType = proto::language_option::Type;

/// A chapter of the current item.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chapter {
    /// Title.
    pub title: Option<String>,
    /// Start in seconds.
    pub start: Option<f64>,
    /// Length in seconds. Computed from the next start if the app does not send it.
    pub duration: Option<f64>,
}

/// An item in the playback queue.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueueItem {
    /// Item id.
    pub id: Option<String>,
    /// Title.
    pub title: Option<String>,
    /// Artist.
    pub artist: Option<String>,
    /// Length in seconds.
    pub duration: Option<f64>,
}

/// Converts a time to MRP time (seconds since 2001-01-01).
#[must_use]
pub fn cocoa_time(t: SystemTime) -> f64 {
    match t.duration_since(SystemTime::UNIX_EPOCH + COCOA_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

/// Snapshot of what the active player is playing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NowPlaying {
    /// Bundle id of the app that plays.
    pub app: Option<String>,
    /// Display name of the app.
    pub app_name: Option<String>,
    /// Playback state. `None` when nothing is loaded.
    pub state: Option<PlaybackState>,
    /// Title.
    pub title: Option<String>,
    /// Subtitle.
    pub subtitle: Option<String>,
    /// Artist.
    pub artist: Option<String>,
    /// Album.
    pub album: Option<String>,
    /// Album artist.
    pub album_artist: Option<String>,
    /// Composer.
    pub composer: Option<String>,
    /// Director.
    pub director: Option<String>,
    /// Genre.
    pub genre: Option<String>,
    /// Release year of the album, as sent by the app.
    pub album_year: Option<String>,
    /// Release date, MRP time (see [`cocoa_time`]).
    pub release_date: Option<f64>,
    /// Track number on the disc.
    pub track_number: Option<i32>,
    /// Number of tracks on the disc.
    pub track_count: Option<i32>,
    /// Disc number.
    pub disc_number: Option<i32>,
    /// Number of discs.
    pub disc_count: Option<i32>,
    /// Series name, for TV shows.
    pub series_name: Option<String>,
    /// Season number.
    pub season: Option<i32>,
    /// Episode number.
    pub episode: Option<i32>,
    /// Name of the radio station.
    pub radio_station: Option<String>,
    /// Audio or video.
    pub media_type: Option<MediaType>,
    /// Kind of audio or video.
    pub media_sub_type: Option<MediaSubType>,
    /// True for live streams that have no fixed length.
    pub is_live: Option<bool>,
    /// True while an advertisement plays.
    pub is_advertisement: Option<bool>,
    /// True for explicit content.
    pub is_explicit: Option<bool>,
    /// Age rating, for example `PG-13`.
    pub content_rating: Option<String>,
    /// True when the item is on the wish list.
    pub in_wish_list: Option<bool>,
    /// True when the item has lyrics.
    pub lyrics_available: Option<bool>,
    /// Number of chapters.
    pub chapter_count: Option<i32>,
    /// True when the item has audio or subtitle language options.
    pub language_options_available: Option<bool>,
    /// Format of the audio that plays, for example sample rate and spatial audio.
    pub audio_format: Option<proto::AudioFormat>,
    /// Description of the item.
    pub description: Option<String>,
    /// Audio languages of the item.
    pub audio_languages: Vec<LanguageOption>,
    /// Subtitle languages of the item.
    pub subtitle_languages: Vec<LanguageOption>,
    /// Current audio language.
    pub audio_language: Option<LanguageOption>,
    /// Current subtitle language. The tag `__AUTO__` means automatic. `None` when off.
    pub subtitle_language: Option<LanguageOption>,
    /// Chapters of the item.
    pub chapters: Vec<Chapter>,
    /// Items after the current one.
    pub up_next: Vec<QueueItem>,
    /// Content id of the item.
    pub content_identifier: Option<String>,
    /// Length in seconds.
    pub duration: Option<f64>,
    /// Position in seconds at `elapsed_timestamp`.
    pub elapsed: Option<f64>,
    /// MRP time of `elapsed`, see [`cocoa_time`].
    pub elapsed_timestamp: Option<f64>,
    /// Playback rate, 1.0 is normal speed.
    pub rate: Option<f32>,
    /// Shuffle mode.
    pub shuffle: Option<proto::shuffle_mode::Enum>,
    /// Repeat mode.
    pub repeat: Option<proto::repeat_mode::Enum>,
    /// Enabled commands.
    pub commands: Vec<Command>,
    /// Playback rates the app accepts for [`Command::ChangePlaybackRate`].
    pub supported_rates: Vec<f32>,
    /// True when the item is liked. `None` if the app does not support likes.
    pub liked: Option<bool>,
    /// True when the item is disliked. `None` if the app does not support dislikes.
    pub disliked: Option<bool>,
    /// Lowest and highest rating for [`Command::RateTrack`].
    pub rating_range: Option<(f32, f32)>,
    /// Changes when the artwork changes. `None` if the item has no artwork.
    pub artwork_id: Option<String>,
    /// Artwork URL, if the app provides one.
    pub artwork_url: Option<String>,
}

impl NowPlaying {
    /// Position in seconds at `now` (MRP time, see [`cocoa_time`]).
    /// Devices sometimes send NaN; such values are treated as missing.
    #[must_use]
    pub fn position(&self, now: f64) -> Option<f64> {
        let elapsed = self.elapsed.filter(|e| e.is_finite())?;
        let ts = self.elapsed_timestamp.filter(|t| t.is_finite());
        let rate = self.rate.filter(|r| r.is_finite());
        let (Some(ts), Some(rate)) = (ts, rate) else {
            return Some(elapsed);
        };
        let pos = (elapsed + (now - ts) * f64::from(rate)).max(0.0);
        Some(
            self.duration
                .filter(|d| *d > 0.0)
                .map_or(pos, |d| pos.min(d)),
        )
    }

    /// Artwork URL for about this size, if the app provides one.
    /// A size of 0 keeps the aspect ratio. Download it with any HTTP client.
    #[must_use]
    pub fn artwork_url_sized(&self, width: u32, height: u32) -> Option<String> {
        // Some apps send a URL template like `.../{w}x{h}bb.jpg`.
        let template = self
            .artwork_id
            .as_deref()
            .filter(|t| t.starts_with("http") && t.contains("{w}"));
        let Some(template) = template else {
            return self.artwork_url.clone();
        };
        // The image server keeps the aspect ratio for a large value.
        let size = |v: u32| if v == 0 { 999_999 } else { v };
        Some(
            template
                .replace("{w}", &size(width).to_string())
                .replace("{h}", &size(height).to_string())
                .replace("{c}", "bb")
                .replace("{f}", "jpg"),
        )
    }

    /// Start of the chapter after `position` seconds.
    #[must_use]
    pub fn next_chapter_start(&self, position: f64) -> Option<f64> {
        // Small margin, so a seek to a chapter start does not count as before it.
        self.chapters
            .iter()
            .filter_map(|c| c.start)
            .find(|&s| s > position + 1.0)
    }

    /// Start of the current chapter, or of the previous one in the first 3 s of a chapter.
    #[must_use]
    pub fn previous_chapter_start(&self, position: f64) -> Option<f64> {
        self.chapters
            .iter()
            .filter_map(|c| c.start)
            .take_while(|&s| s < position - 3.0)
            .last()
    }

    /// True if `command` is enabled.
    #[must_use]
    pub fn supports(&self, command: Command) -> bool {
        self.commands.contains(&command)
    }
}

#[derive(Debug)]
struct Player {
    id: String,
    state: Option<i32>,
    state_timestamp: Option<f64>,
    info: Option<NowPlayingInfo>,
    commands: Vec<CommandInfo>,
    items: Vec<ContentItem>,
    location: usize,
}

impl Player {
    const fn new(id: String) -> Self {
        Self {
            id,
            state: None,
            state_timestamp: None,
            info: None,
            commands: Vec::new(),
            items: Vec::new(),
            location: 0,
        }
    }

    fn item(&self) -> Option<&ContentItem> {
        self.items.get(self.location)
    }

    fn metadata(&self) -> Option<&ContentItemMetadata> {
        self.item()?.metadata.as_ref()
    }

    fn set_state(&mut self, msg: SetStateMessage) {
        // Ignore updates that are older than the current state.
        if let (Some(new), Some(old)) = (msg.playback_state_timestamp, self.state_timestamp)
            && new < old
        {
            return;
        }
        if msg.playback_state.is_some() {
            self.state = msg.playback_state;
        }
        if msg.playback_state_timestamp.is_some() {
            self.state_timestamp = msg.playback_state_timestamp;
        }
        if msg.now_playing_info.is_some() {
            self.info = msg.now_playing_info;
        }
        if let Some(c) = msg.supported_commands {
            self.commands = c.supported_commands;
        }
        if let Some(q) = msg.playback_queue {
            self.set_queue(q);
        }
    }

    fn set_queue(&mut self, queue: PlaybackQueue) {
        let location = queue
            .location
            .and_then(|l| usize::try_from(l).ok())
            .unwrap_or(0);
        let mut items = queue.content_items;
        let mut old = std::mem::take(&mut self.items);
        let same_item = match (items.get(location), old.get(self.location)) {
            (Some(n), Some(o)) => n.identifier.is_some() && n.identifier == o.identifier,
            _ => false,
        };
        for item in &mut items {
            // Images are only read with `artwork`, so do not keep them.
            item.artwork_data = None;
            for s in &mut item.sections {
                s.artwork_data = None;
            }
            // Pushed queues often have no details. Keep the ones that are known.
            if let Some(o) = old
                .iter_mut()
                .find(|o| o.identifier.is_some() && o.identifier == item.identifier)
            {
                keep_details(item, o);
            }
        }
        // Pushed queues often have only the current item. Keep the known next items.
        if same_item && items.len() == location + 1 && old.len() > self.location + 1 {
            items.extend(old.drain(self.location + 1..));
        }
        self.items = items;
        self.location = location;
    }

    fn update_items(&mut self, items: Vec<ContentItem>) {
        for new in items {
            let Some(old) = self
                .items
                .iter_mut()
                .find(|i| i.identifier.is_some() && i.identifier == new.identifier)
            else {
                continue;
            };
            if let Some(meta) = new.metadata {
                match &mut old.metadata {
                    // Protobuf merge keeps fields that are not set in the update.
                    Some(m) => m.merge(meta.encode_to_vec().as_slice()).unwrap_or_default(),
                    None => old.metadata = Some(meta),
                }
            }
            if new.lyrics.is_some() {
                old.lyrics = new.lyrics;
            }
            if new.info.is_some() {
                old.info = new.info;
            }
            if !new.available_language_options.is_empty() {
                old.available_language_options = new.available_language_options;
            }
            if !new.current_language_options.is_empty() {
                old.current_language_options = new.current_language_options;
            }
            if !new.sections.is_empty() {
                old.sections = new.sections;
            }
        }
    }

    fn playback_state(&self) -> Option<PlaybackState> {
        let state = PlaybackState::try_from(self.state?).ok()?;
        match state {
            PlaybackState::Paused if self.metadata().is_none() => None,
            PlaybackState::Playing => match self.rate() {
                Some(0.0) => Some(PlaybackState::Paused),
                Some(r) if (r - 1.0).abs() >= 0.01 => Some(PlaybackState::Seeking),
                _ => Some(state),
            },
            _ => Some(state),
        }
    }

    fn rate(&self) -> Option<f32> {
        self.info
            .as_ref()
            .and_then(|i| i.playback_rate)
            .or_else(|| self.metadata()?.playback_rate)
    }
}

#[derive(Debug)]
struct Client {
    bundle_id: String,
    display_name: Option<String>,
    default_commands: Vec<CommandInfo>,
    players: Vec<Player>,
    active_player: Option<String>,
}

impl Client {
    fn player_mut(&mut self, player: Option<&NowPlayingPlayer>) -> &mut Player {
        let id = player
            .and_then(|p| p.identifier.as_deref())
            .unwrap_or(DEFAULT_PLAYER);
        if let Some(i) = self.players.iter().position(|p| p.id == id) {
            return &mut self.players[i];
        }
        self.players.push(Player::new(id.to_owned()));
        self.players.last_mut().expect("just pushed")
    }

    fn active(&self) -> Option<&Player> {
        let id = self.active_player.as_deref().unwrap_or(DEFAULT_PLAYER);
        self.players.iter().find(|p| p.id == id)
    }
}

/// Tracks clients (apps) and players from device messages.
///
/// Pass every received message to [`apply`](Self::apply) and read
/// [`now_playing`](Self::now_playing).
#[derive(Debug, Default)]
pub struct NowPlayingState {
    clients: Vec<Client>,
    active_client: Option<String>,
}

impl NowPlayingState {
    /// Empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a message. Returns `true` if it was a now playing message.
    pub fn apply(&mut self, env: &Envelope) -> Result<bool, Error> {
        if env.is::<SetStateMessage>() {
            let msg: SetStateMessage = env.payload()?;
            // Replies to queue requests have no player path.
            let player = match &msg.player_path {
                Some(path) => self.player_mut(Some(path)),
                None => self.active_player_mut(),
            };
            if let Some(p) = player {
                p.set_state(msg);
            }
        } else if env.is::<UpdateContentItemMessage>() {
            let msg: UpdateContentItemMessage = env.payload()?;
            if let Some(p) = self.player_mut(msg.player_path.as_ref()) {
                p.update_items(msg.content_items);
            }
        } else if env.is::<SetNowPlayingClientMessage>() {
            let msg: SetNowPlayingClientMessage = env.payload()?;
            if let Some(c) = msg.client.as_ref().and_then(|c| self.client_mut(c)) {
                let id = c.bundle_id.clone();
                self.active_client = Some(id);
            }
        } else if env.is::<SetNowPlayingPlayerMessage>() {
            let msg: SetNowPlayingPlayerMessage = env.payload()?;
            if let Some(path) = &msg.player_path
                && let Some(c) = path.client.as_ref().and_then(|c| self.client_mut(c))
            {
                let id = c.player_mut(path.player.as_ref()).id.clone();
                c.active_player = Some(id);
            }
        } else if env.is::<UpdatePlayerMessage>() {
            let msg: UpdatePlayerMessage = env.payload()?;
            if let Some(path) = &msg.player_path
                && path.player.is_some()
            {
                self.player_mut(Some(path));
            }
        } else if env.is::<UpdateClientMessage>() {
            let msg: UpdateClientMessage = env.payload()?;
            if let Some(c) = &msg.client {
                self.client_mut(c);
            }
        } else if env.is::<RemoveClientMessage>() {
            let msg: RemoveClientMessage = env.payload()?;
            if let Some(id) = msg.client.and_then(|c| c.bundle_identifier) {
                self.clients.retain(|c| c.bundle_id != id);
                if self.active_client.as_deref() == Some(&id) {
                    self.active_client = None;
                }
            }
        } else if env.is::<RemovePlayerMessage>() {
            let msg: RemovePlayerMessage = env.payload()?;
            let Some(path) = &msg.player_path else {
                return Ok(true);
            };
            let id = path.player.as_ref().and_then(|p| p.identifier.as_deref());
            if let (Some(c), Some(id)) = (path.client.as_ref().and_then(|c| self.client_mut(c)), id)
            {
                c.players.retain(|p| p.id != id);
                if c.active_player.as_deref() == Some(id) {
                    c.active_player = None;
                }
            }
        } else if env.is::<SetDefaultSupportedCommandsMessage>() {
            let msg: SetDefaultSupportedCommandsMessage = env.payload()?;
            if let Some(path) = &msg.player_path
                && let Some(c) = path.client.as_ref().and_then(|c| self.client_mut(c))
            {
                c.default_commands = msg
                    .supported_commands
                    .map(|s| s.supported_commands)
                    .unwrap_or_default();
            }
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    /// Bundle id of the active app.
    #[must_use]
    pub fn active_app(&self) -> Option<&str> {
        self.active_client.as_deref()
    }

    /// Id of the current item. Request details with
    /// [`playback_queue_details`](crate::messages::playback_queue_details) when it changes.
    #[must_use]
    pub fn item_id(&self) -> Option<&str> {
        self.active_client()?
            .active()?
            .item()?
            .identifier
            .as_deref()
    }

    /// Snapshot of the active player.
    #[must_use]
    pub fn now_playing(&self) -> NowPlaying {
        let Some(client) = self.active_client() else {
            return NowPlaying::default();
        };
        let mut np = NowPlaying {
            app: Some(client.bundle_id.clone()),
            app_name: client.display_name.clone(),
            ..NowPlaying::default()
        };
        let Some(player) = client.active() else {
            np.commands = enabled(&client.default_commands);
            return np;
        };
        let info = player.info.as_ref();
        let meta = player.metadata();
        let s = |m: Option<&String>, i: Option<&String>| text(m).or_else(|| text(i));

        np.state = player.playback_state();
        np.title = s(
            meta.and_then(|m| m.title.as_ref()),
            info.and_then(|i| i.title.as_ref()),
        );
        np.artist = s(
            meta.and_then(|m| m.track_artist_name.as_ref()),
            info.and_then(|i| i.artist.as_ref()),
        );
        np.album = s(
            meta.and_then(|m| m.album_name.as_ref()),
            info.and_then(|i| i.album.as_ref()),
        );
        if let Some(m) = meta {
            copy_metadata(m, &mut np);
        }
        np.is_live = np.is_live.or_else(|| info?.is_always_live);
        np.is_advertisement = np.is_advertisement.or_else(|| info?.is_advertisement);
        np.is_explicit = np.is_explicit.or_else(|| info?.is_explicit_track);
        np.radio_station = np
            .radio_station
            .take()
            .or_else(|| text(info?.radio_station_name.as_ref()));
        np.content_identifier = meta.and_then(|m| m.content_identifier.clone());
        np.artwork_url = meta.and_then(|m| m.artwork_url.clone());
        if meta.is_some_and(|m| m.artwork_available == Some(true)) || np.artwork_url.is_some() {
            np.artwork_id = meta
                .and_then(|m| m.artwork_identifier.clone())
                .or_else(|| np.content_identifier.clone())
                .or_else(|| player.item()?.identifier.clone());
        }
        np.duration = info.and_then(|i| i.duration).or_else(|| meta?.duration);
        (np.elapsed, np.elapsed_timestamp) = info.and_then(|i| i.elapsed_time).map_or_else(
            || {
                (
                    meta.and_then(|m| m.elapsed_time),
                    meta.and_then(|m| m.elapsed_time_timestamp),
                )
            },
            |e| (Some(e), info.and_then(|i| i.timestamp)),
        );
        np.rate = player.rate();
        np.shuffle = mode(&player.commands, Command::ChangeShuffleMode, |c| {
            c.shuffle_mode
        })
        .or_else(|| info?.shuffle_mode)
        .and_then(|v| proto::shuffle_mode::Enum::try_from(v).ok());
        np.repeat = mode(&player.commands, Command::ChangeRepeatMode, |c| {
            c.repeat_mode
        })
        .or_else(|| info?.repeat_mode)
        .and_then(|v| proto::repeat_mode::Enum::try_from(v).ok());
        let commands = if player.commands.is_empty() {
            &client.default_commands
        } else {
            &player.commands
        };
        np.commands = enabled(commands);
        let find = |cmd: Command| commands.iter().find(|c| c.command == Some(cmd as i32));
        if let Some(item) = player.item() {
            copy_item(item, &mut np);
        }
        np.up_next = player
            .items
            .iter()
            .skip(player.location + 1)
            .map(queue_item)
            .collect();
        np.supported_rates = find(Command::ChangePlaybackRate)
            .map(|c| c.supported_rates.clone())
            .unwrap_or_default();
        np.liked = find(Command::LikeTrack).and_then(|c| c.active);
        np.disliked = find(Command::DislikeTrack).and_then(|c| c.active);
        np.rating_range =
            find(Command::RateTrack).and_then(|c| Some((c.minimum_rating?, c.maximum_rating?)));
        np
    }

    fn active_client(&self) -> Option<&Client> {
        let id = self.active_client.as_deref()?;
        self.clients.iter().find(|c| c.bundle_id == id)
    }

    fn client_mut(&mut self, client: &NowPlayingClient) -> Option<&mut Client> {
        let id = client.bundle_identifier.as_deref()?;
        let i = if let Some(i) = self.clients.iter().position(|c| c.bundle_id == id) {
            i
        } else {
            self.clients.push(Client {
                bundle_id: id.to_owned(),
                display_name: None,
                default_commands: Vec::new(),
                players: Vec::new(),
                active_player: None,
            });
            self.clients.len() - 1
        };
        let c = &mut self.clients[i];
        if client.display_name.is_some() {
            c.display_name.clone_from(&client.display_name);
        }
        Some(c)
    }

    fn active_player_mut(&mut self) -> Option<&mut Player> {
        let id = self.active_client.as_deref()?;
        let client = self.clients.iter_mut().find(|c| c.bundle_id == id)?;
        let player = client.active_player.as_deref().unwrap_or(DEFAULT_PLAYER);
        client.players.iter_mut().find(|p| p.id == player)
    }

    fn player_mut(&mut self, path: Option<&PlayerPath>) -> Option<&mut Player> {
        let path = path?;
        let client = self.client_mut(path.client.as_ref()?)?;
        Some(client.player_mut(path.player.as_ref()))
    }
}

/// Apps send empty strings for missing values.
fn text(s: Option<&String>) -> Option<String> {
    s.filter(|s| !s.is_empty()).cloned()
}

/// Four character code as text, for example `ec+3` (Dolby Atmos) for [`proto::AudioFormat::codec`].
#[must_use]
pub fn four_cc(code: u32) -> String {
    code.to_be_bytes().iter().map(|&b| char::from(b)).collect()
}

/// Moves details that `new` lacks from `old`.
fn keep_details(new: &mut ContentItem, old: &mut ContentItem) {
    if new.available_language_options.is_empty() {
        new.available_language_options = std::mem::take(&mut old.available_language_options);
    }
    if new.current_language_options.is_empty() {
        new.current_language_options = std::mem::take(&mut old.current_language_options);
    }
    if new.sections.is_empty() {
        new.sections = std::mem::take(&mut old.sections);
    }
    if new.info.is_none() {
        new.info = old.info.take();
    }
}

/// Copies description, languages and chapters.
fn copy_item(item: &ContentItem, np: &mut NowPlaying) {
    np.description = text(item.info.as_ref());
    for option in item
        .available_language_options
        .iter()
        .flat_map(|g| &g.language_options)
    {
        match option.r#type() {
            LanguageType::Audible => np.audio_languages.push(option.clone()),
            LanguageType::Legible => np.subtitle_languages.push(option.clone()),
        }
    }
    for option in &item.current_language_options {
        let current = match option.r#type() {
            LanguageType::Audible => &mut np.audio_language,
            LanguageType::Legible => &mut np.subtitle_language,
        };
        *current = Some(option.clone());
    }
    np.chapters = item
        .sections
        .iter()
        .map(|s| {
            let m = s.metadata.as_ref();
            Chapter {
                title: text(m.and_then(|m| m.title.as_ref())),
                start: m.and_then(|m| m.start_time),
                duration: m.and_then(|m| m.duration),
            }
        })
        .collect();
    // Apps send only the start of each chapter.
    let end = item.metadata.as_ref().and_then(|m| m.duration);
    let starts: Vec<_> = np.chapters.iter().map(|c| c.start).collect();
    for (i, chapter) in np.chapters.iter_mut().enumerate() {
        if chapter.duration.is_none() {
            let next = starts.get(i + 1).copied().unwrap_or(end);
            chapter.duration = chapter.start.zip(next).map(|(s, e)| e - s);
        }
    }
}

fn queue_item(item: &ContentItem) -> QueueItem {
    let m = item.metadata.as_ref();
    QueueItem {
        id: item.identifier.clone(),
        title: text(m.and_then(|m| m.title.as_ref())),
        artist: text(m.and_then(|m| m.track_artist_name.as_ref())),
        duration: m.and_then(|m| m.duration),
    }
}

/// Copies the metadata fields that need no fallback.
fn copy_metadata(m: &ContentItemMetadata, np: &mut NowPlaying) {
    np.subtitle = text(m.subtitle.as_ref());
    np.album_artist = text(m.album_artist_name.as_ref());
    np.composer = text(m.composer.as_ref());
    np.director = text(m.director_name.as_ref());
    np.genre = text(m.genre.as_ref());
    np.album_year = text(m.album_year.as_ref());
    np.release_date = m.release_date;
    np.track_number = m.track_number;
    np.track_count = m.total_track_count;
    np.disc_number = m.disc_number;
    np.disc_count = m.total_disc_count;
    np.series_name = text(m.series_name.as_ref());
    np.season = m.season_number;
    np.episode = m.episode_number;
    np.radio_station = text(m.radio_station_name.as_ref());
    np.media_type = m.media_type.and_then(|v| MediaType::try_from(v).ok());
    np.media_sub_type = m
        .media_sub_type
        .and_then(|v| MediaSubType::try_from(v).ok());
    np.is_live = m.is_always_live;
    np.is_advertisement = m.is_advertisement;
    np.is_explicit = m.is_explicit_item;
    np.content_rating = text(m.localized_content_rating.as_ref());
    np.in_wish_list = m.is_in_wish_list;
    np.lyrics_available = m.lyrics_available;
    np.chapter_count = m.chapter_count;
    np.language_options_available = m.language_options_available;
    np.audio_format.clone_from(&m.active_format);
}

fn enabled(commands: &[CommandInfo]) -> Vec<Command> {
    commands
        .iter()
        .filter(|c| c.enabled != Some(false))
        .filter_map(|c| Command::try_from(c.command?).ok())
        .collect()
}

fn mode(
    commands: &[CommandInfo],
    cmd: Command,
    f: impl Fn(&CommandInfo) -> Option<i32>,
) -> Option<i32> {
    commands
        .iter()
        .find(|c| c.command == Some(cmd as i32))
        .and_then(f)
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::*;
    use crate::proto::{PlaybackQueue, SupportedCommands};
    use crate::{Payload, encode, frames};

    fn env<P: Payload>(p: &P) -> Envelope {
        let mut out = BytesMut::new();
        encode(p, None, &mut out);
        frames(out.freeze()).next().unwrap().unwrap()
    }

    #[allow(clippy::unnecessary_wraps)]
    fn path(bundle: &str, player: Option<&str>) -> Option<PlayerPath> {
        Some(PlayerPath {
            origin: None,
            client: Some(NowPlayingClient {
                bundle_identifier: Some(bundle.into()),
                display_name: Some("Music".into()),
                ..Default::default()
            }),
            player: player.map(|id| NowPlayingPlayer {
                identifier: Some(id.into()),
                ..Default::default()
            }),
        })
    }

    fn item(id: &str, title: &str) -> ContentItem {
        ContentItem {
            identifier: Some(id.into()),
            metadata: Some(ContentItemMetadata {
                title: Some(title.into()),
                duration: Some(200.0),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn set_client(s: &mut NowPlayingState, bundle: &str) {
        let msg = SetNowPlayingClientMessage {
            client: path(bundle, None).unwrap().client,
        };
        assert!(s.apply(&env(&msg)).unwrap());
    }

    fn playing() -> SetStateMessage {
        SetStateMessage {
            player_path: path("com.apple.TVMusic", None),
            playback_state: Some(PlaybackState::Playing as i32),
            playback_state_timestamp: Some(100.0),
            now_playing_info: Some(NowPlayingInfo {
                artist: Some("Artist".into()),
                elapsed_time: Some(10.0),
                timestamp: Some(1000.0),
                playback_rate: Some(1.0),
                ..Default::default()
            }),
            supported_commands: Some(SupportedCommands {
                supported_commands: vec![
                    CommandInfo {
                        command: Some(Command::Play as i32),
                        ..Default::default()
                    },
                    CommandInfo {
                        command: Some(Command::Pause as i32),
                        enabled: Some(false),
                        ..Default::default()
                    },
                    CommandInfo {
                        command: Some(Command::ChangeShuffleMode as i32),
                        shuffle_mode: Some(proto::shuffle_mode::Enum::Songs as i32),
                        ..Default::default()
                    },
                ],
            }),
            playback_queue: Some(PlaybackQueue {
                location: Some(0),
                content_items: vec![item("a", "Song")],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn artwork_id() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        let mut msg = playing();
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(s.now_playing().artwork_id, None);

        let item = &mut msg.playback_queue.as_mut().unwrap().content_items[0];
        item.artwork_data = Some(bytes::Bytes::from_static(b"image"));
        let meta = item.metadata.as_mut().unwrap();
        meta.artwork_available = Some(true);
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(s.now_playing().artwork_id.as_deref(), Some("a"));

        let meta = msg.playback_queue.as_mut().unwrap().content_items[0]
            .metadata
            .as_mut()
            .unwrap();
        meta.artwork_identifier = Some("art-1".into());
        meta.artwork_url = Some("https://example.com/a.jpg".into());
        assert!(s.apply(&env(&msg)).unwrap());
        let np = s.now_playing();
        assert_eq!(np.artwork_id.as_deref(), Some("art-1"));
        assert_eq!(np.artwork_url.as_deref(), Some("https://example.com/a.jpg"));
        assert_eq!(
            np.artwork_url_sized(300, 300).as_deref(),
            Some("https://example.com/a.jpg")
        );
        let templated = NowPlaying {
            artwork_id: Some("https://img.example.com/cover.jpg/{w}x{h}{c}.{f}".into()),
            ..NowPlaying::default()
        };
        assert_eq!(
            templated.artwork_url_sized(600, 0).as_deref(),
            Some("https://img.example.com/cover.jpg/600x999999bb.jpg")
        );
        assert_eq!(NowPlaying::default().artwork_url_sized(1, 1), None);
        // The state does not keep image data.
        let player = s.active_client().unwrap().active().unwrap();
        assert_eq!(player.item().unwrap().artwork_data, None);
    }

    #[test]
    fn set_state() {
        let mut s = NowPlayingState::new();
        assert_eq!(s.now_playing(), NowPlaying::default());
        set_client(&mut s, "com.apple.TVMusic");
        assert!(s.apply(&env(&playing())).unwrap());

        let np = s.now_playing();
        assert_eq!(s.active_app(), Some("com.apple.TVMusic"));
        assert_eq!(np.app_name.as_deref(), Some("Music"));
        assert_eq!(np.state, Some(PlaybackState::Playing));
        assert_eq!(np.title.as_deref(), Some("Song"));
        assert_eq!(np.artist.as_deref(), Some("Artist"));
        assert_eq!(np.duration, Some(200.0));
        assert_eq!(np.shuffle, Some(proto::shuffle_mode::Enum::Songs));
        assert!(np.supports(Command::Play));
        assert!(!np.supports(Command::Pause));
        assert_eq!(np.position(1005.0), Some(15.0));
        assert_eq!(np.position(5000.0), Some(200.0));
        let nan_ts = NowPlaying {
            elapsed_timestamp: Some(f64::NAN),
            ..np.clone()
        };
        assert_eq!(nan_ts.position(5000.0), np.elapsed);
        let nan_elapsed = NowPlaying {
            elapsed: Some(f64::NAN),
            ..np
        };
        assert_eq!(nan_elapsed.position(5000.0), None);
    }

    #[test]
    fn rates_and_ratings() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        let mut msg = playing();
        let infos = &mut msg.supported_commands.as_mut().unwrap().supported_commands;
        infos.push(CommandInfo {
            command: Some(Command::ChangePlaybackRate as i32),
            supported_rates: vec![0.5, 1.0, 1.5, 2.0],
            ..Default::default()
        });
        infos.push(CommandInfo {
            command: Some(Command::LikeTrack as i32),
            active: Some(true),
            ..Default::default()
        });
        infos.push(CommandInfo {
            command: Some(Command::RateTrack as i32),
            minimum_rating: Some(0.0),
            maximum_rating: Some(5.0),
            ..Default::default()
        });
        assert!(s.apply(&env(&msg)).unwrap());
        let np = s.now_playing();
        assert_eq!(np.supported_rates, [0.5, 1.0, 1.5, 2.0]);
        assert_eq!(np.liked, Some(true));
        assert_eq!(np.disliked, None);
        assert_eq!(np.rating_range, Some((0.0, 5.0)));
    }

    #[test]
    fn extra_metadata() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        let mut msg = playing();
        let queue = msg.playback_queue.as_mut().unwrap();
        let meta = queue.content_items[0].metadata.as_mut().unwrap();
        meta.composer = Some("Bach".into());
        meta.track_number = Some(3);
        meta.total_track_count = Some(12);
        meta.media_type = Some(MediaType::Audio as i32);
        meta.media_sub_type = Some(MediaSubType::Music as i32);
        meta.chapter_count = Some(4);
        meta.active_format = Some(proto::AudioFormat {
            spatialized: Some(true),
            ..Default::default()
        });
        msg.now_playing_info.as_mut().unwrap().is_always_live = Some(true);
        assert!(s.apply(&env(&msg)).unwrap());
        let np = s.now_playing();
        assert_eq!(np.composer.as_deref(), Some("Bach"));
        assert_eq!((np.track_number, np.track_count), (Some(3), Some(12)));
        assert_eq!(np.media_type, Some(MediaType::Audio));
        assert_eq!(np.media_sub_type, Some(MediaSubType::Music));
        assert_eq!(np.chapter_count, Some(4));
        assert_eq!(np.audio_format.unwrap().spatialized, Some(true));
        // The metadata has no value, so it comes from the now playing info.
        assert_eq!(np.is_live, Some(true));
    }

    #[test]
    fn empty_text_and_four_cc() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        let mut msg = playing();
        let queue = msg.playback_queue.as_mut().unwrap();
        queue.content_items[0].metadata.as_mut().unwrap().composer = Some(String::new());
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(s.now_playing().composer, None);
        assert_eq!(four_cc(1_700_997_939), "ec+3");
    }

    fn language(kind: LanguageType, tag: &str) -> LanguageOption {
        LanguageOption {
            r#type: Some(kind as i32),
            language_tag: Some(tag.into()),
            ..Default::default()
        }
    }

    #[test]
    fn details_kept_after_pushed_queue() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        // Reply to `playback_queue_details`: details and next items.
        let mut msg = playing();
        let mut current = item("a", "Movie");
        current.info = Some("About".into());
        current.available_language_options = vec![proto::LanguageOptionGroup {
            language_options: vec![
                language(LanguageType::Audible, "en"),
                language(LanguageType::Legible, "sv"),
            ],
            ..Default::default()
        }];
        current.current_language_options = vec![language(LanguageType::Legible, "sv")];
        let chapter = |id, start| {
            let mut c = item(id, "Chapter");
            let m = c.metadata.as_mut().unwrap();
            m.start_time = Some(start);
            m.duration = None;
            c
        };
        current.sections = vec![chapter("c1", 0.0), chapter("c2", 50.0)];
        msg.playback_queue.as_mut().unwrap().content_items =
            vec![current, item("b", "Next"), item("c", "Last")];
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(s.item_id(), Some("a"));

        // Pushed update: only the current item, without details.
        assert!(s.apply(&env(&playing())).unwrap());
        let np = s.now_playing();
        assert_eq!(np.description.as_deref(), Some("About"));
        assert_eq!(np.audio_languages, [language(LanguageType::Audible, "en")]);
        assert_eq!(
            np.subtitle_languages,
            [language(LanguageType::Legible, "sv")]
        );
        assert_eq!(
            np.subtitle_language,
            Some(language(LanguageType::Legible, "sv"))
        );
        assert_eq!(np.audio_language, None);
        let chapters: Vec<_> = np.chapters.iter().map(|c| (c.start, c.duration)).collect();
        // The item is 200 s long.
        assert_eq!(
            chapters,
            [(Some(0.0), Some(50.0)), (Some(50.0), Some(150.0))]
        );
        let next: Vec<_> = np.up_next.iter().map(|i| i.id.as_deref()).collect();
        assert_eq!(next, [Some("b"), Some("c")]);

        // A new item drops the old next items.
        let mut msg = playing();
        msg.playback_queue.as_mut().unwrap().content_items = vec![item("b", "Next")];
        assert!(s.apply(&env(&msg)).unwrap());
        let np = s.now_playing();
        assert_eq!(np.up_next, []);
        assert_eq!(np.audio_languages, []);
        assert_eq!(s.item_id(), Some("b"));
    }

    #[test]
    fn chapter_starts() {
        let np = NowPlaying {
            chapters: [0.0, 100.0, 200.0]
                .map(|s| Chapter {
                    start: Some(s),
                    ..Default::default()
                })
                .to_vec(),
            ..Default::default()
        };
        assert_eq!(np.next_chapter_start(50.0), Some(100.0));
        assert_eq!(np.next_chapter_start(100.2), Some(200.0));
        assert_eq!(np.next_chapter_start(250.0), None);
        assert_eq!(np.previous_chapter_start(150.0), Some(100.0));
        assert_eq!(np.previous_chapter_start(101.0), Some(0.0));
        assert_eq!(np.previous_chapter_start(1.0), None);
    }

    #[test]
    fn reply_without_path_updates_active_player() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        assert!(s.apply(&env(&playing())).unwrap());
        let mut reply = SetStateMessage {
            playback_queue: playing().playback_queue,
            ..Default::default()
        };
        reply.playback_queue.as_mut().unwrap().content_items[0].info = Some("About".into());
        assert!(s.apply(&env(&reply)).unwrap());
        assert_eq!(s.now_playing().description.as_deref(), Some("About"));
    }

    #[test]
    fn rate_changes_state() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        let mut msg = playing();
        msg.now_playing_info.as_mut().unwrap().playback_rate = Some(0.0);
        s.apply(&env(&msg)).unwrap();
        assert_eq!(s.now_playing().state, Some(PlaybackState::Paused));
        assert_eq!(s.now_playing().position(2000.0), Some(10.0));

        msg.playback_state_timestamp = Some(101.0);
        msg.now_playing_info.as_mut().unwrap().playback_rate = Some(2.0);
        s.apply(&env(&msg)).unwrap();
        assert_eq!(s.now_playing().state, Some(PlaybackState::Seeking));
    }

    #[test]
    fn old_state_ignored() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        s.apply(&env(&playing())).unwrap();
        let old = SetStateMessage {
            player_path: path("com.apple.TVMusic", None),
            playback_state: Some(PlaybackState::Stopped as i32),
            playback_state_timestamp: Some(50.0),
            ..Default::default()
        };
        s.apply(&env(&old)).unwrap();
        assert_eq!(s.now_playing().state, Some(PlaybackState::Playing));
    }

    #[test]
    fn paused_without_metadata_is_none() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "app");
        let msg = SetStateMessage {
            player_path: path("app", None),
            playback_state: Some(PlaybackState::Paused as i32),
            ..Default::default()
        };
        s.apply(&env(&msg)).unwrap();
        assert_eq!(s.now_playing().state, None);
    }

    #[test]
    fn content_item_update_merges() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "com.apple.TVMusic");
        s.apply(&env(&playing())).unwrap();
        let update = UpdateContentItemMessage {
            player_path: path("com.apple.TVMusic", None),
            content_items: vec![
                ContentItem {
                    identifier: Some("a".into()),
                    metadata: Some(ContentItemMetadata {
                        album_name: Some("Album".into()),
                        ..Default::default()
                    }),
                    info: Some("info".into()),
                    ..Default::default()
                },
                item("unknown", "Other"),
            ],
        };
        s.apply(&env(&update)).unwrap();
        let np = s.now_playing();
        assert_eq!(np.title.as_deref(), Some("Song"));
        assert_eq!(np.album.as_deref(), Some("Album"));
    }

    #[test]
    fn active_player_and_removal() {
        let mut s = NowPlayingState::new();
        set_client(&mut s, "tv");
        let mut msg = playing();
        msg.player_path = path("tv", Some("p1"));
        s.apply(&env(&msg)).unwrap();
        // Default player has no state yet.
        assert_eq!(s.now_playing().state, None);

        let set = SetNowPlayingPlayerMessage {
            player_path: path("tv", Some("p1")),
        };
        s.apply(&env(&set)).unwrap();
        assert_eq!(s.now_playing().state, Some(PlaybackState::Playing));

        let rm = RemovePlayerMessage {
            player_path: path("tv", Some("p1")),
        };
        s.apply(&env(&rm)).unwrap();
        assert_eq!(s.now_playing().state, None);

        let defaults = SetDefaultSupportedCommandsMessage {
            player_path: path("tv", None),
            supported_commands: Some(SupportedCommands {
                supported_commands: vec![CommandInfo {
                    command: Some(Command::Stop as i32),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        s.apply(&env(&defaults)).unwrap();
        assert!(s.now_playing().supports(Command::Stop));

        s.apply(&env(&UpdatePlayerMessage {
            player_path: path("tv", Some("p2")),
        }))
        .unwrap();
        s.apply(&env(&UpdateClientMessage {
            client: path("other", None).unwrap().client,
        }))
        .unwrap();

        let rm = RemoveClientMessage {
            client: path("tv", None).unwrap().client,
        };
        s.apply(&env(&rm)).unwrap();
        assert_eq!(s.active_app(), None);
        assert_eq!(s.now_playing(), NowPlaying::default());
    }

    #[test]
    fn unrelated_message() {
        let mut s = NowPlayingState::new();
        assert!(
            !s.apply(&env(&proto::SetConnectionStateMessage::default()))
                .unwrap()
        );
    }

    #[test]
    fn cocoa_epoch() {
        assert!(cocoa_time(SystemTime::UNIX_EPOCH + COCOA_EPOCH).abs() < f64::EPSILON);
        assert!((cocoa_time(SystemTime::UNIX_EPOCH) + 978_307_200.0).abs() < 1e-6);
    }
}
