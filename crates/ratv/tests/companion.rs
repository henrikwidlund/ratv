//! End-to-end tests against a fake Apple TV over an in-memory stream.

include!("support/companion_device.rs");

#[tokio::test]
async fn pairing_creates_credentials() {
    let (creds, controller) = pair().await;
    assert_eq!(creds.accessory_id(), DEVICE_ID);
    assert_eq!(creds.client_id(), controller.id);
    let meta = ratv_codec::opack::decode(controller.metadata.as_deref().unwrap()).unwrap();
    assert_eq!(meta.get("name").and_then(Value::as_str), Some("ratv"));
    // Credentials are the same after conversion to text and back.
    assert_eq!(
        creds
            .to_secret_string()
            .expose_secret()
            .parse::<Credentials>()
            .unwrap(),
        creds
    );
}

#[tokio::test]
async fn pairing_skips_other_frames() {
    let (client, device) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_pair_setup_with_noise(device, true));
    let prompt = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap();
    let creds = prompt.finish(PIN).await.unwrap();
    assert_eq!(creds.client_id(), device.await.unwrap().id);
}

#[tokio::test]
async fn invalid_pairing_reply_shows_frame() {
    let (client, mut device) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut conn = Connection::new();
        read_frame(&mut device, &mut conn).await.unwrap();
        send(&mut device, &mut conn, FrameType::PAIR_SETUP_NEXT, &[]).await;
        read_frame(&mut device, &mut conn).await;
    });
    let err = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "pair-setup M2: invalid pairing reply (unexpected end of input): frame type 4, 0 bytes, starts with "
    );
}

#[tokio::test]
async fn pairing_close_shows_skipped_frames() {
    let (client, mut device) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut conn = Connection::new();
        read_frame(&mut device, &mut conn).await.unwrap();
        send(&mut device, &mut conn, FrameType::NO_OP, &[]).await;
        send(&mut device, &mut conn, FrameType::U_OPACK, &[0xE0]).await;
    });
    let err = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "pair-setup M2: connection closed (skipped frames: type 1, 0 bytes; type 7, 1 bytes)"
    );
}

#[tokio::test(start_paused = true)]
async fn pairing_timeout_names_step() {
    let (client, mut device) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut conn = Connection::new();
        read_frame(&mut device, &mut conn).await.unwrap();
        read_frame(&mut device, &mut conn).await;
    });
    let err = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "pair-setup M2: timed out");
}

#[tokio::test]
async fn pairing_with_wrong_pin_fails() {
    let (client, device) = tokio::io::duplex(8192);
    tokio::spawn(device_pair_setup(device));
    let prompt = companion::pair_stream(client, &PairOptions::default())
        .await
        .unwrap();
    let err = prompt.finish("0000").await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Hap(ratv_hap::Error::Device(
                ratv_hap::DeviceError::Authentication
            ))
        ),
        "{err}"
    );
}

#[tokio::test]
async fn session_commands() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_session(device_io, controller, log.clone()));

    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    assert!(client.is_connected());
    assert_eq!(client.session_id() >> 32, 77);
    let mut events = client.events();

    client.press(HidCommand::Select).await.unwrap();
    client.press(HidCommand::Screensaver).await.unwrap();
    client
        .hold(HidCommand::Home, Duration::from_millis(1))
        .await
        .unwrap();
    client.turn_on().await.unwrap();
    client.turn_off().await.unwrap();
    assert!((client.volume().await.unwrap() - 0.25).abs() < f32::EPSILON);
    client.set_volume(1.5).await.unwrap();
    // Limited like `set_volume`, and -0.0 becomes 0.0.
    for (sent, expected) in [(-0.0, 0.0), (2.0, 1.0)] {
        client
            .media(companion::MediaControl::SetVolume(sent))
            .await
            .unwrap();
        settle(&client).await;
        let volume = log.lock().unwrap().volume.unwrap();
        assert_eq!(volume, expected);
        assert!(volume.is_sign_positive());
    }
    client.media(companion::MediaControl::Play).await.unwrap();
    let err = client
        .media(companion::MediaControl::SkipBy(f64::NAN))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument("seconds")), "{err}");
    let err = client
        .media(companion::MediaControl::SetVolume(f64::NAN))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument("volume")), "{err}");
    assert_eq!(client.power_state().await.unwrap(), AttentionState::Awake);
    let accounts = client.accounts().await.unwrap();
    assert_eq!(accounts[1].name, "Ben");
    client.switch_account(&accounts[1].id).await.unwrap();
    let apps = client.apps().await.unwrap();
    assert_eq!(apps[0].name, "Netflix");
    client.launch_app("com.netflix.Netflix").await.unwrap();
    client.open_url("https://example.com").await.unwrap();

    assert_eq!(client.text().await.unwrap().as_deref(), Some("hel"));
    client.append_text("lo").await.unwrap();
    assert_eq!(client.text().await.unwrap().as_deref(), Some("hello"));
    client.set_text("abc").await.unwrap();
    assert_eq!(client.text().await.unwrap().as_deref(), Some("abc"));
    client.clear_text().await.unwrap();
    assert_eq!(client.text().await.unwrap().as_deref(), Some(""));

    client
        .swipe((0.8, 0.5), (0.2, 0.5), Duration::from_millis(48))
        .await
        .unwrap();
    let start = Instant::now();
    client.tap(0.25, 0.75).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));

    match client.launch_app("missing").await.unwrap_err() {
        Error::Companion(ratv_companion::Error::Device { code, message, .. }) => {
            assert_eq!(code, -6727);
            assert_eq!(message, "App not found");
        }
        other => panic!("unexpected {other}"),
    }

    // Event sent by the device after `_interest` (may already be consumed before subscribe).
    if let Ok(Ok(Event::MediaControl(flags))) =
        tokio::time::timeout(Duration::from_millis(50), events.recv()).await
    {
        assert!(flags.contains(MediaControlFlags::VOLUME));
    }

    client.close().await.unwrap();
    device.await.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(&log.hid[..4], &[(6, 1), (6, 2), (11, 1), (11, 2)]);
    assert_eq!(log.hid.len(), 10);
    assert_eq!(log.volume, Some(1.0));
    assert_eq!(log.launched, ["com.netflix.Netflix", "https://example.com"]);
    assert_eq!(log.account.as_deref(), Some("acc-2"));
    // Press, three moves ending at the target, release there, on a 1000 x 1000
    // touch area. Then a tap.
    assert_eq!(
        log.touches,
        [
            (800, 500, 1),
            (600, 500, 3),
            (400, 500, 3),
            (200, 500, 3),
            (200, 500, 4),
            (250, 750, 1),
            (250, 750, 4)
        ]
    );
    assert!(
        !log.session_stopped,
        "device ended the session after launch"
    );
}

