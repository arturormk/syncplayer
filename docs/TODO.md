The repository has implemented the core V1 configuration, playback, synchronization, state
protocol, event, and installation-control layers described by the blueprint. The remaining work is
primarily physical validation, unattended-operation hardening, platform qualification, and
deployment support.

I treated explicitly optional/future items and the V1 exclusions in BLUEPRINT.md (docs/BLUEPRINT.md:3061) as non-blocking.

## Remaining work, in priority order

### P0 — Full shared program configuration

Status: implemented.

Replace the current host-local list of video paths with the blueprint’s logical program model.

Implement:

- [x] Versioned ProgramConfig with program ID, loop policy, ordered cue IDs, aliases, players, defaults, and strongly typed per-cue settings.
- [x] Player identity resolution: --player-id, hostname fallback, then aliases.
- [x] Player-specific mapping with players.default fallback.
- [x] Cue-local video, external audio, mute, gain, and event definitions.
- [x] Relative path resolution against the program file.
- [x] Canonical serialization and whole-program hash.
- [x] A syncplayer validate show.toml command covering structure, mappings, files, durations, events, audio conflicts, and transition warnings.
- [x] Example program files and migration/removal of the prototype playlist schema.

Why first: nearly every remaining subsystem needs logical cue IDs, player IDs, program hashes, and typed events. The current model contains only looping and videos (playlist.rs (src/playlist.rs:9)); the blueprint specifies the richer model in sections 8–13 and 75–77 (BLUEPRINT.md (docs/BLUEPRINT.md:371)).

### P0 — Production subscription and participant registry

Status: implemented.

Once the program model exists, replace the laboratory welcome exchange with a real subscription handshake.

Implement:

- [x] Subscription containing player ID, program ID/hash, protocol/software version, capabilities, and state endpoint.
- [x] Rejection of incompatible program hashes and protocol versions.
- [x] Duplicate active player-ID detection.
- [x] Per-cue duration exchange and configurable mismatch policy.
- [x] A Master subscriber registry with last-seen, readiness, clock status, and remote address.
- [x] Stale-subscriber expiration.
- [x] Status information suitable for the later installation console.

The subscription handshake introduced in version 5 exchanges and validates this information before
clock calibration; the authoritative transport work below advances the combined protocol to version
6. Independent state transport and production encoding remain in the later state-protocol milestone.

### P0 — Authoritative transport and cue progression

Status: implemented.

Introduce a real Master transport controller rather than a single immutable startup plan.

Implement:

- [x] Mutable runtime state containing program/cue identity, cue occurrence, local cue position, duration, paused/playing state, debug loop, session, and generation.
- [x] Generation increments for goto, stop, pause/release discontinuities, loop changes, and program loads.
- [x] Master-owned cue completion and transition decisions.
- [x] Slaves holding or diagnosing early local EOS rather than advancing independently.
- [x] Prepared, exact-clock cue transitions and playlist wrap.
- [x] Defined behavior for local duration differences and early/late EOS.
- [x] Recovery when the Master pipeline reports CLOCK_LOST.

The version 9 snapshot carries complete program/cue transport identity. `TransportController`
projects cue boundaries and playlist wraps from the exported Master clock, while the local decode
branches prepare the known next video and audio URIs. Cue duration mismatches are rejected by
default (or explicitly warned), local Slave EOS is held and diagnosed, and Master clock loss
re-pins the exported clock before resuming. Physical transition limits still require the P2
validation matrix below.

### P0 — Turn the networking prototype into the specified state protocol

Status: implemented.

Preserve the working shared-clock/reconnection mechanism, but separate its responsibilities.

Implement:

- [x] Reliable control channel for subscription, preparation, readiness, and discontinuous operations.
- [x] Independent complete state snapshots, normally unicast, at an experimentally selected rate around the blueprint’s suggested 5–20 Hz.
- [x] Packet ordering and stale/duplicate rejection within a generation.
- [x] Bounded, strongly specified production encoding rather than newline text.
- [x] Configurable control/state ports and keepalive intervals.
- [x] Continue free-running when only state/control traffic is lost.

Protocol version 9 uses bounded binary framing for reliable TCP control and independent complete
binary UDP state snapshots. The Master fans snapshots out to registered unicast endpoints at 10 Hz
by default; Slaves reject wrong-session, stale-generation, duplicate, and reordered packets. Slaves
originate configurable control keepalives, registry expiry is configurable, and loss of either
traffic class leaves playback free-running. See `docs/state-protocol.md` for the wire contract.

### P1 — Complete the local playback/output feature set

Status: implemented.

The local player now provides:

- [x] `--gain-db`, composed with cue-local gain, including positive-gain warnings.
- [x] `--fullscreen`, paired width/height, Wayland/X11/DRM display selection, and contain/cover/stretch.
- [x] `--clock auto|audio|system` on the Master; Slaves remain pinned to the network clock.
- [x] `--hwdec auto|off` plus selected decoder and sink logging.
- [x] External audio files on the same pipeline clock and transport as cue video.
- [x] Strict external video/audio stream and duration validation (50 ms tolerance).
- [x] The same stream, duration, and gain probing for one-cue and multi-cue programs.
- [x] Hybrid gapless playback: `playbin3` coordinates embedded-audio media, while independently
  prepared `uridecodebin3` video/audio branches support programs that map external audio. This
  retains ahead-of-boundary URI preparation without giving up `playbin3`'s internal stream
  coordination for the common case.

