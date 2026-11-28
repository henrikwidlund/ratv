# ratv

Rust library to control Apple TV devices on the local network.

Contents:

- [What it does](#what-it-does)
- [Supported devices](#supported-devices)
- [Apple TV setup](#apple-tv-setup)
- [Install](#install)
- [Usage](#usage)
  - [Pair](#pair)
  - [Connect](#connect)
  - [Connections](#connections)
  - [What is playing](#what-is-playing)
  - [Siri](#siri)
- [Features](#features)
- [Crates](#crates)
- [Examples](#examples)
- [Logging](#logging)
- [Tested](#tested), [Not tested](#not-tested),
  [Notes about the Apple TV](#notes-about-the-apple-tv) and
  [Notes about apps](#notes-about-apps)
- [Not supported](#not-supported)
- [Details](#details): more options and log spans
  - [Networks](#networks)
  - [Log spans](#log-spans)
- [Development](#development), [Credits](#credits) and [License](#license)

## What it does

`AppleTv` connects with two protocols, Companion and MRP over AirPlay 2.
Each method uses the protocol that supports it:

| Feature                                                                                                   | Protocol                                      |
|-----------------------------------------------------------------------------------------------------------|-----------------------------------------------|
| buttons                                                                                                   | Companion, else MRP                           |
| play, pause, next, previous, skip, fast-forward, rewind                                                   | MRP, else Companion                           |
| seek, stop, shuffle, repeat, speed, rating, like, languages, chapters, queue, artwork, volume state, mute | MRP                                           |
| what is playing                                                                                           | MRP, with Companion merged in, else Companion |
| volume                                                                                                    | MRP, else Companion                           |
| power, apps, accounts, text input, swipe, tap                                                             | Companion                                     |
| Siri                                                                                                      | Companion, else MRP                           |

Some calls are only on the protocol clients from `AppleTv::companion` and
`AppleTv::mrp`, for example the power state and changes to the output
devices.

What each protocol gives. This matters for a build with one protocol only
(see [Features](#features)):

- Companion: keys, touch, power, apps, accounts, text input, volume,
  playback commands (play, pause, next, previous, skip, fast-forward,
  rewind), what is playing (title, state, rate, position, duration, artwork,
  subtitle state), which media controls the app in front supports, Guide
  button support, Siri voice input.
- MRP: keys, what is playing (all fields except the artwork image and
  subtitle state), artwork, playback commands, languages, chapters, play
  queue, volume, output devices, Siri voice input.

What you can do:

- Discovery with mDNS, and pairing with the PIN that the TV shows.
- Remote keys: arrows, select, menu, home, play/pause, volume, Siri, Guide,
  channel up and down, screensaver and more (see `Key`). Double tap, hold,
  and separate key down and up. `app_switcher`, `context_menu` and
  `control_center` open those screens.
- Touch: swipe and tap.
- Power: wake, sleep and the power state.
- Apps: list installed apps, launch an app, open a URL.
- User accounts: list them and switch.
- Text input: read, set, append and clear the focused text field.
- Playback: play, pause, stop, next, previous, seek, skip, fast-forward,
  rewind, playback speed, shuffle, repeat, chapters, audio and subtitle
  languages, rating, like and dislike.
- What is playing: title, artist, album, genre, age rating, season,
  episode, state, rate, position, duration, artwork, subtitle state, and the
  commands that the app supports. The Apple TV app sends some of them in a
  separate archive, see [What is playing](#what-is-playing).
- Play queue of the player (`NowPlaying::up_next`), and play an item from
  it.
- Volume and mute, AirPlay output devices.
- Which media controls the app in front supports, and whether the Guide
  button does something on the current screen.
- Siri voice input.

The protocol crates do no I/O. They take bytes and return bytes. The `ratv`
crate runs them with tokio.

The version is 0.1. The API can change.

## Supported devices

Apple TV HD and Apple TV 4K. MRP needs tvOS 15 or later. Only two Apple TV
4K models were tested, see [Tested](#tested).

Client platforms: Linux, macOS and Windows on x64 and arm64, including ARM
Linux boards (`aarch64-unknown-linux-gnu`). No C dependencies.

Minimum Rust version: 1.100.

## Apple TV setup

- Siri must be on for Siri voice input (Settings > General > Siri).
- The TV lists each pairing in Settings > Remotes and Devices with
  `PairOptions::name` (default: `ratv`). Give each app its own name. When an
  entry is removed there, its credentials stop working. When several entries
  have the same name, removing one can remove the others too.
- The library uses these ports of the TV: Companion on the port of
  `_companion-link._tcp`, AirPlay on the port of `_airplay._tcp` (usually
  7000), and the event and data ports that the TV gives in the AirPlay
  setup. Discovery uses mDNS on UDP port 5353. See [Networks](#networks)
  for other subnets.

## Install

The crates are not on crates.io yet. Use the repository:

```toml
[dependencies]
ratv = { git = "https://github.com/henrikwidlund/ratv" }
tokio = { version = "1", features = ["macros", "rt"] }
```

`cargo doc -p ratv --open` shows the full API.

## Usage

### Pair

Pair one time with the PIN that the TV shows. Save the credentials.
`pair` uses Companion when the TV has it. Credentials from Companion pairing
work for Companion and MRP on recent tvOS versions. They contain a secret
key, so make sure that only the owner can read the file. Get `endpoints`
from `scan`, as in [Connect](#connect).

```rust
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use ratv::{Endpoints, ExposeSecret, PairOptions};

async fn pair(endpoints: &Endpoints) -> Result<(), Box<dyn std::error::Error>> {
    let prompt = ratv::pair(endpoints, &PairOptions::default()).await?;
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

### Connect

Find the device and connect:

```rust
use ratv::discovery::{ScanOptions, scan};
use ratv::{AppleTv, ClientOptions, Credentials, Endpoints, ExposeSecret, Key, SecretString};

async fn control() -> Result<(), Box<dyn std::error::Error>> {
    let devices = scan(&ScanOptions::default()).await?;
    let text = SecretString::from(std::fs::read_to_string("tv.credentials")?);
    let credentials: Credentials = text.expose_secret().parse()?;
    let endpoints = Endpoints::from(&devices[0]);
    let tv = AppleTv::connect(&endpoints, &credentials, &ClientOptions::default()).await?;

    tv.press(Key::Select).await?;
    tv.pause().await?;
    tv.launch_app("com.netflix.Netflix").await?;
    let mut np = tv.now_playing();
    np.changed().await?;
    println!("{:?}", np.borrow().title);
    // Returns when one connection closes. See "Connections".
    let reason = tv.closed().await;
    println!("closed: {reason:?}");
    tv.close().await?;
    Ok(())
}
```

### Connections

- The TV closes an open Companion connection when a new one connects with
  the same credentials. Share one client with `clone()` instead of
  connecting again. `AppleTv` and the clients are fast to clone.
- When a call fails with an error where `Error::is_closed` is true, the
  connection is closed; connect again.
- `AppleTv::closed` returns when one of its connections closes. Then close
  all and connect again, or call `connect_missing`. `connect_missing` also
  adds a protocol that did not connect (`Error::NotConnected`).

### What is playing

`AppleTv::now_playing` uses the MRP data, with state and position from
Companion, which change about 0.5 s sooner. The artwork image and subtitle
state also come from Companion, when both protocols name the same item.

The Apple TV app sends some fields in a separate archive, not in the normal
metadata fields. `NowPlaying` reads them too: genre and age rating for
movies and shows, and season, episode number and episode title for shows.

Apps decide which commands they support, see
[Notes about apps](#notes-about-apps).

### Siri

Siri must be on, see [Apple TV setup](#apple-tv-setup). Audio is Opus,
16 kHz, mono, 20 ms per packet:

```rust
async fn ask_siri(tv: &ratv::AppleTv, packets: Vec<Vec<u8>>) -> Result<(), ratv::Error> {
    let mut siri = tv.start_siri().await?; // presses Siri
    for packet in packets {
        siri.send(packet).await?;          // send each packet after 20 ms
    }
    siri.finish().await                    // the TV then answers
}
```

Siri uses Companion, as the iPhone remote does, and MRP if Companion is not
connected (see [Notes about the Apple TV](#notes-about-the-apple-tv)). With
feature `opus`, send 16-bit, 16 kHz, mono PCM in chunks of any size with
`push_pcm` instead. `Siri` can move to another task.

Over Companion, keep the connection open until Siri has answered, for example
for 20 s after `finish`. On tvOS 27.2.1, when the connection closed a few
seconds after `finish`, Siri did not answer. Over MRP this was not tested.

Over Companion on tvOS 27.2.1, when the TV stopped listening before
`finish` (see `Siri::ended`), the dimmed Siri layer could stay on the screen
after the answer, and no remote could navigate in the app for some minutes.
Control Center still worked. Apple's Siri Remote does the same when it is held
too long. To avoid it, call `finish` before the TV stops listening. Pressing
Siri and then Back (`Key::Menu`) twice removed the Siri layer.

## Features

| Feature          | Default | Content                                                      |
|------------------|---------|--------------------------------------------------------------|
| `companion`      | yes     | Companion protocol                                           |
| `mrp`            | yes     | MRP over AirPlay 2                                           |
| `discovery`      | yes     | mDNS scan                                                    |
| `opus`           | no      | Siri input as PCM (turns on `mrp`)                           |
| `sensitive-logs` | no      | Full messages in trace logs. They can contain personal data. |

`AppleTv` needs `companion` and `mrp`. Small build for ARM boards:

```sh
cargo build -p ratv --no-default-features --features companion --profile embedded --target aarch64-unknown-linux-gnu
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

Diagrams of the crates and the connection flow:
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Examples

```sh
cargo run --example scan                                                 # list devices
cargo run --example companion -- 192.168.1.20:49153 pair tv.credentials  # pair, save credentials
cargo run --example remote -- tv.credentials play                        # send a command to the first Apple TV
cargo run --example remote -- tv.credentials watch                       # print now playing changes
```

Each file in [`crates/ratv/examples`](crates/ratv/examples) lists its
commands at the top.

## Logging

The library logs with [`tracing`](https://docs.rs/tracing).

- `warn`: something went wrong, but the library continues, for example when
  one protocol did not connect, because the connect can succeed without it.
- `debug`: connect, disconnect, handshake steps, failed or timed out
  requests, Siri steps.
- `trace`: each message sent and received, with kind, id and size.

Errors are returned. Some are also logged at `debug`, for example a failed
connect step.

Credentials, keys, PINs, text, account data and audio are not logged. Feature
`sensitive-logs` adds full messages at `trace`.

```sh
RUST_LOG=ratv=debug cargo run --example remote -- tv.credentials play
```

Each connection and pairing has a `session` span, see [Log spans](#log-spans).

## Tested

- Apple TV 4K (AppleTV6,2) with tvOS 26.6.1: all commands of the `remote`
  example, except the ones in [Not tested](#not-tested).
- Apple TV 4K (AppleTV18,1) with tvOS 27.2.1: what is playing, playback
  speed, fast-forward and rewind, power, Guide, Siri over Companion, the play
  queue, separate key down and up, and IPv6 for both protocols.

## Not tested

- `AppleTv::open_url`.
- `mrp::Client::set_output_volume`.
- `AppleTv::set_rating`: no app on the test devices sent a rating range.

## Notes about the Apple TV

- Power: tvOS 27 ignores a wake that comes less than about 5 s after the
  device starts to sleep. `turn_on` waits for this when it knows when the
  sleep started. The power state is `Awake` about 5 s after the wake.
- Siri over MRP worked on tvOS 26.6.1. On tvOS 27.2 the TV did not answer
  in a test with synthetic speech.
- User accounts: on tvOS 26.6.1, `switch_account` switched one time, but
  switching back to the first account failed.
- `Key::TopMenu`: some apps go to the Home Screen, some do nothing.
  `Key::PageUp` did nothing on the test device.

## Notes about apps

Apps decide how they handle commands. Check `NowPlaying::supports` before
you show a control.

- Fast-forward and rewind: Disney+ does not stop when asked, so `AppleTv`
  skips forward and back instead. HBO Max and Netflix do not list them.
  Skip works in all three.
- Playback speed: some apps list `ChangePlaybackRate` without any rates, or
  with scan rates only. Apple Podcasts then skips to the next episode, and
  the Apple TV app changes its scan speed. So `set_playback_rate` sends only
  rates that the app lists, and 1.0 with other listed speeds
  (`NowPlaying::accepts_rate`). Show speeds from `NowPlaying::playback_speeds`,
  and only when it is not empty and `NowPlaying::supports` is true for
  `ChangePlaybackRate`.

## Not supported

The iPhone remote can also do these over Companion. ratv does not do them:

- Up Next in the Apple TV app: read the list, add and remove items, mark
  items as watched. This list is not the play queue in
  `NowPlaying::up_next`.
- Read the Top Shelf items of the app in focus.
- Play a given movie, show or song.

## Details

### Networks

- Discovery and pairing work when the client and the TV are on different
  subnets or VLANs, if an mDNS reflector forwards the multicast traffic.
- If multicast is blocked, set `ScanOptions::targets` to the TV's address with
  `MDNS_PORT`. The scan then asks the TV directly.
- The default scan uses IPv4. A link-local IPv6 address (`fe80::`) needs the
  scope id of its interface, and the scan learns it only over IPv6: set
  `ScanOptions::bind` to an IPv6 address, for example `[::]:0`, and
  `ScanOptions::targets` to `discovery::MDNS_IPV6` and `MDNS_PORT` with the
  scope id of the interface, or to a link-local IPv6 address of the TV with
  its scope id.
  With `[::]:0`, IPv4 targets can stay in the list, if the system allows IPv4
  on IPv6 sockets.
- `discovery::scan_until` ends the scan when a test returns true, for
  example when the wanted device has answered. With multicast, `scan` waits
  for all of `ScanOptions::timeout`.
- `ClientOptions::connect_timeout` limits each connect step (default:
  `timeout`), and `timeout` limits each request after connect. Pairing uses
  `PairOptions::timeout`.
- `ClientOptions::stall_timeout` starts a connect again, one time, when a
  step gets no answer in that time. This helps when packets are lost, for
  example right after the network is ready.

### Log spans

Each connection and pairing has a `session` span with `protocol`, `id` and
`addr`. A connection or pairing on a given stream has no `addr`.
`AppleTv::connect` and `connect_missing` have a `session` span with
`protocol="apple tv"` and `id`. The spans of their connections are inside it.
After `connect_missing`, protocols that stayed open keep the span of the
earlier connect.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo deny check
cargo fmt --manifest-path fuzz/Cargo.toml
cargo clippy --manifest-path fuzz/Cargo.toml --all-targets
```

Fuzz targets: [`fuzz`](fuzz), for example `cargo +nightly fuzz run opack`.
`cargo run -p proto-gen` creates `crates/ratv-mrp/src/generated.rs` from
the `.proto` files.

## Credits

- [bunatv](https://github.com/JarekToro/bunatv) by JarekToro (MIT, from its
  `package.json`): main protocol reference. The `.proto` files in
  [`crates/ratv-mrp/proto`](crates/ratv-mrp/proto) are from bunatv.
- [pyatv](https://github.com/postlund/pyatv) (MIT): second protocol reference.
- Our own tests on Apple TV devices.

This project is not made or supported by Apple. Apple TV, AirPlay and Siri
are trademarks of Apple Inc.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0
([LICENSE-APACHE](LICENSE-APACHE)). You can choose one.