/// Message that the fake device never answers.
struct Silent;

impl ratv_companion::message::Outgoing for Silent {
    const ID: &str = "silent";
    fn content(&self, e: &mut Encoder<'_>) {
        empty(e);
    }
}

/// The fake device never answers `silent`, so the request times out.
/// After two such timeouts with nothing received, the connection closes.
#[tokio::test]
async fn request_timeout() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let mut opts = ClientOptions::default();
    opts.timeout = Duration::from_millis(100);
    opts.connect_timeout = Some(Duration::from_secs(2));
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    let err = client.request(&Silent).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    assert!(client.is_connected());
    let _ = client.request(&Silent).await;
    let reason = tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason.as_deref(), Some(Error::NoAnswer("requests"))),
        "{reason:?}"
    );
}

#[tokio::test]
async fn disconnect_fails_pending_requests() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    let device = tokio::spawn(device_session(device_io, controller, Arc::default()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    device.abort();
    let _ = device.await;
    let err = client.press(HidCommand::Up).await.unwrap_err();
    // `Closed` if the client saw the end before the press.
    assert!(matches!(err, Error::Disconnected | Error::Closed), "{err}");
    let reason = tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(reason.is_some());
    assert!(!client.is_connected());
}

#[tokio::test]
async fn clones_share_session() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let clone = client.clone();
    tokio::spawn(async move { clone.press(HidCommand::Up).await })
        .await
        .unwrap()
        .unwrap();
    client.close().await.unwrap();
    // Closed with `close`, so there is no error.
    assert!(client.closed().await.is_none());
    assert!(!client.is_connected());
    let err = client.press(HidCommand::Up).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    assert!(err.is_not_sent());
    client.close().await.unwrap();
}

/// Stream whose writes wait while it is blocked, or fail.
struct Gate {
    io: DuplexStream,
    blocked: Blocked,
}

