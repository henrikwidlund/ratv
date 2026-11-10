//! Now playing from MRP, made faster with Companion.
//!
//! MRP has the most data. Companion reports state changes about 0.5 s
//! earlier, and sends the artwork and the subtitle state.

use std::borrow::Cow;
use std::time::SystemTime;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::companion;
use crate::mrp::{NowPlaying, PlaybackState, cocoa_time};

/// Which protocol changed a group of fields last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Source {
    #[default]
    Mrp,
    Companion,
}

/// Combines MRP and Companion now playing. Keeps the last values of each
/// side, so the side that changed last wins.
#[derive(Debug, Default)]
pub(crate) struct Merger {
    mrp_state: (Option<PlaybackState>, Option<f32>),
    mrp_app: Option<String>,
    companion_state: (Option<companion::PlaybackState>, Option<f32>),
    mrp_position: (Option<f64>, Option<f64>),
    companion_position: (Option<f64>, Option<f64>),
    state_from: Source,
    position_from: Source,
    /// Last sent (elapsed, timestamp, rate).
    last: (Option<f64>, Option<f64>, Option<f32>),
    /// Position fixed at a rate change that came without a new position.
    held: Option<(f64, f64)>,
}

/// Fields that the merge sets on top of the MRP data. Small, so a
/// Companion change does not copy the whole [`NowPlaying`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fields {
    state: Option<PlaybackState>,
    rate: Option<f32>,
    elapsed: Option<f64>,
    elapsed_timestamp: Option<f64>,
    artwork_data: Option<bytes::Bytes>,
    captions_enabled: Option<bool>,
    /// After Home, MRP may name an older app that plays nothing.
    clear_app: bool,
}

impl Fields {
    /// Sets the fields on `np`, which holds the MRP data of `base`.
    /// Returns true if something changed.
    fn apply(&self, base: &NowPlaying, np: &mut NowPlaying) -> bool {
        let mut changed = false;
        changed |= set(&mut np.state, self.state);
        changed |= set(&mut np.rate, self.rate);
        changed |= set(&mut np.elapsed, self.elapsed);
        changed |= set(&mut np.elapsed_timestamp, self.elapsed_timestamp);
        changed |= set(&mut np.captions_enabled, self.captions_enabled);
        if !same_image(np.artwork_data.as_ref(), self.artwork_data.as_ref()) {
            np.artwork_data.clone_from(&self.artwork_data);
            changed = true;
        }
        let (app, app_name) = if self.clear_app {
            (None, None)
        } else {
            (base.app.as_deref(), base.app_name.as_deref())
        };
        if np.app.as_deref() != app {
            np.app = app.map(str::to_owned);
            changed = true;
        }
        if np.app_name.as_deref() != app_name {
            np.app_name = app_name.map(str::to_owned);
            changed = true;
        }
        changed
    }
}

fn set<T: PartialEq>(field: &mut T, value: T) -> bool {
    if *field == value {
        return false;
    }
    *field = value;
    true
}

/// The image is usually the same buffer, so compare the address first.
fn same_image(a: Option<&bytes::Bytes>, b: Option<&bytes::Bytes>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => std::ptr::eq(&raw const a[..], &raw const b[..]) || a == b,
        (a, b) => a.is_none() && b.is_none(),
    }
}

impl Merger {
    /// MRP data, with state and position from Companion when Companion
    /// changed them last for the same item. `now` is MRP time.
    #[cfg(test)]
    pub(crate) fn merge(
        &mut self,
        mrp: Option<&NowPlaying>,
        comp: &companion::NowPlaying,
        now: f64,
    ) -> NowPlaying {
        let base = mrp.cloned().unwrap_or_else(|| from_companion(comp));
        let mut np = base.clone();
        self.fields(&base, comp, now).apply(&base, &mut np);
        np
    }

