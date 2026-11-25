//! End-to-end tests against a fake AirPlay device on localhost.

include!("support/mrp_device.rs");

/// Numbers that are not finite, and rates the item does not list, are not
/// sent.
async fn invalid_numbers_not_sent(client: &Client) {
    // Rates are refused as not listed, not for a missing command list.
    assert!(
        client
            .now_playing()
            .borrow()
            .supports(Command::ChangePlaybackRate)
    );
    let invalid = [
        (client.seek(f64::NAN).await, "position"),
        (client.skip(f64::INFINITY).await, "seconds"),
        (client.skip(1e300).await, "seconds"),
        (client.set_playback_rate(f32::NAN).await, "rate"),
        // The fake device lists only scan rates.
        (client.set_playback_rate(1.5).await, "rate"),
        (client.set_playback_rate(1.0).await, "rate"),
        (client.set_rating(f32::NEG_INFINITY).await, "rating"),
    ];
    for (result, field) in invalid {
        let err = result.unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument(f) if f == field),
            "{err}"
        );
    }
}

/// A rate the item lists is sent.
#[tokio::test]
async fn listed_rate_sent() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut np = client.now_playing();
    tokio::time::timeout(
        Duration::from_secs(2),
        np.wait_for(|n| !n.supported_rates.is_empty()),
    )
    .await
    .unwrap()
    .unwrap();
    client.set_playback_rate(8.0).await.unwrap();
    client.close().await.unwrap();
    // The last state of a closed connection is not used.
    let err = client.set_playback_rate(8.0).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    let log = device.log.lock().unwrap();
    assert_eq!(log.commands, [Command::ChangePlaybackRate as i32]);
}

#[tokio::test]
async fn session() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    assert!(client.is_connected());
    assert_eq!(client.device_info().name, "Fake TV");

    let mut np = client.now_playing();
    tokio::time::timeout(Duration::from_secs(2), np.wait_for(|n| n.title.is_some()))
        .await
        .unwrap()
        .unwrap();
    {
        let n = np.borrow();
        assert_eq!(n.title.as_deref(), Some("Song"));
        assert_eq!(n.state, Some(PlaybackState::Playing));
        assert_eq!(n.app.as_deref(), Some("com.apple.TVMusic"));
        assert_eq!(n.app_name.as_deref(), Some("Music"));
    }

    let mut messages = client.messages();
    client.press(Button::Select).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client.seek(30.0).await.unwrap();
    invalid_numbers_not_sent(&client).await;
    match client.command(Command::Stop).await.unwrap_err() {
        Error::CommandFailed { send_error, .. } => assert_eq!(send_error, 1),
        other => panic!("unexpected {other}"),
    }
    match client.command(Command::NextTrack).await.unwrap_err() {
        Error::Mrp(ratv_mrp::Error::Device { code, .. }) => assert_eq!(code, 6),
        other => panic!("unexpected {other}"),
    }
    // Other messages, such as the reply to the details request, can come first.
    while !messages
        .recv()
        .await
        .unwrap()
        .is::<proto::SendCommandResultMessage>()
    {}
    let art = client.artwork(300, 0).await.unwrap().unwrap();
    assert_eq!(&art.data[..], b"\x89PNG");
    assert_eq!(art.mime_type.as_deref(), Some("image/png"));
    assert_eq!(art.width, Some(300));
    client.set_volume(0.5).await.unwrap();

    // Sent right after DEVICE_INFO, so it is known when connect returns.
    assert_eq!(client.volume().borrow().capabilities, Some(3));
    // Initial volume comes from the replies to the connect requests.
    let mut volume = client.volume();
    let wait = volume.wait_for(|v| v.level.is_some() && v.muted.is_some());
    let v = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap()
        .clone();
    assert_eq!((v.level, v.muted), (Some(0.3), Some(false)));
    client.set_muted(true).await.unwrap();
    let wait = volume.wait_for(|v| v.muted == Some(true));
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();

    // Output devices: add a speaker, then change its volume.
    client.add_outputs(&["HP-1"]).await.unwrap();
    let wait = volume.wait_for(|v| v.outputs.iter().any(|o| o.uid == "HP-1"));
    let outputs = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap()
        .outputs
        .clone();
    assert_eq!(outputs[0].name, "Speaker HP-1");
    client.set_output_volume("HP-1", 0.4).await.unwrap();
    let wait = volume.wait_for(|v| v.outputs[0].volume == Some(0.4));
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    // The connected device keeps its own volume.
    assert_eq!(volume.borrow().level, Some(0.3));
    client
        .hold(Button::Home, Duration::from_millis(1))
        .await
        .unwrap();

    // Wait for one feedback request (sent every 2 seconds).
    tokio::time::sleep(Duration::from_millis(2300)).await;
    client.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(&log.buttons[..2], &[(1, 0x89, true), (1, 0x89, false)]);
    assert_eq!(
        &log.buttons[2..],
        &[(0x0c, 0x40, true), (0x0c, 0x40, false)]
    );
    assert_eq!(
        log.commands,
        [
            Command::Pause as i32,
            Command::SeekToPlaybackPosition as i32,
            Command::Stop as i32,
            Command::NextTrack as i32
        ]
    );
    assert_eq!(log.volume, Some(0.5));
    assert_eq!(log.event_acks, 1);
    assert!(log.feedback >= 1);
    assert!(log.teardown);
}

