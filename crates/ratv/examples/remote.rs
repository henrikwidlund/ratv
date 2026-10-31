//! Find an Apple TV, connect with Companion and MRP, and send a command.
//!
//! ```text
//! cargo run --example remote -- <credentials> <command> [device name or IP]
//! ```
//!
//! Without a name the first device found is used. Commands: `up`, `down`,
//! `left`, `right`, `select`, `menu`, `home`, `guide`, `control_center`, `play`, `pause`, `toggle`,
//! `next`, `previous`, `stop`, `on`, `off`, `seek=<s>`, `skip=<s>`,
//! `volume=<0..1>`, `launch=<bundle id>`, `artwork=<file>`, `watch`,
//! `text` (print the focused text field), `text=<text>`, `append=<text>`, `clear_text`,
//! `swipe_up`, `swipe_down`, `swipe_left`, `swipe_right`, `progress`,
//! `volume` (print volume and mute), `watch_volume`, `mute`, `unmute`, `page_up`,
//! `screensaver`, `channel_up`, `channel_down`, `siri`, `tap=<x>,<y>` (0.0 to 1.0),
//! `speed=<rate>`, `rate=<rating>`, `like`, `unlike`, `dislike`, `undislike`, `accounts`, `account=<id>`,
//! `ff=<s>` and `rw=<s>` (fast forward or rewind for some seconds), `shuffle`, `repeat`
//! (next mode), `shuffle=<off|songs|albums>`, `repeat=<off|one|all>`,
//! `info` (print all now playing fields), `languages` (list with
//! tags), `audio=<tag>`, `subtitles=<tag>` (for example `sv`), `subtitles_off`, `queue`,
//! `play_item=<n>` (number from `queue`), `chapters`, `next_chapter`, `previous_chapter`,
//! `siri=<file.caf>` (ask Siri with 16 kHz mono Opus audio),
//! `siri_pcm=<file.wav>` (ask Siri with 16-bit 16 kHz mono PCM, feature `opus`).
//! `watch` prints changes until the connection ends or Ctrl+C.
//! `progress` prints the position every second and marks updates from the device.
//! Set `RUST_LOG=ratv=debug` to see which protocols connect.

#[path = "common/caf.rs"]
mod caf;
#[cfg(feature = "opus")]
#[path = "common/wav.rs"]
mod wav;

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime};

use ratv::discovery::{MDNS_PORT, ScanOptions, scan};
use ratv::mrp::{self, RepeatMode, ShuffleMode, cocoa_time};
use ratv::{AppleTv, ConnectOptions, Endpoints, Key};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let credentials = args
        .first()
        .ok_or("missing credentials")?
        .parse()
        .map_err(|_| "invalid credentials, expected the string printed by `pair`")?;
    let command = args.get(1).ok_or("missing command")?.as_str();
    let wanted = args.get(2);

    let mut options = ScanOptions::default();
    if let Some(ip) = wanted.and_then(|w| w.parse::<IpAddr>().ok()) {
        options.targets = vec![SocketAddr::new(ip, MDNS_PORT)];
    }
    let devices = scan(&options).await?;
    let device = devices
        .iter()
        .find(|d| {
            wanted.is_none_or(|w| d.name() == w || d.addresses.iter().any(|a| a.to_string() == *w))
        })
        .ok_or("device not found")?;
    let endpoints = Endpoints::from(device);
    println!(
        "{}: companion {:?}, airplay {:?}",
        device.name(),
        endpoints.companion,
        endpoints.airplay
    );

    let tv = AppleTv::connect(&endpoints, &credentials, &ConnectOptions::default()).await?;
    println!(
        "connected: companion {}, mrp {}",
        tv.companion().is_some(),
        tv.mrp().is_some()
    );

    if let Some((from, to)) = command.strip_prefix("swipe_").and_then(swipe_points) {
        tv.swipe(from, to, Duration::from_millis(200)).await?;
        tv.close().await?;
        return Ok(());
    }
    let key = key(command);
    match (key, command) {
        (Some(k), _) => tv.press(k).await?,
        (None, "play") => tv.play().await?,
        (None, "pause") => tv.pause().await?,
        (None, "toggle") => tv.play_pause().await?,
        (None, "next") => tv.next().await?,
        (None, "previous") => tv.previous().await?,
        (None, "stop") => tv.stop().await?,
        (None, "accounts") => {
            for a in tv.accounts().await? {
                println!("{}\t{}", a.id, a.name);
            }
        }
        (None, "like") => tv.like(true).await?,
        (None, "unlike") => tv.like(false).await?,
        (None, "dislike") => tv.dislike(true).await?,
        (None, "undislike") => tv.dislike(false).await?,
        (None, "shuffle") => tv.cycle_shuffle().await?,
        (None, "repeat") => tv.cycle_repeat().await?,
        (None, "info") => info(&tv).await?,
        (None, "languages") => print_languages(&tv).await?,
        (None, "subtitles_off") => subtitles_off(&tv).await?,
        (None, "queue") => print_queue(&tv).await?,
        (None, "chapters") => {
            for c in details(&tv, |n| !n.chapters.is_empty()).await?.chapters {
                println!("{:?}\t{:?}\t{:?}", c.start, c.duration, c.title);
            }
        }
        (None, "next_chapter") => {
            // Chapters arrive shortly after connect.
            details(&tv, |n| !n.chapters.is_empty()).await?;
            tv.next_chapter().await?;
        }
        (None, "previous_chapter") => {
            details(&tv, |n| !n.chapters.is_empty()).await?;
            tv.previous_chapter().await?;
        }
        (None, "on") => tv.turn_on().await?,
        (None, "off") => tv.turn_off().await?,
        (None, "watch") => watch(&tv).await?,
        (None, "progress") => progress(&tv).await?,
        (None, "volume") => print_volume(&tv, false).await?,
        (None, "watch_volume") => print_volume(&tv, true).await?,
        (None, "mute") => tv.set_muted(true).await?,
        (None, "unmute") => tv.set_muted(false).await?,
        (None, "text") => println!("{:?}", tv.text().await?),
        (None, "clear_text") => tv.clear_text().await?,
        (None, _) => run_value(&tv, command).await?,
    }
    tv.close().await?;
    Ok(())
}