    /// The fields to set on `base`, the MRP data, or the Companion data
    /// without MRP. `now` is MRP time.
    pub(crate) fn fields(
        &mut self,
        base: &NowPlaying,
        comp: &companion::NowPlaying,
        now: f64,
    ) -> Fields {
        let old_position = (self.companion_position, self.mrp_position);
        let mut f = self.combine(base, comp);
        // Companion sends the new state about 0.4 s before the new rate.
        // Some apps also send a paused item with rate 1 over MRP.
        match f.state {
            Some(PlaybackState::Paused | PlaybackState::Stopped)
                if f.rate.is_some_and(|r| r != 0.0) =>
            {
                f.rate = Some(0.0);
            }
            Some(PlaybackState::Playing)
                if self.state_from == Source::Companion && f.rate == Some(0.0) =>
            {
                f.rate = Some(1.0);
            }
            _ => {}
        }
        // MRP shows playing at another rate as seeking.
        if f.state == Some(PlaybackState::Playing)
            && f.rate.is_some_and(|r| r != 0.0 && (r - 1.0).abs() >= 0.01)
        {
            f.state = Some(PlaybackState::Seeking);
        }
        // A rate can change without a new position. The old position is then
        // wrong, so keep the position that was shown at the change.
        if old_position != (self.companion_position, self.mrp_position) {
            self.held = None;
        } else if f.rate != self.last.2
            && let (Some(elapsed), Some(ts), Some(rate)) = self.last
        {
            self.held = Some((elapsed + (now - ts) * f64::from(rate), now));
        }
        if let Some((elapsed, ts)) = self.held {
            f.elapsed = Some(elapsed);
            f.elapsed_timestamp = Some(ts);
        }
        self.last = (f.elapsed, f.elapsed_timestamp, f.rate);
        f
    }

    fn combine(&mut self, mrp: &NowPlaying, comp: &companion::NowPlaying) -> Fields {
        let comp_state = (comp.state, comp.rate);
        if comp_state != self.companion_state {
            self.companion_state = comp_state;
            self.state_from = Source::Companion;
        }
        let comp_position = (comp.elapsed, comp.elapsed_timestamp);
        if comp_position != self.companion_position {
            self.companion_position = comp_position;
            self.position_from = Source::Companion;
        }
        let mrp_state = (mrp.state, mrp.rate);
        if mrp_state != self.mrp_state {
            self.mrp_state = mrp_state;
            self.state_from = Source::Mrp;
        }
        // Another app is newer, also if its state is the same.
        if mrp.app != self.mrp_app {
            self.mrp_app.clone_from(&mrp.app);
            self.state_from = Source::Mrp;
        }
        let mrp_position = (mrp.elapsed, mrp.elapsed_timestamp);
        if mrp_position != self.mrp_position {
            self.mrp_position = mrp_position;
            self.position_from = Source::Mrp;
        }

        let mut f = Fields {
            state: mrp.state,
            rate: mrp.rate,
            elapsed: mrp.elapsed,
            elapsed_timestamp: mrp.elapsed_timestamp,
            artwork_data: mrp.artwork_data.clone(),
            captions_enabled: mrp.captions_enabled,
            clear_app: false,
        };
        if comp.title.is_some() && comp.title == mrp.title {
            f.artwork_data.clone_from(&comp.artwork);
            f.captions_enabled = comp.captions_enabled;
            if self.state_from == Source::Companion {
                if let Some(state) = comp.state {
                    f.state = state_of(state);
                }
                if comp.rate.is_some() {
                    f.rate = comp.rate;
                }
            }
            if self.position_from == Source::Companion && comp.elapsed.is_some() {
                f.elapsed = comp.elapsed;
                f.elapsed_timestamp = comp.elapsed_timestamp;
            }
        } else {
            f.clear_app = self.state_from == Source::Companion
                && comp.state == Some(companion::PlaybackState::Stopped)
                && mrp.title.is_none();
        }
        f
    }
}

fn from_companion(comp: &companion::NowPlaying) -> NowPlaying {
    let mut np = NowPlaying::default();
    np.app.clone_from(&comp.app);
    np.title.clone_from(&comp.title);
    np.state = comp.state.and_then(state_of);
    np.rate = comp.rate;
    np.duration = comp.duration;
    np.elapsed = comp.elapsed;
    np.elapsed_timestamp = comp.elapsed_timestamp;
    np.artwork_data.clone_from(&comp.artwork);
    np.captions_enabled = comp.captions_enabled;
    np
}