impl tokio::io::AsyncRead for Gate {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Gate {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        {
            let mut blocked = self.blocked.lock().unwrap();
            if blocked.2 {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            if blocked.0 {
                blocked.1 = Some(cx.waker().clone());
                return std::task::Poll::Pending;
            }
        }
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Writes wait, waker of the waiting write, writes fail.
type Blocked = Arc<Mutex<(bool, Option<std::task::Waker>, bool)>>;

/// Connects through a [`Gate`] with a timeout of 100 ms. Silent timeouts
/// never close the connection.
async fn connect_gated() -> (companion::Client, Arc<Mutex<Log>>, Blocked) {
    // With the default of 2, two silent timeouts would close the connection.
    connect_gated_with(0).await
}

/// Connects through a [`Gate`] with a timeout of 100 ms.
async fn connect_gated_with(
    max_silent_timeouts: u32,
) -> (companion::Client, Arc<Mutex<Log>>, Blocked) {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let blocked = Blocked::default();
    let gate = Gate {
        io: client_io,
        blocked: blocked.clone(),
    };
    let mut opts = options();
    opts.timeout = Duration::from_millis(100);
    opts.connect_timeout = Some(Duration::from_secs(2));
    opts.max_silent_timeouts = max_silent_timeouts;
    let client = companion::Client::connect_stream(gate, &creds, &opts)
        .await
        .unwrap();
    (client, log, blocked)
}

/// Blocks writes and waits until the background task waits in a write.
/// Messages queued after this stay in the outbox.
async fn block_writes(client: &companion::Client, blocked: &Blocked) {
    blocked.lock().unwrap().0 = true;
    client.send_event(&Silent).await.unwrap();
    while blocked.lock().unwrap().1.is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Makes the waiting write and all later writes fail.
fn fail_writes(blocked: &Blocked) {
    let waker = {
        let mut blocked = blocked.lock().unwrap();
        blocked.2 = true;
        blocked.1.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

fn unblock_writes(blocked: &Blocked) {
    let waker = {
        let mut blocked = blocked.lock().unwrap();
        blocked.0 = false;
        blocked.1.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[tokio::test]
async fn request_not_written_is_not_sent() {
    let (client, log, blocked) = connect_gated().await;
    blocked.lock().unwrap().0 = true;
    // The background task takes it and then waits in the write.
    let err = client
        .button(HidCommand::Select, ButtonState::Down)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NoAnswer("_hidC")), "{err}");
    assert!(!err.is_not_sent());
    // Stays in the outbox, so it is removed.
    let err = client
        .button(HidCommand::Menu, ButtonState::Down)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    assert!(err.is_not_sent());
    assert!(client.is_connected());

    assert!(blocked.lock().unwrap().1.is_some());
    unblock_writes(&blocked);
    client
        .button(HidCommand::Up, ButtonState::Down)
        .await
        .unwrap();
    let hid = log.lock().unwrap().hid.clone();
    assert_eq!(hid, [(6, 1), (1, 1)]);
}

/// A request that is still in the outbox when the connection closes was not
/// sent.
#[tokio::test]
async fn request_queued_at_close_is_not_sent() {
    let (client, _log, blocked) = connect_gated().await;
    block_writes(&client, &blocked).await;
    let request = client.request(&Silent);
    let fail = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        fail_writes(&blocked);
    };
    let (result, ()) = tokio::join!(request, fail);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    assert!(err.is_not_sent());
    assert!(!client.is_connected());
}

/// A button release that was not sent in time is sent by the drop guard.
#[tokio::test]
async fn hold_release_not_written_is_sent_later() {
    let (client, log, blocked) = connect_gated().await;
    let hold = client.hold(HidCommand::Select, Duration::from_millis(200));
    let block = async {
        while log.lock().unwrap().hid.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        block_writes(&client, &blocked).await;
    };
    let (result, ()) = tokio::join!(hold, block);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    assert_eq!(log.lock().unwrap().hid, [(6, 1), (6, 2)]);
}

/// A Siri release that was not sent in time is sent by the drop.
#[tokio::test]
async fn siri_release_not_written_is_sent_later() {
    let (client, log, blocked) = connect_gated().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    for size in 1..=3 {
        siri.send(&vec![0; size]).await.unwrap();
    }
    block_writes(&client, &blocked).await;
    let err = siri.finish().await.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_siriStop")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    assert_eq!(log.lock().unwrap().siri, ["start", "1+2+3", "stop"]);
}

/// Requests that were not sent in time do not count as silent timeouts, so
/// the release of a hold is still sent with the default limit.
#[tokio::test]
async fn removed_requests_do_not_close() {
    let (client, log, blocked) =
        connect_gated_with(ClientOptions::default().max_silent_timeouts).await;
    let hold = client.hold(HidCommand::Select, Duration::from_millis(300));
    let other = async {
        while log.lock().unwrap().hid.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        block_writes(&client, &blocked).await;
        let err = client.request(&Silent).await.unwrap_err();
        assert!(matches!(err, Error::SendTimeout("silent")), "{err}");
    };
    let (result, ()) = tokio::join!(hold, other);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    assert!(client.is_connected());
    assert_eq!(log.lock().unwrap().hid, [(6, 1), (6, 2)]);
}

/// A press or Siri start that was not sent in time sends no release.
#[tokio::test]
async fn press_not_written_sends_no_release() {
    let (client, log, blocked) = connect_gated().await;
    block_writes(&client, &blocked).await;
    let err = client.press(HidCommand::Select).await.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    let err = client
        .hold(HidCommand::Menu, Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    let options = companion::SiriOptions::default();
    let err = client.start_siri(&options).await.err().unwrap();
    assert!(matches!(err, Error::SendTimeout("_siriStart")), "{err}");
    unblock_writes(&blocked);
    settle(&client).await;
    let log = log.lock().unwrap();
    assert!(log.hid.is_empty(), "{:?}", log.hid);
    assert!(log.siri.is_empty(), "{:?}", log.siri);
}

/// Message with `len` bytes of data, which the fake device ignores.
struct Filler(usize);

impl ratv_companion::message::Outgoing for Filler {
    const ID: &str = "filler";
    fn content(&self, e: &mut Encoder<'_>) {
        e.dict(1, |e| {
            e.str("_d").data(&vec![0; self.0]);
        });
    }
}

/// Releases skip the outbox limit. A press that does not fit sends nothing.
#[tokio::test]
async fn release_queued_when_outbox_full() {
    let (client, log, blocked) = connect_gated().await;
    let hold = client.hold(HidCommand::Select, Duration::from_millis(50));
    tokio::pin!(hold);
    // Sends the press. The hold is not polled again here.
    while log.lock().unwrap().hid.is_empty() {
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut hold)
                .await
                .is_err()
        );
    }
    block_writes(&client, &blocked).await;
    let mut queued = 0;
    let err = loop {
        match client.send_event(&Filler(256 * 1024)).await {
            Ok(()) => queued += 1,
            Err(err) => break err,
        }
        assert!(queued < 8, "outbox not full");
    };
    assert!(matches!(err, Error::QueueFull), "{err}");
    let err = client.press(HidCommand::Menu).await.unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    // No lift without a touch.
    let err = client.tap(0.5, 0.5).await.unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    let err = client
        .swipe((0.2, 0.5), (0.8, 0.5), Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    // The release request is removed, then the guard queues it again.
    let err = hold.await.unwrap_err();
    assert!(matches!(err, Error::SendTimeout("_hidC")), "{err}");
    unblock_writes(&blocked);
    // Not `settle`: its request does not fit while the outbox is full.
    let start = Instant::now();
    while log.lock().unwrap().hid.len() < 2 {
        assert!(start.elapsed() < Duration::from_secs(5), "no release");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The press that did not fit would come before the release.
    assert_eq!(log.lock().unwrap().hid, [(6, 1), (6, 2)]);
    assert_eq!(log.lock().unwrap().touches, []);
}

/// `close` ends also when the stop request was not sent in time.
#[tokio::test]
async fn close_with_stop_not_written() {
    let (client, log, blocked) = connect_gated().await;
    block_writes(&client, &blocked).await;
    let close = tokio::spawn({
        let client = client.clone();
        async move { client.close().await }
    });
    // Unblocks after the stop request was removed, so the task then sees
    // `close` and an empty outbox.
    tokio::time::sleep(Duration::from_millis(500)).await;
    unblock_writes(&blocked);
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!client.is_connected());
    assert!(!log.lock().unwrap().session_stopped);
}

/// A close after a silent timeout, while the write waits, is a clean close.
#[tokio::test]
async fn close_while_stalled() {
    let (client, _log, blocked) = connect_gated_with(1).await;
    blocked.lock().unwrap().0 = true;
    // Taken by the task, so it counts as a silent timeout.
    let err = client.request(&Silent).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    let close = tokio::spawn({
        let client = client.clone();
        async move { client.close().await }
    });
    // The stop request (100 ms limit) must end before the write does.
    tokio::time::sleep(Duration::from_millis(500)).await;
    unblock_writes(&blocked);
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reason = client.closed().await;
    assert!(reason.is_none(), "{reason:?}");
}

/// A stop request without an answer is a clean close.
#[tokio::test]
async fn close_without_stop_reply() {
    let (client, log, _blocked) = connect_gated_with(1).await;
    log.lock().unwrap().no_stop_reply = true;
    tokio::time::timeout(Duration::from_secs(2), client.close())
        .await
        .unwrap()
        .unwrap();
    assert!(log.lock().unwrap().session_stopped);
    let reason = client.closed().await;
    assert!(reason.is_none(), "{reason:?}");
}

/// Event receivers close with the connection, also ones made after it.
#[tokio::test]
async fn events_close_with_connection() {
    let (client, _log) = connect().await;
    let mut before = client.events();
    client.close().await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(1), before.recv())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        err,
        tokio::sync::broadcast::error::RecvError::Closed
    ));
    let mut after = client.events();
    let after = tokio::time::timeout(Duration::from_secs(1), after.recv()).await;
    assert!(after.unwrap().is_err());
}

/// A close dropped while the stop request waits still closes.
#[tokio::test]
async fn close_dropped_still_closes() {
    let (client, log, _blocked) = connect_gated_with(1).await;
    log.lock().unwrap().no_stop_reply = true;
    let dropped = tokio::time::timeout(Duration::from_millis(20), client.close()).await;
    assert!(dropped.is_err());
    tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(!client.is_connected());
}

/// With `max_silent_timeouts` 1, the first timeout without data closes.
#[tokio::test]
async fn one_silent_timeout_closes() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let mut opts = options();
    opts.timeout = Duration::from_millis(100);
    opts.connect_timeout = Some(Duration::from_secs(2));
    opts.max_silent_timeouts = 1;
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    let err = client.request(&Silent).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    let reason = tokio::time::timeout(Duration::from_secs(1), client.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason.as_deref(), Some(Error::NoAnswer("requests"))),
        "{reason:?}"
    );
}