/// Sends a CAF file to Siri, one packet every 20 ms.
async fn ask_siri(tv: &AppleTv, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = bytes::Bytes::from(std::fs::read(path)?);
    let packets = caf::packets(&file)?;
    let mut siri = tv.start_siri().await?;
    println!("pressed, sending {} packets", packets.len());
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    for packet in packets {
        tick.tick().await;
        siri.send(file.slice_ref(packet)).await?;
    }
    siri.finish().await?;
    println!("done");
    Ok(())
}

/// Sends a WAV file to Siri as PCM, 100 ms every 100 ms.
#[cfg(feature = "opus")]
async fn ask_siri_pcm(tv: &AppleTv, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::read(path)?;
    let samples: Vec<i16> = wav::pcm(&file)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| i16::from_le_bytes(b))
        .collect();
    let mut siri = tv.start_siri().await?;
    println!("pressed, sending {} samples", samples.len());
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    for chunk in samples.chunks(1600) {
        tick.tick().await;
        siri.push_pcm(chunk).await?;
    }
    siri.finish().await?;
    println!("done");
    Ok(())
}

/// Runs a `name=value` command.
async fn run_value(tv: &AppleTv, command: &str) -> Result<(), Box<dyn std::error::Error>> {
    let value = |prefix: &str| command.strip_prefix(prefix);
    if let Some(v) = value("seek=") {
        tv.seek(v.parse()?).await?;
    } else if let Some(v) = value("skip=") {
        tv.skip(v.parse()?).await?;
    } else if let Some(v) = value("volume=") {
        tv.set_volume(v.parse()?).await?;
    } else if let Some(v) = value("account=") {
        tv.switch_account(v).await?;
    } else if let Some(v) = value("speed=") {
        tv.set_playback_rate(v.parse()?).await?;
    } else if let Some(v) = value("rate=") {
        tv.rate(v.parse()?).await?;
    } else if let Some(v) = value("ff=") {
        scrub(tv, true, v.parse()?).await?;
    } else if let Some(v) = value("rw=") {
        scrub(tv, false, v.parse()?).await?;
    } else if let Some(v) = value("shuffle=") {
        tv.set_shuffle(ShuffleMode::from_str_name(&capitalize(v)).ok_or("bad mode")?)
            .await?;
    } else if let Some(v) = value("repeat=") {
        tv.set_repeat(RepeatMode::from_str_name(&capitalize(v)).ok_or("bad mode")?)
            .await?;
    } else if let Some(v) = value("play_item=") {
        let np = details(tv, |n| !n.up_next.is_empty()).await?;
        let index: usize = v.parse()?;
        println!("before: {:?}", np.title);
        let next = np.up_next.get(index).ok_or("no such item")?;
        println!("expect: {:?}", next.title);
        tv.play_queue_item(index).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        println!("after:  {:?}", details(tv, |_| true).await?.title);
    } else if let Some((kind, v)) = value("audio=")
        .map(|v| ("a", v))
        .or_else(|| value("subtitles=").map(|v| ("s", v)))
    {
        let np = details(tv, |n| !n.audio_languages.is_empty()).await?;
        let list = if kind == "a" {
            &np.audio_languages
        } else {
            &np.subtitle_languages
        };
        let option = list
            .iter()
            .find(|o| o.language_tag() == v)
            .ok_or("no such language")?;
        print_languages_now(tv, "before").await?;
        println!(
            "enable: {} ({})",
            option.display_name(),
            option.language_tag()
        );
        tv.enable_language(option).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        print_languages_now(tv, "after").await?;
    } else if let Some(v) = value("tap=") {
        let (x, y) = v.split_once(',').ok_or("expected tap=<x>,<y>")?;
        tv.tap(x.parse()?, y.parse()?).await?;
    } else if let Some(v) = value("text=") {
        tv.set_text(v).await?;
    } else if let Some(v) = value("append=") {
        tv.append_text(v).await?;
    } else if let Some(v) = value("launch=") {
        tv.launch_app(v).await?;
    } else if let Some(path) = value("artwork=") {
        save_artwork(tv, path).await?;
    } else if let Some(path) = value("siri=") {
        ask_siri(tv, path).await?;
    } else if let Some(path) = value("siri_pcm=") {
        #[cfg(feature = "opus")]
        ask_siri_pcm(tv, path).await?;
        #[cfg(not(feature = "opus"))]
        return Err(format!("siri_pcm needs feature opus, not {path}").into());
    } else {
        return Err(format!("unknown command {command}").into());
    }
    Ok(())
}