/// A `Siri` kept over a close gives `Closed`, not `SiriActive`, so
/// `is_closed` is true.
#[tokio::test]
async fn siri_kept_over_close() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    // Recording never starts. `send` and `finish` must still give `Closed`
    // and not wait for recording.
    device.log.lock().unwrap().no_recording = true;
    let mut kept = client.start_siri().await.unwrap();
    client.close().await.unwrap();
    let err = client.start_siri().await.err().unwrap();
    assert!(matches!(err, Error::Closed), "{err}");
    // The packet size is checked first.
    let err = kept.send(vec![0; 65_536]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument("packet size")),
        "{err}"
    );
    // Only the error can be seen here, not the kept packets.
    let err = kept.send(&b"one"[..]).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    let err = kept.finish().await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");

    // Also after recording started.
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut siri = client.start_siri().await.unwrap();
    // Packets are sent once the device records.
    for _ in 0..250 {
        siri.send(&b"one"[..]).await.unwrap();
        if !device.log.lock().unwrap().voice.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(device.log.lock().unwrap().voice.len(), 0);
    client.close().await.unwrap();
    for _ in 0..2 {
        let err = siri.send(&b"two"[..]).await.unwrap_err();
        assert!(matches!(err, Error::Closed), "{err}");
    }
}

/// Packets over 65535 bytes are refused, as over Companion.
#[tokio::test]
async fn siri_packet_too_large() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut siri = client.start_siri().await.unwrap();
    let err = siri.send(vec![0; 65_536]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument("packet size")),
        "{err}"
    );
    siri.send(vec![0; 65_535]).await.unwrap();
}

#[tokio::test]
async fn siri() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();

    let mut siri = client.start_siri().await.unwrap();
    assert!(matches!(client.start_siri().await, Err(Error::SiriActive)));
    siri.send(&b"one"[..]).await.unwrap();
    siri.send(&b"three"[..]).await.unwrap();
    siri.finish().await.unwrap();
    // Dropping releases Siri too. The device is registered once.
    drop(client.start_siri().await.unwrap());
    // A start can finish or be canceled here. Both release Siri and allow a new start.
    if let Ok(result) = tokio::time::timeout(Duration::ZERO, client.start_siri()).await {
        drop(result.unwrap());
    }
    drop(client.start_siri().await.unwrap());
    // Lets the device read the last release before the connection closes.
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice_devices, 1);
    assert_eq!(log.voice, [(0.0, 3), (320.0, 5)]);
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, [siri; 4].concat());
}

#[tokio::test]
async fn closed_by_device() {
    let device = start_faulty_device(Fault::DropData).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason.as_deref(), Some(Error::Disconnected | Error::Io(_))),
        "{reason:?}"
    );
    assert!(!client.is_connected());
    let err = client.press(Button::Select).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    assert!(err.is_not_sent());
    // Nothing to close any more.
    client.close().await.unwrap();
}

#[tokio::test]
async fn closed_without_feedback_answer() {
    let device = start_faulty_device(Fault::NoFeedback).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    // Time moves forward by itself while all tasks wait.
    tokio::time::pause();
    let reason = client.closed().await;
    assert!(
        matches!(reason.as_deref(), Some(Error::NoAnswer("feedback"))),
        "{reason:?}"
    );
    // The device can count the last requests after the client has closed.
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(1), async {
        while device.log.lock().unwrap().feedback < 4 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("device saw fewer than 4 feedback requests");
}

#[tokio::test]
async fn siri_in_task() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    // `Siri` owns a clone of the client, so it can move to another task.
    let mut siri = client.start_siri().await.unwrap();
    tokio::spawn(async move {
        siri.send(&b"one"[..]).await?;
        siri.finish().await
    })
    .await
    .unwrap()
    .unwrap();
    client.close().await.unwrap();
    assert!(client.closed().await.is_none());
    let err = client.press(Button::Select).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    // Lets the device read TEARDOWN.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice, [(0.0, 3)]);
    // The release is sent before TEARDOWN.
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, siri);
    assert!(log.teardown);
}

#[tokio::test]
async fn siri_without_audio() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let siri = client.start_siri().await.unwrap();
    assert!(matches!(siri.finish().await, Err(Error::SiriNoAudio)));
    // Siri is released, so a new start works.
    drop(client.start_siri().await.unwrap());
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice, []);
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, [siri; 2].concat());
}