#[tokio::test]
async fn zero_silent_timeouts_keep_connection() {
    let (creds, controller) = pair().await;
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let mut opts = options();
    opts.timeout = Duration::from_millis(50);
    opts.connect_timeout = Some(Duration::from_secs(2));
    opts.max_silent_timeouts = 0;
    let client = companion::Client::connect_stream(client_io, &creds, &opts)
        .await
        .unwrap();
    for _ in 0..3 {
        let err = client.request(&Silent).await.unwrap_err();
        assert!(matches!(err, Error::NoAnswer("silent")), "{err}");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(client.is_connected());
    client.press(HidCommand::Up).await.unwrap();
}

#[tokio::test]
async fn verify_with_unknown_credentials_fails() {
    let (creds, controller) = pair().await;
    // Different client key: the device rejects the M3 signature.
    let creds = Credentials::new(
        *creds.accessory_ltpk(),
        [1u8; 32].into(),
        creds.accessory_id().to_vec(),
        creds.client_id().to_vec(),
    );
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let err = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Hap(ratv_hap::Error::Device(_))),
        "{err}"
    );
    assert!(err.is_auth(), "{err}");
    assert!(!err.is_other_device(), "{err}");
}

#[tokio::test]
async fn verify_with_other_device_fails() {
    let (creds, controller) = pair().await;
    let creds = Credentials::new(
        *creds.accessory_ltpk(),
        [1u8; 32].into(),
        b"58:55:CA:00:00:03".to_vec(),
        creds.client_id().to_vec(),
    );
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, Arc::default()));
    let err = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap_err();
    assert!(err.is_other_device(), "{err}");
    assert!(!err.is_auth(), "{err}");
}

