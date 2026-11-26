//! MRP frames, envelopes and now-playing state.
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use ratv_mrp::proto::Command;
use ratv_mrp::{NowPlayingState, PlaybackState, VolumeState, artwork, frames};

fuzz_target!(|data: &[u8]| {
    let mut state = NowPlayingState::new();
    let mut volume = VolumeState::new();
    for env in frames(Bytes::copy_from_slice(data)) {
        let Ok(env) = env else {
            return;
        };
        let _ = artwork(&env);
        if volume.apply(&env).is_ok() {
            // Levels that are not finite are not taken, so the same message
            // again is no change. Other levels are limited to 0.0 to 1.0, and
            // -0.0 becomes 0.0.
            let v = volume.volume();
            let ok = |l: f32| (0.0..=1.0).contains(&l) && l.is_sign_positive();
            assert!(v.level.is_none_or(ok));
            assert!(v.outputs.iter().all(|o| o.volume.is_none_or(ok)));
            // Each UID is listed once.
            for (i, o) in v.outputs.iter().enumerate() {
                assert!(v.outputs[..i].iter().all(|p| p.uid != o.uid));
            }
            assert!(!volume.apply(&env).unwrap());
        }
        if state.apply(&env).is_ok() {
            let np = state.now_playing();
            // Numbers that are not finite are dropped, so the value is equal
            // to itself, and a publish of the same data is no change.
            assert_eq!(np, np.clone());
            // A position that is not finite is `None`, also for extreme times.
            // Also not below 0 or above a duration greater than 0.
            let max = np.duration.filter(|d| *d > 0.0).unwrap_or(f64::MAX);
            for now in [0.0, 1e308, -1e308, f64::NAN] {
                assert!(
                    np.position(now)
                        .is_none_or(|p| p.is_finite() && (0.0..=max).contains(&p))
                );
            }
            assert!(np.rate.is_none_or(f32::is_finite));
            // Current automatic subtitles are always in the list.
            let subtitles = &np.subtitle_languages;
            assert!(!np.subtitles_auto() || subtitles.iter().any(|o| o.is_auto()));
            assert!(
                np.chapters
                    .iter()
                    .all(|c| c.duration.is_none_or(|d| d >= 0.0))
            );
            // Normal speeds: sorted, above 0 and up to 2.0, with 1.0.
            let speeds = np.playback_speeds();
            assert!(speeds.is_sorted());
            assert!(speeds.iter().all(|r| *r > 0.0 && *r <= 2.0));
            assert!(speeds.is_empty() || speeds.contains(&1.0));
            assert!(speeds.iter().all(|r| np.is_playback_speed(*r)));
            let enabled = np.supports(Command::ChangePlaybackRate);
            assert!(speeds.iter().all(|r| np.accepts_rate(*r) == enabled));
            assert!(np.supported_rates.iter().all(|&r| {
                np.listed_rate(r)
                    .map_or(!enabled, |l| enabled && (l - r).abs() < 0.01)
            }));
            // The rate to send is a listed rate, or 1.0 with other normal speeds,
            // and within 0.01 of the asked rate.
            let near = np
                .supported_rates
                .iter()
                .flat_map(|&r| [r - 0.005, r, r + 0.005]);
            let fixed = [0.0, 0.993, 1.0, 1.007, 1.5, 8.0, -8.0];
            for x in fixed.into_iter().chain(np.rate).chain(near) {
                let listed = np.listed_rate(x);
                assert!(enabled || listed.is_none());
                assert!(listed.is_none_or(|l| (np.supported_rates.contains(&l)
                    || (l == 1.0 && !speeds.is_empty()))
                    && (l - x).abs() < 0.01));
            }
            // Paused or stopped: rate 0. Playing: a normal speed.
            match np.state {
                Some(PlaybackState::Paused | PlaybackState::Stopped) => {
                    assert!(np.rate.is_none_or(|r| r == 0.0));
                }
                Some(PlaybackState::Playing) => {
                    assert!(np.rate.is_none_or(|r| r != 0.0 && np.is_playback_speed(r)));
                }
                _ => {}
            }
            for v in [np.duration, np.elapsed, np.elapsed_timestamp] {
                assert!(v.is_none_or(f64::is_finite));
            }
            let _ = np.artwork_url_sized(100, 100);
            let _ = (
                np.playback_speeds(),
                np.rate.map(|r| np.is_playback_speed(r)),
            );
            let _ = state.player_without_item();
        }
    }
});