#[tokio::test]
async fn siri_debug_logs() {
    // Runs the Siri debug logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ratv=debug")
        .with_test_writer()
        .try_init();
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut siri = client.start_siri().await.unwrap();
    for _ in 0..10 {
        siri.send(&b"x"[..]).await.unwrap();
    }
    siri.finish().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();

    let log = device.log.lock().unwrap();
    let times: Vec<f64> = log.voice.iter().map(|&(t, _)| t).collect();
    assert_eq!(
        times,
        (0..10).map(|i| f64::from(i * 320)).collect::<Vec<_>>()
    );
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, siri);
}

#[tokio::test]
async fn volume_not_available() {
    let device = start_device().await;
    device.log.lock().unwrap().no_volume_control = true;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut volume = client.volume();
    let ready = volume.wait_for(|v| v.available == Some(false));
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    drop(volume);
    let err = client.set_volume(0.5).await.unwrap_err();
    assert!(matches!(err, Error::VolumeNotAvailable), "{err}");
    let err = client.set_muted(true).await.unwrap_err();
    assert!(matches!(err, Error::VolumeNotAvailable), "{err}");
    client.close().await.unwrap();
    // Not `VolumeNotAvailable` from the last value, so `is_closed` is true.
    let err = client.set_volume(0.5).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    let err = client.set_muted(true).await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
}

/// Message receivers close with the connection, also ones made after it.
#[tokio::test]
async fn messages_close_with_connection() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut before = client.messages();
    client.close().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if before.recv().await == Err(tokio::sync::broadcast::error::RecvError::Closed) {
                break;
            }
        }
    });
    closed.await.unwrap();
    let mut after = client.messages();
    let after = tokio::time::timeout(Duration::from_secs(1), after.recv()).await;
    assert!(after.unwrap().is_err());
}

/// `AppleTv::set_volume` checks the argument before the volume state.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_volume_argument_first() {
    use ratv::{AppleTv, Endpoints};

    let device = start_device().await;
    device.log.lock().unwrap().no_volume_control = true;
    let endpoints = Endpoints {
        companion: None,
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let tv = AppleTv::connect(&endpoints, &credentials(), &options())
        .await
        .unwrap();
    let mut volume = tv.volume().unwrap();
    let ready = volume.wait_for(|v| v.available == Some(false));
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    drop(volume);
    let err = tv.set_volume(f32::NAN).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument("volume")), "{err}");
    let err = tv.set_volume(0.5).await.unwrap_err();
    assert!(matches!(err, Error::VolumeNotAvailable), "{err}");
    tv.close().await.unwrap();
}

#[tokio::test]
async fn siri_not_recording() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut opts = options();
    opts.timeout = Duration::from_millis(200);
    let client = Client::connect(addr, &credentials(), &opts).await.unwrap();
    device.log.lock().unwrap().no_recording = true;

    // Kept audio has a limit.
    let mut siri = client.start_siri().await.unwrap();
    let mut result = Ok(());
    for _ in 0..2000 {
        result = siri.send(&b"x"[..]).await;
        if result.is_err() {
            break;
        }
    }
    assert!(matches!(result, Err(Error::SiriNotRecording)));
    drop(siri);

    // `finish` waits for the timeout.
    let mut siri = client.start_siri().await.unwrap();
    siri.send(&b"x"[..]).await.unwrap();
    assert!(matches!(siri.finish().await, Err(Error::SiriNotRecording)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();

    let log = device.log.lock().unwrap();
    assert_eq!(log.voice, []);
    // Both sessions release Siri.
    let siri = [(0x0c, 0x04, true), (0x0c, 0x04, false)];
    assert_eq!(log.buttons, [siri; 2].concat());
}

#[cfg(feature = "opus")]
#[tokio::test]
async fn siri_pcm() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();

    let mut siri = client.start_siri().await.unwrap();
    // 100 + 600 samples: two frames now, the rest padded by `finish`.
    siri.push_pcm(&[100; 100]).await.unwrap();
    siri.push_pcm(&[-100; 600]).await.unwrap();
    siri.finish().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.close().await.unwrap();

    let log = device.log.lock().unwrap();
    let times: Vec<f64> = log.voice.iter().map(|&(t, _)| t).collect();
    assert_eq!(times, [0.0, 320.0, 640.0]);
    assert!(log.voice.iter().all(|&(_, size)| size > 0));
}

#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_siri_without_companion_uses_mrp() {
    use ratv::{AppleTv, Endpoints, Siri};

    let device = start_device().await;
    let endpoints = Endpoints {
        companion: None,
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let tv = AppleTv::connect(&endpoints, &credentials(), &options())
        .await
        .unwrap();
    let mut siri = tv.start_siri().await.unwrap();
    assert!(matches!(siri, Siri::Mrp(_)));
    assert!(!siri.ended());
    siri.send(&b"one"[..]).await.unwrap();
    siri.finish().await.unwrap();
    tv.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(device.log.lock().unwrap().voice, [(0.0, 3)]);
}

