//! Now playing from MRP, made faster with Companion.
//!
//! MRP has the most data. Companion reports state changes about 0.5 s
//! earlier, and sends the artwork and the subtitle state.

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

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
    /// Which side changed last: the Companion state or the MRP app.
    app_from: Source,
    /// Companion has sent a state or a title.
    companion_seen: bool,
    /// The Companion value at the start is known, but MRP had no app yet.
    /// A first MRP app before this time is then older than the Companion
    /// value.
    first_mrp_app_old: Option<Instant>,
    /// Last merged (elapsed, timestamp, rate).
    last: (Option<f64>, Option<f64>, Option<f32>),
    /// Position fixed at a rate change that came without a new position.
    held: Option<(f64, f64)>,
}

impl Merger {
    /// At the start, the Companion value is data from the device. It counts
    /// as newer than `mrp_app`, so an empty value clears the app after Home.
    /// If MRP has no app yet, this also holds for the first MRP app, if it
    /// comes within [`FIRST_MRP_APP_WINDOW`].
    fn companion_known(&mut self, mrp_app: Option<&str>) {
        self.companion_seen = true;
        self.app_from = Source::Companion;
        self.mrp_app = mrp_app.map(str::to_owned);
        self.first_mrp_app_old = mrp_app
            .is_none()
            .then(|| Instant::now() + FIRST_MRP_APP_WINDOW);
    }

    /// Call when MRP closes. State and position then come from Companion, and
    /// a position held from MRP is dropped.
    const fn mrp_closed(&mut self) {
        self.state_from = Source::Companion;
        self.position_from = Source::Companion;
        self.held = None;
        self.last = (None, None, None);
    }
}

