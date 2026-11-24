# ratv-companion

Apple TV Companion protocol (also called Rapport).

- Framing and encryption: takes received bytes, returns bytes to send.
- Requests, events and responses as OPACK messages.
- What the device plays, from now playing events.
- Pairing messages and keyboard text.

The crate does no I/O.

Most users want the [`ratv`](https://crates.io/crates/ratv) crate, which runs
this crate with tokio.

## License

MIT or Apache-2.0. You can choose one.
