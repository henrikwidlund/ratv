# ratv-discovery

mDNS discovery of Apple TV devices.

- DNS message encoding and parsing.
- Queries for multicast (IPv4 and IPv6) or for one host.
- Groups records from many answers into devices, with their services and TXT
  records.

The crate does no I/O.

Most users want the [`ratv`](https://crates.io/crates/ratv) crate, which runs
this crate with tokio.

## License

MIT or Apache-2.0. You can choose one.
