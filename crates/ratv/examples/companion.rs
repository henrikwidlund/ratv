//! Pair with an Apple TV or send a command.
//!
//! ```text
//! cargo run --example companion -- <ip:port> pair
//! cargo run --example companion -- <ip:port> <credentials> <command>
//! ```
//!
//! Commands: `up`, `down`, `left`, `right`, `select`, `menu`, `home`, `play_pause`,
//! `volume_up`, `volume_down`, `on`, `off`, `apps`, `state`, `launch=<bundle id>`.
//! The port is the one of the `_companion-link._tcp` service.
//! Set `RUST_LOG=ratv=trace` to log messages.

use std::io::BufRead;

use ratv::companion::{self, ClientOptions, HidCommand, PairOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args.first().ok_or("missing address")?.parse()?;

    if args.get(1).map(String::as_str) == Some("pair") {
        let prompt = companion::pair(addr, &PairOptions::default()).await?;
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
    let command = args.get(2).ok_or("missing command")?;
    let client = companion::Client::connect(addr, &credentials, &ClientOptions::default()).await?;
    let key = match command.as_str() {
        "up" => Some(HidCommand::Up),
        "down" => Some(HidCommand::Down),
        "left" => Some(HidCommand::Left),
        "right" => Some(HidCommand::Right),
        "select" => Some(HidCommand::Select),
        "menu" => Some(HidCommand::Menu),
        "home" => Some(HidCommand::Home),
        "play_pause" => Some(HidCommand::PlayPause),
        "volume_up" => Some(HidCommand::VolumeUp),
        "volume_down" => Some(HidCommand::VolumeDown),
        _ => None,
    };
    match (key, command.as_str()) {
        (Some(key), _) => client.press(key).await?,
        (None, "on") => client.turn_on().await?,
        (None, "off") => client.turn_off().await?,
        (None, "state") => println!("{:?}", client.power_state().await?),
        (None, "apps") => {
            for app in client.apps().await? {
                println!("{}\t{}", app.bundle_id, app.name);
            }
        }
        (None, cmd) if cmd.starts_with("launch=") => {
            client.launch_app(&cmd["launch=".len()..]).await?;
        }
        (None, cmd) => return Err(format!("unknown command {cmd}").into()),
    }
    client.close().await?;
    Ok(())
}
