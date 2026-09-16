# Syncplayer

Syncplayer is intended to be a Rust/GStreamer video player for installations in which every host
decodes independent local media against one Master-derived timeline. The accepted architecture is
recorded in [`docs/adr/`](docs/adr/); [`docs/BLUEPRINT.md`](docs/BLUEPRINT.md) provides the broader
design context.

The repository currently contains the first risk-reduction prototype, not the production player.
It proves that two Linux pipelines can use one exported monotonic clock and a future clock release
timestamp. `playbin3` and the newline control messages are experimental and are not commitments for
the final playback engine or protocol.

## Prerequisites

- Rust 1.95 (selected by `rust-toolchain.toml`)
- GStreamer 1.20 or newer, including base/playback and network libraries
- GStreamer plugins needed to decode the chosen test media and drive the selected sinks

On Ubuntu, development packages typically include `libgstreamer1.0-dev` and
`libgstreamer-plugins-base1.0-dev`. Runtime plugin selection depends on the media and machine.

## Build and test

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

## Two-host experiment

Use a numbered-frame test video copied to both machines. Start the Master first:

```sh
RUST_LOG=syncplayer=debug cargo run --release -- \
  master --media /path/to/test.mp4 --bind 0.0.0.0:46000 --clock-port 46001
```

Then start the Slave, replacing the address with the Master's LAN address:

```sh
RUST_LOG=syncplayer=debug cargo run --release -- \
  slave --media /different/local/path/test.mp4 --master 192.168.1.40:46000 --mute
```

Allow TCP port 46000 and UDP port 46001 through host firewalls. For a headless loopback smoke test,
pass `--video-sink fakesink --audio-sink fakesink` to both processes.

The Master waits until the Slave network clock is calibrated and its media is prerolled, then both
pipelines enter `PLAYING` against a release timestamp five seconds in the future. Closing the
control connection after release is intentional: playback must not depend on a live command stream.

See [`docs/prototypes/clock-sync.md`](docs/prototypes/clock-sync.md) for the measurement procedure,
acceptance criteria, and known boundaries.

