# ratv-hap

HAP (HomeKit Accessory Protocol) pairing and session encryption, as used by
Apple TV for the Companion protocol and AirPlay 2.

- Pair-setup: first pairing with a PIN (SRP), or transient pairing.
- Pair-verify: key agreement for each connection with stored credentials.
- ChaCha20-Poly1305 frame encryption.

The crate does no I/O. Functions take received bytes and return bytes to send.

Feature `accessory`: the Apple TV side of pairing, for fake devices in tests.

Most users want the [`ratv`](https://crates.io/crates/ratv) crate, which uses
this crate.

## License

MIT or Apache-2.0. You can choose one.