/// Fast forwards, or rewinds, for `seconds`.
async fn scrub(tv: &AppleTv, forward: bool, seconds: f64) -> Result<(), ratv::Error> {
    if forward {
        tv.begin_fast_forward().await?;
    } else {
        tv.begin_rewind().await?;
    }
    tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
    if forward {
        tv.end_fast_forward().await
    } else {
        tv.end_rewind().await
    }
}

/// `off` to `Off`.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Prints audio and subtitle languages with their tags.
async fn print_languages(tv: &AppleTv) -> Result<(), Box<dyn std::error::Error>> {
    let np = details(tv, |n| !n.audio_languages.is_empty()).await?;
    let name = |o: &mrp::LanguageOption| format!("{} ({})", o.display_name(), o.language_tag());
    for (i, o) in np.audio_languages.iter().enumerate() {
        println!("a{i}\t{}", name(o));
    }
    for (i, o) in np.subtitle_languages.iter().enumerate() {
        println!("s{i}\t{}", name(o));
    }
    println!("current audio {:?}", np.audio_language.as_ref().map(name));
    println!(
        "current subtitles {:?}",
        np.subtitle_language.as_ref().map(name)
    );
    Ok(())
}

/// Turns off the current subtitles.
async fn subtitles_off(tv: &AppleTv) -> Result<(), Box<dyn std::error::Error>> {
    details(tv, |n| !n.audio_languages.is_empty()).await?;
    print_languages_now(tv, "before").await?;
    tv.disable_subtitles().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    print_languages_now(tv, "after").await?;
    Ok(())
}

/// Prints the current audio and subtitle language as the device reports them.
async fn print_languages_now(tv: &AppleTv, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let np = details(tv, |_| true).await?;
    let name = |o: &mrp::LanguageOption| format!("{} ({})", o.display_name(), o.language_tag());
    println!(
        "{label}: audio {:?}, subtitles {:?}",
        np.audio_language.as_ref().map(name),
        np.subtitle_language.as_ref().map(name)
    );
    Ok(())
}