#[tokio::test]
async fn connect_timeout_limits_pair_verify() {
    let (creds, _) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok(Ok((s, _))) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            open.push(s);
        }
        open.len()
    });
    let mut options = options();
    options.timeout = Duration::from_secs(30);
    options.connect_timeout = Some(Duration::from_millis(100));
    let start = Instant::now();
    let err = companion::Client::connect(addr, &creds, &options)
        .await
        .unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(err.to_string(), "pair-verify M2: timed out");
    assert_eq!(accepted.await.unwrap(), 1);
}

#[tokio::test]
async fn stalled_connect_starts_again() {
    let (creds, controller) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // The first connection gets no answer.
        let (_silent, _) = listener.accept().await.unwrap();
        let (mut tcp, _) = listener.accept().await.unwrap();
        let (mut client_io, device_io) = tokio::io::duplex(8192);
        tokio::spawn(device_session(device_io, controller, Arc::default()));
        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
    });
    let mut options = options();
    options.stall_timeout = Some(Duration::from_millis(200));
    let start = Instant::now();
    companion::Client::connect(addr, &creds, &options)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test]
async fn stalled_connect_starts_again_only_once() {
    let (creds, _) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok(Ok((s, _))) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            open.push(s);
        }
        open.len()
    });
    let mut options = options();
    options.timeout = Duration::from_millis(300);
    options.stall_timeout = Some(Duration::from_millis(100));
    let err = companion::Client::connect(addr, &creds, &options)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "pair-verify M2: timed out");
    assert_eq!(accepted.await.unwrap(), 2);
}

#[tokio::test]
async fn hold_released_when_canceled() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let hold = client.hold(HidCommand::Select, Duration::from_secs(10));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), hold)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let hid = log.lock().unwrap().hid.clone();
    // Down, then up from the dropped future.
    assert_eq!(hid.len(), 2, "{hid:?}");
    assert_eq!((hid[0].0, hid[0].1, hid[1].1), (hid[1].0, 1, 2));
}

#[tokio::test]
async fn double_tap_sends_two_clicks() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    client.double_tap(HidCommand::Home).await.unwrap();
    let hid = log.lock().unwrap().hid.clone();
    assert_eq!(hid, [(7, 1), (7, 2), (7, 1), (7, 2)]);
}

#[tokio::test]
async fn too_large_message_fails_and_connection_stays() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let url = "a".repeat(ratv_companion::MAX_MESSAGE_LEN);
    match client.open_url(&url).await.unwrap_err() {
        Error::Companion(ratv_companion::Error::FrameTooLarge(len)) => {
            assert!(len > ratv_companion::MAX_MESSAGE_LEN);
        }
        other => panic!("unexpected {other}"),
    }
    // Text that is too large: the old text is not cleared first.
    match client.set_text(&url).await.unwrap_err() {
        Error::Companion(ratv_companion::Error::FrameTooLarge(len)) => {
            assert!(len > ratv_companion::MAX_MESSAGE_LEN);
        }
        other => panic!("unexpected {other}"),
    }
    assert_eq!(client.text().await.unwrap().as_deref(), Some("hel"));
    client.double_tap(HidCommand::Home).await.unwrap();
    assert_eq!(log.lock().unwrap().hid.len(), 4);
}

