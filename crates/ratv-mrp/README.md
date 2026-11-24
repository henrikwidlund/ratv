# ratv-mrp

Apple Media Remote Protocol (MRP) messages and player state.

- Reads and writes length-prefixed protobuf messages.
- Builds the messages a remote control sends.
- Tracks what is playing, and reads artwork.
- Feature `opus`: encodes PCM for Siri voice input.

The crate does no I/O.

The `.proto` files are from [bunatv](https://github.com/JarekToro/bunatv) by
JarekToro (MIT, from its `package.json`). Other protocol details come from
[pyatv](https://github.com/postlund/pyatv) and our own tests on Apple TV
devices.

Most users want the [`ratv`](https://crates.io/crates/ratv) crate, which runs
this crate with tokio.

## License

MIT or Apache-2.0. You can choose one.