#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_chapters_and_subtitles() {
    use ratv::{AppleTv, Endpoints};

    let device = start_device().await;
    let endpoints = Endpoints {
        companion: None,
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let opts = options();
    let tv = AppleTv::connect(&endpoints, &credentials(), &opts)
        .await
        .unwrap();
    let mut np = tv.now_playing();
    let ready = np.wait_for(|n| n.chapters.len() == 4 && n.subtitle_language.is_some());
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    drop(np);

    // At about 100 s: the app supports `NextChapter`, but not `PreviousChapter`.
    tv.next_chapter().await.unwrap();
    tv.previous_chapter().await.unwrap();
    // Turns off the current track, not only the type.
    tv.disable_subtitles().await.unwrap();
    tv.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(
        log.commands,
        [
            Command::NextChapter as i32,
            Command::SeekToPlaybackPosition as i32,
            Command::DisableLanguageOption as i32
        ]
    );
    assert_eq!(log.seeks, [60.0]);
    let archive = &log.language_options[0];
    assert!(archive.windows(6).any(|w| w == b"sub-sv"));
}

/// The app lists only some commands, so the others use another way.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_uses_supported_commands() {
    use ratv::{AppleTv, Endpoints};

    let device = start_device().await;
    let endpoints = Endpoints {
        companion: None,
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let tv = AppleTv::connect(&endpoints, &credentials(), &options())
        .await
        .unwrap();

    // Playing at about 100 s. The first call waits for the command list.
    let err = tv.set_playback_rate(1.5).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument("rate")), "{err}");
    tv.set_playback_rate(8.0).await.unwrap(); // Listed scan rate.
    tv.play().await.unwrap(); // Already playing: nothing is sent.
    tv.play_pause().await.unwrap(); // Pause.
    tv.stop().await.unwrap(); // Pause.
    tv.skip(10.0).await.unwrap(); // Seek.
    tv.skip(-10.0).await.unwrap(); // SkipBackward.
    tv.begin_rewind().await.unwrap(); // SkipBackward instead of rewind.
    tv.end_rewind().await.unwrap(); // Nothing.
    // No skip forward either and no Companion: MRP is tried.
    tv.begin_fast_forward().await.unwrap();
    tv.end_fast_forward().await.unwrap();
    tv.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(
        log.commands,
        [
            Command::ChangePlaybackRate as i32,
            Command::Pause as i32,
            Command::Pause as i32,
            Command::SeekToPlaybackPosition as i32,
            Command::SkipBackward as i32,
            Command::SkipBackward as i32,
            Command::BeginFastForward as i32,
            Command::EndFastForward as i32,
        ]
    );
    assert!((110.0..111.0).contains(&log.seeks[0]), "{:?}", log.seeks);
}

/// A paused player in the background is sent without its item.
/// One queue request gets it.
#[tokio::test]
async fn paused_item_requested() {
    let device = start_device().await;
    device.log.lock().unwrap().no_item = NoItem::State;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut np = client.now_playing();
    let wait = np.wait_for(|n| n.duration.is_some() && n.chapters.len() == 4);
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(np.borrow().state, Some(PlaybackState::Paused));
    client.close().await.unwrap();
    assert_eq!(device.log.lock().unwrap().item_requests, 1);
}

/// A reply without an item does not start another request.
#[tokio::test]
async fn paused_item_requested_once() {
    let device = start_device().await;
    device.log.lock().unwrap().no_item = NoItem::Reply;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    // Subscribed after connect. This works because `#[tokio::test]` runs on
    // one thread, so the task has not read the reply yet.
    let mut messages = client.messages();
    // Wait for the reply, then for more requests.
    let reply = async {
        while messages
            .recv()
            .await
            .unwrap()
            .identifier()
            .is_none_or(|id| id != "item")
        {}
    };
    tokio::time::timeout(Duration::from_secs(2), reply)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.close().await.unwrap();
    assert_eq!(device.log.lock().unwrap().item_requests, 1);
    assert_eq!(client.now_playing().borrow().title, None);
}

/// Waits until `done` is true for the log of the fake device.
async fn wait_log(log: &Mutex<Log>, done: impl Fn(&Log) -> bool) {
    wait_log_within(log, Duration::from_secs(5), done).await;
}

