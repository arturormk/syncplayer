# Syncplayer

[![CI](https://github.com/arturormk/syncplayer/actions/workflows/ci.yml/badge.svg)](https://github.com/arturormk/syncplayer/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENCE.md)

Syncplayer is intended to be a Rust/GStreamer video player for installations in which every host
decodes independent local media against one Master-derived timeline. The accepted architecture is
recorded in [`docs/adr/`](docs/adr/); [`docs/BLUEPRINT.md`](docs/BLUEPRINT.md) provides the broader
design context.

The repository currently contains a risk-reduction prototype, not the production player. It proves
that Linux pipelines can use one exported monotonic clock, join at a future clock release, detect a
stalled Master control process, refresh authoritative transport state, and repair sustained local
drift. `playbin3` and the newline control messages are experimental and are not commitments for the
final playback engine or protocol.

## Prerequisites

- Rust 1.95 (selected by `rust-toolchain.toml`)
- GStreamer 1.20 or newer, including base/playback, pbutils, and network libraries
- GStreamer plugins needed to decode the chosen test media and drive the selected sinks

On Ubuntu, development packages typically include `libgstreamer1.0-dev` and
`libgstreamer-plugins-base1.0-dev`. Runtime plugin selection depends on the media and machine.

## Quickstart

On x86-64 Linux or a Raspberry Pi running a 64-bit OS, run:

```sh
./quickstart.sh
```

The script detects the native platform and offers to install missing build dependencies on Ubuntu,
Debian, and Raspberry Pi OS. It can also install Rust through `rustup`. After a locked release
build, it prints the absolute path to the executable. On systems with less than 2 GiB of RAM, it
limits Cargo to one concurrent job to prevent the Linux OOM killer from terminating `rustc`. Set
`CARGO_BUILD_JOBS` explicitly to override that default.

Builds are native because Syncplayer links to platform-specific GStreamer libraries; the script
does not cross-compile or support 32-bit ARM. GStreamer runtime plugins for codecs, hardware
decoding, video output, and audio output remain dependent on the chosen media and machine.

## Prepare program media

Normalize every video in a program before deploying it. For Linux PCs, run:

```sh
./tools/prepare-video-for-pc.sh -o video-pc.mp4 video.mov
```

For Raspberry Pi players, use the hardware-decoder-compatible profile:

```sh
./tools/prepare-video-for-rpi.sh -o video-rpi.mp4 video.mov
```

Both tools default to an exact 1920x1080 canvas at 25 fps and 48 kHz stereo AAC. They preserve and
normalize an existing audio stream or insert silence when the input has no audio, keeping the
Master's audio-driven clock available across program cue transitions. Aspect ratio is preserved with
black padding. The Raspberry Pi profile also enforces conservative H.264 decoder limits. Both
profiles tone-map HDR sources to SDR Rec.709. See
[`docs/media-preparation.md`](docs/media-preparation.md) for options and the workflow for keeping
host-specific renditions synchronized.

## Build and test

```sh
cargo build --release
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

## Command-line reference

Syncplayer has three subcommands:

```text
syncplayer master [OPTIONS] <PROGRAM>
syncplayer slave  [OPTIONS] <PROGRAM> --master <IP:PORT>
syncplayer validate [OPTIONS] <PROGRAM>
```

Run the corresponding `--help` command for generated details. `--help` and `--version` are also
available as `-h` and `-V`.

### Playback options

These options are accepted by both `master` and `slave`:

| Option | Required | Description |
| --- | :---: | --- |
| `<PROGRAM>` | Yes | Shared version 1 TOML program definition. Each host loads a semantically equivalent copy. |
| `--player-id <NAME>` | No | Logical player identity. The system hostname and then `[aliases]` are used when omitted. |
| `--mute` | No | Silence the selected audio stream. The audio pipeline remains active, so its timing behavior is preserved. |
| `--video-sink <FACTORY>` | No | Override GStreamer's automatically selected video-sink factory, for example `fakesink`. Primarily useful for diagnostics and headless testing. |
| `--audio-sink <FACTORY>` | No | Override GStreamer's automatically selected audio-sink factory, for example `fakesink`. A test sink does not validate physical audio/video timing. |

The program defines ordered logical cues and maps each cue to host-local media:

```toml
version = 1

[program]
id = "museum-show"
loop = true

[[program.cue]]
id = "intro"

[[program.cue]]
id = "main"

[aliases]
"projection-pc-01" = "left-wall"

[players.default.intro]
video = "media/common/intro.mp4"

[players.left-wall.main]
video = "media/left/main.mp4"
```

Player-specific fields override `[players.default.<cue>]` field by field. Relative media paths
resolve against the program file; absolute paths are also accepted. Unknown fields, duplicate cue
IDs, unsupported versions, incomplete mappings, malformed events, and invalid aliases are rejected.
The canonical SHA-256 hash covers the complete semantic program, including every player's mappings
and events, while ignoring TOML comments and formatting. See
[`examples/simple-show.toml`](examples/simple-show.toml) and
[`examples/full-show.toml`](examples/full-show.toml).

Run `syncplayer validate show.toml [--player-id NAME]` on each host before playback. It probes the
resolved player's video and external-audio files, checks durations and cue-relative event times,
warns about suspicious stream-format transitions, and prints the program hash. External audio,
cue-local mute/gain, and timed events are represented and validated but their runtime engines are
not implemented yet; playback rejects a selected mapping that uses them instead of silently
ignoring them. The global `--mute` option remains available.

### Master options

The Master plays from its locally selected pipeline clock, exports that clock, and accepts Slave
control connections.

| Option | Default | Description |
| --- | --- | --- |
| `--bind <IP:PORT>` | `0.0.0.0:46000` | TCP address on which to accept Slave control connections. Use a specific interface address to restrict access. |
| `--clock-port <PORT>` | `46001` | UDP port on which `GstNetTimeProvider` exposes the Master's playback clock. |
| `--release-delay <SECONDS>` | `5` | Positive preparation interval between sending a late-joining or reconnecting Slave a release plan and executing it. |
| `--startup-delay <SECONDS>` | `0` | Non-negative best-effort grace period during which the Master remains prerolled on its first frame so ready Slaves can share the initial release deadline. Use a value greater than `--release-delay`. |
| `--duration-tolerance-ms <MS>` | `100` | Maximum allowed difference between the Master's and a Slave's rendition of each cue. |
| `--duration-mismatch <reject|warn>` | `reject` | Reject an excessive cue-duration difference or accept it with a prominent warning. |

TCP `--bind` and UDP `--clock-port` are separate endpoints; both ports must be reachable from every
Slave. A zero startup delay starts the Master immediately. The Master never waits beyond a non-zero
startup deadline for a slow or absent Slave.

### Slave options

| Option | Required | Description |
| --- | :---: | --- |
| `--master <IP:PORT>` | Yes | Master's reachable TCP control address, matching its `--bind` port. The Master supplies the separate UDP clock port during the connection handshake. |

The Slave retries the initial Master connection once per second until the Master is available. It
first subscribes with its player ID, program ID/hash, software/protocol version, capabilities,
control-channel state endpoint, and probed cue durations. The Master rejects program mismatches,
missing capabilities, duplicate active player IDs, and duration mismatches outside its configured
policy before the Slave calibrates a network-derived clock and accepts a release plan. The Master
sends application
heartbeats every two seconds and requires matching acknowledgements. If no valid heartbeat arrives
for eight seconds—even when TCP still appears connected—the Slave closes the stale connection,
continues playing from its last disciplined clock estimate, and attempts to reconnect in the
background. A successful reconnect always creates and calibrates a new network clock before a
prepared pause/seek/release.

Protocol version 5 is intentionally incompatible with older prototype binaries. Deploy the same
Syncplayer build to the Master and every Slave.

### Logging

Set `RUST_LOG` to control diagnostic output. Normal operation defaults to informational messages;
`RUST_LOG=syncplayer=debug` additionally emits clock release and expected-versus-actual position
telemetry, heartbeat round-trip times, authoritative snapshots, recovery attempts, and cooldown
decisions. Routine heartbeats are not logged at info level.

## Two-host experiment

Create semantically equivalent program files on both machines, mapping the same cue IDs to each
host's local numbered-frame test videos. If either machine is a Raspberry Pi, prepare its rendition
as described above. Validate the program on each host, then start the Master:

```sh
RUST_LOG=syncplayer=debug cargo run --release -- \
  master /path/to/show.toml --player-id master \
  --bind 0.0.0.0:46000 --clock-port 46001 \
  --startup-delay 30
```

Then start the Slave, replacing the address with the Master's LAN address:

```sh
RUST_LOG=syncplayer=debug cargo run --release -- \
  slave /path/to/show.toml --player-id left-wall \
  --master 192.168.1.40:46000 --mute
```

Set `program.loop = true` to repeat the complete cue sequence. Cues are handed to `playbin3` before
the current item finishes for gapless transitions. Compatible codecs, dimensions, frame rates, and
audio formats give the best transition behavior. The control protocol continues to send only one
absolute sequence position plus logical cue index, cue-local position, and program iteration; local
paths never cross the network.

Allow TCP port 46000 and UDP port 46001 through host firewalls. For a headless loopback smoke test,
pass `--video-sink fakesink --audio-sink fakesink` to both processes.

By default, the Master starts playing immediately and behaves like an ordinary local player even
when no Slaves are present. The optional `--startup-delay SECONDS` shown above instead holds the
prerolled first frame until one shared Master-clock deadline. Slaves ready during that grace period
receive the same deadline and start with the Master. Set the startup delay longer than
`--release-delay` (five seconds by default), since a Slave needs that final preparation window.
This is best effort: the Master starts at the deadline even if no Slaves are ready, and late Slaves
catch up through the normal join process.

The Master accepts any number of Slaves while it plays. Each new Slave calibrates its network clock
and receives the Master's current position plus a future release timestamp, allowing it to join
without interrupting the Master or already-connected players.

After joining, a Slave keeps playing from its disciplined local clock if the Master or control
network disappears. It reconnects in the background. When the Master returns, the Slave pauses,
seeks to the newly authoritative position, and resumes at the supplied timestamp; this deliberate
interruption prevents especially noticeable audio divergence after an outage.

While connected, every heartbeat contains a complete authoritative state snapshot. Normal
snapshots refresh the Slave's expected timeline without seeking. A new Master session or transport
generation uses the prepared reconnect path; stale state is ignored. The Master's snapshots are
derived from its logical plan and playback clock rather than its renderer's queried position, so a
local Master rendering stall does not pull healthy Slaves backward.

Each player samples expected versus actual sequence position once per second. Samples are ignored
while paused, preparing, seeking, waiting for release, crossing an ambiguous cue handoff, or
using an unhealthy clock. Syncplayer warns after error exceeds 100 ms for three valid samples and
performs one local prepared repair after error exceeds 250 ms for three valid samples. Repairs have
a ten-second cooldown. These conservative prototype thresholds are intended to recover obvious
stalls; the physical-host experiment must determine whether they should change.

See [`docs/prototypes/clock-sync.md`](docs/prototypes/clock-sync.md) for the measurement procedure,
acceptance criteria, and known boundaries.

## Contributing

This is an early-stage project, but focused bug reports and pull requests are welcome. Read
[`CONTRIBUTING.md`](CONTRIBUTING.md) before proposing a change. Security issues should be reported
privately as described in [`SECURITY.md`](SECURITY.md).

## License

Licensed under either the Apache License, Version 2.0 or the MIT license, at your option. See
[`LICENCE.md`](LICENCE.md) for details.
