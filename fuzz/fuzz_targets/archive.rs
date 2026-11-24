//! Keyed archives in Companion events: now playing and Siri status.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_codec::opack::Encoder;
use ratv_companion::message::{Event, Message};
use ratv_companion::now_playing::NowPlaying;

fn event(id: &str, key: &str, data: &[u8], list: bool) -> Vec<u8> {
    let mut out = Vec::new();
    Encoder::new(&mut out).dict(3, |e| {
        e.str("_i").str(id);
        // Event.
        e.str("_t").int(1);
        e.str("_c").dict(1, |e| {
            e.str(key);
            if list {
                e.array(1, |e| {
                    e.data(data);
                });
            } else {
                e.data(data);
            }
        });
    });
    out
}

fuzz_target!(|data: &[u8]| {
    let raw = event("NowPlayingInfo", "NowPlayingInfoKey", data, false);
    if let Ok(msg) = Message::decode(&raw) {
        let mut np = NowPlaying::default();
        let first = np.apply(&msg);
        // An event that cannot be read changes nothing.
        if first.is_err() {
            assert_eq!(np, NowPlaying::default());
        }
        // A position that is not finite is `None`, also for extreme times.
        // Also not below 0.
        for now in [0.0, 1e308, -1e308, f64::NAN] {
            assert!(np.position(now).is_none_or(|p| p.is_finite() && p >= 0.0));
        }
        // Numbers that are not finite are dropped, so the same event again
        // is no change.
        assert!(np.rate.is_none_or(f32::is_finite));
        for v in [np.duration, np.elapsed, np.elapsed_timestamp] {
            assert!(v.is_none_or(f64::is_finite));
        }
        assert_eq!(np.apply(&msg), first.map(|_| false));
    }
    let raw = event("SiriStatus", "deviceCapabilitiesV2", data, true);
    if let Ok(msg) = Message::decode(&raw) {
        let _ = Event::parse(&msg);
    }
});
