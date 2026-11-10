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
        if np.apply(&msg) {
            let _ = np.position(0.0);
        }
        let _ = np.apply(&msg);
    }
    let raw = event("SiriStatus", "deviceCapabilitiesV2", data, true);
    if let Ok(msg) = Message::decode(&raw) {
        let _ = Event::parse(&msg);
    }
});
