# ratv-codec

Wire formats used by Apple TV protocols:

- OPACK: Apple's compact binary format, used by the Companion protocol.
- TLV8: HomeKit type-length-value format, used during pairing.
- Binary property lists, used by AirPlay.

Decoders borrow from the input where possible. The crate does no I/O.

Most users want the [`ratv`](https://crates.io/crates/ratv) crate, which uses
this crate.

## License

MIT or Apache-2.0. You can choose one.