/// Waits at most `limit` until `done` is true for the log of the fake device.
async fn wait_log_within(log: &Mutex<Log>, limit: Duration, done: impl Fn(&Log) -> bool) {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done(&log.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out: {:?}", log.lock().unwrap());
}

/// Voice message with 64 KiB of audio, to fill the socket buffers.
fn filler() -> proto::SendVoiceInputMessage {
    proto::SendVoiceInputMessage {
        data_block: Some(proto::AudioDataBlock {
            buffer: Some(proto::AudioBuffer {
                contents: Some(vec![0; 64 * 1024].into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
    }
}

/// Time for the fake device to read several MiB, also in a slow debug build.
/// Each client write must still end within its 10 s write limit.
const DRAIN: Duration = Duration::from_secs(30);

/// Makes the fake device send `out` after it reads the next message.
async fn push(client: &Client, log: &Mutex<Log>, out: Vec<u8>) {
    log.lock().unwrap().push = Some(out);
    client.press(Button::Select).await.unwrap();
}

/// The item is requested again when the same player loses it, and when
/// another app becomes active with a state but no item.
#[tokio::test]
async fn missing_item_requested_again() {
    let device = start_device().await;
    let log = &device.log;
    log.lock().unwrap().no_item = NoItem::State;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let mut np = client.now_playing();
    let has_item = |n: &mrp::NowPlaying| n.duration.is_some();
    let wait = np.wait_for(has_item);
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(log.lock().unwrap().item_requests, 1);

    let player_path = |bundle: &str| {
        Some(proto::PlayerPath {
            client: Some(proto::NowPlayingClient {
                bundle_identifier: Some(bundle.into()),
                ..Default::default()
            }),
            ..Default::default()
        })
    };
    // The same player loses its item.
    let lost = proto::SetStateMessage {
        player_path: player_path("com.apple.TVMusic"),
        playback_queue: Some(proto::PlaybackQueue::default()),
        ..Default::default()
    };
    push(&client, log, mrp(&lost, None, 0)).await;
    wait_log(log, |l| l.item_requests == 2).await;
    let wait = np.wait_for(has_item);
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();

    // Another app with a paused player and no item.
    let other = "com.example.Video";
    let mut out = mrp(
        &proto::SetNowPlayingClientMessage {
            client: player_path(other).and_then(|p| p.client),
        },
        None,
        0,
    );
    let paused = proto::SetStateMessage {
        player_path: player_path(other),
        playback_state: Some(PlaybackState::Paused as i32),
        ..Default::default()
    };
    out.extend(mrp(&paused, None, 0));
    push(&client, log, out).await;
    let wait = np.wait_for(|n| n.app.as_deref() == Some(other) && n.duration.is_some());
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    client.close().await.unwrap();
    assert_eq!(log.lock().unwrap().item_requests, 3);
}

/// A request that stays in the outbox while the device does not read is
/// removed. Later messages are still sent whole.
#[tokio::test]
async fn request_not_written_is_not_sent() {
    let device = start_device().await;
    let log = &device.log;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut opts = options();
    opts.timeout = Duration::from_millis(300);
    opts.connect_timeout = Some(Duration::from_secs(2));
    let client = Client::connect(addr, &credentials(), &opts).await.unwrap();
    log.lock().unwrap().stop_reading = true;
    // The last message that the device reads.
    client.command(Command::Play).await.unwrap();

    // The task takes it and writes it, but the device does not read it.
    let err = client.command(Command::Pause).await.unwrap_err();
    assert!(matches!(err, Error::NoAnswer(_)), "{err}");
    assert!(!err.is_not_sent());

    // Fills the socket buffers until the task waits in a write.
    let filler = filler();
    let mut written = 0;
    let err = loop {
        // 512 KiB per loop.
        for _ in 0..8 {
            client.send(&filler).await.unwrap();
        }
        let err = client.command(Command::Stop).await.unwrap_err();
        if !matches!(err, Error::NoAnswer(_)) {
            break err;
        }
        written += 1;
        assert!(written < 16, "writes did not wait after 8 MiB");
    };
    assert!(matches!(err, Error::SendTimeout(_)), "{err}");
    assert!(err.is_not_sent());
    assert!(client.is_connected());
    // Queued after the removed request.
    client.press(Button::Menu).await.unwrap();

    log.lock().unwrap().stop_reading = false;
    wait_log_within(log, DRAIN, |l| l.buttons.len() == 2).await;
    let log = log.lock().unwrap();
    let mut commands = vec![Command::Play as i32, Command::Pause as i32];
    commands.extend(std::iter::repeat_n(Command::Stop as i32, written));
    assert_eq!(log.commands, commands);
    assert_eq!(log.buttons, [(1, 0x86, true), (1, 0x86, false)]);
}

/// A request that is still in the outbox when the connection closes was not
/// sent.
#[tokio::test]
async fn request_queued_at_close_is_not_sent() {
    let device = start_device().await;
    let log = &device.log;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut opts = options();
    opts.timeout = Duration::from_millis(300);
    opts.connect_timeout = Some(Duration::from_secs(2));
    let client = Client::connect(addr, &credentials(), &opts).await.unwrap();
    log.lock().unwrap().stop_reading = true;
    // The last message that the device reads.
    client.command(Command::Play).await.unwrap();

    // Fills the socket buffers until the task waits in a write.
    let filler = filler();
    for round in 0.. {
        // 512 KiB per loop.
        for _ in 0..8 {
            client.send(&filler).await.unwrap();
        }
        let err = client.command(Command::Stop).await.unwrap_err();
        if matches!(err, Error::SendTimeout(_)) {
            break;
        }
        assert!(matches!(err, Error::NoAnswer(_)), "{err}");
        assert!(round < 16, "writes did not wait after 8 MiB");
    }

    let request = client.command(Command::Pause);
    let hang_up = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        log.lock().unwrap().hang_up = true;
    };
    let (result, ()) = tokio::join!(request, hang_up);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
    assert!(err.is_not_sent());
    assert!(!client.is_connected());
}

/// Releases skip the outbox limit. A press that does not fit sends nothing.
#[tokio::test]
async fn release_queued_when_outbox_full() {
    let device = start_device().await;
    let log = &device.log;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let hold = client.hold(Button::Home, Duration::from_millis(50));
    tokio::pin!(hold);
    // Sends the press, then waits in the sleep, which is not polled again here.
    assert!(
        tokio::time::timeout(Duration::from_millis(5), &mut hold)
            .await
            .is_err()
    );
    wait_log(log, |l| l.buttons.len() == 1).await;
    log.lock().unwrap().stop_reading = true;
    // The last message that the device reads.
    client.command(Command::Play).await.unwrap();

    // Lets the task write until the socket is full and the write waits.
    // Only then the outbox fills up.
    let filler = filler();
    let mut sent = 0;
    let err = loop {
        match client.send(&filler).await {
            Ok(()) => sent += 1,
            Err(err) => break err,
        }
        assert!(sent < 512, "outbox not full after 32 MiB");
        tokio::time::sleep(Duration::from_millis(1)).await;
    };
    assert!(matches!(err, Error::QueueFull), "{err}");
    let err = client.press(Button::Menu).await.unwrap_err();
    assert!(matches!(err, Error::QueueFull), "{err}");
    // The task still waits in the write, so the outbox stays full.
    hold.await.unwrap();

    log.lock().unwrap().stop_reading = false;
    wait_log_within(log, DRAIN, |l| l.buttons.len() == 2).await;
    assert_eq!(
        log.lock().unwrap().buttons,
        [(0x0c, 0x40, true), (0x0c, 0x40, false)]
    );
    assert!(client.is_connected());
}

/// Waits until the fake device got TEARDOWN.
async fn wait_teardown(log: &Mutex<Log>) {
    for _ in 0..100 {
        if log.lock().unwrap().teardown {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no TEARDOWN");
}

#[tokio::test]
async fn failed_setup_sends_teardown() {
    let device = start_faulty_device(Fault::DataPort).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let result = Client::connect(addr, &credentials(), &options()).await;
    // Windows retries a refused connect for about 2 s, so the timeout can come first.
    assert!(matches!(result, Err(Error::Io(_) | Error::Timeout)));
    wait_teardown(&device.log).await;
}

#[tokio::test]
async fn missing_device_info_sends_teardown() {
    let device = start_faulty_device(Fault::DeviceInfo).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut options = options();
    options.timeout = Duration::from_millis(200);
    let result = Client::connect(addr, &credentials(), &options).await;
    assert!(matches!(result, Err(Error::Timeout)));
    wait_teardown(&device.log).await;
}

#[tokio::test]
async fn wrong_credentials_fail() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let good = credentials();
    let creds = Credentials::new(
        [1; 32],
        [0; 32].into(),
        good.accessory_id().to_vec(),
        good.client_id().to_vec(),
    );
    let err = Client::connect(addr, &creds, &options()).await.unwrap_err();
    assert!(err.is_auth(), "{err}");
}

#[tokio::test]
async fn connection_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    // Windows retries a refused connect for about 2 s, so the timeout can come first.
    assert!(matches!(
        Client::connect(addr, &credentials(), &options()).await,
        Err(Error::Io(_) | Error::Timeout)
    ));
}

#[tokio::test]
async fn connect_timeout_limits_setup_steps() {
    let device = start_faulty_device(Fault::DeviceInfo).await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let mut options = options();
    options.timeout = Duration::from_secs(30);
    options.connect_timeout = Some(Duration::from_millis(200));
    let start = Instant::now();
    let result = Client::connect(addr, &credentials(), &options).await;
    assert!(matches!(result, Err(Error::Timeout)));
    assert!(start.elapsed() < Duration::from_secs(5));
    wait_teardown(&device.log).await;
}

#[tokio::test]
async fn other_device_fails() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let good = credentials();
    let creds = Credentials::new(
        *good.accessory_ltpk(),
        CLIENT_SEED.into(),
        b"other".to_vec(),
        good.client_id().to_vec(),
    );
    let err = Client::connect(addr, &creds, &options()).await.unwrap_err();
    assert!(err.is_other_device(), "{err}");
    assert!(!err.is_auth(), "{err}");
}

#[tokio::test]
async fn stalled_connect_starts_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // The first connection gets no answer.
        let (_silent, _) = listener.accept().await.unwrap();
        run_device(listener, Arc::default(), Fault::None).await;
    });
    let mut options = options();
    options.stall_timeout = Some(Duration::from_millis(200));
    let start = Instant::now();
    Client::connect(addr, &credentials(), &options)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

/// A closed MRP connects again without the refused Companion, and the
/// refused error is kept.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_connects_missing_protocol() {
    use ratv::{AppleTv, Endpoints, Key, Protocol};

    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let companion = refused.local_addr().unwrap();
    drop(refused);
    let first = start_device().await;
    let mut endpoints = Endpoints {
        companion: Some(companion),
        airplay: Some(format!("127.0.0.1:{}", first.port).parse().unwrap()),
    };
    let mut opts = options();
    // Windows tries a refused connect again for about 2 s.
    opts.timeout = Duration::from_secs(10);
    let tv = AppleTv::connect(&endpoints, &credentials(), &opts)
        .await
        .unwrap();
    let err = tv.connect_error(Protocol::Companion).unwrap();
    assert!(err.is_refused(), "{err:?}");
    assert!(tv.connect_error(Protocol::Mrp).is_none());
    assert!(!tv.is_open(Protocol::Companion));
    assert!(tv.is_open(Protocol::Mrp));

    // Companion is still refused: fails, and MRP stays connected.
    let err = tv
        .connect_missing(&endpoints, &credentials(), &opts)
        .await
        .unwrap_err();
    assert!(err.is_refused(), "{err:?}");
    assert!(tv.mrp().unwrap().is_connected());

    // No endpoint for the missing protocol.
    let no_companion = Endpoints {
        companion: None,
        ..endpoints
    };
    let err = tv
        .connect_missing(&no_companion, &credentials(), &opts)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NoEndpoints));

    // MRP closes, so both are missing. Only MRP connects again, on a new port.
    tv.mrp().unwrap().close().await.unwrap();
    assert!(!tv.is_open(Protocol::Mrp));
    // Calls on the closed protocol give `Closed`.
    for err in [
        tv.seek(1.0).await.unwrap_err(),
        tv.set_muted(true).await.unwrap_err(),
        tv.press(Key::Select).await.unwrap_err(),
        tv.artwork(0, 0).await.unwrap_err(),
        tv.set_volume(0.5).await.unwrap_err(),
        tv.begin_fast_forward().await.unwrap_err(),
        tv.end_fast_forward().await.unwrap_err(),
        tv.play_pause().await.unwrap_err(),
        tv.next_chapter().await.unwrap_err(),
        tv.start_siri().await.err().unwrap(),
    ] {
        assert!(matches!(err, Error::Closed), "{err}");
    }
    let second = start_device().await;
    endpoints.airplay = Some(format!("127.0.0.1:{}", second.port).parse().unwrap());
    let new = tv
        .connect_missing(&endpoints, &credentials(), &opts)
        .await
        .unwrap()
        .unwrap();
    assert!(new.is_connected());
    assert!(new.is_open(Protocol::Mrp));
    assert!(!tv.is_connected());
    assert!(new.connect_error(Protocol::Companion).unwrap().is_refused());
    assert!(new.connect_error(Protocol::Mrp).is_none());
    new.press(Key::Select).await.unwrap();
    new.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(second.log.lock().unwrap().buttons[0], (1, 0x89, true));
}

