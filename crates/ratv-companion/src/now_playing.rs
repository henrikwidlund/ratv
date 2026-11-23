//! What the device plays, from [`event::NOW_PLAYING_INFO`](crate::message::event::NOW_PLAYING_INFO).
//!
//! Each event holds a keyed archive of `TVRCNowPlayingInfo`. Most events
//! hold only the fields that changed.

use bytes::Bytes;
use ratv_codec::bplist::{self, Value as P};
use ratv_codec::opack::{Encoder, Value};

use crate::message::{Message, Outgoing};

/// Makes the device send [`event::NOW_PLAYING_INFO`](crate::message::event::NOW_PLAYING_INFO)
/// now. The response is empty. A paused item may not be sent.
#[derive(Debug, Clone, Copy)]
pub struct FetchNowPlaying;

impl Outgoing for FetchNowPlaying {
    const ID: &str = "FetchCurrentNowPlayingInfoEvent";

    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(0, |_| {});
    }
}

/// Playback state. Same values as in MRP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
#[non_exhaustive]
pub enum PlaybackState {
    Unknown,
    Playing,
    Paused,
    Stopped,
    Interrupted,
    Seeking,
}

impl PlaybackState {
    const fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Playing,
            2 => Self::Paused,
            3 => Self::Stopped,
            4 => Self::Interrupted,
            5 => Self::Seeking,
            _ => Self::Unknown,
        }
    }

    /// Number used on the wire.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Playing => 1,
            Self::Paused => 2,
            Self::Stopped => 3,
            Self::Interrupted => 4,
            Self::Seeking => 5,
        }
    }
}

/// What the device plays.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct NowPlaying {
    /// Title.
    pub title: Option<String>,
    /// Bundle id of the app. Not sent by all apps.
    pub app: Option<String>,
    /// Player id. Changes when another player or item becomes active.
    pub player: Option<String>,
    /// Playback state.
    pub state: Option<PlaybackState>,
    /// Playback rate, 1.0 is normal speed. The device can send the old rate
    /// for a short time after the state changes, so it can be 1 while
    /// paused. The merged value of `ratv::AppleTv` uses 0 then.
    pub rate: Option<f32>,
    /// Length in seconds. 0 for live streams. Can be below 0 if the app sends
    /// it.
    pub duration: Option<f64>,
    /// Position in seconds at `elapsed_timestamp`.
    pub elapsed: Option<f64>,
    /// Time of `elapsed`, seconds since 2001-01-01 (MRP time).
    pub elapsed_timestamp: Option<f64>,
    /// Image of the item, usually JPEG.
    pub artwork: Option<Bytes>,
    /// True if subtitles are shown.
    pub captions_enabled: Option<bool>,
}

