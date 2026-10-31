# ratv

Rust library to control Apple TV devices on the local network.

Protocols:

- **Companion**: pairing, buttons, apps, power, volume, accounts, text input, touch.
- **MRP over AirPlay 2**: now playing, artwork, playback, languages, chapters,
  Up Next, volume, output devices, Siri voice input.

`AppleTv` connects with both protocols. Each method uses the protocol that
supports it.

The protocol crates do no I/O. They take bytes and return bytes. The `ratv`
crate runs them with tokio.

## Platforms

Linux, macOS and Windows on x64 and arm64, including ARM Linux boards
(`aarch64-unknown-linux-gnu`).
No C dependencies.

Minimum Rust version: 1.88.

## Usage

```toml
[dependencies]
ratv = { git = "https://github.com/henrikwidlund/ratv" }
tokio = { version = "1", features = ["macros", "rt"] }
```

Pair one time with the PIN that the TV shows. Save the credentials.
They work for Companion and MRP. They contain a secret key, so only the
owner must be able to read the file.

```rust
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use ratv::ExposeSecret;
use ratv::companion::{self, PairOptions};

async fn pair() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "192.168.1.20:49153".parse()?; // _companion-link._tcp
    let prompt = companion::pair(addr, &PairOptions::default()).await?;
    let credentials = prompt.finish("1234").await?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600); // only the owner can read and write
    options
        .open("tv.credentials")?
        .write_all(credentials.to_secret_string().expose_secret().as_bytes())?;
    Ok(())
}
```

On Windows the file gets the permissions of its folder. Use a folder in the
user profile.

Find the device and connect:

```rust
use ratv::discovery::{ScanOptions, scan};
use ratv::{AppleTv, ConnectOptions, Credentials, Endpoints, Key};

async fn control() -> Result<(), Box<dyn std::error::Error>> {
    let devices = scan(&ScanOptions::default()).await?;
    let credentials: Credentials = std::fs::read_to_string("tv.credentials")?.parse()?;
    let endpoints = Endpoints::from(&devices[0]);
    let tv = AppleTv::connect(&endpoints, &credentials, &ConnectOptions::default()).await?;

    tv.press(Key::Select).await?;
    tv.pause().await?;
    tv.launch_app("com.netflix.Netflix").await?;
    if let Some(mut np) = tv.now_playing() {
        np.changed().await?;
        println!("{:?}", np.borrow().title);
    }
    Ok(())
}
```

Siri voice input. Audio is Opus, 16 kHz, mono, 20 ms per packet:

```rust
async fn ask_siri(tv: &ratv::AppleTv, packets: Vec<Vec<u8>>) -> Result<(), ratv::Error> {
    let mut siri = tv.start_siri().await?; // presses Siri
    for packet in packets {
        siri.send(packet).await?;          // send each packet after 20 ms
    }
    siri.finish().await                    // the TV then answers
}
```

Audio is kept until the TV records, then sent. With feature `opus`, send
16-bit, 16 kHz, mono PCM in chunks of any size with `push_pcm` instead.

## Features

| Feature          | Default | Content                                                      |
|------------------|---------|--------------------------------------------------------------|
| `companion`      | yes     | Companion protocol                                           |
| `mrp`            | yes     | MRP over AirPlay 2                                           |
| `discovery`      | yes     | mDNS scan                                                    |
| `opus`           | no      | Siri input as PCM                                            |
| `sensitive-logs` | no      | Full messages in trace logs. They can contain personal data. |

`AppleTv` needs `companion` and `mrp`. Small build for ARM boards:

```sh
cargo build -p ratv --no-default-features --features companion --profile embedded --target aarch64-unknown-linux-gnu
```

## Logging

The library logs with [`tracing`](https://docs.rs/tracing). Each connection
and pairing has a `session` span with `protocol`, `id` and `addr`.

- `warn`: the library continues with less function.
- `debug`: connect, disconnect, handshake steps, failed or timed out requests, Siri steps.
- `trace`: each message sent and received, with kind, id and size.

Errors are returned, not logged. Credentials, keys, PINs, text, account data
and audio are not logged. Feature `sensitive-logs` adds full messages at `trace`.

```sh
RUST_LOG=ratv=debug cargo run --example remote -- tv.credentials play
```

## Crates

| Crate            | Content                                        |
|------------------|------------------------------------------------|
| `ratv`           | Public API, tokio                              |
| `ratv-codec`     | TLV8, OPACK, binary plist                      |
| `ratv-hap`       | HAP pairing and encryption                     |
| `ratv-companion` | Companion frames and messages                  |
| `ratv-airplay`   | AirPlay 2 RTSP, encrypted channel, data stream |
| `ratv-mrp`       | MRP messages and player state                  |
| `ratv-discovery` | mDNS queries and TXT records                   |

## Examples

```sh
cargo run --example scan                                                 # list devices
cargo run --example companion -- 192.168.1.20:49153 pair tv.credentials  # pair, save credentials
cargo run --example remote -- tv.credentials play                        # send a command to the first device
cargo run --example remote -- tv.credentials watch                       # print now playing changes
```

Each file in [`crates/ratv/examples`](crates/ratv/examples) lists its commands at the top.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo deny check
```

Fuzz targets: [`fuzz`](fuzz), for example `cargo +nightly fuzz run opack`.
`cargo run -p proto-gen` creates `crates/ratv-mrp/src/generated.rs` from the `.proto` files.

## Credits

- [bunatv](https://github.com/JarekToro/bunatv) by JarekToro (MIT, from its
  `package.json`): main protocol reference. The `.proto` files in
  [`crates/ratv-mrp/proto`](crates/ratv-mrp/proto) are from bunatv. They
  describe Apple's message formats and come from Apple frameworks on macOS.
- [pyatv](https://github.com/postlund/pyatv) (MIT): second protocol reference.

This project is not made or supported by Apple. Apple TV, AirPlay and Siri
are trademarks of Apple Inc.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)).
You can choose one.