Fullscreen and display selection intentionally follow sink capabilities: Wayland display names,
X11 display names, and numeric DRM/KMS connector IDs are supported by the display abstraction, and
an explicit sink that lacks the requested property fails clearly instead of ignoring it.

### P1 — Host-local timed events

Status: implemented.

The local event subsystem now provides:

- [x] `at` and `before_end` parsing into integer nanoseconds.
- [x] Typed process and UDP actions, including textual and hexadecimal payloads.
- [x] Timeline-crossing scheduler.
- [x] Traversal identity across cue and playlist iterations.
- [x] No firing on seek, initial join, or resynchronization.
- [x] Re-eligibility on the next normal traversal.
- [x] Duplicate suppression around jitter/corrections.
- [x] Low-priority asynchronous process executor with safe output handling and child reaping.
- [x] Prepared/reused UDP sockets.
- [x] Structured firing, completion, and failure logs.
- [x] Default suppression during debug loops.

Events advance from the last authoritative shared-clock anchor and continue free-running during
state-traffic loss. The scheduler treats generation changes and local recovery as discontinuities,
while normal cue and playlist traversal changes re-enable each cue's events.

### P1 — Installation transport controls and console

Status: implemented.

Build the transport operations on top of the new controller, then expose them through an opt-in TCP console:

- [x] Status, pause, synchronized play [delay], stop, and goto.
- [x] Human-friendly time parsing.
- [x] In-cue diagnostic loop SECONDS and unloop.
- [x] Ready/preparing status and late-participant policy.
- [x] Optional OSD with player, role, program, cue, time, and connection state.
- [x] Safe bind defaults; console disabled unless explicitly enabled.
- [x] Console text translated into structured operations rather than forwarded to Slaves.
- [x] Structured Slave telemetry centralized by the Master with console statistics and optional
  per-sample CSV logging. The rate estimator exposes systematic playback error in parts per million
  and resets across discontinuities, disconnects, and recoveries.

Console discontinuities reuse the prepared-rejoin and shared-clock release path. A participant that
misses the common deadline rejoins with a new preparation window rather than delaying ready
players. The console binds only when `--console-bind` is supplied; loopback is recommended because
V1 has no authentication.

Events should precede debug looping so event suppression can be designed and tested correctly.

### P2 — Physical synchronization and transition validation

Complete and record the experiments that turn current thresholds and pipeline choices into supported behavior:

- Two or more physical hosts with filmed numbered frames.
- Different host-local renditions of equal logical duration.
- Same-file, A→B, and playlist-wrap transitions.
- Codec, resolution, frame-rate, and audio-format changes.
- Network loss versus Master process restart.
- CPU/I/O pressure and sustained-drift injection.
- Real audio-backed Master clock.
- Master clock-loss recovery.
- Define duration tolerance, drift thresholds, early-EOS behavior, and acceptable transition limits from results.

The procedure exists, but the results section contains no recorded physical test results. The document still calls its correction thresholds provisional.

### P2 — Platform, lifecycle, and unattended-operation hardening

Implement/test:

- Ubuntu x86-64 and Raspberry Pi AArch64 hardware decoding/output.
- Display selection across the supported Wayland/X11/DRM deployment paths.
- [x] Repeatable CPU, memory, thread, file-descriptor, and I/O sampling in the Master and Slave
  test launchers.
- Hours/days of loop and reconnect soak testing using the resource sampler; qualification results
  still need to be recorded.
- Graceful SIGINT/SIGTERM shutdown.
- Clean shutdown of listeners, subscriber tasks, event workers, sockets, and pipelines.
- Packaging or a clearly supported native runtime bundle.
- [x] Structured startup reporting for decoder, sinks, clock, and hardware acceleration, plus
  periodic clock health and video-sink rendered/dropped-frame statistics.

Quickstart scripts and media preparation are already present, but Phase 8 and long-duration qualification remain incomplete (BLUEPRINT.md (docs/BLUEPRINT.md:3282)).

### P3 — Optional deployment modes and later enhancements

Only after unicast V1 is stable:

- Multicast and broadcast snapshot distribution.
- Runtime program switching/loading.
- Targeted OSD.
- Optional console authentication.
- Scheduled ahead-of-time external-controller events.
- Live reload as an explicit new generation.

Automatic Master election, event failover, GUI/web UI, media transfer, cross-cue debug loops, and similar items should remain out of V1.

## What is already substantially implemented

The following should be evolved rather than rebuilt:

- GStreamer local playback and muting.
- Gapless `uridecodebin3` video/audio handoff for playlists and loops.
- Media probing and timeline calculations for playlists.
- Master playback-clock export and Slave network-clock calibration.
- Future-clock preparation/release.
- Unique Master sessions.
- Complete prototype snapshots with sequence/cue-local positions.
- Late join and reconnect.
- Slave free-running during Master loss.
- Snapshot refresh without constant seeking.
- Sustained-drift detection and prepared local repair.
- Multiple concurrent Slaves.
- Linux/Pi build assistance and media-normalization tooling.

The next milestone is physical and long-duration qualification, supported by lifecycle hardening
and operational diagnostics. Optional deployment modes remain deferred until unicast V1 has been
validated on representative installation hardware.
