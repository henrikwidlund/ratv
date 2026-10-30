//! Media Remote Protocol over AirPlay: now playing, playback and buttons.
//!
//! ```text
//! cargo run --example mrp -- <ip:port> pair
//! cargo run --example mrp -- <ip:port> <credentials> <command>
//! ```
//!
//! The port is the one of the `_airplay._tcp` service (usually 7000).
//! Companion credentials from `cargo run --example companion -- <ip:port> pair` also work.
//!
//! Commands: `watch` (print now playing changes for 30 s), `play`, `pause`,
//! `toggle`, `next`, `previous`, `stop`, `seek=<seconds>`, `skip=<seconds>`,
//! `volume=<0..1>`, and buttons `up`, `down`, `left`, `right`, `select`,
//! `menu`, `home`.
//! Set `RUST_LOG=ratv=trace` to log messages.

use std::io::BufRead;
use std::time::{Duration, SystemTime};

use ratv::mrp::{self, Button, Client, ClientOptions, Command, PairOptions, cocoa_time};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args.first().ok_or("missing address")?.parse()?;

    if args.get(1).map(String::as_str) == Some("pair") {
        let prompt = mrp::pair(addr, &PairOptions::default()).await?;
        println!("Enter PIN shown on the TV:");
        let pin = std::io::stdin().lock().lines().next().ok_or("no PIN")??;
        let credentials = prompt.finish(pin.trim()).await?;
        println!("Credentials: {credentials}");
        return Ok(());
    }

    let credentials = args
        .get(1)
        .ok_or("missing credentials")?
        .parse()
        .map_err(|_| "invalid credentials, expected the string printed by `pair`")?;
    let command = args.get(2).map_or("watch", String::as_str);

    let client = Client::connect(addr, &credentials, &ClientOptions::default()).await?;
    println!("connected to {}", client.device_info().name);

    let button = match command {
        "up" => Some(Button::Up),
        "down" => Some(Button::Down),
        "left" => Some(Button::Left),
        "right" => Some(Button::Right),
        "select" => Some(Button::Select),
        "menu" => Some(Button::Menu),
        "home" => Some(Button::Home),
        _ => None,
    };
    let value = |prefix: &str| command.strip_prefix(prefix).map(str::parse);
    match (button, command) {
        (Some(b), _) => client.press(b).await?,
        (None, "play") => client.command(Command::Play).await?,
        (None, "pause") => client.command(Command::Pause).await?,
        (None, "toggle") => client.command(Command::TogglePlayPause).await?,
        (None, "next") => client.command(Command::NextTrack).await?,
        (None, "previous") => client.command(Command::PreviousTrack).await?,
        (None, "stop") => client.command(Command::Stop).await?,
        (None, "watch") => watch(&client).await,
        (None, cmd) => {
            if let Some(v) = value("seek=") {
                client.seek(v?).await?;
            } else if let Some(v) = value("skip=") {
                client.skip(v? as f32).await?;
            } else if let Some(v) = value("volume=") {
                client.set_volume(v? as f32).await?;
            } else {
                return Err(format!("unknown command {cmd}").into());
            }
        }
    }
    client.close().await?;
    Ok(())
}

async fn watch(client: &Client) {
    let mut rx = client.now_playing();
    let end = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        {
            let np = rx.borrow_and_update();
            let pos = np.position(cocoa_time(SystemTime::now()));
            println!(
                "{:?} | {} | {} - {} | {:.0?}/{:.0?} s | app {}",
                np.state,
                np.title.as_deref().unwrap_or("-"),
                np.artist.as_deref().unwrap_or("-"),
                np.album.as_deref().unwrap_or("-"),
                pos,
                np.duration,
                np.app.as_deref().unwrap_or("-"),
            );
        }
        if tokio::time::timeout_at(end, rx.changed()).await.is_err() {
            break;
        }
    }
}
