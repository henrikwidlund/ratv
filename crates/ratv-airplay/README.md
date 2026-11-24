# ratv-airplay

AirPlay 2 control transport for remote control of an Apple TV.

- Control channel: HTTP/RTSP, pair-verify, then encrypted RTSP requests.
- Event channel: requests from the device.
- Data stream: frames that carry MRP messages.

The crate does no I/O.

Most users want the [`ratv`](https://crates.io/crates/ratv) crate, which runs
this crate with tokio.

## License

MIT or Apache-2.0. You can choose one.