/// Prints the items after the current one.
async fn print_queue(tv: &AppleTv) -> Result<(), Box<dyn std::error::Error>> {
    let np = details(tv, |n| !n.up_next.is_empty()).await?;
    for (i, item) in np.up_next.iter().enumerate() {
        println!(
            "{i}\t{}\t{}",
            item.title.as_deref().unwrap_or("-"),
            item.artist.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// Now playing once `ready` is true, or after 3 s.
async fn details(
    tv: &AppleTv,
    ready: impl Fn(&mrp::NowPlaying) -> bool,
) -> Result<mrp::NowPlaying, Box<dyn std::error::Error>> {
    let mut np = tv.now_playing().ok_or("MRP is not connected")?;
    let _ = tokio::time::timeout(Duration::from_secs(3), np.wait_for(ready)).await;
    Ok(np.borrow().clone())
}

/// Prints all now playing fields once the device has sent them.
async fn info(tv: &AppleTv) -> Result<(), Box<dyn std::error::Error>> {
    let mut np = tv.now_playing().ok_or("MRP is not connected")?;
    let _ = tokio::time::timeout(Duration::from_secs(3), np.wait_for(|n| n.title.is_some())).await;
    println!("{:#?}", *np.borrow());
    Ok(())
}

async fn save_artwork(tv: &AppleTv, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // The device sends the image only after it has sent the now playing state.
    let mut rx = tv.now_playing().ok_or("MRP is not connected")?;
    let wait = rx.wait_for(|np| np.artwork_id.is_some());
    if let Ok(np) = tokio::time::timeout(Duration::from_secs(3), wait).await {
        println!("url: {}", opt(np?.artwork_url_sized(600, 600)));
    }
    match tv.artwork(512.0, -1.0).await? {
        Some(art) => {
            std::fs::write(path, &art.data)?;
            println!(
                "{} bytes, {}, {}x{}",
                art.data.len(),
                art.mime_type.as_deref().unwrap_or("unknown type"),
                opt(art.width),
                opt(art.height),
            );
        }
        None => println!("no artwork"),
    }
    Ok(())
}

async fn watch(tv: &AppleTv) -> Result<(), Box<dyn std::error::Error>> {
    let mut rx = tv.now_playing().ok_or("MRP is not connected")?;
    // Print the current state first, then every change.
    rx.mark_changed();
    while rx.changed().await.is_ok() {
        let np = rx.borrow_and_update();
        println!(
            "{} | {} | {} | {}/{} s | artwork {} | rates {:?} liked {} rating {:?}",
            np.state.map_or("-", |s| s.as_str_name()),
            np.title.as_deref().unwrap_or("-"),
            np.artist.as_deref().unwrap_or("-"),
            opt(np.position(cocoa_time(SystemTime::now())).map(f64::round)),
            opt(np.duration.map(f64::round)),
            np.artwork_id.as_deref().unwrap_or("-"),
            np.supported_rates,
            opt(np.liked),
            np.rating_range,
        );
    }
    println!("disconnected");
    Ok(())
}

async fn progress(tv: &AppleTv) -> Result<(), Box<dyn std::error::Error>> {
    let mut rx = tv.now_playing().ok_or("MRP is not connected")?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        // `*` marks a state update from the device; other lines are computed locally.
        let mark = tokio::select! {
            _ = tick.tick() => ' ',
            r = rx.changed() => {
                if r.is_err() {
                    println!("disconnected");
                    return Ok(());
                }
                '*'
            }
        };
        let np = rx.borrow_and_update();
        println!(
            "{mark} {} {:.1}/{} s rate {}",
            np.state.map_or("-", |s| s.as_str_name()),
            np.position(cocoa_time(SystemTime::now()))
                .unwrap_or(f64::NAN),
            opt(np.duration.map(f64::round)),
            opt(np.rate),
        );
    }
}

/// Prints the volume once, or every change with `watch`.
async fn print_volume(tv: &AppleTv, watch: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut rx = tv.volume().ok_or("MRP is not connected")?;
    // The initial values arrive shortly after connect. Availability comes last.
    let wait = rx.wait_for(|v| v.available.is_some());
    let _ = tokio::time::timeout(Duration::from_secs(2), wait).await;
    loop {
        let v = rx.borrow_and_update().clone();
        println!(
            "level {} muted {} available {} capabilities {}",
            opt(v.level),
            opt(v.muted),
            opt(v.available),
            opt(v.capabilities),
        );
        for o in &v.outputs {
            println!(
                "  output {} ({}): volume {} muted {}",
                o.name,
                o.uid,
                opt(o.volume),
                opt(o.muted)
            );
        }
        if !watch || rx.changed().await.is_err() {
            return Ok(());
        }
    }
}

/// Key for a command name.
fn key(command: &str) -> Option<Key> {
    match command {
        "up" => Some(Key::Up),
        "down" => Some(Key::Down),
        "left" => Some(Key::Left),
        "right" => Some(Key::Right),
        "select" => Some(Key::Select),
        "menu" => Some(Key::Menu),
        "home" => Some(Key::Home),
        "guide" => Some(Key::Guide),
        "control_center" => Some(Key::PageDown),
        "page_up" => Some(Key::PageUp),
        "screensaver" => Some(Key::Screensaver),
        "channel_up" => Some(Key::ChannelUp),
        "channel_down" => Some(Key::ChannelDown),
        "siri" => Some(Key::Siri),
        _ => None,
    }
}

/// Formats a value, or `-` if missing.
fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// Start and end of a swipe in a direction, in 0.0 to 1.0 coordinates.
fn swipe_points(direction: &str) -> Option<((f64, f64), (f64, f64))> {
    Some(match direction {
        "up" => ((0.5, 0.7), (0.5, 0.3)),
        "down" => ((0.5, 0.3), (0.5, 0.7)),
        "left" => ((0.7, 0.5), (0.3, 0.5)),
        "right" => ((0.3, 0.5), (0.7, 0.5)),
        _ => return None,
    })
}