#[tokio::test]
async fn swipe_lifted_when_canceled() {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    let swipe = client.swipe((0.5, 0.5), (0.5, 0.1), Duration::from_secs(10));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), swipe)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let touches = log.lock().unwrap().touches.clone();
    let phases: Vec<_> = touches.iter().map(|t| t.2).collect();
    // Press (1), moves (3), then release (4) from the dropped future.
    assert_eq!(phases.first(), Some(&1), "{touches:?}");
    assert_eq!(phases.last(), Some(&4), "{touches:?}");
    // Lifted at the last point sent, not at the end of the swipe.
    let [.., last, up] = touches[..] else {
        panic!("{touches:?}");
    };
    assert_eq!((up.0, up.1), (last.0, last.1), "{touches:?}");
}

/// A swipe whose move does not fit in the outbox lifts the finger at the
/// last point sent.
#[tokio::test]
async fn swipe_lifted_when_move_not_queued() {
    let (client, log, blocked) = connect_gated().await;
    let swipe = client.swipe((0.5, 0.5), (0.5, 0.1), Duration::from_secs(10));
    tokio::pin!(swipe);
    while log.lock().unwrap().touches.is_empty() {
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut swipe)
                .await
                .is_err()
        );
    }
    block_writes(&client, &blocked).await;
    let mut queued = 0;
    while client.send_event(&Filler(256 * 1024)).await.is_ok() {
        queued += 1;
        assert!(queued < 8, "outbox not full");
    }
    let err = tokio::time::timeout(Duration::from_secs(2), swipe)
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    unblock_writes(&blocked);
    let start = Instant::now();
    while log.lock().unwrap().touches.last().map(|t| t.2) != Some(4) {
        assert!(start.elapsed() < Duration::from_secs(5), "no release");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let touches = log.lock().unwrap().touches.clone();
    let [.., last, up] = touches[..] else {
        panic!("{touches:?}");
    };
    assert_eq!((up.0, up.1), (last.0, last.1), "{touches:?}");
}

async fn connect() -> (companion::Client, Arc<Mutex<Log>>) {
    connect_with(Log::default()).await
}

async fn connect_with(log: Log) -> (companion::Client, Arc<Mutex<Log>>) {
    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(log));
    let (client_io, device_io) = tokio::io::duplex(8192);
    tokio::spawn(device_session(device_io, controller, log.clone()));
    let client = companion::Client::connect_stream(client_io, &creds, &options())
        .await
        .unwrap();
    (client, log)
}

/// Waits until the device has read all messages that were sent before.
async fn settle(client: &companion::Client) {
    client.volume().await.unwrap();
}

#[tokio::test]
async fn siri() {
    let (client, log) = connect().await;
    let options = companion::SiriOptions::default();
    let mut siri = client.start_siri(&options).await.unwrap();
    assert!(matches!(
        client.start_siri(&options).await,
        Err(Error::SiriActive)
    ));
    let start = Instant::now();
    for size in 1..=12 {
        siri.send(&vec![0; size]).await.unwrap();
    }
    siri.finish().await.unwrap();
    // Events of 100 ms, no faster than real time: the third is sent after 200 ms.
    assert!(start.elapsed() >= Duration::from_millis(200));
    // Dropping releases Siri too.
    drop(client.start_siri(&options).await.unwrap());
    settle(&client).await;

    let log = log.lock().unwrap();
    assert_eq!(
        log.siri,
        [
            "start",
            "1+2+3+4+5",
            "6+7+8+9+10",
            "11+12",
            "stop",
            "start",
            "stop"
        ]
    );
    assert_eq!(log.siri_ptt, [None; 4]);
}

#[tokio::test]
async fn state_read_when_connecting() {
    let (client, _) = connect().await;
    assert_eq!(client.siri_enabled(), Some(true));
    assert_eq!(client.guide_supported(), Some(true));
}

#[tokio::test]
async fn now_playing_read_when_connecting() {
    let (client, _) = connect().await;
    let np = client.now_playing().borrow().clone();
    assert_eq!(np.title.as_deref(), Some("Nocturne"));
    assert_eq!(np.state, Some(companion::PlaybackState::Playing));
    assert_eq!(np.artwork.as_deref(), Some(&b"jpeg"[..]));
}