impl NowPlaying {
    /// Applies a [`event::NOW_PLAYING_INFO`](crate::message::event::NOW_PLAYING_INFO)
    /// event. Fields that the event does not hold keep their value. An event
    /// with metadata replaces all metadata fields and the artwork. A number
    /// that is not finite, or a rate too large for f32, clears its field.
    /// Returns true if something changed.
    pub fn apply(&mut self, msg: &Message<'_>) -> bool {
        let Some(data) = msg.field("NowPlayingInfoKey").and_then(Value::as_bytes) else {
            return false;
        };
        let Ok(archive) = bplist::decode(data) else {
            return false;
        };
        let Some(objects) = archive.get("$objects").and_then(P::as_array) else {
            return false;
        };
        let Some(root) = archive
            .get("$top")
            .and_then(|t| t.get("root"))
            .and_then(|v| resolve(objects, v))
        else {
            return false;
        };

        let mut changed = false;
        if let Some(v) = field(objects, root, "playbackState").and_then(P::as_i64) {
            changed |= set(&mut self.state, Some(PlaybackState::from_code(v)));
        }
        if let Some(v) = field(objects, root, "playbackRate").and_then(number) {
            // Rates are small, so `f32` keeps them exact enough.
            #[allow(clippy::cast_possible_truncation)]
            let rate = v as f32;
            // NaN is never equal to itself, so it would count as a change
            // each time. A rate that is not finite is not known.
            changed |= set(&mut self.rate, rate.is_finite().then_some(rate));
        }
        if let Some(v) = field(objects, root, "captionsEnabled").and_then(P::as_bool) {
            changed |= set(&mut self.captions_enabled, Some(v));
        }
        if let Some(v) = field(objects, root, "playerIdentifier").and_then(|v| string(objects, v))
            && self.player.as_deref() != Some(v)
        {
            self.player = Some(v.to_owned());
            changed = true;
        }
        // The device sends the image again with every full update, which is
        // an update with metadata. A full update without an image means that
        // the item has no image.
        let metadata = field(objects, root, "metadata");
        match field(objects, root, "imageData").and_then(|v| bytes(objects, v)) {
            Some(v) if self.artwork.as_deref() != Some(v) => {
                self.artwork = Some(Bytes::copy_from_slice(v));
                changed = true;
            }
            None if metadata.is_some() && self.artwork.is_some() => {
                self.artwork = None;
                changed = true;
            }
            _ => {}
        }
        // Metadata is always sent whole.
        if let Some(md) = metadata {
            let text = |key| field(objects, md, key).and_then(|v| string(objects, v));
            for (field, key) in [(&mut self.title, "title"), (&mut self.app, "bundleID")] {
                let value = text(key);
                if field.as_deref() != value {
                    *field = value.map(str::to_owned);
                    changed = true;
                }
            }
            changed |= set(
                &mut self.duration,
                finite(field(objects, md, "duration").and_then(number)),
            );
            changed |= set(
                &mut self.elapsed,
                finite(field(objects, md, "timeOffset").and_then(number)),
            );
            changed |= set(
                &mut self.elapsed_timestamp,
                finite(field(objects, md, "timestamp").and_then(number)),
            );
        }
        changed
    }

    /// Position in seconds at `now` (MRP time). Values that are not finite
    /// are treated as missing, and so is a result that is not finite. Never
    /// below 0. Keeps growing while paused until the rate is 0, see
    /// [`rate`](Self::rate).
    #[must_use]
    pub fn position(&self, now: f64) -> Option<f64> {
        let elapsed = self.elapsed.filter(|e| e.is_finite())?;
        let ts = self.elapsed_timestamp.filter(|t| t.is_finite());
        let rate = self.rate.filter(|r| r.is_finite());
        let pos = match (ts, rate) {
            (Some(ts), Some(rate)) => elapsed + (now - ts) * f64::from(rate),
            _ => elapsed,
        };
        pos.is_finite().then(|| pos.max(0.0))
    }
}

/// Value of `key` in an archived object.
fn field<'v, 'a>(objects: &'v [P<'a>], obj: &'v P<'a>, key: &str) -> Option<&'v P<'a>> {
    resolve(objects, obj.get(key)?)
}

/// Follows a UID. UID 0 is `$null`, which means the field is not sent.
fn resolve<'v, 'a>(objects: &'v [P<'a>], v: &'v P<'a>) -> Option<&'v P<'a>> {
    match *v {
        P::Uid(0) => None,
        P::Uid(i) => objects.get(usize::try_from(i).ok()?),
        _ => Some(v),
    }
}

/// String, or archived `NSString`.
fn string<'v>(objects: &'v [P<'_>], v: &'v P<'_>) -> Option<&'v str> {
    v.as_str()
        .or_else(|| resolve(objects, v.get("NS.string")?)?.as_str())
}

/// Data, or archived `NSData`.
fn bytes<'v>(objects: &'v [P<'_>], v: &'v P<'_>) -> Option<&'v [u8]> {
    v.as_bytes()
        .or_else(|| resolve(objects, v.get("NS.data")?)?.as_bytes())
}

/// `None` if `v` is not finite. NaN is never equal to itself, so it would count
/// as a change each time.
fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|v| v.is_finite())
}