/// The app lists `SkipBackward` but not `BeginRewind`, so rewind skips.
#[cfg(feature = "companion")]
#[tokio::test]
async fn rewind_end_after_failed_skip() {
    use ratv::{AppleTv, Endpoints};

    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let companion = refused.local_addr().unwrap();
    drop(refused);
    let endpoints = |device: &Device| Endpoints {
        companion: Some(companion),
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let mut opts = options();
    // Windows tries a refused connect again for about 2 s.
    opts.timeout = Duration::from_secs(10);

    // After a skip that worked, the end does nothing, also when closed.
    let device = start_device().await;
    let tv = AppleTv::connect(&endpoints(&device), &credentials(), &opts)
        .await
        .unwrap();
    tv.begin_rewind().await.unwrap();
    tv.mrp().unwrap().close().await.unwrap();
    tv.end_rewind().await.unwrap();

    // After a failed skip, the end checks again and gives `Closed`.
    let device = start_device().await;
    device.log.lock().unwrap().fail_skip = true;
    let tv = AppleTv::connect(&endpoints(&device), &credentials(), &opts)
        .await
        .unwrap();
    let err = tv.begin_rewind().await.unwrap_err();
    assert!(matches!(err, Error::CommandFailed { .. }), "{err}");
    tv.mrp().unwrap().close().await.unwrap();
    let err = tv.end_rewind().await.unwrap_err();
    assert!(matches!(err, Error::Closed), "{err}");
}

/// Connects to a fake device that sends the command list only with the 404
/// answer to `SkipForward` or `Stop`.
#[cfg(feature = "companion")]
async fn late_commands_device() -> (Device, ratv::AppleTv) {
    let device = start_device().await;
    device.log.lock().unwrap().late_commands = true;
    let endpoints = ratv::Endpoints {
        companion: None,
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let tv = ratv::AppleTv::connect(&endpoints, &credentials(), &options())
        .await
        .unwrap();
    (device, tv)
}

/// The command list comes with the 404 answer. It does not list `SkipForward`
/// or `Stop`, but lists seeking and `Pause`, so `skip` seeks and `stop` pauses.
#[cfg(feature = "companion")]
#[tokio::test]
async fn commands_come_late() {
    // Each device waits the full state wait time, so both run at once.
    let ((skip_device, skip_tv), (stop_device, stop_tv)) =
        tokio::join!(late_commands_device(), late_commands_device());
    let (skipped, stopped) = tokio::join!(skip_tv.skip(5.0), stop_tv.stop());
    skipped.unwrap();
    stopped.unwrap();
    skip_tv.close().await.unwrap();
    stop_tv.close().await.unwrap();
    let log = skip_device.log.lock().unwrap();
    assert_eq!(
        log.commands,
        [
            Command::SkipForward as i32,
            Command::SeekToPlaybackPosition as i32
        ]
    );
    // The item plays from 100 s.
    assert!(log.seeks[0] >= 105.0, "{:?}", log.seeks);
    assert_eq!(
        stop_device.log.lock().unwrap().commands,
        [Command::Stop as i32, Command::Pause as i32]
    );
}

/// Companion is refused, so `AppleTv` uses only MRP.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_falls_back_to_mrp() {
    use ratv::{AppleTv, Endpoints, Key};

    let device = start_device().await;
    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let companion = refused.local_addr().unwrap();
    drop(refused);
    let endpoints = Endpoints {
        companion: Some(companion),
        airplay: Some(format!("127.0.0.1:{}", device.port).parse().unwrap()),
    };
    let opts = options();
    let tv = AppleTv::connect(&endpoints, &credentials(), &opts)
        .await
        .unwrap();
    assert!(tv.companion().is_none());
    assert!(tv.mrp().is_some());

    tv.press(Key::Select).await.unwrap();
    tv.press(Key::TopMenu).await.unwrap();
    tv.pause().await.unwrap();
    tv.seek(10.0).await.unwrap();
    for seconds in [f64::NAN, 1e300] {
        assert!(matches!(
            tv.skip(seconds).await,
            Err(Error::InvalidArgument("seconds"))
        ));
    }
    tv.set_volume(0.25).await.unwrap();
    assert!(matches!(
        tv.turn_on().await,
        Err(Error::NotConnected("Companion"))
    ));
    assert!(matches!(
        tv.launch_app("x").await,
        Err(Error::NotConnected("Companion"))
    ));
    assert!(matches!(
        tv.control_center().await,
        Err(Error::NotConnected("Companion"))
    ));
    assert!(tv.is_connected());
    tv.clone().close().await.unwrap();
    assert!(tv.closed().await.is_none());
    assert!(!tv.is_connected());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let log = device.log.lock().unwrap();
    assert_eq!(log.buttons[0], (1, 0x89, true));
    assert_eq!(log.buttons[2], (0x0c, 0x60, true));
    assert_eq!(
        log.commands,
        [
            Command::Pause as i32,
            Command::SeekToPlaybackPosition as i32
        ]
    );
    assert_eq!(log.volume, Some(0.25));
    assert!(log.teardown);
}

/// Both endpoints fail: the first error is returned.
#[cfg(feature = "companion")]
#[tokio::test]
async fn apple_tv_fails_when_nothing_connects() {
    use ratv::{AppleTv, Endpoints};

    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = refused.local_addr().unwrap();
    drop(refused);
    let endpoints = Endpoints {
        companion: Some(addr),
        airplay: Some(addr),
    };
    let err = AppleTv::connect(&endpoints, &credentials(), &ClientOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)));
}

