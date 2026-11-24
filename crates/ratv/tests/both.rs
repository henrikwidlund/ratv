//! End-to-end tests of `AppleTv` against a fake MRP device and a fake
//! Companion device at the same time.

// Each fake is in its own module, so equal names do not clash. The tests are
// in a child module, which can use the private items of both.
#[allow(dead_code, unused_imports)]
mod mrp_device {
    include!("support/mrp_device.rs");

    mod companion_device {
        include!("support/companion_device.rs");

        mod tests {
            use std::sync::{Arc, Mutex};

            use ed25519_dalek::SigningKey;
            use ratv::mrp::Command;
            use ratv::{AppleTv, Endpoints};
            use ratv_hap::accessory::PairedController;

            use super::super as mrp;
            use super::{self as companion, listen};

            /// MRP and Companion devices that accept the same credentials.
            async fn connect(
                configure: impl FnOnce(&mut mrp::Log),
            ) -> (Arc<Mutex<mrp::Log>>, Arc<Mutex<companion::Log>>, AppleTv) {
                let device = mrp::start_device().await;
                configure(&mut device.log.lock().unwrap());
                let controller = PairedController {
                    id: mrp::CLIENT_ID.to_vec(),
                    ltpk: SigningKey::from_bytes(&mrp::CLIENT_SEED)
                        .verifying_key()
                        .to_bytes(),
                    metadata: None,
                };
                let comp_log = Arc::new(Mutex::new(companion::Log::default()));
                let comp_addr = listen(controller, comp_log.clone()).await;
                let endpoints = Endpoints {
                    companion: Some(comp_addr),
                    airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
                };
                let tv = AppleTv::connect(&endpoints, &mrp::credentials(), &mrp::options())
                    .await
                    .unwrap();
                (device.log, comp_log, tv)
            }

            /// The command list comes with the 404 answer to `BeginRewind`. It
            /// lists `SkipBackward` but not `BeginRewind`, so the rewind skips
            /// back over MRP instead of using Companion.
            #[tokio::test]
            async fn rewind_skips_when_commands_come_late() {
                let (mrp_log, comp_log, tv) = connect(|l| l.late_commands = true).await;
                tv.begin_rewind().await.unwrap();
                // The skip was done, so the end sends nothing.
                tv.end_rewind().await.unwrap();
                tv.close().await.unwrap();
                assert_eq!(
                    mrp_log.lock().unwrap().commands,
                    [Command::BeginRewind as i32, Command::SkipBackward as i32]
                );
                assert_eq!(comp_log.lock().unwrap().media, []);
            }

            /// Without a command list, commands that MRP refuses with 404 go
            /// to Companion.
            #[tokio::test]
            async fn refused_commands_use_companion() {
                let refused = [
                    Command::Play,
                    Command::Pause,
                    Command::TogglePlayPause,
                    Command::BeginFastForward,
                    Command::EndFastForward,
                ];
                let (mrp_log, comp_log, tv) = connect(|l| {
                    // No command list. It would come only with commands that
                    // are not sent here.
                    l.late_commands = true;
                    l.not_supported = refused.map(|c| c as i32).to_vec();
                })
                .await;
                tv.play().await.unwrap();
                tv.pause().await.unwrap();
                tv.play_pause().await.unwrap();
                tv.begin_fast_forward().await.unwrap();
                tv.end_fast_forward().await.unwrap();
                tv.close().await.unwrap();
                assert_eq!(mrp_log.lock().unwrap().commands, refused.map(|c| c as i32));
                let comp_log = comp_log.lock().unwrap();
                // Play, Pause, FastForwardBegin, FastForwardEnd.
                assert_eq!(comp_log.media, [1, 2, 8, 9]);
                // The play/pause key: down and up.
                assert_eq!(comp_log.hid, [(14, 1), (14, 2)]);
            }
        }
    }
}
