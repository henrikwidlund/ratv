# Changelog

All notable changes to this project are listed in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

First release, version 0.1.0.

### Added

- `AppleTv`: one handle that connects with Companion, and with MRP over
  AirPlay 2, and uses the protocol that supports each method. `connect_missing`
  connects a closed protocol again, or one that did not connect.
- Discovery: mDNS scan over IPv4 and IPv6, scan of given addresses when
  multicast is blocked, `scan_until` to end early.
- Pairing with the PIN that the TV shows. One set of credentials works for
  Companion and MRP.
- Remote control: buttons, playback, seek, skip, fast-forward and rewind,
  playback speed, shuffle and repeat, chapters, rating, like and dislike,
  app switcher, control center, power, volume, text input, touch.
- Audio and subtitle languages: turn on and off, turn off subtitles.
- Play queue (`NowPlaying::up_next`): read the items after the current one
  and play one with `play_queue_item`.
- Output devices: list (`Volume::outputs`), set, add and remove, and set
  their volume.
- Apps: list, launch, open a URL. Accounts: list and switch.
- Now playing: title, artist, state, rate, position, duration, artwork,
  audio and subtitle languages, chapters, and the extra fields of the Apple
  TV app (genre, age rating, season, episode, episode title). MRP data
  merged with state and position from Companion, which change sooner.
  Companion data only when MRP is not connected.
- Siri voice input as Opus packets, or as PCM with feature `opus`.
- Features `companion`, `mrp` and `discovery` (on by default), `opus` and
  `sensitive-logs`.
- Logs with `tracing` when feature `companion`, `mrp` or `discovery` is on,
  with a `session` span for each connection and pairing, and for
  `AppleTv::connect` and `connect_missing`.
- Sans-IO protocol crates: `ratv-codec`, `ratv-hap`, `ratv-companion`,
  `ratv-airplay`, `ratv-mrp`, `ratv-discovery`.
- Platforms: Linux, macOS and Windows on x64 and arm64, including
  ARM Linux boards (aarch64).
  Minimum Rust version 1.100.

[Unreleased]: https://github.com/henrikwidlund/ratv/commits/main