fn state_of(state: companion::PlaybackState) -> Option<PlaybackState> {
    PlaybackState::try_from(state.code()).ok()
}

/// Stops the merge task when the last [`AppleTv`](crate::AppleTv) clone is dropped.
#[derive(Debug)]
pub(crate) struct MergeTask(JoinHandle<()>);

impl Drop for MergeTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts a task that sends the merged now playing on each change.
pub(crate) fn spawn(
    mut mrp: Option<watch::Receiver<NowPlaying>>,
    mut comp: watch::Receiver<companion::NowPlaying>,
) -> (watch::Receiver<NowPlaying>, MergeTask) {
    let mut merger = Merger::default();
    let (tx, mut rx) = watch::channel(NowPlaying::default());
    update(&mut merger, &tx, mrp.as_ref(), &comp, true);
    // The first value is not a change.
    rx.mark_unchanged();
    let task = tokio::spawn(async move {
        loop {
            let mrp_changed = async {
                match &mut mrp {
                    Some(m) => m.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            // A new MRP value is copied; a Companion change only sets the
            // merged fields in place.
            let base_changed = tokio::select! {
                ok = mrp_changed => {
                    if !ok {
                        mrp = None;
                    }
                    true
                },
                ok = comp.changed() => {
                    if ok.is_err() {
                        break;
                    }
                    // Without MRP, the base is the Companion data.
                    mrp.is_none()
                },
            };
            update(&mut merger, &tx, mrp.as_ref(), &comp, base_changed);
        }
        // Companion closed: MRP data only.
        while let Some(m) = &mut mrp
            && m.changed().await.is_ok()
        {
            let np = m.borrow().clone();
            publish(&tx, np);
        }
    });
    (rx, MergeTask(task))
}

/// Merges the current values and sends the result if it changed.
fn update(
    merger: &mut Merger,
    tx: &watch::Sender<NowPlaying>,
    mrp: Option<&watch::Receiver<NowPlaying>>,
    comp: &watch::Receiver<companion::NowPlaying>,
    base_changed: bool,
) {
    let now = cocoa_time(SystemTime::now());
    let comp = comp.borrow();
    let mrp = mrp.map(watch::Receiver::borrow);
    let base = mrp
        .as_deref()
        .map_or_else(|| Cow::Owned(from_companion(&comp)), Cow::Borrowed);
    let base = &*base;
    let fields = merger.fields(base, &comp, now);
    if base_changed {
        let mut np = base.clone();
        fields.apply(base, &mut np);
        publish(tx, np);
    } else {
        tx.send_if_modified(|np| fields.apply(base, np));
    }
}

/// Sends `np` if it differs from the current value.
fn publish(tx: &watch::Sender<NowPlaying>, mut np: NowPlaying) {
    tx.send_if_modified(|old| {
        // Leave the image out of the full compare.
        let (old_art, new_art) = (old.artwork_data.take(), np.artwork_data.take());
        let changed = !same_image(old_art.as_ref(), new_art.as_ref()) || *old != np;
        np.artwork_data = new_art;
        if changed {
            *old = np;
        } else {
            old.artwork_data = old_art;
        }
        changed
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use companion::PlaybackState as C;

    fn mrp(title: &str, state: PlaybackState, rate: f32, elapsed: f64) -> NowPlaying {
        let mut np = NowPlaying::default();
        np.app = Some("com.apple.TVWatchList".into());
        np.title = Some(title.into());
        np.artist = Some("Artist".into());
        np.state = Some(state);
        np.rate = Some(rate);
        np.elapsed = Some(elapsed);
        np.elapsed_timestamp = Some(100.0);
        np
    }

    fn merge(m: &mut Merger, mrp: Option<&NowPlaying>, comp: &companion::NowPlaying) -> NowPlaying {
        m.merge(mrp, comp, 102.0)
    }

    fn comp(title: Option<&str>, state: C, rate: f32, elapsed: f64) -> companion::NowPlaying {
        let mut np = companion::NowPlaying::default();
        np.title = title.map(str::to_owned);
        np.state = Some(state);
        np.rate = Some(rate);
        np.elapsed = Some(elapsed);
        np.elapsed_timestamp = Some(101.0);
        np.artwork = Some(bytes::Bytes::from_static(b"jpeg"));
        np.captions_enabled = Some(true);
        np
    }

    #[test]
    fn companion_is_faster_for_the_same_item() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        let np = merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.0, 11.0),
        );
        // Both changed in the same call: MRP wins, extra data comes from Companion.
        assert_eq!(np.elapsed, Some(10.0));
        assert_eq!(np.artist.as_deref(), Some("Artist"));
        assert_eq!(np.artwork_data.as_deref(), Some(&b"jpeg"[..]));
        assert_eq!(np.captions_enabled, Some(true));

        // Companion reports the pause first.
        let paused = comp(Some("Nocturne"), C::Paused, 0.0, 12.0);
        let np = merge(&mut m, Some(&playing), &paused);
        assert_eq!(np.state, Some(PlaybackState::Paused));
        assert_eq!(np.rate, Some(0.0));
        assert_eq!(np.elapsed, Some(12.0));

        // MRP reports it later; nothing changes.
        let mrp_paused = mrp("Nocturne", PlaybackState::Paused, 0.0, 12.0);
        let later = merge(&mut m, Some(&mrp_paused), &paused);
        assert_eq!(later.state, Some(PlaybackState::Paused));
        assert_eq!(later.elapsed, Some(12.0));
    }

    #[test]
    fn rate_change_without_position_keeps_the_position() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        let mut np = comp(Some("Nocturne"), C::Playing, 1.0, 11.0);
        merge(&mut m, Some(&playing), &np);
        // Companion sends only the new rate. Its position is old.
        np.rate = Some(2.0);
        let faster = merge(&mut m, Some(&playing), &np);
        assert_eq!(faster.rate, Some(2.0));
        assert_eq!(faster.elapsed, Some(12.0));
        assert_eq!(faster.elapsed_timestamp, Some(102.0));
        // A new position ends the hold.
        np.elapsed = Some(12.5);
        assert_eq!(merge(&mut m, Some(&playing), &np).elapsed, Some(12.5));
    }

    #[test]
    fn state_before_rate() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        let mut np = comp(Some("Nocturne"), C::Playing, 1.0, 11.0);
        merge(&mut m, Some(&playing), &np);
        // Pause: the state comes first, with the old rate.
        np.state = Some(C::Paused);
        let paused = merge(&mut m, Some(&playing), &np);
        assert_eq!(paused.rate, Some(0.0));
        assert_eq!(paused.elapsed, Some(12.0));
        // Play: the state comes first, with rate 0.
        np.rate = Some(0.0);
        merge(&mut m, Some(&playing), &np);
        np.state = Some(C::Playing);
        assert_eq!(merge(&mut m, Some(&playing), &np).rate, Some(1.0));
        // MRP sends a paused item with rate 1.
        let mut m = Merger::default();
        let paused = mrp("Nocturne", PlaybackState::Paused, 1.0, 10.0);
        let np = merge(&mut m, Some(&paused), &companion::NowPlaying::default());
        assert_eq!(np.rate, Some(0.0));
    }

    #[test]
    fn other_item_keeps_mrp() {
        let mut m = Merger::default();
        let playing = mrp("Song", PlaybackState::Paused, 0.0, 5.0);
        // Companion sent nothing for a paused item.
        let np = merge(&mut m, Some(&playing), &companion::NowPlaying::default());
        assert_eq!(np, playing);
        let np = merge(
            &mut m,
            Some(&playing),
            &comp(Some("Other"), C::Playing, 1.0, 1.0),
        );
        assert_eq!(np, playing);
    }

    #[test]
    fn stopped_after_home_clears_the_app() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.0, 10.0),
        );
        // Order seen on the TV: Companion pauses, MRP names an older app,
        // then Companion stops.
        let paused = comp(Some("Nocturne"), C::Paused, 0.0, 10.0);
        merge(&mut m, Some(&playing), &paused);
        let mut other_app = NowPlaying::default();
        other_app.app = Some("com.teliasonera.playplus.telia".into());
        assert_eq!(merge(&mut m, Some(&other_app), &paused), other_app);
        let np = merge(
            &mut m,
            Some(&other_app),
            &comp(Some("Nocturne"), C::Stopped, 0.0, 10.0),
        );
        assert_eq!(np.app, None);
        assert_eq!(np.title, None);
        // Without a stop from Companion, MRP keeps its app.
        let mut m = Merger::default();
        assert_eq!(
            merge(&mut m, Some(&other_app), &companion::NowPlaying::default()),
            other_app
        );
    }

    #[test]
    fn other_rate_is_seeking() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.0, 10.0),
        );
        let fast = merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 2.0, 10.0),
        );
        assert_eq!(fast.state, Some(PlaybackState::Seeking));
        assert_eq!(fast.rate, Some(2.0));
    }

    #[test]
    fn new_app_after_home_is_shown() {
        let mut m = Merger::default();
        let mut home = NowPlaying::default();
        home.app = Some("com.teliasonera.playplus.telia".into());
        let stopped = comp(Some("Nocturne"), C::Stopped, 0.0, 10.0);
        merge(&mut m, Some(&home), &companion::NowPlaying::default());
        assert_eq!(merge(&mut m, Some(&home), &stopped).app, None);
        // Another app with the same (empty) state.
        let mut other = NowPlaying::default();
        other.app = Some("com.apple.TVMusic".into());
        assert_eq!(merge(&mut m, Some(&other), &stopped), other);
    }

    #[test]
    fn publish_compares_the_image() {
        let (tx, _rx) = watch::channel(NowPlaying::default());
        let mut np = NowPlaying::default();
        np.artwork_data = Some(bytes::Bytes::from_static(b"jpeg"));
        publish(&tx, np.clone());
        assert!(tx.borrow().artwork_data.is_some());
        // Same buffer, and a copy with the same bytes: no change.
        let mut rx = tx.subscribe();
        publish(&tx, np.clone());
        let mut copy = np.clone();
        copy.artwork_data = Some(bytes::Bytes::copy_from_slice(b"jpeg"));
        publish(&tx, copy);
        assert!(!rx.has_changed().unwrap());
        // Other bytes or no image: changed.
        let mut other = np;
        other.artwork_data = Some(bytes::Bytes::from_static(b"png"));
        publish(&tx, other);
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        publish(&tx, NowPlaying::default());
        assert!(rx.has_changed().unwrap());
        assert_eq!(tx.borrow().artwork_data, None);
    }

    #[test]
    fn companion_only() {
        let mut m = Merger::default();
        let np = merge(&mut m, None, &comp(Some("Nocturne"), C::Paused, 0.0, 12.0));
        assert_eq!(np.title.as_deref(), Some("Nocturne"));
        assert_eq!(np.state, Some(PlaybackState::Paused));
        assert_eq!(np.elapsed, Some(12.0));
        assert_eq!(np.artwork_data.as_deref(), Some(&b"jpeg"[..]));
    }

    #[tokio::test]
    async fn task_sends_changes() {
        let (mrp_tx, mrp_rx) = watch::channel(mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0));
        let (comp_tx, comp_rx) = watch::channel(companion::NowPlaying::default());
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx);
        assert_eq!(rx.borrow().state, Some(PlaybackState::Playing));
        comp_tx.send_replace(comp(Some("Nocturne"), C::Paused, 0.0, 2.0));
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow_and_update().state, Some(PlaybackState::Paused));
        assert_eq!(rx.borrow().artist.as_deref(), Some("Artist"));
        assert!(rx.borrow().artwork_data.is_some());
        // Companion moves to another item: the MRP values come back.
        comp_tx.send_replace(comp(Some("Other"), C::Paused, 0.0, 2.0));
        rx.changed().await.unwrap();
        let mut np = rx.borrow_and_update().clone();
        // The position is held at the pause, then moves on.
        assert_eq!(np.elapsed, Some(2.0));
        (np.elapsed, np.elapsed_timestamp) = (Some(1.0), Some(100.0));
        assert_eq!(np, mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0));
        // Companion closes: MRP data only.
        drop(comp_tx);
        mrp_tx.send_replace(mrp("Song", PlaybackState::Playing, 1.0, 3.0));
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().title.as_deref(), Some("Song"));
        drop(task);
    }
}