/// An optional request that gets no answer does not start the connect again.
#[tokio::test]
async fn connect_without_optional_answer() {
    let (creds, controller) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Log {
        no_now_playing: true,
        ..Log::default()
    }));
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::Relaxed);
            let (mut client_io, device_io) = tokio::io::duplex(8192);
            tokio::spawn(device_session(device_io, controller.clone(), log.clone()));
            tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
            });
        }
    });
    let mut opts = options();
    opts.stall_timeout = Some(Duration::from_millis(300));
    let client = companion::Client::connect(addr, &creds, &opts)
        .await
        .unwrap();
    assert!(client.is_connected());
    assert_eq!(client.now_playing().borrow().title, None);
    assert_eq!(accepted.load(Ordering::Relaxed), 1);
}

/// After a now playing event that cannot be read, the first one that can be
/// read counts as a change, also when it is empty.
#[tokio::test]
async fn first_now_playing_event_is_a_change() {
    let (client, _log) = connect_with(Log {
        late_now_playing: true,
        ..Log::default()
    })
    .await;
    let mut rx = client.now_playing();
    rx.borrow_and_update();
    client.accounts().await.unwrap();
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), companion::NowPlaying::default());
}

/// A `Siri` kept over a close gives `Closed`, not `SiriActive`.
#[tokio::test]
async fn siri_kept_over_close() {
    let (client, _log) = connect().await;
    let kept = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    client.close().await.unwrap();
    let err = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Closed), "{err}");
    let mut kept = kept;
    let err = kept.send(&[1, 2, 3]).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
}

#[tokio::test]
async fn siri_off_fails_before_start() {
    let (client, log) = connect_with(Log {
        siri_off: true,
        ..Log::default()
    })
    .await;
    assert_eq!(client.siri_enabled(), Some(false));
    assert!(matches!(
        client.start_siri(&companion::SiriOptions::default()).await,
        Err(Error::SiriDisabled)
    ));
    settle(&client).await;
    assert_eq!(log.lock().unwrap().siri, Vec::<String>::new());
    // Not `SiriDisabled` from the last value, so `is_closed` is true.
    client.close().await.unwrap();
    let err = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Closed), "{err}");
}

#[tokio::test]
async fn siri_status_event_updates_state() {
    struct TurnSiriOff;
    impl ratv_companion::message::Outgoing for TurnSiriOff {
        const ID: &str = "turnSiriOff";
        fn content(&self, e: &mut Encoder<'_>) {
            empty(e);
        }
    }
    let (client, _) = connect().await;
    let mut events = client.events();
    client.request(&TurnSiriOff).await.unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        Event::SiriStatus { enabled: false }
    );
    assert_eq!(client.siri_enabled(), Some(false));
    assert!(matches!(
        client.start_siri(&companion::SiriOptions::default()).await,
        Err(Error::SiriDisabled)
    ));
}

#[tokio::test]
async fn siri_endpoint() {
    let (client, log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    for size in [99, 1, 1, 1, 1] {
        siri.send(&vec![0; size]).await.unwrap();
    }
    assert!(!siri.ended());
    // The device answers the first event with `_siriEndpoint`. At most 5 s.
    for _ in 0..250 {
        if siri.ended() {
            break;
        }
        siri.send(&[2]).await.unwrap();
    }
    assert!(siri.ended());
    siri.send(&[3]).await.unwrap();
    siri.finish().await.unwrap();
    settle(&client).await;
    assert_eq!(
        log.lock().unwrap().siri,
        ["start", "99+1+1+1+1", "endpoint reply", "stop"]
    );
}

/// Device volumes outside 0.0 to 1.0 are limited; a value too large for f32
/// is an error.
#[tokio::test]
async fn volume_limited() {
    let (client, log) = connect().await;
    for (device, expected) in [(1.5, 1.0), (-0.2, 0.0), (-0.0, 0.0)] {
        log.lock().unwrap().reply_volume = Some(device);
        let volume = client.volume().await.unwrap();
        assert_eq!(volume, expected);
        // Never -0.0.
        assert!(volume.is_sign_positive());
    }
    // Finite, but too large for f32.
    log.lock().unwrap().reply_volume = Some(1e300);
    let err = client.volume().await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Companion(ratv_companion::Error::InvalidField("_vol"))
        ),
        "{err}"
    );
}

/// After the device stopped recording, sending is not an error, also on a
/// closed connection.
#[tokio::test]
async fn siri_ended_then_closed() {
    let (client, _log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    for size in [99, 1, 1, 1, 1] {
        siri.send(&vec![0; size]).await.unwrap();
    }
    // The device answers the first event with `_siriEndpoint`. At most 5 s.
    for _ in 0..250 {
        if siri.ended() {
            break;
        }
        siri.send(&[2]).await.unwrap();
    }
    assert!(siri.ended());
    // The packet size is checked first.
    let err = siri.send(&vec![0; 65_536]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument("packet size")),
        "{err}"
    );
    client.close().await.unwrap();
    siri.send(&[3]).await.unwrap();
    let err = siri.send(&vec![0; 65_536]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument("packet size")),
        "{err}"
    );
}