const fn number(v: &P<'_>) -> Option<f64> {
    match *v {
        P::Real(f) => Some(f),
        // Values are small, so the conversion is exact.
        #[allow(clippy::cast_precision_loss)]
        P::Int(i) => Some(i as f64),
        _ => None,
    }
}

fn set<T: PartialEq>(field: &mut T, value: T) -> bool {
    if *field == value {
        return false;
    }
    *field = value;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::encode;

    /// Builds an event like the device sends. `None` values are `$null`.
    pub(crate) fn info_event(
        state: Option<i64>,
        rate: Option<f64>,
        image: Option<&[u8]>,
        metadata: Option<(&str, f64, f64)>,
    ) -> Vec<u8> {
        info_event_with_duration(state, rate, image, metadata, 3600.0)
    }

    /// As [`info_event`], with this duration in the metadata.
    fn info_event_with_duration(
        state: Option<i64>,
        rate: Option<f64>,
        image: Option<&[u8]>,
        metadata: Option<(&str, f64, f64)>,
        duration: f64,
    ) -> Vec<u8> {
        let mut objects = vec![P::String("$null".into())];
        let mut add = |v: P<'static>| {
            objects.push(v);
            P::Uid(objects.len() as u64 - 1)
        };
        let null = || P::Uid(0);
        let state = state.map_or_else(null, |s| add(P::Int(s)));
        let rate = rate.map_or_else(null, |r| add(P::Real(r)));
        let image = image.map_or_else(null, |i| add(P::Data(i.to_vec().into())));
        let metadata = metadata.map_or_else(null, |(title, elapsed, ts)| {
            let title = add(P::String(title.to_owned().into()));
            let bundle = add(P::Dict(vec![(
                "NS.string".into(),
                P::String("com.apple.TVWatchList".into()),
            )]));
            let duration = add(P::Real(duration));
            let elapsed = add(P::Real(elapsed));
            let ts = add(P::Real(ts));
            add(P::Dict(vec![
                ("title".into(), title),
                ("bundleID".into(), bundle),
                ("duration".into(), duration),
                ("timeOffset".into(), elapsed),
                ("timestamp".into(), ts),
                ("episodeTitle".into(), P::Uid(0)),
            ]))
        });
        let captions = add(P::Bool(false));
        let player = add(P::String("avkit-1".into()));
        let root = add(P::Dict(vec![
            ("playbackState".into(), state),
            ("playbackRate".into(), rate),
            ("imageData".into(), image),
            ("metadata".into(), metadata),
            ("captionsEnabled".into(), captions),
            ("playerIdentifier".into(), player),
        ]));
        let archive = P::Dict(vec![
            ("$top".into(), P::Dict(vec![("root".into(), root)])),
            ("$objects".into(), P::Array(objects)),
        ])
        .encode();
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(3, |e| {
            e.str("_i").str("NowPlayingInfo");
            e.str("_t").int(1);
            e.str("_c").dict(1, |e| {
                e.str("NowPlayingInfoKey").data(&archive);
            });
        });
        out
    }

    fn apply(np: &mut NowPlaying, raw: &[u8]) -> bool {
        np.apply(&Message::decode(raw).unwrap())
    }

    #[test]
    fn full_then_partial() {
        let mut np = NowPlaying::default();
        let full = info_event(
            Some(1),
            Some(1.0),
            Some(b"jpeg"),
            Some(("Nocturne", 10.0, 100.0)),
        );
        assert!(apply(&mut np, &full));
        assert_eq!(np.title.as_deref(), Some("Nocturne"));
        assert_eq!(np.app.as_deref(), Some("com.apple.TVWatchList"));
        assert_eq!(np.player.as_deref(), Some("avkit-1"));
        assert_eq!(np.state, Some(PlaybackState::Playing));
        assert_eq!(np.rate, Some(1.0));
        assert_eq!(np.duration, Some(3600.0));
        assert_eq!(np.artwork.as_deref(), Some(&b"jpeg"[..]));
        assert_eq!(np.captions_enabled, Some(false));
        assert_eq!(np.position(105.0), Some(15.0));

        // The same event again changes nothing and keeps the image.
        let artwork = np.artwork.clone().unwrap();
        assert!(!apply(&mut np, &full));
        assert_eq!(np.artwork.unwrap().as_ptr(), artwork.as_ptr());

        // A partial event changes only its fields.
        let mut np = NowPlaying::default();
        apply(&mut np, &full);
        assert!(apply(&mut np, &info_event(Some(2), None, None, None)));
        assert_eq!(np.state, Some(PlaybackState::Paused));
        assert_eq!(np.rate, Some(1.0));
        assert_eq!(np.title.as_deref(), Some("Nocturne"));
        assert!(apply(&mut np, &info_event(None, Some(0.0), None, None)));
        assert_eq!(np.position(200.0), Some(10.0));
        // Finite input that overflows.
        let overflow = NowPlaying {
            elapsed_timestamp: Some(-1.7e308),
            rate: Some(2.0),
            ..np.clone()
        };
        assert_eq!(overflow.position(200.0), None);
        // Also when it overflows below 0.
        let backward = NowPlaying {
            rate: Some(-2.0),
            ..overflow
        };
        assert_eq!(backward.position(200.0), None);
        assert_eq!(np.position(f64::NAN), None);
        // Without a rate, the elapsed time is limited too.
        let fixed = NowPlaying {
            elapsed: Some(-3.0),
            rate: None,
            ..np.clone()
        };
        assert_eq!(fixed.position(200.0), Some(0.0));
        assert!(apply(&mut np, &info_event(Some(3), None, None, None)));
        assert_eq!(np.state, Some(PlaybackState::Stopped));
        assert!(np.artwork.is_some());

        // A full event without an image: the new item has no image.
        let next = info_event(Some(1), Some(1.0), None, Some(("News", 0.0, 100.0)));
        assert!(apply(&mut np, &next));
        assert_eq!(np.title.as_deref(), Some("News"));
        assert_eq!(np.artwork, None);

        // A rate that is not finite is not known, and is no change again.
        let nan = info_event(None, Some(f64::NAN), None, None);
        assert!(apply(&mut np, &nan));
        assert_eq!(np.rate, None);
        assert!(!apply(&mut np, &nan));
        // The same for the position.
        let nan = info_event(None, None, None, Some(("News", f64::NAN, f64::NAN)));
        assert!(apply(&mut np, &nan));
        assert_eq!((np.elapsed, np.elapsed_timestamp), (None, None));
        assert!(!apply(&mut np, &nan));
        // The same for the duration.
        let nan = info_event_with_duration(None, None, None, Some(("News", 1.0, 1.0)), f64::NAN);
        apply(&mut np, &nan);
        assert_eq!(np.duration, None);
        assert!(!apply(&mut np, &nan));
    }

    #[test]
    fn invalid_events_change_nothing() {
        let mut np = NowPlaying::default();
        let mut out = Vec::new();
        encode(&FetchNowPlaying, 1, &mut out);
        assert!(!apply(&mut np, &out));
        let mut out = Vec::new();
        Encoder::new(&mut out).dict(2, |e| {
            e.str("_i").str("NowPlayingInfo");
            e.str("_c").dict(1, |e| {
                e.str("NowPlayingInfoKey").data(b"not a plist");
            });
        });
        assert!(!apply(&mut np, &out));
        assert_eq!(np, NowPlaying::default());
        assert_eq!(PlaybackState::from_code(9), PlaybackState::Unknown);
        assert_eq!(PlaybackState::Seeking.code(), 5);
    }
}
