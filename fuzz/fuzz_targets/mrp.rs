//! MRP frames, envelopes and now-playing state.
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use ratv_mrp::{NowPlayingState, artwork, frames};

fuzz_target!(|data: &[u8]| {
    let mut state = NowPlayingState::new();
    for env in frames(Bytes::copy_from_slice(data)) {
        let Ok(env) = env else {
            return;
        };
        let _ = artwork(&env);
        if state.apply(&env).is_ok() {
            let np = state.now_playing();
            let _ = (np.position(0.0), np.artwork_url_sized(100, 100));
        }
    }
});