/// How long after the start the first MRP app counts as older than the
/// Companion value. MRP sends the app soon after connect.
const FIRST_MRP_APP_WINDOW: Duration = Duration::from_secs(2);

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
        let mut changed = self.apply_values(np);
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

    /// Sets the fields on `np`, which is the base itself.
    fn apply_to_base(&self, np: &mut NowPlaying) {
        self.apply_values(np);
        if self.clear_app {
            np.app = None;
            np.app_name = None;
        }
    }

    /// Sets all fields but the app. Returns true if something changed.
    fn apply_values(&self, np: &mut NowPlaying) -> bool {
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
        let mut np = mrp.cloned().unwrap_or_else(|| from_companion(comp));
        self.fields(&np, mrp.is_some(), comp, now)
            .apply_to_base(&mut np);
        np
    }

    /// The fields to set on `base`, the MRP data. Without MRP (`has_mrp`
    /// false), `base` is the Companion data. `now` is MRP time.
    pub(crate) fn fields(
        &mut self,
        base: &NowPlaying,
        has_mrp: bool,
        comp: &companion::NowPlaying,
        now: f64,
    ) -> Fields {
        let old_position = (self.companion_position, self.mrp_position);
        let mut f = self.combine(base, has_mrp, comp);
        // Companion sends the new state about 0.4 s before the new rate.
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
        // MRP shows playing at a scan speed as seeking.
        if f.state == Some(PlaybackState::Playing)
            && f.rate
                .is_some_and(|r| r != 0.0 && !base.is_playback_speed(r))
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
            let at = elapsed + (now - ts) * f64::from(rate);
            // Bad but finite input can overflow. Then no position is held.
            // Limited like `position`.
            let max = base.duration.filter(|d| *d > 0.0).unwrap_or(f64::INFINITY);
            self.held = at.is_finite().then(|| (at.clamp(0.0, max), now));
        }
        if let Some((elapsed, ts)) = self.held {
            f.elapsed = Some(elapsed);
            f.elapsed_timestamp = Some(ts);
        }
        self.last = (f.elapsed, f.elapsed_timestamp, f.rate);
        f
    }

    fn combine(&mut self, mrp: &NowPlaying, has_mrp: bool, comp: &companion::NowPlaying) -> Fields {
        let comp_state = (comp.state, comp.rate);
        if comp_state != self.companion_state {
            self.companion_state = comp_state;
            self.state_from = Source::Companion;
            self.app_from = Source::Companion;
        }
        self.companion_seen |= comp.state.is_some() || comp.title.is_some();
        let comp_position = (comp.elapsed, comp.elapsed_timestamp);
        if comp_position != self.companion_position {
            self.companion_position = comp_position;
            self.position_from = Source::Companion;
        }
        // Without MRP, `mrp` is the Companion data and must not count as an MRP change.
        if has_mrp {
            let mrp_state = (mrp.state, mrp.rate);
            if mrp_state != self.mrp_state {
                self.mrp_state = mrp_state;
                self.state_from = Source::Mrp;
            }
            // Another app is newer, also if its state is the same.
            if mrp.app != self.mrp_app {
                self.mrp_app.clone_from(&mrp.app);
                self.state_from = Source::Mrp;
                let old = self
                    .first_mrp_app_old
                    .take()
                    .is_some_and(|until| Instant::now() < until);
                if !old {
                    self.app_from = Source::Mrp;
                }
            }
            let mrp_position = (mrp.elapsed, mrp.elapsed_timestamp);
            if mrp_position != self.mrp_position {
                self.mrp_position = mrp_position;
                self.position_from = Source::Mrp;
            }
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
        } else if has_mrp && mrp.title.is_none() {
            // After Home, MRP keeps an app that plays nothing. Clear it when
            // Companion stops, or when Companion, after it sent real data, has
            // no state and no title (idle on Home). Not when MRP names a new
            // app after that.
            f.clear_app = match comp.state {
                Some(companion::PlaybackState::Stopped) => self.app_from == Source::Companion,
                None => {
                    self.companion_seen
                        && comp.title.is_none()
                        && self.app_from == Source::Companion
                }
                Some(_) => false,
            };
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
/// `comp_known`: the Companion value is data from the device, see
/// [`Merger::companion_known`].
pub(crate) fn spawn(
    mut mrp: Option<watch::Receiver<NowPlaying>>,
    mut comp: watch::Receiver<companion::NowPlaying>,
    comp_known: bool,
) -> (watch::Receiver<NowPlaying>, MergeTask) {
    let mut merger = Merger::default();
    if comp_known && let Some(m) = &mrp {
        merger.companion_known(m.borrow().app.as_deref());
    }
    let (tx, rx) = watch::channel(NowPlaying::default());
    // A first value that is not empty counts as a change, as for MRP alone.
    update(&mut merger, &tx, mrp.as_mut(), &mut comp, true);
    let task = tokio::spawn(async move {
        loop {
            let mrp_changed = async {
                match &mut mrp {
                    Some(m) => m.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            // A new MRP value is copied, also when the Companion change comes
            // first (see `update`). Else a Companion change only sets the
            // merged fields in place.
            let base_changed = tokio::select! {
                ok = mrp_changed => {
                    if !ok {
                        mrp = None;
                        merger.mrp_closed();
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
            update(&mut merger, &tx, mrp.as_mut(), &mut comp, base_changed);
        }
        // Companion closed: MRP data only, also the current value.
        if let Some(m) = &mut mrp {
            let np = m.borrow_and_update().clone();
            publish(&tx, np);
        }
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
    mrp: Option<&mut watch::Receiver<NowPlaying>>,
    comp: &mut watch::Receiver<companion::NowPlaying>,
    base_changed: bool,
) {
    let now = cocoa_time(SystemTime::now());
    // Marked as seen here, so a value is not handled again in the next turn.
    let comp = comp.borrow_and_update();
    let mrp = mrp.map(watch::Receiver::borrow_and_update);
    // A new MRP value is copied, also when the Companion change came first,
    // so the fields are not set on the old item.
    let base_changed = base_changed || mrp.as_ref().is_some_and(watch::Ref::has_changed);
    let base = mrp
        .as_deref()
        .map_or_else(|| Cow::Owned(from_companion(&comp)), Cow::Borrowed);
    let fields = merger.fields(&base, mrp.is_some(), &comp, now);
    if base_changed {
        let np = match base {
            Cow::Borrowed(b) => {
                let mut np = b.clone();
                fields.apply(b, &mut np);
                np
            }
            // Companion data without MRP: no second copy.
            Cow::Owned(mut np) => {
                fields.apply_to_base(&mut np);
                np
            }
        };
        publish(tx, np);
    } else {
        tx.send_if_modified(|np| fields.apply(&base, np));
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

    /// Waits for a change, so a missing change fails the test and does not
    /// hang it.
    async fn changed(rx: &mut watch::Receiver<NowPlaying>) -> Result<(), watch::error::RecvError> {
        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("no change")
    }

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
    fn companion_only_playing_gets_rate_one() {
        let mut m = Merger::default();
        let np = merge(&mut m, None, &comp(Some("Nocturne"), C::Paused, 0.0, 5.0));
        assert_eq!(
            (np.state, np.rate),
            (Some(PlaybackState::Paused), Some(0.0))
        );
        // Companion sends the new state before the new rate.
        let np = merge(&mut m, None, &comp(Some("Nocturne"), C::Playing, 0.0, 5.0));
        assert_eq!(
            (np.state, np.rate),
            (Some(PlaybackState::Playing), Some(1.0))
        );
        assert_eq!(np.artwork_data.as_deref(), Some(&b"jpeg"[..]));
    }

    #[test]
    fn companion_only_after_mrp_closed() {
        let mut m = Merger::default();
        let paused = mrp("Nocturne", PlaybackState::Paused, 0.0, 5.0);
        merge(
            &mut m,
            Some(&paused),
            &comp(Some("Nocturne"), C::Paused, 0.0, 5.0),
        );
        // MRP is gone.
        let np = merge(&mut m, None, &comp(Some("Nocturne"), C::Playing, 0.0, 5.0));
        assert_eq!(
            (np.state, np.rate),
            (Some(PlaybackState::Playing), Some(1.0))
        );
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
    fn held_position_stays_finite() {
        let mut m = Merger::default();
        let mut playing = mrp("Nocturne", PlaybackState::Playing, 2.0, 10.0);
        playing.elapsed_timestamp = Some(-1.7e308);
        let mut np = comp(Some("Nocturne"), C::Playing, 2.0, 11.0);
        np.elapsed_timestamp = Some(-1.7e308);
        let first = merge(&mut m, Some(&playing), &np);
        // The held position would be infinite.
        np.rate = Some(1.0);
        let slower = merge(&mut m, Some(&playing), &np);
        assert_eq!(slower.rate, Some(1.0));
        assert_eq!(
            (slower.elapsed, slower.elapsed_timestamp),
            (first.elapsed, first.elapsed_timestamp)
        );
    }

    #[test]
    fn held_position_is_limited() {
        let mut m = Merger::default();
        let mut playing = mrp("Nocturne", PlaybackState::Playing, -2.0, 3.0);
        playing.duration = Some(200.0);
        let mut np = comp(Some("Nocturne"), C::Playing, -2.0, 3.0);
        merge(&mut m, Some(&playing), &np);
        // At -2 for 2 s from 3 s: below 0.
        np.rate = Some(1.0);
        assert_eq!(merge(&mut m, Some(&playing), &np).elapsed, Some(0.0));

        // At 2 for 2 s from 199 s: past the end.
        let mut m = Merger::default();
        let mut playing = mrp("Nocturne", PlaybackState::Playing, 2.0, 199.0);
        playing.duration = Some(200.0);
        let mut np = comp(Some("Nocturne"), C::Playing, 2.0, 199.0);
        merge(&mut m, Some(&playing), &np);
        np.rate = Some(1.0);
        assert_eq!(merge(&mut m, Some(&playing), &np).elapsed, Some(200.0));
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
    fn idle_home_clears_the_app() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.0, 10.0),
        );
        // Home with nothing loaded: Companion sends no state and no title,
        // MRP keeps the app with no state and no title.
        let idle = companion::NowPlaying::default();
        merge(&mut m, Some(&playing), &idle);
        let mut home = NowPlaying::default();
        home.app = playing.app.clone();
        let np = merge(&mut m, Some(&home), &idle);
        assert_eq!((np.app, np.title, np.state), (None, None, None));
        // Also when MRP changes first.
        let mut m = Merger::default();
        merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.0, 10.0),
        );
        merge(
            &mut m,
            Some(&home),
            &comp(Some("Nocturne"), C::Playing, 1.0, 10.0),
        );
        assert_eq!(merge(&mut m, Some(&home), &idle).app, None);
        // A new app after that is shown.
        let mut other = NowPlaying::default();
        other.app = Some("com.apple.TVMusic".into());
        assert_eq!(merge(&mut m, Some(&other), &idle), other);
        assert_eq!(merge(&mut m, Some(&other), &idle), other);
    }

    #[test]
    fn mrp_state_after_stop_keeps_the_app_cleared() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.0, 10.0),
        );
        let stopped = comp(None, C::Stopped, 0.0, 0.0);
        let mut home = NowPlaying::default();
        home.app = playing.app.clone();
        home.state = Some(PlaybackState::Paused);
        assert_eq!(merge(&mut m, Some(&home), &stopped).app, None);
        // Only the MRP state changes, the app is the same.
        home.state = None;
        assert_eq!(merge(&mut m, Some(&home), &stopped).app, None);
    }

    #[test]
    fn known_empty_companion_at_start_clears_the_app() {
        let mut m = Merger::default();
        let mut home = NowPlaying::default();
        home.app = Some("com.apple.TVWatchList".into());
        m.companion_known(home.app.as_deref());
        let idle = companion::NowPlaying::default();
        assert_eq!(merge(&mut m, Some(&home), &idle).app, None);
        // A new app after that is shown.
        let mut other = NowPlaying::default();
        other.app = Some("com.apple.TVMusic".into());
        assert_eq!(merge(&mut m, Some(&other), &idle), other);
    }

    #[test]
    fn known_empty_companion_before_mrp_app_clears_the_app() {
        let mut m = Merger::default();
        m.companion_known(None);
        let idle = companion::NowPlaying::default();
        assert_eq!(merge(&mut m, Some(&NowPlaying::default()), &idle).app, None);
        // The first MRP app comes after the start.
        let mut home = NowPlaying::default();
        home.app = Some("com.apple.TVWatchList".into());
        assert_eq!(merge(&mut m, Some(&home), &idle).app, None);
        // A new app after that is shown.
        let mut other = NowPlaying::default();
        other.app = Some("com.apple.TVMusic".into());
        assert_eq!(merge(&mut m, Some(&other), &idle), other);
    }

    #[test]
    fn mrp_closed_uses_companion_position() {
        let mut m = Merger::default();
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        let other = comp(Some("Other"), C::Paused, 0.0, 50.0);
        merge(&mut m, Some(&playing), &other);
        m.mrp_closed();
        let np = merge(&mut m, None, &other);
        assert_eq!(np.title.as_deref(), Some("Other"));
        assert_eq!(np.state, Some(PlaybackState::Paused));
        assert_eq!((np.elapsed, np.rate), (Some(50.0), Some(0.0)));
    }

    #[test]
    fn mrp_closed_uses_companion_state() {
        let mut m = Merger::default();
        let playing = mrp("Other", PlaybackState::Playing, 1.0, 10.0);
        // Companion sends the state before the rate. MRP changes last.
        let early = comp(Some("Other"), C::Playing, 0.0, 10.0);
        merge(&mut m, Some(&playing), &early);
        m.mrp_closed();
        assert_eq!(merge(&mut m, None, &early).rate, Some(1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn late_first_mrp_app_is_shown() {
        let mut m = Merger::default();
        m.companion_known(None);
        let idle = companion::NowPlaying::default();
        assert_eq!(merge(&mut m, Some(&NowPlaying::default()), &idle).app, None);
        // The first MRP app comes at the end of the window, with a state but
        // no title.
        let mut loading = NowPlaying::default();
        loading.app = Some("com.apple.TVMusic".into());
        loading.state = Some(PlaybackState::Paused);
        tokio::time::advance(FIRST_MRP_APP_WINDOW).await;
        assert_eq!(merge(&mut m, Some(&loading), &idle).app, loading.app);
    }

    #[test]
    fn first_mrp_app_with_title_is_shown() {
        let mut m = Merger::default();
        m.companion_known(None);
        let idle = companion::NowPlaying::default();
        let mut playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        playing.app = Some("com.apple.TVMusic".into());
        assert_eq!(merge(&mut m, Some(&playing), &idle), playing);
    }

    #[test]
    fn companion_empty_at_start_keeps_the_app() {
        let mut m = Merger::default();
        let mut home = NowPlaying::default();
        home.app = Some("com.apple.TVWatchList".into());
        let idle = companion::NowPlaying::default();
        assert_eq!(merge(&mut m, Some(&home), &idle), home);
        assert_eq!(merge(&mut m, Some(&home), &idle), home);
    }

    #[test]
    fn track_change_with_empty_title() {
        let mut m = Merger::default();
        let mut old = mrp("Song 1", PlaybackState::Playing, 1.0, 10.0);
        old.app = Some("com.apple.TVMusic".into());
        merge(
            &mut m,
            Some(&old),
            &comp(Some("Song 1"), C::Playing, 1.0, 10.0),
        );
        // MRP has the new title first. Companion then sends an empty title
        // with Stopped, then the new title with Playing and rate 0.
        let mut new = mrp("Song 2", PlaybackState::Playing, 1.0, 0.0);
        new.app.clone_from(&old.app);
        assert_eq!(
            merge(
                &mut m,
                Some(&new),
                &comp(Some("Song 1"), C::Playing, 1.0, 10.0)
            ),
            new
        );
        let np = merge(&mut m, Some(&new), &comp(Some(""), C::Stopped, 0.0, 0.0));
        assert_eq!(np, new);
        let np = merge(
            &mut m,
            Some(&new),
            &comp(Some("Song 2"), C::Playing, 0.0, 0.0),
        );
        assert_eq!(np.app, new.app);
        assert_eq!(
            (np.state, np.rate),
            (Some(PlaybackState::Playing), Some(1.0))
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
        // A normal speed other than 1.0 is playing.
        let faster = merge(
            &mut m,
            Some(&playing),
            &comp(Some("Nocturne"), C::Playing, 1.5, 10.0),
        );
        assert_eq!(faster.state, Some(PlaybackState::Playing));
        // The app lists 2.0 as a speed.
        let mut listed = playing.clone();
        listed.supported_rates = vec![1.0, 2.0];
        let fast = merge(
            &mut m,
            Some(&listed),
            &comp(Some("Nocturne"), C::Playing, 2.0, 10.0),
        );
        assert_eq!(fast.state, Some(PlaybackState::Playing));
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
    async fn first_value_is_a_change() {
        // Empty at the start: the first real state is a change.
        let (mrp_tx, mrp_rx) = watch::channel(NowPlaying::default());
        let (_comp_tx, comp_rx) = watch::channel(companion::NowPlaying::default());
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx, false);
        assert!(!rx.has_changed().unwrap());
        mrp_tx.send_replace(mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0));
        changed(&mut rx).await.unwrap();
        assert_eq!(rx.borrow().title.as_deref(), Some("Nocturne"));
        drop(task);

        // MRP data at the start: a change, as for the MRP receiver.
        let (_mrp_tx, mrp_rx) = watch::channel(mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0));
        let (_comp_tx, comp_rx) = watch::channel(companion::NowPlaying::default());
        let (rx, task) = spawn(Some(mrp_rx), comp_rx, false);
        assert!(rx.has_changed().unwrap());
        drop(task);
    }

    #[tokio::test]
    async fn idle_home_at_connect_clears_the_app() {
        // MRP connect returns before the device sends its app.
        let (mrp_tx, mrp_rx) = watch::channel(NowPlaying::default());
        let (_comp_tx, comp_rx) = watch::channel(companion::NowPlaying::default());
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx, true);
        let mut home = NowPlaying::default();
        home.app = Some("com.apple.TVWatchList".into());
        mrp_tx.send_replace(home);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!rx.has_changed().unwrap());
        let mut playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0);
        playing.app = Some("com.apple.TVMusic".into());
        mrp_tx.send_replace(playing.clone());
        changed(&mut rx).await.unwrap();
        assert_eq!(*rx.borrow(), playing);
        drop(task);
    }

    #[tokio::test]
    async fn companion_close_sends_mrp_value() {
        let mut home = NowPlaying::default();
        home.app = Some("com.apple.TVWatchList".into());
        let (_mrp_tx, mrp_rx) = watch::channel(home.clone());
        let (comp_tx, comp_rx) = watch::channel(companion::NowPlaying::default());
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx, true);
        assert_eq!(rx.borrow_and_update().app, None);
        // Companion closes: the MRP value is sent without a new MRP change.
        drop(comp_tx);
        changed(&mut rx).await.unwrap();
        assert_eq!(*rx.borrow(), home);
        drop(task);
    }

    #[tokio::test]
    async fn task_sends_changes() {
        let (mrp_tx, mrp_rx) = watch::channel(mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0));
        let (comp_tx, comp_rx) = watch::channel(companion::NowPlaying::default());
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx, false);
        assert_eq!(rx.borrow_and_update().state, Some(PlaybackState::Playing));
        comp_tx.send_replace(comp(Some("Nocturne"), C::Paused, 0.0, 2.0));
        changed(&mut rx).await.unwrap();
        assert_eq!(rx.borrow_and_update().state, Some(PlaybackState::Paused));
        assert_eq!(rx.borrow().artist.as_deref(), Some("Artist"));
        assert!(rx.borrow().artwork_data.is_some());
        // Companion moves to another item: the MRP values come back.
        comp_tx.send_replace(comp(Some("Other"), C::Paused, 0.0, 2.0));
        changed(&mut rx).await.unwrap();
        let mut np = rx.borrow_and_update().clone();
        // The position is held at the pause, then moves on.
        assert_eq!(np.elapsed, Some(2.0));
        (np.elapsed, np.elapsed_timestamp) = (Some(1.0), Some(100.0));
        assert_eq!(np, mrp("Nocturne", PlaybackState::Playing, 1.0, 1.0));
        // Companion closes: MRP data only.
        drop(comp_tx);
        mrp_tx.send_replace(mrp("Song", PlaybackState::Playing, 1.0, 3.0));
        changed(&mut rx).await.unwrap();
        assert_eq!(rx.borrow().title.as_deref(), Some("Song"));
        drop(task);
    }

    #[tokio::test]
    async fn task_after_mrp_closed() {
        let (mrp_tx, mrp_rx) = watch::channel(mrp("Nocturne", PlaybackState::Paused, 0.0, 5.0));
        let (comp_tx, comp_rx) = watch::channel(comp(Some("Nocturne"), C::Paused, 0.0, 5.0));
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx, true);
        rx.borrow_and_update();
        // MRP closes: Companion data only.
        drop(mrp_tx);
        comp_tx.send_replace(comp(Some("Nocturne"), C::Playing, 1.0, 6.0));
        loop {
            changed(&mut rx).await.unwrap();
            if rx.borrow_and_update().state == Some(PlaybackState::Playing) {
                break;
            }
        }
        assert_eq!(rx.borrow().artist, None);
        // Then Companion closes: the receiver closes.
        drop(comp_tx);
        assert!(changed(&mut rx).await.is_err());
        drop(task);
    }

    #[tokio::test]
    async fn task_clears_and_shows_the_app() {
        let playing = mrp("Nocturne", PlaybackState::Playing, 1.0, 10.0);
        let (mrp_tx, mrp_rx) = watch::channel(playing);
        let (comp_tx, comp_rx) = watch::channel(comp(Some("Nocturne"), C::Playing, 1.0, 10.0));
        let (mut rx, task) = spawn(Some(mrp_rx), comp_rx, true);
        rx.borrow_and_update();
        comp_tx.send_replace(comp(Some("Nocturne"), C::Paused, 0.0, 10.0));
        changed(&mut rx).await.unwrap();
        let mut other_app = NowPlaying::default();
        other_app.app = Some("com.teliasonera.playplus.telia".into());
        mrp_tx.send_replace(other_app.clone());
        changed(&mut rx).await.unwrap();
        assert_eq!(*rx.borrow_and_update(), other_app);
        // Companion stops: the app is cleared in place.
        comp_tx.send_replace(comp(Some("Nocturne"), C::Stopped, 0.0, 10.0));
        changed(&mut rx).await.unwrap();
        assert_eq!(rx.borrow_and_update().app, None);
        // Companion plays again: the app is shown again.
        comp_tx.send_replace(comp(Some("Other"), C::Playing, 1.0, 0.0));
        changed(&mut rx).await.unwrap();
        assert_eq!(rx.borrow().app, other_app.app);
        drop(task);
    }
}
