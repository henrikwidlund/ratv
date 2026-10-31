//! Prints what the device sends about the current item: metadata, chapters,
//! language options, lyrics and keyboard state.
//!
//! ```text
//! cargo run --example item_probe -- <ip:7000> <credentials file> [seconds]
//! ```

#[path = "common/credentials.rs"]
mod credentials;

use std::time::Duration;

use ratv::mrp::{Client, ClientOptions};
use ratv_mrp::Envelope;
use ratv_mrp::proto::{
    Command, CommandInfo, ContentItem, KeyboardMessage, PlaybackQueue, PlaybackQueueRequestMessage,
    SetDefaultSupportedCommandsMessage, SetStateMessage, UpdateContentItemMessage,
};

fn print_item(label: &str, item: &ContentItem) {
    let mut item = item.clone();
    item.artwork_data = None;
    for s in &mut item.sections {
        s.artwork_data = None;
    }
    println!("--- {label}: {item:#?}");
}

fn print_queue(label: &str, queue: &PlaybackQueue) {
    println!(
        "--- {label}: location {:?}, {} items",
        queue.location,
        queue.content_items.len()
    );
    for item in &queue.content_items {
        print_item(label, item);
    }
}

/// Prints commands by number, so commands missing from [`Command`] show too.
fn print_commands(label: &str, commands: &[CommandInfo]) {
    println!("--- {label} commands:");
    for c in commands {
        let name = c
            .command
            .and_then(|n| Command::try_from(n).ok())
            .map_or("?", |c| c.as_str_name());
        println!("  {:?} {name}: {c:?}", c.command);
    }
}

fn print(env: &Envelope) -> Result<(), ratv::Error> {
    if env.is::<SetStateMessage>() {
        let msg: SetStateMessage = env.payload()?;
        if let Some(q) = &msg.playback_queue {
            print_queue("SET_STATE queue", q);
        }
        if let Some(c) = &msg.supported_commands {
            print_commands("SET_STATE", &c.supported_commands);
        }
    } else if env.is::<UpdateContentItemMessage>() {
        let msg: UpdateContentItemMessage = env.payload()?;
        for item in &msg.content_items {
            print_item("UPDATE_CONTENT_ITEM", item);
        }
    } else if env.is::<SetDefaultSupportedCommandsMessage>() {
        let msg: SetDefaultSupportedCommandsMessage = env.payload()?;
        if let Some(c) = &msg.supported_commands {
            print_commands("DEFAULT", &c.supported_commands);
        }
    } else if env.is::<KeyboardMessage>() {
        let msg: KeyboardMessage = env.payload()?;
        println!("--- KEYBOARD: {msg:#?}");
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args.first().ok_or("missing address")?.parse()?;
    let credentials = credentials::load(args.get(1).ok_or("missing credentials file")?)?;
    let seconds = args.get(2).map_or(Ok(10), |s| s.parse())?;

    let client = Client::connect(addr, &credentials, &ClientOptions::default()).await?;
    let mut messages = client.messages();

    let request = PlaybackQueueRequestMessage {
        location: Some(0),
        length: Some(5),
        include_metadata: Some(true),
        include_lyrics: Some(true),
        include_sections: Some(true),
        include_info: Some(true),
        include_language_options: Some(true),
        ..Default::default()
    };
    match client.request(&request).await {
        Ok(env) => {
            println!("=== requested queue (kind {})", env.kind());
            let msg: SetStateMessage = env.payload()?;
            match &msg.playback_queue {
                Some(q) => print_queue("requested", q),
                None => println!("no queue in reply"),
            }
        }
        Err(err) => println!("queue request failed: {err}"),
    }

    println!("=== pushed messages for {seconds} s");
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            env = messages.recv() => match env {
                Ok(env) => print(&env)?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
        }
    }
    client.close().await?;
    Ok(())
}