#[tokio::test]
async fn siri_without_audio() {
    let (client, log) = connect().await;
    let siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    assert!(matches!(siri.finish().await, Err(Error::SiriNoAudio)));
    settle(&client).await;
    assert_eq!(log.lock().unwrap().siri, ["start", "stop"]);
}

#[tokio::test]
async fn siri_ptt_flags() {
    let (client, log) = connect().await;
    let mut options = companion::SiriOptions::default();
    options.ptt_flags = true;
    let mut siri = client.start_siri(&options).await.unwrap();
    siri.send(&[1, 2]).await.unwrap();
    siri.finish().await.unwrap();
    assert_eq!(log.lock().unwrap().siri_ptt, [Some(1); 2]);
}

#[tokio::test]
async fn siri_packet_too_large() {
    let (client, _log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    let err = siri.send(&vec![0; 70_000]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument("packet size")),
        "{err}"
    );
}

#[cfg(feature = "opus")]
#[tokio::test]
async fn siri_pcm() {
    let (client, log) = connect().await;
    let mut siri = client
        .start_siri(&companion::SiriOptions::default())
        .await
        .unwrap();
    // 100 + 600 samples: two packets now, the rest padded by `finish`.
    siri.push_pcm(&[100; 100]).await.unwrap();
    siri.push_pcm(&[-100; 600]).await.unwrap();
    siri.finish().await.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(log.siri.len(), 3);
    assert_eq!(log.siri[1].split('+').count(), 3);
}

#[cfg(feature = "mrp")]
#[tokio::test]
async fn apple_tv_siri_uses_companion() {
    use ratv::{AppleTv, Endpoints, Siri};

    let (creds, controller) = pair().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Log::default()));
    let device_log = log.clone();
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let (mut client_io, device_io) = tokio::io::duplex(8192);
        tokio::spawn(device_session(device_io, controller, device_log));
        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut client_io).await;
    });
    let endpoints = Endpoints {
        companion: Some(addr),
        airplay: None,
    };
    let tv = AppleTv::connect(&endpoints, &creds, &options())
        .await
        .unwrap();
    // Without MRP, now playing comes from Companion.
    let np = tv.now_playing().borrow().clone();
    assert_eq!(np.title.as_deref(), Some("Nocturne"));
    assert_eq!(np.state, Some(ratv::mrp::PlaybackState::Playing));
    assert_eq!(np.elapsed, Some(12.0));
    assert_eq!(np.artwork_data.as_deref(), Some(&b"jpeg"[..]));
    let mut siri = tv.start_siri().await.unwrap();
    assert!(matches!(siri, Siri::Companion(_)));
    assert!(!siri.ended());
    siri.send(vec![1, 2, 3]).await.unwrap();
    siri.finish().await.unwrap();
    assert_eq!(log.lock().unwrap().siri, ["start", "3", "stop"]);
    assert_eq!(tv.siri_enabled(), Some(true));
    assert_eq!(tv.guide_supported(), Some(true));
    // The argument is checked before the protocol.
    for (err, field) in [
        (tv.seek(f64::NAN).await.unwrap_err(), "position"),
        (tv.set_playback_rate(f32::NAN).await.unwrap_err(), "rate"),
        (tv.set_rating(f32::INFINITY).await.unwrap_err(), "rating"),
    ] {
        assert!(
            matches!(err, Error::InvalidArgument(f) if f == field),
            "{err}"
        );
    }
    // Not the last value of a closed connection.
    tv.close().await.unwrap();
    assert_eq!(tv.siri_enabled(), None);
    assert_eq!(tv.guide_supported(), None);
}

/// A closed Companion connects again, and the new handle has the merged
/// now playing again.
#[cfg(feature = "mrp")]
#[tokio::test]
async fn apple_tv_connects_missing_companion() {
    use ratv::{AppleTv, Endpoints, Protocol};

    let (creds, controller) = pair().await;
    let log = Arc::new(Mutex::new(Log::default()));
    let endpoints = Endpoints {
        companion: Some(listen(controller, log).await),
        airplay: None,
    };
    let tv = AppleTv::connect(&endpoints, &creds, &options())
        .await
        .unwrap();
    assert!(tv.is_open(Protocol::Companion));
    assert!(!tv.is_open(Protocol::Mrp));
    assert!(tv.connect_error(Protocol::Mrp).is_none());

    tv.companion().unwrap().close().await.unwrap();
    assert!(!tv.is_open(Protocol::Companion));
    let new = tv
        .connect_missing(&endpoints, &creds, &options())
        .await
        .unwrap()
        .unwrap();
    assert!(new.is_open(Protocol::Companion));
    assert!(!tv.is_open(Protocol::Companion));
    let np = new.now_playing().borrow().clone();
    assert_eq!(np.title.as_deref(), Some("Nocturne"));
    new.turn_on().await.unwrap();
    new.close().await.unwrap();
}