#[tokio::test]
async fn pairing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let device = tokio::spawn(pair_device(listener));
    let mut options = PairOptions::default();
    options.name = "Test Remote".into();
    options.client_id = Some("client-9".into());
    let creds = mrp::pair(addr, &options)
        .await
        .unwrap()
        .finish("1234")
        .await
        .unwrap();
    assert_eq!(creds.accessory_id(), DEVICE_ID);
    assert_eq!(creds.client_id(), b"client-9");
    assert_eq!(
        *creds.accessory_ltpk(),
        SigningKey::from_bytes(&DEVICE_LTSK)
            .verifying_key()
            .to_bytes()
    );

    let (controller, names) = device.await.unwrap();
    let controller = controller.unwrap();
    assert_eq!(controller.id, b"client-9");
    // pin-start and the three pair-setup requests carry the name.
    assert_eq!(names, ["Test Remote"; 4]);
    let metadata = controller.metadata.unwrap();
    let meta = ratv_codec::opack::decode(&metadata).unwrap();
    assert_eq!(
        meta.get("name").and_then(ratv_codec::opack::Value::as_str),
        Some("Test Remote")
    );
}

#[tokio::test]
async fn pairing_with_wrong_pin_fails() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pair_device(listener));
    let prompt = mrp::pair(addr, &PairOptions::default()).await.unwrap();
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
async fn hold_released_when_canceled() {
    let device = start_device().await;
    let addr = format!("127.0.0.1:{}", device.port).parse().unwrap();
    let client = Client::connect(addr, &credentials(), &options())
        .await
        .unwrap();
    let hold = client.hold(Button::Home, Duration::from_secs(10));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), hold)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Down, then up from the dropped future.
    assert_eq!(
        device.log.lock().unwrap().buttons,
        [(0x0c, 0x40, true), (0x0c, 0x40, false)]
    );
}
