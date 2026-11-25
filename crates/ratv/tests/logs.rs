//! Checks that logs are in the `session` span of their connection, pairing or
//! `AppleTv` connect.
//!
//! One test in its own binary: no other test thread can register a log
//! callsite while the capture starts, which could turn it off.

// Each fake is in its own module, so equal names do not clash. The test is
// in a child module, which can use the private items of both.
#[allow(dead_code, unused_imports)]
mod mrp_device {
    include!("support/mrp_device.rs");

    mod companion_device {
        include!("support/companion_device.rs");

        mod tests {
            use std::sync::{Arc, Mutex};

            use ed25519_dalek::SigningKey;
            use ratv::companion::{self, HidCommand, SiriOptions};
            use ratv::mrp::{self, Button};
            use ratv::{AppleTv, Endpoints};
            use ratv_hap::accessory::PairedController;
            use tokio::net::TcpListener;

            use super as companion_fake;
            use super::super as mrp_fake;

            include!("support/logs.rs");

            #[tokio::test]
            async fn logs_are_in_session_span() {
                companion_logs().await;
                mrp_logs().await;
                apple_tv_logs().await;
            }

            /// Pairing, connect, task, request and Siri logs are in a `session`
            /// span with the protocol, a new id and the address. The pairing
            /// span has no address, because the pairing uses a stream.
            async fn companion_logs() {
                let (logs, _guard) = LogBuffer::capture();
                let (creds, controller) = companion_fake::pair().await;
                let addr = companion_fake::listen(controller, Arc::default()).await;
                let client = companion::Client::connect(addr, &creds, &companion_fake::options())
                    .await
                    .unwrap();
                client.press(HidCommand::Select).await.unwrap();
                let siri_options = SiriOptions::default();
                let mut siri = client.start_siri(&siri_options).await.unwrap();
                siri.send(&[0; 4]).await.unwrap();
                siri.finish().await.unwrap();
                drop(client.start_siri(&siri_options).await.unwrap());
                client.close().await.unwrap();

                let pairing = logs.span_of("waiting for PIN");
                let pairing_id = session_id(&pairing);
                assert_eq!(
                    pairing,
                    format!("session{{protocol=\"companion pairing\" id={pairing_id}}}")
                );
                assert_eq!(logs.span_of("paired"), pairing);

                let session = logs.span_of("connecting");
                let id = session_id(&session);
                assert_eq!(
                    session,
                    format!("session{{protocol=\"companion\" id={id} addr={addr}}}")
                );
                assert_ne!(id, pairing_id);
                // Connect, a request and Siri from the caller, and the end of the task.
                for message in [
                    "pair-verify done",
                    "connected",
                    "request id=\"_hidC\"",
                    "Siri started",
                    "Siri first audio",
                    "Siri released",
                    "Siri released on drop",
                    "connection closed",
                ] {
                    assert_eq!(logs.span_of(message), session, "{message}");
                }
                logs.assert_all_in(&[&pairing, &session]);
            }

            /// Pairing, connect, task, request and Siri logs are in a `session`
            /// span with the protocol, a new id and the address.
            async fn mrp_logs() {
                let (logs, _guard) = LogBuffer::capture();
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let pair_addr = listener.local_addr().unwrap();
                tokio::spawn(mrp_fake::pair_device(listener));
                mrp::pair(pair_addr, &mrp::PairOptions::default())
                    .await
                    .unwrap()
                    .finish("1234")
                    .await
                    .unwrap();

                let device = mrp_fake::start_device().await;
                let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
                let client =
                    mrp::Client::connect(addr, &mrp_fake::credentials(), &mrp_fake::options())
                        .await
                        .unwrap();
                client.press(Button::Select).await.unwrap();
                let mut siri = client.start_siri().await.unwrap();
                siri.send(&b"one"[..]).await.unwrap();
                siri.finish().await.unwrap();
                drop(client.start_siri().await.unwrap());
                client.close().await.unwrap();

                let pairing = logs.span_of("waiting for PIN");
                let pairing_id = session_id(&pairing);
                assert_eq!(
                    pairing,
                    format!(
                        "session{{protocol=\"airplay pairing\" id={pairing_id} addr={pair_addr}}}"
                    )
                );
                assert_eq!(logs.span_of("paired"), pairing);

                let session = logs.span_of("connecting");
                let id = session_id(&session);
                assert_eq!(
                    session,
                    format!("session{{protocol=\"mrp\" id={id} addr={addr}}}")
                );
                assert_ne!(id, pairing_id);
                // Connect, a send and Siri from the caller, and the end of the task.
                for message in [
                    "pair-verify done",
                    "device info received",
                    "connected",
                    "send",
                    "Siri pressed",
                    "Siri recording",
                    "first voice packet",
                    "Siri released",
                    "Siri released on drop",
                    "connection closed",
                ] {
                    assert_eq!(logs.span_of(message), session, "{message}");
                }
                logs.assert_all_in(&[&pairing, &session]);
            }

            /// `AppleTv::connect` and `connect_missing` logs are in a `session`
            /// span with the protocol and a new id. The connection spans are
            /// inside it. A protocol that stays open keeps the first span.
            async fn apple_tv_logs() {
                let (logs, _guard) = LogBuffer::capture();
                let controller = PairedController {
                    id: mrp_fake::CLIENT_ID.to_vec(),
                    ltpk: SigningKey::from_bytes(&mrp_fake::CLIENT_SEED)
                        .verifying_key()
                        .to_bytes(),
                    metadata: None,
                };
                let comp_addr = companion_fake::listen(controller, Arc::default()).await;
                let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let closed_addr = closed.local_addr().unwrap();
                drop(closed);
                let credentials = mrp_fake::credentials();
                let options = mrp_fake::options();
                let first = Endpoints {
                    companion: Some(comp_addr),
                    airplay: Some(closed_addr),
                };
                let tv = AppleTv::connect(&first, &credentials, &options)
                    .await
                    .unwrap();
                let device = mrp_fake::start_device().await;
                let second = Endpoints {
                    companion: Some(comp_addr),
                    airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
                };
                let tv = tv
                    .connect_missing(&second, &credentials, &options)
                    .await
                    .unwrap()
                    .unwrap();
                tv.close().await.unwrap();

                let first_span = logs.span_of("one protocol failed to connect");
                let first_id = session_id(&first_span);
                assert_eq!(
                    first_span,
                    format!("session{{protocol=\"apple tv\" id={first_id}}}")
                );
                assert_eq!(logs.span_of("connection failed"), first_span);
                let second_span = logs.span_of("connected companion=true mrp=true");
                let second_id = session_id(&second_span);
                assert_eq!(
                    second_span,
                    format!("session{{protocol=\"apple tv\" id={second_id}}}")
                );
                assert_ne!(first_id, second_id);
                let text = logs.text();
                for inner in [
                    format!("{first_span}:session{{protocol=\"companion\" id="),
                    format!("{first_span}:session{{protocol=\"mrp\" id="),
                    format!("{second_span}:session{{protocol=\"mrp\" id="),
                ] {
                    assert!(text.contains(&inner), "{inner}");
                }
                // Companion stayed connected, so the second connect did not open it.
                assert!(!text.contains(&format!("{second_span}:session{{protocol=\"companion\"")));
                logs.assert_all_under(&[&first_span, &second_span]);
            }
        }
    }
}
